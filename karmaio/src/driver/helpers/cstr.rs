use std::{io, path::Path};

#[cfg(unix)]
pub(crate) type OsPath = std::ffi::CString;
#[cfg(windows)]
pub(crate) type OsPath = Vec<u16>;

pub(crate) fn cstr(p: &Path) -> io::Result<OsPath> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(std::ffi::CString::new(p.as_os_str().as_bytes())?)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let mut wide: Vec<u16> = p.as_os_str().encode_wide().collect();
        wide.push(0);
        Ok(wide)
    }
}
