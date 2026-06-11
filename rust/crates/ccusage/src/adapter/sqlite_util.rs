use std::path::Path;

pub(crate) fn open_readonly(path: &Path) -> sqlite::Result<sqlite::Connection> {
    sqlite::Connection::open_with_flags(path, sqlite::OpenFlags::new().with_read_only())
}

/// `None` on any column read failure, so callers `continue` past the row.
pub(crate) fn read_id_session_data(
    statement: &sqlite::Statement,
) -> Option<(String, String, String)> {
    let id = statement.read::<String, _>(0).ok()?;
    let session_id = statement.read::<String, _>(1).ok()?;
    let data = statement.read::<String, _>(2).ok()?;
    Some((id, session_id, data))
}
