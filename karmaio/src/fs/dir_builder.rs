use std::io;
use std::path::Path;

use crate::driver::ops::Op;

#[derive(Debug)]
pub struct DirBuilder {
    #[cfg(unix)]
    mode: u32,
    recursive: bool,
}

impl DirBuilder {
    /// Creates a new set of options with default mode/security settings for all
    /// platforms and also non-recursive.
    ///
    /// This is an async version of [`std::fs::DirBuilder::new`].
    #[cfg(unix)]
    pub fn new() -> Self {
        Self {
            mode: 0o777,
            recursive: false,
        }
    }

    /// Creates a new set of options with default mode/security settings for all
    /// platforms and also non-recursive.
    ///
    /// This is an async version of [`std::fs::DirBuilder::new`].
    #[cfg(target_os = "windows")]
    pub fn new() -> Self {
        Self { recursive: false }
    }

    /// Indicates whether to create directories recursively (including all parent directories).
    /// Parents that do not exist are created with the same security and permissions settings.
    ///
    /// This option defaults to `false`.
    ///
    /// This is an async version of [`std::fs::DirBuilder::recursive`].
    pub fn recursive(&mut self, recursive: bool) -> &mut Self {
        self.recursive = recursive;
        self
    }

    /// Creates the specified directory with the configured options.
    ///
    /// It is considered an error if the directory already exists unless
    /// recursive mode is enabled.
    ///
    /// This is an async version of [`std::fs::DirBuilder::create`].
    ///
    /// # Errors
    ///
    /// An error will be returned under the following circumstances:
    ///
    /// * Path already points to an existing file.
    /// * Path already points to an existing directory and the mode is
    ///   non-recursive.
    /// * The calling process doesn't have permissions to create the directory
    ///   or its missing parents.
    /// * Other I/O error occurred.
    pub async fn create(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref();

        if self.recursive {
            self.create_recursive(path).await
        } else {
            self.create_single(path).await
        }
    }

    async fn create_single(&self, path: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        let op = Op::create_dir(path, self.mode);
        #[cfg(target_os = "windows")]
        let op = Op::create_dir(path);

        op?.await
    }

    async fn create_recursive(&self, path: &Path) -> std::io::Result<()> {
        if path.as_os_str().is_empty() {
            return Ok(());
        }

        let mut missing: Vec<&Path> = Vec::new();
        let mut current = path;

        loop {
            match self.create_single(current).await {
                Ok(()) => {
                    for p in missing.iter().rev() {
                        match self.create_single(p).await {
                            Ok(()) => {}
                            Err(ref e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                            Err(e) => return Err(e),
                        }
                    }
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    // If the target itself (no missing parents collected) already exists,
                    // verify it is a directory. Existing regular file must error, per std.
                    if missing.is_empty() {
                        match crate::fs::metadata(current).await {
                            Ok(m) if m.is_dir() => return Ok(()),
                            _ => return Err(e),
                        }
                    }
                    for p in missing.iter().rev() {
                        match self.create_single(p).await {
                            Ok(()) => {}
                            Err(ref e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                            Err(e) => return Err(e),
                        }
                    }
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => match current.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => {
                        missing.push(current);
                        current = parent;
                    }
                    _ => return Err(e),
                },
                Err(e) => return Err(e),
            }
        }
    }
}

impl Default for DirBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unix)]
impl std::os::unix::fs::DirBuilderExt for DirBuilder {
    /// Sets the mode to create new directories with.
    ///
    /// This option defaults to 0o777.
    fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = mode;
        self
    }
}
