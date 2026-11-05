//! A database file-length ceiling, not an allocated-block or memory quota.
use redb::{BackendError, StorageBackend, backends::FileBackend};
use std::{fs::File, io, ops::Bound};

#[derive(Debug)]
pub(super) struct CappedFile {
    file: FileBackend,
    ceiling: u64,
}

impl CappedFile {
    pub(super) fn new(file: File, ceiling: u64) -> anyhow::Result<Self> {
        // Never truncate an existing database to satisfy a lowered policy.
        if file.metadata()?.len() > ceiling {
            return Err(full().into());
        }
        Ok(Self {
            file: FileBackend::new(file)?,
            ceiling,
        })
    }
}

fn full() -> io::Error {
    io::Error::new(io::ErrorKind::StorageFull, "database file-length ceiling exceeded")
}

impl StorageBackend for CappedFile {
    fn len(&self) -> io::Result<u64> {
        self.file.len()
    }
    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.file.read(offset, out)
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        if len > self.ceiling {
            return Err(full());
        }
        self.file.set_len(len)
    }
    fn write(&self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        if offset
            .checked_add(bytes.len() as u64)
            .is_none_or(|end| end > self.ceiling)
        {
            return Err(full());
        }
        self.file.write(offset, bytes)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }
    fn close(&self) -> io::Result<()> {
        self.file.close()
    }
    fn try_lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<bool, BackendError> {
        self.file.try_lock_range(start, end)
    }
    fn try_lock_shared_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<bool, BackendError> {
        self.file.try_lock_shared_range(start, end)
    }
    fn lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<(), BackendError> {
        self.file.lock_range(start, end)
    }
    fn lock_shared_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<(), BackendError> {
        self.file.lock_shared_range(start, end)
    }
    fn unlock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<(), BackendError> {
        self.file.unlock_range(start, end)
    }
    fn query_lock_range(&self, start: Bound<u64>, end: Bound<u64>) -> Result<bool, BackendError> {
        self.file.query_lock_range(start, end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    fn path(label: &str) -> PathBuf {
        let root = std::env::current_dir().unwrap().join("target");
        fs::create_dir_all(&root).unwrap();
        root.join(format!("capped-{label}-{}", std::process::id()))
    }

    #[test]
    fn bounds_apply_before_writes_and_preserve_existing_files() -> anyhow::Result<()> {
        let path = path("bounds");
        let file = File::options().create_new(true).read(true).write(true).open(&path)?;
        let capped = CappedFile::new(file, 8192)?;
        capped.set_len(8192)?;
        capped.write(8189, b"end")?;
        assert_eq!(capped.set_len(8193).unwrap_err().kind(), io::ErrorKind::StorageFull);
        assert_eq!(
            capped.write(8191, b"bad").unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
        assert_eq!(
            capped.write(u64::MAX, b"xx").unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
        let mut end = [0; 3];
        capped.read(8189, &mut end)?;
        assert_eq!(&end, b"end", "rejected write modified its in-range prefix");
        assert_eq!(capped.len()?, 8192);
        capped.sync_data()?;
        let file = File::options().read(true).write(true).open(&path)?;
        assert!(CappedFile::new(file, 8191).is_err());
        assert_eq!(fs::metadata(&path)?.len(), 8192, "lowered limit truncated data");
        capped.set_len(1024)?;
        assert_eq!(capped.len()?, 1024, "at-cap storage cannot shrink");
        capped.close()?;
        fs::remove_file(path)?;
        Ok(())
    }
}
