#[cfg(target_os = "linux")]
mod linux;
#[cfg(all(unix, not(target_os = "linux")))]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(all(unix, not(target_os = "linux")))]
use unix as sys;
#[cfg(windows)]
use windows as sys;

use std::{io, path::Path, time::SystemTime};

use super::open_options;

/// Metadata information about a file.
///
/// This struct mirrors the API of [`std::fs::Metadata`] but is backed by platform-native stat operations
/// (io_uring `statx` on Linux, `fstat` on macOS, `GetFileInformationByHandle` on Windows) rather than the stdlib.
#[derive(Clone)]
pub struct Metadata(sys::Metadata);

impl Metadata {
    #[cfg(target_os = "linux")]
    pub(crate) fn from_statx(statx: libc::statx) -> Self {
        Self(sys::Metadata::from_statx(statx))
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn from_stat(stat: libc::stat) -> Self {
        Self(sys::Metadata::from_stat(stat))
    }

    #[cfg(windows)]
    pub(crate) fn from_handle(handle: windows_sys::Win32::Foundation::HANDLE) -> io::Result<Self> {
        sys::Metadata::from_handle(handle).map(Self)
    }

    /// Returns the file type for this metadata.
    pub fn file_type(&self) -> FileType {
        FileType(self.0.file_type())
    }

    /// Returns `true` if this metadata is for a directory.
    pub fn is_dir(&self) -> bool {
        self.0.is_dir()
    }

    /// Returns `true` if this metadata is for a regular file.
    pub fn is_file(&self) -> bool {
        self.0.is_file()
    }

    /// Returns `true` if this metadata is for a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.0.is_symlink()
    }

    /// Returns the size of the file, in bytes, this metadata is for.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> u64 {
        self.0.len()
    }

    /// Returns the permissions of the file this metadata is for.
    pub fn permissions(&self) -> Permissions {
        Permissions(self.0.permissions())
    }

    /// Returns the last modification time listed in this metadata.
    pub fn modified(&self) -> io::Result<SystemTime> {
        self.0.modified()
    }

    /// Returns the last access time of this metadata.
    pub fn accessed(&self) -> io::Result<SystemTime> {
        self.0.accessed()
    }

    /// Returns the creation time listed in this metadata.
    pub fn created(&self) -> io::Result<SystemTime> {
        self.0.created()
    }
}

#[cfg(windows)]
impl std::os::windows::fs::MetadataExt for Metadata {
    fn file_attributes(&self) -> u32 {
        self.0.file_attributes()
    }

    fn creation_time(&self) -> u64 {
        self.0.creation_time()
    }

    fn last_access_time(&self) -> u64 {
        self.0.last_access_time()
    }

    fn last_write_time(&self) -> u64 {
        self.0.last_write_time()
    }

    fn file_size(&self) -> u64 {
        self.0.len()
    }

    fn volume_serial_number(&self) -> Option<u32> {
        self.0.volume_serial_number()
    }

    fn number_of_links(&self) -> Option<u32> {
        self.0.number_of_links()
    }

    fn file_index(&self) -> Option<u64> {
        self.0.file_index()
    }

    fn change_time(&self) -> Option<u64> {
        self.0.change_time()
    }
}

#[cfg(unix)]
impl std::os::unix::fs::MetadataExt for Metadata {
    fn dev(&self) -> u64 {
        self.0.dev()
    }

    fn ino(&self) -> u64 {
        self.0.ino()
    }

    fn mode(&self) -> u32 {
        self.0.mode()
    }

    fn nlink(&self) -> u64 {
        self.0.nlink()
    }

    fn uid(&self) -> u32 {
        self.0.uid()
    }

    fn gid(&self) -> u32 {
        self.0.gid()
    }

    fn rdev(&self) -> u64 {
        self.0.rdev()
    }

    fn size(&self) -> u64 {
        self.0.size()
    }

    fn atime(&self) -> i64 {
        self.0.atime()
    }

    fn atime_nsec(&self) -> i64 {
        self.0.atime_nsec()
    }

    fn mtime(&self) -> i64 {
        self.0.mtime()
    }

    fn mtime_nsec(&self) -> i64 {
        self.0.mtime_nsec()
    }

    fn ctime(&self) -> i64 {
        self.0.ctime()
    }

    fn ctime_nsec(&self) -> i64 {
        self.0.ctime_nsec()
    }

    fn blksize(&self) -> u64 {
        self.0.blksize()
    }

    fn blocks(&self) -> u64 {
        self.0.blocks()
    }
}

/// A structure representing a type of file with accessors for each file type.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct FileType(sys::FileType);

impl FileType {
    /// Tests whether this file type represents a directory.
    pub fn is_dir(&self) -> bool {
        self.0.is_dir()
    }

    /// Tests whether this file type represents a regular file.
    pub fn is_file(&self) -> bool {
        self.0.is_file()
    }

    /// Tests whether this file type represents a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.0.is_symlink()
    }
}

// We have to do this because `FileTypeExt` is sealed in Windows
#[cfg(windows)]
impl FileType {
    /// Returns `true` if this file type is a symbolic link that is also a directory.
    pub fn is_symlink_dir(&self) -> bool {
        self.0.is_symlink_dir()
    }

    /// Returns `true` if this file type is a symbolic link that is also a file.
    pub fn is_symlink_file(&self) -> bool {
        self.0.is_symlink_file()
    }
}

#[cfg(unix)]
impl std::os::unix::fs::FileTypeExt for FileType {
    fn is_block_device(&self) -> bool {
        self.0.is_block_device()
    }

    fn is_char_device(&self) -> bool {
        self.0.is_char_device()
    }

    fn is_fifo(&self) -> bool {
        self.0.is_fifo()
    }

    fn is_socket(&self) -> bool {
        self.0.is_socket()
    }
}

/// Representation of the various permissions on a file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Permissions(sys::Permissions);

impl Permissions {
    /// Returns `true` if these permissions describe a readonly (unwritable) file.
    pub fn readonly(&self) -> bool {
        self.0.readonly()
    }

    /// Modifies the readonly flag for this set of permissions.
    ///
    /// This operation does **not** modify the file's attributes.
    pub fn set_readonly(&mut self, readonly: bool) {
        self.0.set_readonly(readonly);
    }

    /// Returns the platform-specific file attributes.
    #[cfg(windows)]
    pub(crate) fn attrs(&self) -> u32 {
        self.0.attrs()
    }
}

/// Returns the metadata for a path in the filesystem.
///
/// This is an async version of [`std::fs::metadata`].
pub async fn metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    open_options::open_path(path).await?.metadata().await
}

#[cfg(unix)]
impl std::os::unix::fs::PermissionsExt for Permissions {
    fn mode(&self) -> u32 {
        self.0.mode()
    }

    fn set_mode(&mut self, mode: u32) {
        self.0.set_mode(mode);
    }

    fn from_mode(mode: u32) -> Self {
        Self(sys::Permissions::from_mode(mode))
    }
}
