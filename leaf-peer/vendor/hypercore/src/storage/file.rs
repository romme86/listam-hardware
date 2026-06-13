//! Blocking, `std::fs`-backed [`RandomAccess`] implementation.
//!
//! Unlike `random-access-disk` (which needs tokio or async-std), this uses
//! only synchronous `std::fs`, so it works anywhere `std` is available —
//! including ESP-IDF, whose FATFS/LittleFS mounts are exposed through the
//! standard filesystem API. The async methods simply block; a leaf runs its
//! replication on a dedicated thread, so blocking IO there is fine.

use std::fs::{File, OpenOptions, create_dir_all};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use random_access_storage::{RandomAccess, RandomAccessError};

/// A single file used as a random-access byte store. Keeps a cached length to
/// avoid a metadata syscall on every bounds check (the merkle tree does many
/// small reads).
#[derive(Debug)]
pub struct RandomAccessFile {
    file: File,
    path: PathBuf,
    length: u64,
}

impl RandomAccessFile {
    /// Open (creating if needed) the file at `path` for read+write.
    pub fn open(path: PathBuf) -> Result<Self, RandomAccessError> {
        if let Some(parent) = path.parent() {
            create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        let length = file.metadata()?.len();
        Ok(Self { file, path, length })
    }

    /// Explicitly write zeros over `[self.length, end)`. Hypercore's stores
    /// rely on sparse-file semantics: bytes in the gap behind a past-EOF
    /// write MUST read back as zeros ("blank" tree nodes, empty oplog header
    /// slots). Host filesystems and the memory backend guarantee that, but
    /// FATFS does NOT — FatFs leaves expanded regions UNDEFINED (stale flash
    /// contents), which poisons tree-node and oplog-header parsing. So gaps
    /// are filled by hand.
    fn zero_fill_to(&mut self, end: u64) -> Result<(), RandomAccessError> {
        if end <= self.length {
            return Ok(());
        }
        self.file.seek(SeekFrom::Start(self.length))?;
        let zeros = [0u8; 512];
        let mut remaining = end - self.length;
        while remaining > 0 {
            let n = remaining.min(zeros.len() as u64) as usize;
            self.file.write_all(&zeros[..n])?;
            remaining -= n as u64;
        }
        self.length = end;
        Ok(())
    }

    /// `File::set_len` with an ESP-IDF fallback: the FATFS VFS there has no
    /// fd-level ftruncate and fails with EPERM. Grows become an explicit
    /// zero-fill of the gap (FATFS leaves seek-past-EOF regions undefined);
    /// shrinks rewrite the kept prefix through a truncating reopen of the
    /// same path.
    fn set_len_compat(&mut self, new_len: u64) -> Result<(), RandomAccessError> {
        match self.file.set_len(new_len) {
            Ok(()) => Ok(()),
            Err(err) if cfg!(target_os = "espidf") && err.raw_os_error() == Some(1) => {
                if new_len == self.length {
                    Ok(())
                } else if new_len > self.length {
                    self.zero_fill_to(new_len)?;
                    Ok(())
                } else {
                    let mut prefix = vec![0u8; new_len as usize];
                    if new_len > 0 {
                        self.file.seek(SeekFrom::Start(0))?;
                        self.file.read_exact(&mut prefix)?;
                    }
                    let mut fresh = OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .open(&self.path)?;
                    fresh.write_all(&prefix)?;
                    self.file = fresh;
                    Ok(())
                }
            }
            Err(err) => Err(err.into()),
        }
    }
}

#[async_trait::async_trait]
impl RandomAccess for RandomAccessFile {
    async fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), RandomAccessError> {
        // Hypercore relies on sparse-write semantics: a past-EOF write must
        // leave a ZERO gap. FATFS leaves the gap undefined, so fill it
        // explicitly before writing.
        if offset > self.length {
            self.zero_fill_to(offset)?;
        }
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)?;
        let end = offset + data.len() as u64;
        if end > self.length {
            self.length = end;
        }
        Ok(())
    }

    async fn read(&mut self, offset: u64, length: u64) -> Result<Vec<u8>, RandomAccessError> {
        if offset + length > self.length {
            return Err(RandomAccessError::OutOfBounds {
                offset,
                end: Some(offset + length),
                length: self.length,
            });
        }
        self.file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; length as usize];
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }

    async fn del(&mut self, offset: u64, length: u64) -> Result<(), RandomAccessError> {
        // Match random-access semantics: deleting at/over the end truncates,
        // otherwise zero the range in place.
        if offset >= self.length {
            return Ok(());
        }
        if offset + length >= self.length {
            self.set_len_compat(offset)?;
            self.length = offset;
            return Ok(());
        }
        let zeros = vec![0u8; length as usize];
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&zeros)?;
        Ok(())
    }

    async fn truncate(&mut self, length: u64) -> Result<(), RandomAccessError> {
        self.set_len_compat(length)?;
        self.length = length;
        Ok(())
    }

    async fn len(&mut self) -> Result<u64, RandomAccessError> {
        Ok(self.length)
    }

    async fn is_empty(&mut self) -> Result<bool, RandomAccessError> {
        Ok(self.length == 0)
    }

    async fn sync_all(&mut self) -> Result<(), RandomAccessError> {
        self.file.sync_all()?;
        Ok(())
    }
}

/// Helper used by [`crate::Storage::new_file_storage`]: the four store files
/// live as `tree`/`data`/`bitfield`/`oplog` under `dir`.
pub(crate) fn store_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}
