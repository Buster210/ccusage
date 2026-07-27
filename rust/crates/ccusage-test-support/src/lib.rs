use std::{
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use assert_fs::{
    TempDir,
    fixture::{ChildPath, FileWriteStr, PathChild, PathCreateDir},
};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_lock() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner())
}

/// Shared test lock for `XDG_CACHE_HOME` and `CLAUDE_CONFIG_DIR` isolation.
/// Cache tests and CLI tests both mutate these env vars and must not run
/// concurrently. Deliberately not [`ENV_LOCK`]: a test holding a [`CacheEnv`]
/// still builds `Fixture`/`EnvVarGuard` values, and one non-reentrant mutex for
/// both would deadlock on the second acquire.
pub fn test_env_lock() -> MutexGuard<'static, ()> {
    static CACHE_ENV_LOCK: Mutex<()> = Mutex::new(());
    CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

/// Isolate cache I/O in a temp dir so tests never touch the real cache.
pub struct CacheEnv {
    dir: PathBuf,
    prev_xdg: Option<OsString>,
    _guard: MutexGuard<'static, ()>,
}

impl CacheEnv {
    pub fn new(name: &str) -> Self {
        let guard = test_env_lock();
        let dir = std::env::temp_dir().join(format!("ccusage-cache-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
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
    _guard: MutexGuard<'static, ()>,
}

impl EnvVarGuard {
    pub fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let guard = env_lock();
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
    _guard: MutexGuard<'static, ()>,
}

impl EnvVarsGuard {
    pub fn set_many(vars: impl IntoIterator<Item = (&'static str, Option<OsString>)>) -> Self {
        let guard = env_lock();
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
