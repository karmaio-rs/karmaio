// Path → CString / wide-string conversion used by path-based FS ops.
#[cfg(feature = "fs")]
pub(crate) mod cstr;

// Handle associated with the I/O driver (IOCP-specific behavior).
pub(crate) mod attached_handle;

// Typed shared OS-resource handle (`SharedIoHandle<T>`); always required by the driver.
pub(crate) mod io_handle;

// Low-level socket wrapper used by `net`.
#[cfg(feature = "net")]
pub(crate) mod socket;

// Generational cancellation scopes owned by the driver.
pub(crate) mod scopes;
