use std::path::Path;

/// Removes a directory and all of its contents.
///
/// This is an async version of [`std::fs::remove_dir_all`].
pub async fn remove_dir_all(path: impl AsRef<Path>) -> std::io::Result<()> {
    let path = path.as_ref().to_owned();
    super::asyncify(move || std::fs::remove_dir_all(path)).await
}
