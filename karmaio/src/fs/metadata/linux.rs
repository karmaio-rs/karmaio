use std::{
    io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    time::SystemTime,
};

#[derive(Clone)]
pub(crate) struct Metadata {
    statx: libc::statx,
}

impl Metadata {
    pub(crate) fn from_statx(statx: libc::statx) -> Self {
        Self { statx }
    }

    pub(crate) fn file_type(&self) -> FileType {
        FileType::from_mode(self.statx.stx_mode as libc::mode_t)
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
        self.statx.stx_size
    }

    pub(crate) fn permissions(&self) -> Permissions {
        Permissions::from_mode(self.statx.stx_mode as libc::mode_t)
    }

    pub(crate) fn modified(&self) -> io::Result<SystemTime> {
        timestamp(self.statx.stx_mtime)
    }

    pub(crate) fn accessed(&self) -> io::Result<SystemTime> {
        timestamp(self.statx.stx_atime)
    }

    pub(crate) fn created(&self) -> io::Result<SystemTime> {
        if self.statx.stx_mask & libc::STATX_BTIME == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "creation time is not available for the filesystem",
            ));
        }

        timestamp(self.statx.stx_btime)
    }
}

impl MetadataExt for Metadata {
    fn dev(&self) -> u64 {
        rustix::fs::makedev(self.statx.stx_dev_major, self.statx.stx_dev_minor) as u64
    }

    fn ino(&self) -> u64 {
        self.statx.stx_ino
    }

    fn mode(&self) -> u32 {
        self.statx.stx_mode as u32
    }

    fn nlink(&self) -> u64 {
        self.statx.stx_nlink as u64
    }

    fn uid(&self) -> u32 {
        self.statx.stx_uid
    }

    fn gid(&self) -> u32 {
        self.statx.stx_gid
    }

    fn rdev(&self) -> u64 {
        rustix::fs::makedev(self.statx.stx_rdev_major, self.statx.stx_rdev_minor) as u64
    }

    fn size(&self) -> u64 {
        self.statx.stx_size
    }

    fn atime(&self) -> i64 {
        self.statx.stx_atime.tv_sec as i64
    }

    fn atime_nsec(&self) -> i64 {
        self.statx.stx_atime.tv_nsec as i64
    }

    fn mtime(&self) -> i64 {
        self.statx.stx_mtime.tv_sec as i64
    }

    fn mtime_nsec(&self) -> i64 {
        self.statx.stx_mtime.tv_nsec as i64
    }

    fn ctime(&self) -> i64 {
        self.statx.stx_ctime.tv_sec as i64
    }

    fn ctime_nsec(&self) -> i64 {
        self.statx.stx_ctime.tv_nsec as i64
    }

    fn blksize(&self) -> u64 {
        self.statx.stx_blksize as u64
    }

    fn blocks(&self) -> u64 {
        self.statx.stx_blocks as u64
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct FileType {
    mode: libc::mode_t,
}

impl FileType {
    pub(crate) fn from_std(file_type: std::fs::FileType) -> Self {
        let mode = if file_type.is_symlink() {
            libc::S_IFLNK
        } else if file_type.is_dir() {
            libc::S_IFDIR
        } else if file_type.is_file() {
            libc::S_IFREG
        } else if file_type.is_block_device() {
            libc::S_IFBLK
        } else if file_type.is_char_device() {
            libc::S_IFCHR
        } else if file_type.is_fifo() {
            libc::S_IFIFO
        } else if file_type.is_socket() {
            libc::S_IFSOCK
        } else {
            0
        };
        Self::from_mode(mode as libc::mode_t)
    }

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

fn timestamp(ts: libc::statx_timestamp) -> io::Result<SystemTime> {
    super::system_time_from_unix(ts.tv_sec, ts.tv_nsec)
}
