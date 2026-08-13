use std::{
    cell::RefCell,
    ffi::{OsStr, OsString},
    fs,
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, PoisonError},
};

use assert_fs::{
    TempDir,
    fixture::{ChildPath, FileWriteStr, PathChild, PathCreateDir},
};

/// One lock for every guard that mutates the process-global environment.
/// Two separate mutexes let a thread holding one call `std::env::set_var` while
/// a thread holding the other did the same. That races `getenv` in any code
/// reading the environment concurrently, which is why `set_var` is `unsafe` as
/// of edition 2024. One lock is what makes the mutation actually exclusive.
///
/// Reentrancy below is thread-local, so it does NOT extend to child threads: a
/// guard taken inside a spawned thread while the parent still holds one blocks
/// until the parent releases, and deadlocks outright if the parent is joining
/// that thread. Nothing does this today — `cache.rs`'s
/// `concurrent_writers_preserve_all_file_rows` holds a `CacheEnv` across a
/// `thread::scope`, but no closure inside takes a guard. Keep it that way.
static ENV_MUTEX: Mutex<()> = Mutex::new(());

thread_local! {
    /// Reentrancy depth plus the outermost guard. A single test routinely holds
    /// a [`CacheEnv`] and an [`EnvVarsGuard`] at once, so nesting on the same
    /// thread bumps the depth rather than deadlocking on a mutex it already
    /// owns. `std::sync::ReentrantLock` would do this, but is still unstable.
    static ENV_DEPTH: RefCell<(usize, Option<MutexGuard<'static, ()>>)> =
        const { RefCell::new((0, None)) };
}

/// Held for as long as a guard is mutating the process environment.
/// `PhantomData<*const ()>` keeps it `!Send`, because the depth it decrements is
/// thread-local and dropping it on another thread would unlock someone else's
/// nesting. It is `!Sync` too, which the `MutexGuard` it replaced was not, so
/// every struct holding one (`CacheEnv`, `EnvVarGuard`, `EnvVarsGuard`, and the
/// two test-local env structs) narrows to `!Sync` as well. Nothing shares these
/// across threads today; a `&CacheEnv` sent to another thread stops compiling.
///
/// `must_use` restores the diagnostic the `MutexGuard` this replaced carried for
/// free: dropping it on the spot silently unlocks, so an unbound
/// `test_env_lock();` would leave the environment unguarded and still compile.
#[must_use = "dropping the guard immediately releases the environment lock"]
pub struct EnvLockGuard(PhantomData<*const ()>);

impl Drop for EnvLockGuard {
    fn drop(&mut self) {
        // `with` panics once the thread-local is destroyed, and a panic inside
        // drop during an unwind aborts the process. Stack-local guards never hit
        // that today; `try_with` just declines instead of taking the suite down.
        let _ = ENV_DEPTH.try_with(|state| {
            let mut state = state.borrow_mut();
            state.0 -= 1;
            if state.0 == 0 {
                state.1 = None;
            }
        });
    }
}

pub fn test_env_lock() -> EnvLockGuard {
    ENV_DEPTH.with(|state| {
        let mut state = state.borrow_mut();
        if state.0 == 0 {
            // A panicking test poisons the mutex; `()` carries no invariant to
            // protect, so recovering beats failing every later test.
            state.1 = Some(ENV_MUTEX.lock().unwrap_or_else(PoisonError::into_inner));
        }
        state.0 += 1;
    });
    EnvLockGuard(PhantomData)
}

/// An empty temp dir under `prefix`, one per process.
///
/// The pid keeps two cargo processes on one machine (two checkouts, or a test
/// run alongside a mutation run) off the same directory; [`ENV_MUTEX`] only
/// orders threads inside one process. Three test-env structs across three
/// crates need this, so it lives here rather than being pasted into each.
pub fn fresh_temp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()));
    match fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        // Failing to clear means inheriting the previous run's cache.db —
        // ledger rows, WAL and locks included — which does not surface here but
        // later, as an unrelated test asserting the wrong totals.
        Err(err) => panic!("stale temp dir {} not cleared: {err}", dir.display()),
    }
    fs::create_dir_all(&dir).expect("failed to create temp dir");
    dir
}

