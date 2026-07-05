use std::path::Path;

use crate::fs::DirBuilder;

/// Creates a new, empty directory at the provided path.
///
/// This is an async version of [`std::fs::create_dir`].
pub async fn create_dir(path: impl AsRef<Path>) -> std::io::Result<()> {
    DirBuilder::new().create(path).await
}

/// Recursively creates a directory and all of its parent components if they are missing.
///
/// This is an async version of [`std::fs::create_dir_all`].
pub async fn create_dir_all(path: impl AsRef<Path>) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).create(path).await
}
