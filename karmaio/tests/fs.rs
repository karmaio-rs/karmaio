#![cfg(feature = "fs")]

use std::{
    collections::BTreeSet,
    future::{Future, poll_fn},
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    task::Poll,
};

use karmaio::{
    Runtime,
    buf::IoBuf,
    fs,
    io::{AsyncReadAtExt, AsyncReadExt, AsyncWriteAtExt, AsyncWriteExt},
};

static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

struct SafeInlineBuf<const N: usize> {
    bytes: [u8; N],
    _pinned: std::marker::PhantomPinned,
}

impl<const N: usize> SafeInlineBuf<N> {
    fn new(bytes: [u8; N]) -> Self {
        Self {
            bytes,
            _pinned: std::marker::PhantomPinned,
        }
    }
}

impl<const N: usize> IoBuf for SafeInlineBuf<N> {
    fn as_init(&self) -> &[u8] {
        &self.bytes
    }
}

impl TestDir {
    fn new() -> io::Result<Self> {
        let base = std::env::temp_dir();

        loop {
            let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!("karmaio-fs-{}-{id}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn file_lifecycle_and_path_metadata() -> io::Result<()> {
    let root = TestDir::new()?;
    let source = root.path().join("source.txt");
    let hard_link = root.path().join("hard-link.txt");
    let renamed = root.path().join("renamed.txt");
    let large_path = root.path().join("large.bin");
    let mut runtime = Runtime::new()?;

    runtime.block_on(async {
        fs::write(&source, b"karma").await?;
        assert_eq!(fs::read(&source).await?, b"karma");

        let metadata = fs::metadata(&source).await?;
        assert!(metadata.is_file());
        assert_eq!(metadata.len(), 5);

        let file = fs::File::open(&source).await?;
        assert_eq!(file.metadata().await?.len(), 5);
        file.close().await?;

        let error = match fs::File::create_new(&source).await {
            Ok(_) => panic!("creating an existing file should fail"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

        fs::hard_link(&source, &hard_link).await?;
        fs::rename(&hard_link, &renamed).await?;
        assert_eq!(fs::read(&renamed).await?, b"karma");

        let large_contents = vec![0x5a; 24 * 1024];
        fs::write(&large_path, &large_contents).await?;
        assert_eq!(fs::read(&large_path).await?, large_contents);

        fs::remove_file(&source).await?;
        fs::remove_file(&renamed).await?;
        fs::remove_file(&large_path).await
    })
}

#[test]
fn read_dir_is_batched_and_cancellation_safe() -> io::Result<()> {
    const ENTRY_COUNT: usize = 40;

    let root = TestDir::new()?;
    for index in 0..ENTRY_COUNT {
        std::fs::write(root.path().join(format!("entry-{index:02}")), [])?;
    }

    let mut runtime = Runtime::new()?;
    runtime.block_on(async {
        let mut read_dir = fs::read_dir(root.path()).await?;
        let mut names = BTreeSet::new();

        for _ in 0..32 {
            let entry = read_dir.next_entry().await?.expect("entry should exist");
            assert!(entry.file_type().await?.is_file());
            names.insert(entry.file_name());
        }

        // Poll the next batch once and then cancel the future. Whether that
        // first poll races to completion or remains pending, iteration must
        // resume without losing an entry.
        let first_poll = {
            let mut future = Box::pin(read_dir.next_entry());
            poll_fn(|cx| Poll::Ready(Future::poll(future.as_mut(), cx))).await
        };
        if let Poll::Ready(entry) = first_poll {
            names.insert(entry?.expect("entry should exist").file_name());
        }

        while let Some(entry) = read_dir.next_entry().await? {
            names.insert(entry.file_name());
        }

        assert_eq!(names.len(), ENTRY_COUNT);
        Ok(())
    })
}

#[test]
fn recursive_directory_operations() -> io::Result<()> {
    let root = TestDir::new()?;
    let tree = root.path().join("one/two/three");
    let mut runtime = Runtime::new()?;

    runtime.block_on(async {
        fs::create_dir_all(Path::new("")).await?;
        fs::create_dir_all(&tree).await?;
        fs::write(tree.join("file.txt"), b"content").await?;
        assert!(fs::metadata(&tree).await?.is_dir());

        fs::remove_dir_all(root.path().join("one")).await?;
        let error = match fs::metadata(root.path().join("one")).await {
            Ok(_) => panic!("removed directory should not exist"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        Ok(())
    })
}

#[test]
fn positional_extensions_and_file_cursor() -> io::Result<()> {
    use std::io::Cursor;

    let root = TestDir::new()?;
    let path = root.path().join("positioned.txt");
    let mut runtime = Runtime::new()?;

    runtime.block_on(async {
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .await?;

        let (result, _) = file.write_all_at(b"world".to_vec(), 6).await.into_parts();
        result?;
        let (result, _) = file.write_all_at(b"hello ".to_vec(), 0).await.into_parts();
        result?;

        // A downstream buffer implementation needs no unsafe pointer contract,
        // and may contain inline storage without implementing Unpin.
        let (result, custom) = file.write_all_at(SafeInlineBuf::new(*b"HELLO"), 0).await.into_parts();
        result?;
        assert_eq!(custom.bytes, *b"HELLO");

        // Inline mutable storage remains at a stable address while the native
        // operation is pending.
        let (result, bytes) = file.read_exact_at([0; 5], 6).await.into_parts();
        result?;
        assert_eq!(&bytes, b"world");

        let (result, bytes) = file.read_to_end_at(Vec::new(), 0).await.into_parts();
        assert_eq!(result?, 11);
        assert_eq!(bytes, b"HELLO world");

        let mut cursor = Cursor::new(file);
        cursor.set_position(6);
        let (result, bytes) = cursor.read_exact(Box::new([0; 5])).await.into_parts();
        assert_eq!(result?, 5);
        assert_eq!(&*bytes, b"world");

        cursor.set_position(0);
        let (result, _) = cursor.write_all(b"HELLO".to_vec()).await.into_parts();
        assert_eq!(result?, 5);
        cursor.into_inner().close().await?;

        assert_eq!(fs::read(&path).await?, b"HELLO world");

        let source = b"012345".to_vec();
        let (result, bytes) = source.read_to_end_at(vec![b'x'], 2).await.into_parts();
        assert_eq!(result?, 4);
        assert_eq!(bytes, b"x2345");

        let (result, string) = source.read_to_string_at("value=".into(), 2).await.into_parts();
        assert_eq!(result?, 4);
        assert_eq!(string, "value=2345");

        let buffers = vec![Vec::with_capacity(2), Vec::with_capacity(3)];
        let (result, buffers) = source.read_vectored_exact_at(buffers, 1).await.into_parts();
        result?;
        assert_eq!(buffers, [b"12".to_vec(), b"345".to_vec()]);

        let buffers = [[0u8; 2], [0u8; 2]];
        let (result, buffers) = source.read_vectored_exact_at(buffers, 1).await.into_parts();
        result?;
        assert_eq!(buffers, [*b"12", *b"34"]);

        let mut destination = Vec::new();
        let buffers = vec![b"abc".to_vec(), b"def".to_vec()];
        let (result, returned) = destination.write_vectored_all_at(buffers, 2).await.into_parts();
        result?;
        assert_eq!(returned, [b"abc".to_vec(), b"def".to_vec()]);
        assert_eq!(destination, b"\0\0abcdef");

        let buffers = [*b"gh", *b"ij"];
        let (result, returned) = destination.write_vectored_all_at(buffers, 0).await.into_parts();
        result?;
        assert_eq!(returned, [*b"gh", *b"ij"]);
        assert_eq!(&destination[..4], b"ghij");

        let invalid_utf8 = vec![0xff];
        let (result, string) = invalid_utf8.read_to_string_at("valid".into(), 0).await.into_parts();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(string, "valid");

        let short_source = b"ab".to_vec();
        let buffers = vec![Vec::with_capacity(2), Vec::with_capacity(1)];
        let (result, returned) = short_source.read_vectored_exact_at(buffers, 0).await.into_parts();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(returned.len(), 2);

        let mut short_destination = [0u8; 2];
        let buffers = vec![b"abc".to_vec(), b"def".to_vec()];
        let (result, returned) = short_destination.write_vectored_all_at(buffers, 0).await.into_parts();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WriteZero);
        assert_eq!(returned.len(), 2);
        Ok(())
    })
}

#[test]
fn file_interoperability_and_shared_writes() -> io::Result<()> {
    let root = TestDir::new()?;
    let path = root.path().join("interop.txt");
    let mut runtime = Runtime::new()?;

    runtime.block_on(async {
        let file = fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .await?;
        assert!(format!("{file:?}").contains("File"));

        let mut shared = &file;
        let (result, _) = shared.write_all_at(b"shared".to_vec(), 0).await.into_parts();
        result?;

        #[cfg(unix)]
        {
            use std::os::fd::{AsFd, AsRawFd};
            assert_eq!(file.as_fd().as_raw_fd(), file.as_raw_fd());
        }

        #[cfg(windows)]
        {
            use std::os::windows::io::{AsHandle, AsRawHandle};
            assert_eq!(file.as_handle().as_raw_handle(), file.as_raw_handle());
        }

        let shared_clone = file.clone();
        let file = match file.try_into_std() {
            Ok(_) => panic!("shared file should not unwrap"),
            Err(file) => file,
        };
        drop(shared_clone);

        let duplicate = file.try_clone().await?;
        let std_file = file.try_into_std().expect("unique file should unwrap");
        let file = fs::File::from_std(std_file)?;
        assert_eq!(file.metadata().await?.len(), 6);
        drop(file.into_std().await);
        drop(duplicate.into_std().await);
        Ok(())
    })
}

#[cfg(unix)]
#[test]
fn symlink_and_permissions_operations() -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let root = TestDir::new()?;
    let target = root.path().join("target.txt");
    let link = root.path().join("link.txt");
    std::fs::write(&target, b"target")?;

    let mut runtime = Runtime::new()?;
    runtime.block_on(async {
        fs::symlink(Path::new("target.txt"), &link).await?;
        assert_eq!(fs::read_link(&link).await?, PathBuf::from("target.txt"));
        assert!(fs::metadata(&link).await?.is_file());
        assert!(fs::symlink_metadata(&link).await?.is_symlink());

        let mut entries = fs::read_dir(root.path()).await?;
        let mut found_link = false;
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_name() == "link.txt" {
                assert!(entry.file_type().await?.is_symlink());
                found_link = true;
            }
        }
        assert!(found_link);

        let mut permissions = fs::metadata(&target).await?.permissions();
        permissions.set_mode(0o000);
        fs::set_permissions(&target, permissions).await?;
        assert_eq!(std::fs::metadata(&target)?.permissions().mode() & 0o777, 0);

        let mut permissions = fs::metadata(&target).await?.permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&target, permissions).await
    })
}

#[cfg(windows)]
#[test]
fn path_permissions_operation() -> io::Result<()> {
    let root = TestDir::new()?;
    let target = root.path().join("target.txt");
    std::fs::write(&target, b"target")?;

    let mut runtime = Runtime::new()?;
    runtime.block_on(async {
        let mut permissions = fs::metadata(&target).await?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&target, permissions).await?;
        assert!(std::fs::metadata(&target)?.permissions().readonly());

        let mut permissions = fs::metadata(&target).await?.permissions();
        permissions.set_readonly(false);
        fs::set_permissions(&target, permissions).await
    })
}