/// Isolate cache I/O in a temp dir so tests never touch the real cache.
pub struct CacheEnv {
    dir: PathBuf,
    prev_xdg: Option<OsString>,
    _guard: EnvLockGuard,
}

impl CacheEnv {
    /// The redirected `XDG_CACHE_HOME`, for tests that assert on the cache
    /// database the loader writes underneath it.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn new(name: &str) -> Self {
        let guard = test_env_lock();
        let dir = fresh_temp_dir(&format!("ccusage-cache-test-{name}"));
        let prev_xdg = std::env::var_os("XDG_CACHE_HOME");
        unsafe { std::env::set_var("XDG_CACHE_HOME", &dir) };
        Self {
            dir,
            prev_xdg,
            _guard: guard,
        }
    }
}

impl Drop for CacheEnv {
    fn drop(&mut self) {
        match &self.prev_xdg {
            Some(value) => unsafe { std::env::set_var("XDG_CACHE_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_CACHE_HOME") },
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

pub struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
    _guard: EnvLockGuard,
}

impl EnvVarGuard {
    pub fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let guard = test_env_lock();
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self {
            key,
            previous,
            _guard: guard,
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

pub struct EnvVarsGuard {
    previous: Vec<(&'static str, Option<OsString>)>,
    _guard: EnvLockGuard,
}

impl EnvVarsGuard {
    pub fn set_many(vars: impl IntoIterator<Item = (&'static str, Option<OsString>)>) -> Self {
        let guard = test_env_lock();
        let mut previous = Vec::new();
        for (key, value) in vars {
            previous.push((key, std::env::var_os(key)));
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
        Self {
            previous,
            _guard: guard,
        }
    }
}

impl Drop for EnvVarsGuard {
    fn drop(&mut self) {
        for (key, value) in self.previous.drain(..).rev() {
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }
}

pub struct Fixture {
    dir: TempDir,
}

impl Fixture {
    pub fn new() -> Self {
        Self {
            dir: TempDir::new().expect("failed to create temporary fixture directory"),
        }
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    #[must_use]
    pub fn path(&self, path: impl AsRef<Path>) -> PathBuf {
        self.dir.path().join(path)
    }

    fn child(&self, path: impl AsRef<Path>) -> ChildPath {
        self.dir.child(path)
    }

    #[must_use]
    pub fn write_file(&self, path: impl AsRef<Path>, contents: impl AsRef<str>) -> PathBuf {
        let child = self.child(path);
        if let Some(parent) = child.path().parent() {
            fs::create_dir_all(parent).expect("failed to create fixture file parent directory");
        }
        child
            .write_str(contents.as_ref())
            .expect("failed to write fixture file");
        child.path().to_path_buf()
    }

    #[must_use]
    pub fn create_dir_all(&self, path: impl AsRef<Path>) -> PathBuf {
        let child = self.child(path);
        child
            .create_dir_all()
            .expect("failed to create fixture directory");
        child.path().to_path_buf()
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

#[macro_export]
macro_rules! fs_fixture {
    ({ $($path:literal : $contents:expr_2021),* $(,)? }) => {{
        let fixture = $crate::Fixture::new();
        $(
            let _ = fixture.write_file($path, $contents);
        )*
        fixture
    }};
}

#[cfg(test)]
mod tests {
    #[test]
    fn creates_inline_fixture_tree() {
        let fixture = fs_fixture!({
            "projects/example/session.jsonl": "{}\n",
        });

        assert_eq!(
            std::fs::read_to_string(fixture.path("projects/example/session.jsonl")).unwrap(),
            "{}\n"
        );
    }

    #[test]
    fn creates_incremental_fixture_tree() {
        let fixture = fs_fixture!({});
        let _ = fixture.write_file("projects/example/session/chat.jsonl", "{}\n");

        assert!(
            fixture
                .path("projects/example/session/chat.jsonl")
                .is_file()
        );
    }
}
