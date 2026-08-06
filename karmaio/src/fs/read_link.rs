use std::path::{Path, PathBuf};

/// Reads a symbolic link, returning the path to which it points.
///
/// This is an async version of [`std::fs::read_link`].
pub async fn read_link(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    let path = path.as_ref().to_owned();
    super::asyncify(move || std::fs::read_link(path)).await
}
