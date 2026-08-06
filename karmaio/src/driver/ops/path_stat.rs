use std::{io, path::Path};

#[cfg(target_os = "linux")]
use crate::driver::backends::iouring::{Submission as UringSubmission, UringOperation};
#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
use crate::driver::backends::kqueue::{KqueueAttempt, KqueueOperation};
use crate::{
    driver::{
        helpers::cstr::{OsPath, cstr},
        ops::{Completion, Op},
    },
    fs::Metadata,
    runtime::local::CURRENT_DRIVER,
};

/// Queries metadata for a path, optionally following its final symlink.
pub(crate) struct PathStat {
    path: OsPath,
    follow_symlinks: bool,
    #[cfg(target_os = "linux")]
    statx_buf: Box<libc::statx>,
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    stat_shared: Option<std::sync::Arc<std::sync::Mutex<Option<rustix::fs::Stat>>>>,
}

impl Op<PathStat> {
    pub(crate) fn path_stat(path: &Path, follow_symlinks: bool) -> io::Result<Self> {
        let data = PathStat {
            path: cstr(path)?,
            follow_symlinks,
            #[cfg(target_os = "linux")]
            statx_buf: Box::new(unsafe { std::mem::zeroed() }),
            #[cfg(any(
                target_os = "macos",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd",
                target_os = "dragonfly"
            ))]
            stat_shared: None,
        };

        CURRENT_DRIVER.with(|handle| handle.upgrade().expect("Not in a runtime context").submit_op(data))
    }
}

#[cfg(target_os = "linux")]
unsafe impl UringOperation for PathStat {
    type Output = io::Result<Metadata>;

    fn submit(&mut self) -> UringSubmission {
        use io_uring::{opcode, types};

        let statx_buf = self.statx_buf.as_mut() as *mut libc::statx as *mut types::statx;
        let flags = if self.follow_symlinks {
            0
        } else {
            libc::AT_SYMLINK_NOFOLLOW
        };

        opcode::Statx::new(types::Fd(libc::AT_FDCWD), self.path.as_c_str().as_ptr(), statx_buf)
            .flags(flags)
            .mask(libc::STATX_BASIC_STATS | libc::STATX_BTIME)
            .build()
    }

    fn complete(self, completion: Completion) -> Self::Output {
        completion.result?;
        Ok(Metadata::from_statx(*self.statx_buf))
    }
}

#[cfg(any(
    target_os = "macos",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
))]
impl KqueueOperation for PathStat {
    type Output = io::Result<Metadata>;

    fn attempt(&mut self) -> KqueueAttempt {
        use std::sync::{Arc, Mutex};

        let slot = Arc::new(Mutex::new(None));
        self.stat_shared = Some(Arc::clone(&slot));
        let path = self.path.clone();
        let follow_symlinks = self.follow_symlinks;

        kqueue_syscall_blocking!({
            let result = if follow_symlinks {
                rustix::fs::stat(path.as_c_str())
            } else {
                rustix::fs::lstat(path.as_c_str())
            };

            result
                .map(|stat| {
                    *slot.lock().unwrap_or_else(|error| error.into_inner()) = Some(stat);
                    0_u32
                })
                .map_err(io::Error::from)
        })
    }

    fn complete(self, completion: Completion) -> Self::Output {
        completion.result?;
        let stat = self
            .stat_shared
            .expect("path stat shared slot missing after submission")
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .expect("path stat result missing after successful completion");
        Ok(Metadata::from_stat(stat))
    }
}
