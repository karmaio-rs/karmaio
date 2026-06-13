use std::{
    io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    time::{Duration, SystemTime},
};

#[derive(Clone)]
pub(crate) struct Metadata {
    stat: libc::stat,
}

impl Metadata {
    pub(crate) fn from_stat(stat: libc::stat) -> Self {
        Self { stat }
    }

    pub(crate) fn file_type(&self) -> FileType {
        FileType::from_mode(self.stat.st_mode)
    }

    pub(crate) fn is_dir(&self) -> bool {
        self.file_type().is_dir()
    }

    pub(crate) fn is_file(&self) -> bool {
        self.file_type().is_file()
    }

    pub(crate) fn is_symlink(&self) -> bool {
        self.file_type().is_symlink()
    }

    pub(crate) fn len(&self) -> u64 {
        self.stat.st_size as u64
    }

    pub(crate) fn permissions(&self) -> Permissions {
        Permissions::from_mode(self.stat.st_mode as u32)
    }

    pub(crate) fn modified(&self) -> io::Result<SystemTime> {
        timespec(self.stat.st_mtime, self.stat.st_mtime_nsec)
    }

    pub(crate) fn accessed(&self) -> io::Result<SystemTime> {
        timespec(self.stat.st_atime, self.stat.st_atime_nsec)
    }

    pub(crate) fn created(&self) -> io::Result<SystemTime> {
        #[cfg(target_vendor = "apple")]
        {
            timespec(self.stat.st_birthtime, self.stat.st_birthtime_nsec)
        }

        #[cfg(not(target_vendor = "apple"))]
        {
            let _ = self;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "creation time is not available on this platform currently",
            ))
        }
    }
}

impl MetadataExt for Metadata {
    fn dev(&self) -> u64 {
        self.stat.st_dev as u64
    }

    fn ino(&self) -> u64 {
        self.stat.st_ino as u64
    }

    fn mode(&self) -> u32 {
        self.stat.st_mode as u32
    }

    fn nlink(&self) -> u64 {
        self.stat.st_nlink as u64
    }

    fn uid(&self) -> u32 {
        self.stat.st_uid as u32
    }

    fn gid(&self) -> u32 {
        self.stat.st_gid as u32
    }

    fn rdev(&self) -> u64 {
        self.stat.st_rdev as u64
    }

    fn size(&self) -> u64 {
        self.stat.st_size as u64
    }

    fn atime(&self) -> i64 {
        self.stat.st_atime as i64
    }

    fn atime_nsec(&self) -> i64 {
        self.stat.st_atime_nsec as i64
    }

    fn mtime(&self) -> i64 {
        self.stat.st_mtime as i64
    }

    fn mtime_nsec(&self) -> i64 {
        self.stat.st_mtime_nsec as i64
    }

    fn ctime(&self) -> i64 {
        self.stat.st_ctime as i64
    }

    fn ctime_nsec(&self) -> i64 {
        self.stat.st_ctime_nsec as i64
    }

    fn blksize(&self) -> u64 {
        self.stat.st_blksize as u64
    }

    fn blocks(&self) -> u64 {
        self.stat.st_blocks as u64
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct FileType {
    mode: libc::mode_t,
}

impl FileType {
    fn from_mode(mode: libc::mode_t) -> Self {
        Self { mode }
    }

    pub(crate) fn is_dir(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFDIR
    }

    pub(crate) fn is_file(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFREG
    }

    pub(crate) fn is_symlink(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFLNK
    }
}

impl FileTypeExt for FileType {
    fn is_block_device(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFBLK
    }

    fn is_char_device(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFCHR
    }

    fn is_fifo(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFIFO
    }

    fn is_socket(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFSOCK
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Permissions {
    mode: libc::mode_t,
}

impl Permissions {
    pub(crate) fn readonly(&self) -> bool {
        self.mode & 0o222 == 0
    }

    pub(crate) fn set_readonly(&mut self, readonly: bool) {
        if readonly {
            self.mode &= !0o222;
        } else {
            self.mode |= 0o200;
        }
    }
}

impl PermissionsExt for Permissions {
    fn mode(&self) -> u32 {
        self.mode as u32
    }

    fn set_mode(&mut self, mode: u32) {
        self.mode = mode as libc::mode_t;
    }

    fn from_mode(mode: u32) -> Self {
        Self {
            mode: mode as libc::mode_t,
        }
    }
}

fn timespec(secs: libc::time_t, nsecs: libc::c_long) -> io::Result<SystemTime> {
    Ok(SystemTime::UNIX_EPOCH + Duration::new(secs as u64, nsecs as u32))
}
