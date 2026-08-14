//! A process builder, mirroring [`std::process::Command`].
//!
//! Unlike the offset-free process API in some runtimes, `karmaio`'s
//! [`Command`] wraps the standard library's `Command` directly,
//! so it inherits every configuration knob (`env`, `current_dir`, …) for free.
//! Spawning is synchronous; the resulting [`Child`] is awaited
//! asynchronously and its piped stdio streams are driven by the completion driver.

use std::{
    ffi::OsStr,
    io,
    path::Path,
    process::{Command as StdCommand, ExitStatus, Output, Stdio},
};

use crate::process::child::Child;

/// The standard I/O configuration type for a child stream. This is
/// [`std::process::Stdio`].
pub type StdioConfig = std::process::Stdio;

/// Builder for spawning a child process, modeled after [`std::process::Command`].
///
/// Each configuration method returns `&mut Self` so calls can be chained. Use
/// [`Command::spawn`] to launch the process, or the convenience
/// [`Command::status`]/[`Command::output`] helpers to run it to completion.
pub struct Command {
    inner: StdCommand,
    stdin_set: bool,
    stdout_set: bool,
    stderr_set: bool,
    kill_on_drop: bool,
}

impl Command {
    /// Constructs a new `Command` for launching the program at `program`.
    pub fn new(program: impl AsRef<OsStr>) -> Command {
        Command {
            inner: StdCommand::new(program),
            stdin_set: false,
            stdout_set: false,
            stderr_set: false,
            kill_on_drop: false,
        }
    }

    /// Adds a single argument to pass to the program.
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.inner.arg(arg);
        self
    }

    /// Adds multiple arguments to pass to the program.
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.inner.args(args);
        self
    }

    /// Inserts or updates an environment variable mapping.
    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.inner.env(key, value);
        self
    }

    /// Adds or updates multiple environment variable mappings.
    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.envs(vars);
        self
    }

    /// Removes an environment variable mapping.
    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.inner.env_remove(key);
        self
    }

    /// Clears the entire environment map for the child.
    pub fn env_clear(&mut self) -> &mut Self {
        self.inner.env_clear();
        self
    }

    /// Sets the working directory the child will run in.
    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.inner.current_dir(dir);
        self
    }

    /// Configures the child's standard input.
    pub fn stdin(&mut self, cfg: StdioConfig) -> &mut Self {
        self.stdin_set = true;
        self.inner.stdin(cfg);
        self
    }

    /// Configures the child's standard output.
    pub fn stdout(&mut self, cfg: StdioConfig) -> &mut Self {
        self.stdout_set = true;
        self.inner.stdout(cfg);
        self
    }

    /// Configures the child's standard error.
    pub fn stderr(&mut self, cfg: StdioConfig) -> &mut Self {
        self.stderr_set = true;
        self.inner.stderr(cfg);
        self
    }

    /// If `true`, the child is killed (SIGKILL on Unix) when the [`Child`] is
    /// dropped. Defaults to `false`.
    pub fn kill_on_drop(&mut self, kill_on_drop: bool) -> &mut Self {
        self.kill_on_drop = kill_on_drop;
        self
    }

    /// Spawns the child process, returning a handle to it.
    pub fn spawn(&mut self) -> io::Result<Child> {
        Child::spawn(&mut self.inner, self.kill_on_drop)
    }

    /// Spawns the child and waits for it to finish, returning its exit status.
    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        self.spawn()?.wait().await
    }

    /// Spawns the child, pipes its stdout/stderr (and nulls stdin unless set),
    /// and collects all output, mirroring [`std::process::Command::output`].
    pub async fn output(&mut self) -> io::Result<Output> {
        if !self.stdout_set {
            self.inner.stdout(Stdio::piped());
            self.stdout_set = true;
        }
        if !self.stderr_set {
            self.inner.stderr(Stdio::piped());
            self.stderr_set = true;
        }
        if !self.stdin_set {
            self.inner.stdin(Stdio::null());
        }
        self.spawn()?.wait_with_output().await
    }
}
