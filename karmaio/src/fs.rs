mod dir;
mod dir_builder;
mod file;
mod metadata;
mod open_options;
mod read_dir;
mod read_link;
mod remove_dir_all;
mod symlink;

use std::path::Path;

use crate::{
    buf::Slice,
    driver::ops::Op,
    io::{AsyncReadAtExt, AsyncWriteAt},
};

pub use dir::*;
pub use dir_builder::*;
pub use file::*;
pub use metadata::*;
pub use open_options::*;
pub use read_dir::*;
pub use read_link::*;
pub use remove_dir_all::*;
pub use symlink::*;

pub(crate) async fn asyncify<F, T>(operation: F) -> std::io::Result<T>
where
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    match crate::runtime::spawn_blocking(operation).await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(std::io::Error::other(error.to_string())),
    }
}

/// Reads the entire contents of a file into a bytes vector.
///
/// This is an async version of [`std::fs::read`].
pub async fn read(path: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
    let file = file::File::open(path).await?;
    let (result, contents) = file.read_to_end_at(Vec::new(), 0).await;
    result?;
    Ok(contents)
}

/// Writes a slice as the entire contents of a file.
///
/// This function will create a file if it does not exist, and will entirely replace its contents if it does.
///
/// This is an async version of [`std::fs::write`].
pub async fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    let mut file = file::File::create(path).await?;
    let mut buf = contents.as_ref().to_vec();
    let len = buf.len();
    let mut offset = 0usize;

    while offset < len {
        let slice = Slice::new(buf, offset, len);
        let (res, slice) = file.write_at(slice, offset as u64).await;
        buf = slice.into_inner();
        let n = res?;
        if n == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
        }
        offset += n;
    }

    file.close().await
}

/// Renames a file or directory to a new name, replacing the original file if `to` already exists.
///
/// This is an async version of [`std::fs::rename`].
pub async fn rename<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> std::io::Result<()> {
    Op::rename(from, to)?.await
}

/// Removes a file from the filesystem.
///
/// This is an async version of [`std::fs::remove_file`].
pub async fn remove_file(path: impl AsRef<Path>) -> std::io::Result<()> {
    Op::remove_file(path.as_ref())?.await
}

/// Removes an empty directory.
///
/// This is an async version of [`std::fs::remove_dir`].
pub async fn remove_dir(path: impl AsRef<Path>) -> std::io::Result<()> {
    Op::remove_dir(path.as_ref())?.await
}

/// Creates a new hard link on the filesystem.
///
/// The `link` path will be a hard link pointing to the `original` path.
///
/// This is an async version of [`std::fs::hard_link`].
pub async fn hard_link<P: AsRef<Path>, Q: AsRef<Path>>(original: P, link: Q) -> std::io::Result<()> {
    Op::hardlink(original, link)?.await
}

/// Changes the permissions on a file or directory.
///
/// This is an async version of [`std::fs::set_permissions`].
pub async fn set_permissions(path: impl AsRef<Path>, perm: metadata::Permissions) -> std::io::Result<()> {
    let path = path.as_ref().to_owned();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = perm.mode();
        asyncify(move || {
            rustix::fs::chmod(&path, rustix::fs::Mode::from_raw_mode(mode as _)).map_err(std::io::Error::from)
        })
        .await
    }

    #[cfg(windows)]
    {
        let readonly = perm.readonly();
        asyncify(move || {
            let mut permissions = std::fs::metadata(&path)?.permissions();
            permissions.set_readonly(readonly);
            std::fs::set_permissions(path, permissions)
        })
        .await
    }
}
