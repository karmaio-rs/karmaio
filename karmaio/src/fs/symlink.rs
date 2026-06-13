use std::path::Path;

use crate::driver::ops::Op;

/// Creates a new symbolic link on the filesystem.
/// The dst path will be a symbolic link pointing to the src path.
///
/// This is an async version of std::os::unix::fs::symlink.
#[cfg(unix)]
pub async fn symlink<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> std::io::Result<()> {
    Op::symlink(original, link)?.await
}

/// Creates a new symbolic link on the filesystem.
/// The dst path will be a symbolic link pointing to the src path.
/// This is an async version of std::os::windows::fs::symlink_file.
#[cfg(target_os = "windows")]
pub async fn symlink_file<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> std::io::Result<()> {
    Op::symlink_file(original, link)?.await
}

/// Creates a new symbolic link on the filesystem.
/// The dst path will be a symbolic link pointing to the src path.
/// This is an async version of std::os::windows::fs::symlink_dir.
#[cfg(target_os = "windows")]
pub async fn symlink_dir<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> std::io::Result<()> {
    Op::symlink_dir(original, link)?.await
}
