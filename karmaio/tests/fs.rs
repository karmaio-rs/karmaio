use std::{
    collections::BTreeSet,
    future::{Future, poll_fn},
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    task::Poll,
};

use karmaio::{Runtime, fs};

static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

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
        let (result, _) = shared.write_all_at(b"shared".to_vec(), 0).await;
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
