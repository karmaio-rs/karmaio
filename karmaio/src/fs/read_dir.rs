use std::{
    collections::VecDeque,
    fmt,
    future::{Future, poll_fn},
    io,
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
};

use crate::runtime::JoinHandle;

use super::{FileType, Metadata, metadata_impl};

const CHUNK_SIZE: usize = 32;

type Chunk = (VecDeque<io::Result<DirEntry>>, std::fs::ReadDir, bool);

/// Returns an iterator over the entries within a directory.
///
/// Directory entries are read in bounded chunks on the runtime's blocking
/// pool. This is an async version of [`std::fs::read_dir`].
pub async fn read_dir(path: impl AsRef<Path>) -> io::Result<ReadDir> {
    let path = path.as_ref().to_owned();
    let chunk = super::asyncify(move || Ok(next_chunk(std::fs::read_dir(path)?))).await?;
    Ok(ReadDir {
        state: State::Idle(Some(chunk)),
    })
}

/// An asynchronous iterator over the entries in a directory.
#[must_use = "directory entries are not read unless polled"]
pub struct ReadDir {
    state: State,
}

enum State {
    Idle(Option<Chunk>),
    Pending(JoinHandle<Chunk>),
}

impl ReadDir {
    /// Returns the next entry in the directory.
    ///
    /// This method is cancellation safe. If it is used as the event in a
    /// selection operation and another branch completes first, no entry is
    /// lost.
    pub async fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        poll_fn(|cx| self.poll_next_entry(cx)).await
    }

    /// Attempts to poll the next directory entry.
    pub fn poll_next_entry(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Option<DirEntry>>> {
        loop {
            match &mut self.state {
                State::Idle(chunk) => {
                    let Some((entries, _, remain)) = chunk.as_mut() else {
                        return Poll::Ready(Ok(None));
                    };

                    if let Some(entry) = entries.pop_front() {
                        return Poll::Ready(entry.map(Some));
                    }
                    if !*remain {
                        return Poll::Ready(Ok(None));
                    }

                    let (_, read_dir, _) = chunk.take().expect("read directory state is present");
                    self.state = State::Pending(crate::runtime::spawn_blocking(move || next_chunk(read_dir)));
                }
                State::Pending(handle) => match Pin::new(handle).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(chunk)) => self.state = State::Idle(Some(chunk)),
                    Poll::Ready(Err(error)) => {
                        self.state = State::Idle(None);
                        if error.is_panic() {
                            std::panic::resume_unwind(error.into_panic());
                        }
                        return Poll::Ready(Err(io::Error::other(error.to_string())));
                    }
                },
            }
        }
    }
}

impl fmt::Debug for ReadDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadDir").finish_non_exhaustive()
    }
}

/// An entry returned by [`ReadDir`].
#[derive(Debug)]
pub struct DirEntry {
    entry: std::fs::DirEntry,
    file_type: Option<FileType>,
}

impl DirEntry {
    /// Returns the full path represented by this entry.
    pub fn path(&self) -> PathBuf {
        self.entry.path()
    }

    /// Returns the file name represented by this entry.
    pub fn file_name(&self) -> std::ffi::OsString {
        self.entry.file_name()
    }

    /// Returns the metadata for this entry without following symbolic links.
    pub async fn metadata(&self) -> io::Result<Metadata> {
        metadata_impl(&self.path(), false).await
    }

    /// Returns the file type for this entry without following symbolic links.
    pub async fn file_type(&self) -> io::Result<FileType> {
        if let Some(file_type) = self.file_type {
            return Ok(file_type);
        }

        self.metadata().await.map(|metadata| metadata.file_type())
    }

    /// Returns the inode number for this entry.
    #[cfg(unix)]
    pub fn ino(&self) -> u64 {
        use std::os::unix::fs::DirEntryExt;

        self.entry.ino()
    }
}

fn next_chunk(mut read_dir: std::fs::ReadDir) -> Chunk {
    let mut entries = VecDeque::with_capacity(CHUNK_SIZE);
    let mut remain = true;

    for _ in 0..CHUNK_SIZE {
        match read_dir.next() {
            Some(entry) => entries.push_back(entry.map(|entry| {
                let file_type = entry.file_type().ok().map(FileType::from_std);
                DirEntry { entry, file_type }
            })),
            None => {
                remain = false;
                break;
            }
        }
    }

    (entries, read_dir, remain)
}
