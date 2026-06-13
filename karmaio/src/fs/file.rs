use std::path::Path;

#[cfg(unix)]
use std::os::fd::IntoRawFd;
#[cfg(windows)]
use std::os::windows::io::IntoRawHandle;

use crate::{
    buf::{BoundedIoBuf, BoundedIoBufMut, BufResult},
    driver::{helpers::io_handle::SharedIoHandle, ops::Op},
    fs::{Metadata, OpenOptions, Permissions},
    io::{AsyncReadAt, AsyncWriteAt},
};

/// A reference to an open file on the filesystem.
///
/// An instance of a `File` can be read and/or written depending on what options it was opened with.
/// The `File` type provides **positional** read and write operations.
/// The file does not maintain an internal cursor.
/// The caller is required to specify an offset when issuing an operation.
///
/// While files are automatically closed when they go out of scope,
/// the operation happens asynchronously in the background.
/// It is recommended to call the `close()` function in order to guarantee
/// that the file successfully closed before exiting the scope.
/// Closing a file does not guarantee writes have persisted to disk. Use [`sync_all`]
/// to ensure all writes have reached the filesystem.
pub struct File {
    /// Open file descriptor
    pub(crate) handle: SharedIoHandle,
}

impl File {
    /// Opens a file in write-only mode.
    ///
    /// This function will create a file if it does not exist, and will truncate it if it does.
    ///
    /// See the [`OpenOptions::open`] function for more error details.
    pub async fn create(path: impl AsRef<Path>) -> std::io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .await
    }

    /// Creates a new file in read-write mode; error if the file exists.
    ///
    /// This function will create a file if it does not exist, or return an error if it does.
    /// This way, if the call succeeds, the file returned is guaranteed to be new.
    /// If a file exists at the target location, creating a new file will fail with an error.
    ///
    /// This option is useful because it is atomic. Otherwise between checking whether a file exists and creating a new one,
    /// the file may have been created by another process (a TOCTOU race condition / attack).
    ///
    /// See the [`OpenOptions::open`] function for more error details.
    pub async fn create_new(path: impl AsRef<Path>) -> std::io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .await
    }

    /// Attempts to open a file in read-only mode.
    ///
    /// See the [`OpenOptions::open`] method for more details.
    ///
    /// # Errors
    ///
    /// This function will return an error if `path` does not already exist.
    /// Other errors may also be returned according to [`OpenOptions::open`].
    pub async fn open(path: impl AsRef<Path>) -> std::io::Result<File> {
        OpenOptions::new().read(true).open(path).await
    }

    /// Closes the file or returns an error
    ///
    /// The programmer has the choice of calling this asynchronous close and waiting for the result
    /// or simply letting the file go out of scope and having the library close the file descriptor automatically and synchronously.
    ///
    /// It's OK to drop the [`File`] directly without calling `close`, but the file may not be closed immediately.
    pub async fn close(mut self) -> std::io::Result<()> {
        self.handle.close().await
    }

    /// Attempts to sync all OS-internal metadata to disk.
    ///
    /// This function will attempt to ensure that all in-memory data reaches the filesystem before completing.
    ///
    /// This can be used to handle errors that would otherwise only be caught when the `File` is closed.
    /// Dropping a file will ignore errors in synchronizing this in-memory data.
    pub async fn sync_all(&self) -> std::io::Result<()> {
        Op::sync(&self.handle)?.await
    }

    /// Attempts to sync file data to disk.
    ///
    /// This method is similar to [`sync_all`], except that it may not synchronize file metadata to the filesystem.
    ///
    /// This is intended for use cases that must synchronize content, but don't need the metadata on disk.
    /// The goal of this method is to reduce disk operations.
    ///
    /// Note that some platforms may simply implement this in terms of [`sync_all`].
    ///
    /// [`sync_all`]: File::sync_all
    pub async fn sync_data(&self) -> std::io::Result<()> {
        Op::sync_data(&self.handle)?.await
    }

    /// Queries metadata about the underlying file.
    ///
    /// This function uses the underlying driver (io_uring / kqueue / iocp)
    /// to perform the metadata lookup asynchronously where possible.
    pub async fn metadata(&self) -> std::io::Result<Metadata> {
        Op::stat(&self.handle)?.await
    }

    /// Truncates or extends the underlying file, updating the size of this file to become `size`.
    pub async fn set_len(&self, size: u64) -> std::io::Result<()> {
        Op::truncate(&self.handle, size)?.await
    }

    /// Changes the permissions on the underlying file.
    pub async fn set_permissions(&self, perm: Permissions) -> std::io::Result<()> {
        Op::set_permissions(&self.handle, perm)?.await
    }
}

#[cfg(unix)]
impl From<std::fs::File> for File {
    fn from(file: std::fs::File) -> Self {
        File::from(SharedIoHandle::new(file.into_raw_fd()))
    }
}

#[cfg(windows)]
impl From<std::fs::File> for File {
    fn from(file: std::fs::File) -> Self {
        // Transfer ownership of the underlying handle out of the std File
        // so that SharedIoHandle becomes responsible for closing it.
        let handle = file.into_raw_handle();
        File::from(SharedIoHandle::new_file(handle))
    }
}

impl From<SharedIoHandle> for File {
    fn from(handle: SharedIoHandle) -> Self {
        Self { handle }
    }
}

impl AsyncReadAt for File {
    async fn read_at<B: BoundedIoBufMut>(&mut self, buf: B, pos: u64) -> BufResult<usize, B> {
        Op::read_at(&self.handle, buf, pos)
            .expect("Failed to submit read operation (no runtime or driver error)")
            .await
    }

    async fn read_vectored_at<B: BoundedIoBufMut>(&mut self, bufs: Vec<B>, pos: u64) -> BufResult<usize, Vec<B>> {
        Op::readv(&self.handle, bufs, pos)
            .expect("Failed to submit readv operation (no runtime or driver error)")
            .await
    }
}

impl AsyncWriteAt for File {
    async fn write_at<B: BoundedIoBuf>(&mut self, buf: B, pos: u64) -> BufResult<usize, B> {
        Op::write_at(&self.handle, buf, pos)
            .expect("Failed to submit write operation (no runtime or driver error)")
            .await
    }

    async fn write_vectored_at<B: BoundedIoBuf>(&mut self, bufs: Vec<B>, pos: u64) -> BufResult<usize, Vec<B>> {
        Op::writev(&self.handle, bufs, pos)
            .expect("Failed to submit writev operation (no runtime or driver error)")
            .await
    }
}
