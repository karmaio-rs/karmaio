use std::{
    io,
    time::{Duration, SystemTime},
};

use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FileAttributeTagInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
};

#[derive(Clone)]
pub(crate) struct Metadata {
    attributes: u32,
    creation_time: windows_sys::Win32::Foundation::FILETIME,
    last_access_time: windows_sys::Win32::Foundation::FILETIME,
    last_write_time: windows_sys::Win32::Foundation::FILETIME,
    file_size: u64,
    reparse_tag: u32,
    volume_serial_number: Option<u32>,
    number_of_links: Option<u32>,
    file_index: Option<u64>,
}

impl Metadata {
    pub(crate) fn from_handle(handle: windows_sys::Win32::Foundation::HANDLE) -> io::Result<Self> {
        unsafe {
            let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
            if GetFileInformationByHandle(handle, &mut info) == 0 {
                return Err(io::Error::last_os_error());
            }

            let mut reparse_tag = 0;
            if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                let mut attr_tag: FILE_ATTRIBUTE_TAG_INFO = std::mem::zeroed();
                if GetFileInformationByHandleEx(
                    handle,
                    FileAttributeTagInfo,
                    (&raw mut attr_tag).cast(),
                    std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
                ) != 0
                    && attr_tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
                {
                    reparse_tag = attr_tag.ReparseTag;
                }
            }

            Ok(Self {
                attributes: info.dwFileAttributes,
                creation_time: info.ftCreationTime,
                last_access_time: info.ftLastAccessTime,
                last_write_time: info.ftLastWriteTime,
                file_size: info.nFileSizeLow as u64 | ((info.nFileSizeHigh as u64) << 32),
                reparse_tag,
                volume_serial_number: Some(info.dwVolumeSerialNumber),
                number_of_links: Some(info.nNumberOfLinks),
                file_index: Some(info.nFileIndexLow as u64 | ((info.nFileIndexHigh as u64) << 32)),
            })
        }
    }

    pub(crate) fn file_type(&self) -> FileType {
        FileType::new(self.attributes, self.reparse_tag)
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
        self.file_size
    }

    pub(crate) fn permissions(&self) -> Permissions {
        Permissions { attrs: self.attributes }
    }

    pub(crate) fn modified(&self) -> io::Result<SystemTime> {
        filetime_to_system_time(&self.last_write_time)
    }

    pub(crate) fn accessed(&self) -> io::Result<SystemTime> {
        filetime_to_system_time(&self.last_access_time)
    }

    pub(crate) fn created(&self) -> io::Result<SystemTime> {
        filetime_to_system_time(&self.creation_time)
    }

    pub(crate) fn file_attributes(&self) -> u32 {
        self.attributes
    }

    pub(crate) fn creation_time(&self) -> u64 {
        filetime_to_u64(&self.creation_time)
    }

    pub(crate) fn last_access_time(&self) -> u64 {
        filetime_to_u64(&self.last_access_time)
    }

    pub(crate) fn last_write_time(&self) -> u64 {
        filetime_to_u64(&self.last_write_time)
    }

    pub(crate) fn volume_serial_number(&self) -> Option<u32> {
        self.volume_serial_number
    }

    pub(crate) fn number_of_links(&self) -> Option<u32> {
        self.number_of_links
    }

    pub(crate) fn file_index(&self) -> Option<u64> {
        self.file_index
    }

    pub(crate) fn change_time(&self) -> Option<u64> {
        None
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct FileType {
    is_directory: bool,
    is_symlink: bool,
}

impl FileType {
    fn new(attributes: u32, reparse_tag: u32) -> Self {
        let is_directory = attributes & FILE_ATTRIBUTE_DIRECTORY != 0;
        let is_symlink = {
            let is_reparse_point = attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0;
            let is_reparse_tag_name_surrogate = reparse_tag & 0x2000_0000 != 0;
            is_reparse_point && is_reparse_tag_name_surrogate
        };

        Self {
            is_directory,
            is_symlink,
        }
    }

    pub(crate) fn is_dir(&self) -> bool {
        !self.is_symlink && self.is_directory
    }

    pub(crate) fn is_file(&self) -> bool {
        !self.is_symlink && !self.is_directory
    }

    pub(crate) fn is_symlink(&self) -> bool {
        self.is_symlink
    }

    pub(crate) fn is_symlink_dir(&self) -> bool {
        self.is_symlink && self.is_directory
    }

    pub(crate) fn is_symlink_file(&self) -> bool {
        self.is_symlink && !self.is_directory
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Permissions {
    attrs: u32,
}

impl Permissions {
    pub(crate) fn readonly(&self) -> bool {
        self.attrs & FILE_ATTRIBUTE_READONLY != 0
    }

    pub(crate) fn set_readonly(&mut self, readonly: bool) {
        if readonly {
            self.attrs |= FILE_ATTRIBUTE_READONLY;
        } else {
            self.attrs &= !FILE_ATTRIBUTE_READONLY;
        }
    }

    pub(crate) fn attrs(&self) -> u32 {
        self.attrs
    }
}

fn filetime_to_u64(ft: &windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (ft.dwLowDateTime as u64) | ((ft.dwHighDateTime as u64) << 32)
}

fn filetime_to_system_time(ft: &windows_sys::Win32::Foundation::FILETIME) -> io::Result<SystemTime> {
    const INTERVALS_PER_SEC: u64 = 10_000_000;
    const SECS_BETWEEN_EPOCHS: u64 = 11644473600;

    let intervals = filetime_to_u64(ft);
    let secs = intervals / INTERVALS_PER_SEC;
    let subsec_intervals = intervals % INTERVALS_PER_SEC;
    let nanos = subsec_intervals * 100;

    Ok(SystemTime::UNIX_EPOCH + Duration::new(secs.saturating_sub(SECS_BETWEEN_EPOCHS), nanos as u32))
}
