use std::path::Path;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_INVALID_PARAMETER, GENERIC_READ, GENERIC_WRITE},
    Storage::FileSystem::{
        CREATE_ALWAYS, CREATE_NEW, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_OVERLAPPED, FILE_GENERIC_WRITE,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_WRITE_DATA, OPEN_ALWAYS, OPEN_EXISTING,
        TRUNCATE_EXISTING,
    },
};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use crate::{driver::ops::Op, fs::File};

/// Options and flags which can be used to configure how a file is opened.
///
/// This builder exposes the ability to configure how a [`File`] is opened and what operations are permitted on the open file.
/// The [`File::open`] and [`File::create`] methods are aliases for commonly used options using this builder.
///
/// Generally speaking, when using `OpenOptions`,
/// you'll first call [`OpenOptions::new`], then chain calls to methods to set each option,
/// then call [`OpenOptions::open`], passing the path of the file you're trying to open.
/// This will give you a [`std::io::Result`] with a [`File`] inside that you can further operate on.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,

    #[cfg(unix)]
    pub(crate) mode: libc::mode_t,
    #[cfg(unix)]
    pub(crate) custom_flags: libc::c_int,

    #[cfg(windows)]
    pub(crate) access_mode: Option<u32>,
    #[cfg(windows)]
    pub(crate) share_mode: u32,
    #[cfg(windows)]
    pub(crate) custom_flags: u32,
    #[cfg(windows)]
    pub(crate) attributes: u32,
    #[cfg(windows)]
    pub(crate) security_qos_flags: u32,
}

impl OpenOptions {
    /// Creates a blank new set of options ready for configuration.
    ///
    /// All options are initially set to `false`.
    pub fn new() -> OpenOptions {
        OpenOptions {
            // generic
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,

            #[cfg(unix)]
            mode: 0o666,
            #[cfg(unix)]
            custom_flags: 0,

            #[cfg(windows)]
            access_mode: None,
            #[cfg(windows)]
            share_mode: FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            #[cfg(windows)]
            custom_flags: 0,
            #[cfg(windows)]
            attributes: 0,
            #[cfg(windows)]
            security_qos_flags: 0,
        }
    }

    /// Sets the option for read access.
    ///
    /// This option, when true, will indicate that the file should be `read`-able if opened.
    pub fn read(&mut self, read: bool) -> &mut OpenOptions {
        self.read = read;
        self
    }

    /// Sets the option for write access.
    ///
    /// This option, when true, will indicate that the file should be `write`-able if opened.
    ///
    /// If the file already exists, any write calls on it will overwrite its contents, without truncating it.
    pub fn write(&mut self, write: bool) -> &mut OpenOptions {
        self.write = write;
        self
    }

    /// Sets the option for the append mode.
    ///
    /// This option, when true, means that writes will append to a file instead of overwriting previous contents.
    /// Note that setting `.write(true).append(true)` has the same effect as setting only `.append(true)`.
    ///
    /// For most filesystems, the operating system guarantees that all writes are atomic:
    /// no writes get mangled because another process writes at the same time.
    ///
    /// ## Note
    ///
    /// This function doesn't create the file if it doesn't exist. Use the [`OpenOptions::create`] method to do so.
    pub fn append(&mut self, append: bool) -> &mut OpenOptions {
        self.append = append;
        self
    }

    /// Sets the option for truncating a previous file.
    ///
    /// If a file is successfully opened with this option set it will truncate the file to 0 length if it already exists.
    ///
    /// The file must be opened with write access for truncate to work.
    pub fn truncate(&mut self, truncate: bool) -> &mut OpenOptions {
        self.truncate = truncate;
        self
    }

    /// Sets the option to create a new file, or open it if it already exists.
    ///
    /// In order for the file to be created, [`OpenOptions::write`] or [`OpenOptions::append`] access must be used.
    pub fn create(&mut self, create: bool) -> &mut OpenOptions {
        self.create = create;
        self
    }

    /// Sets the option to create a new file, failing if it already exists.
    ///
    /// No file is allowed to exist at the target location, also no (dangling) symlink.
    /// In this way, if the call succeeds, the file returned is guaranteed to be new.
    ///
    /// This option is useful because it is atomic.
    /// Otherwise between checking whether a file exists and creating a new one,
    /// the file may have been created by another process (a TOCTOU race condition / attack).
    ///
    /// If `.create_new(true)` is set, [`.create()`] and [`.truncate()`] are ignored.
    ///
    /// The file must be opened with write or append access in order to create a new file.
    ///
    /// [`.create()`]: OpenOptions::create
    /// [`.truncate()`]: OpenOptions::truncate
    pub fn create_new(&mut self, create_new: bool) -> &mut OpenOptions {
        self.create_new = create_new;
        self
    }

