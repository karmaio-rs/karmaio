// Path → CString / wide-string conversion used by path-based FS ops.
#[cfg(feature = "fs")]
pub(crate) mod cstr;

// Shared file / socket handle tracking; always required by the driver.
pub(crate) mod io_handle;

// Low-level socket wrapper used by `net`.
#[cfg(feature = "net")]
pub(crate) mod socket;
