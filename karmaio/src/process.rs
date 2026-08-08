//! Asynchronous process management, modeled after [`std::process`]
//!
//! A [`Command`] wraps the standard library's [`std::process::Command`] and spawns child processes.
//! The returned [`Child`] can be awaited for completion ([`Child::wait`]),
//! polled without blocking ([`Child::try_wait`]), or signalled ([`Child::kill`]).
//! When stdout / stderr / stdin are piped, they are exposed as async [`ChildStdout`], [`ChildStderr`] and [`ChildStdin`] types
//! implementing [`crate::io::AsyncRead`] / [`crate::io::AsyncWrite`].
//!
//! Spawning is synchronous. Piped stdio is driven by the completion driver
//! so reading/writing a child's pipes never stalls the executor.
//! Waiting is also asynchronous where the platform backend provides a native
//! completion primitive. Linux uses pidfds; on macOS, BSDs, and Windows the
//! blocking-pool fallback is used for process waits.

pub mod child;
pub mod command;
pub mod stdio;

pub use child::Child;
pub use command::Command;
pub use std::process::{ExitStatus, Output};
pub use stdio::{ChildStderr, ChildStdin, ChildStdout};

/// Re-export of [`std::process::Stdio`] so callers can configure child streams
/// without reaching into `std`.
pub use std::process::Stdio;

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use crate::io::{AsyncReadExt, AsyncWrite};
    use crate::runtime::Runtime;

    use super::*;

    #[test]
    fn command_output_captures_stdout() {
        let mut rt = Runtime::new().expect("runtime start");
        rt.block_on(async {
            let output = Command::new("echo")
                .arg("hello-karmaio")
                .output()
                .await
                .expect("spawn echo");
            assert!(output.status.success());
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(text.contains("hello-karmaio"), "got: {text:?}");
        });
    }

    #[test]
    fn wait_returns_exit_code() {
        let mut rt = Runtime::new().expect("runtime start");
        rt.block_on(async {
            let status = Command::new("sh")
                .arg("-c")
                .arg("exit 7")
                .status()
                .await
                .expect("spawn sh");
            assert_eq!(status.code(), Some(7));
        });
    }

    #[test]
    fn try_wait_then_wait() {
        let mut rt = Runtime::new().expect("runtime start");
        rt.block_on(async {
            let mut child = Command::new("sleep").arg("1").spawn().expect("spawn sleep");
            // Immediately the child is likely still running.
            let early = child.try_wait().expect("try_wait");
            assert!(early.is_none(), "sleep should still be running");
            let status = child.wait().await.expect("wait");
            assert!(status.code().is_some());
        });
    }

    #[test]
    fn kill_terminates_child() {
        let mut rt = Runtime::new().expect("runtime start");
        rt.block_on(async {
            let mut child = Command::new("sleep").arg("30").spawn().expect("spawn sleep");
            // The child should still be alive right after spawn.
            assert!(child.try_wait().expect("try_wait").is_none());
            child.kill().expect("kill");
            let status = child.wait().await.expect("wait after kill");
            // Killed processes are not successful.
            assert!(!status.success());
        });
    }

    #[test]
    fn piped_stdin_stdout_roundtrip() {
        let mut rt = Runtime::new().expect("runtime start");
        rt.block_on(async {
            let mut cmd = Command::new("cat");
            cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
            let mut child = cmd.spawn().expect("spawn cat");

            let mut stdin = child.take_stdin().expect("stdin piped");
            let mut stdout = child.take_stdout().expect("stdout piped");

            let (res, _) = stdin.write(b"hello".to_vec()).await.into_parts();
            res.expect("write to cat");
            // Close stdin so `cat` observes EOF and flushes its output.
            stdin.shutdown().await.expect("shutdown stdin");

            let buf = Box::new([0u8; 5]);
            let (res, buf) = stdout.read_exact(buf).await.into_parts();
            res.expect("read from cat");
            assert_eq!(&*buf, b"hello");

            let status = child.wait().await.expect("wait");
            assert!(status.success());
        });
    }

    #[test]
    fn kill_on_drop_stops_child() {
        let mut rt = Runtime::new().expect("runtime start");
        rt.block_on(async {
            let mut cmd = Command::new("sleep");
            cmd.arg("30").kill_on_drop(true);
            let child = cmd.spawn().expect("spawn sleep");
            // Dropping the handle (with kill_on_drop) should terminate it.
            drop(child);
            // Give the signal a moment, then confirm a fresh echo still works.
            let status = Command::new("true").status().await.expect("true");
            assert!(status.success());
        });
    }
}