    /// Opens a file at `path` with the options specified by `self`.
    pub async fn open(&self, path: impl AsRef<Path>) -> std::io::Result<File> {
        Op::open(path.as_ref(), self.clone())?.await
    }
}

#[cfg(unix)]
impl OpenOptions {
    pub(crate) fn access_mode(&self) -> std::io::Result<libc::c_int> {
        match (self.read, self.write, self.append) {
            (true, false, false) => Ok(libc::O_RDONLY),
            (false, true, false) => Ok(libc::O_WRONLY),
            (true, true, false) => Ok(libc::O_RDWR),
            (false, _, true) => Ok(libc::O_WRONLY | libc::O_APPEND),
            (true, _, true) => Ok(libc::O_RDWR | libc::O_APPEND),
            (false, false, false) => Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
        }
    }

    pub(crate) fn creation_mode(&self) -> std::io::Result<libc::c_int> {
        match (self.write, self.append) {
            (true, false) => {}
            (false, false) => {
                if self.truncate || self.create || self.create_new {
                    return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
                }
            }
            (_, true) => {
                if self.truncate && !self.create_new {
                    return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
                }
            }
        }

        Ok(match (self.create, self.truncate, self.create_new) {
            (false, false, false) => 0,
            (true, false, false) => libc::O_CREAT,
            (false, true, false) => libc::O_TRUNC,
            (true, true, false) => libc::O_CREAT | libc::O_TRUNC,
            (_, _, true) => libc::O_CREAT | libc::O_EXCL,
        })
    }
}

#[cfg(windows)]
impl OpenOptions {
    pub(crate) fn access_mode(&self) -> std::io::Result<u32> {
        match (self.read, self.write, self.append, self.access_mode) {
            (.., Some(mode)) => Ok(mode),
            (true, false, false, None) => Ok(GENERIC_READ),
            (false, true, false, None) => Ok(GENERIC_WRITE),
            (true, true, false, None) => Ok(GENERIC_READ | GENERIC_WRITE),
            (false, _, true, None) => Ok(FILE_GENERIC_WRITE & !FILE_WRITE_DATA),
            (true, _, true, None) => Ok(GENERIC_READ | (FILE_GENERIC_WRITE & !FILE_WRITE_DATA)),
            (false, false, false, None) => Err(std::io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER as _)),
        }
    }

    pub(crate) fn creation_mode(&self) -> std::io::Result<u32> {
        match (self.write, self.append) {
            (true, false) => {}
            (false, false) => {
                if self.truncate || self.create || self.create_new {
                    return Err(std::io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER as _));
                }
            }
            (_, true) => {
                if self.truncate && !self.create_new {
                    return Err(std::io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER as _));
                }
            }
        }

        Ok(match (self.create, self.truncate, self.create_new) {
            (false, false, false) => OPEN_EXISTING,
            (true, false, false) => OPEN_ALWAYS,
            (false, true, false) => TRUNCATE_EXISTING,
            (true, true, false) => CREATE_ALWAYS,
            (_, _, true) => CREATE_NEW,
        })
    }

    pub(crate) fn get_flags_and_attributes(&self) -> u32 {
        self.custom_flags
            | self.attributes
            | self.security_qos_flags
            | FILE_FLAG_OVERLAPPED
            | if self.create_new {
                FILE_FLAG_OPEN_REPARSE_POINT as _
            } else {
                0
            }
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unix)]
impl OpenOptionsExt for OpenOptions {
    fn mode(&mut self, mode: u32) -> &mut OpenOptions {
        self.mode = mode as libc::mode_t;
        self
    }

    fn custom_flags(&mut self, flags: i32) -> &mut OpenOptions {
        self.custom_flags = flags;
        self
    }
}

#[cfg(windows)]
impl OpenOptionsExt for OpenOptions {
    fn access_mode(&mut self, access: u32) -> &mut Self {
        self.access_mode = Some(access);
        self
    }

    fn share_mode(&mut self, val: u32) -> &mut Self {
        self.share_mode = val;
        self
    }

    fn custom_flags(&mut self, flags: u32) -> &mut Self {
        self.custom_flags = flags;
        self
    }

    fn attributes(&mut self, val: u32) -> &mut Self {
        self.attributes = val;
        self
    }

    fn security_qos_flags(&mut self, flags: u32) -> &mut Self {
        self.security_qos_flags = flags;
        self
    }
}
