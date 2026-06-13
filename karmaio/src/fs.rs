mod dir;
mod dir_builder;
mod file;
mod metadata;
mod open_options;
mod symlink;

use std::path::Path;

use crate::{
    buf::Slice,
    driver::ops::Op,
    io::{AsyncReadAt, AsyncWriteAt},
};

pub use dir::*;
pub use dir_builder::*;
pub use file::*;
pub use metadata::*;
pub use open_options::*;
pub use symlink::*;

const DEFAULT_BUF_SIZE: usize = 8192;

/// Reads the entire contents of a file into a bytes vector.
///
/// This is an async version of [`std::fs::read`].
pub async fn read(path: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
    let mut file = file::File::open(path).await?;
    let metadata = file.metadata().await?;
    let len = metadata.len();

    let mut result = Vec::new();
    if len <= usize::MAX as u64 {
        result.reserve(len as usize);
    }

    let mut offset = 0u64;
    loop {
        let buf = vec![0u8; DEFAULT_BUF_SIZE];
        let (res, buf) = file.read_at(buf, offset).await;
        let n = res?;
        if n == 0 {
            break;
        }
        result.extend_from_slice(&buf[..n]);
        offset += n as u64;
    }

    Ok(result)
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
    open_options::open_path(path).await?.set_permissions(perm).await
}
