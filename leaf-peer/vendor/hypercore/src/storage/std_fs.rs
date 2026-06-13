//! Blocking `std::fs` random-access backend.
//!
//! Unlike `random-access-disk` this pulls in no async runtime (tokio /
//! async-std), so it works on minimal-std targets like ESP-IDF, where the
//! filesystem is a FAT partition behind the VFS. Operations block the calling
//! task; hypercore storage I/O is small and sequential, so on a
//! single-executor embedded target that is the right trade.

use async_trait::async_trait;
use random_access_storage::{RandomAccess, RandomAccessError};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const ZERO_CHUNK: [u8; 4096] = [0u8; 4096];

/// `RandomAccess` over a plain `std::fs::File`.
#[derive(Debug)]
pub struct RandomAccessStdFs {
    file: File,
    length: u64,
}

impl RandomAccessStdFs {
    /// Open (or create) the file at `path`, creating parent directories.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let length = file.metadata()?.len();
        Ok(Self { file, length })
    }

    /// Write `count` zero bytes at the current file position.
    fn write_zeros(&mut self, mut count: u64) -> Result<(), std::io::Error> {
        while count > 0 {
            let n = count.min(ZERO_CHUNK.len() as u64);
            self.file.write_all(&ZERO_CHUNK[..n as usize])?;
            count -= n;
        }
        Ok(())
    }
}

#[async_trait]
impl RandomAccess for RandomAccessStdFs {
    async fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), RandomAccessError> {
        if offset > self.length {
            // POSIX zero-fills the gap on writes past EOF, but FATFS leaves
            // it undefined — fill explicitly so reads of the gap are zeros.
            self.file.seek(SeekFrom::Start(self.length))?;
            self.write_zeros(offset - self.length)?;
        }
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)?;
        self.length = self.length.max(offset + data.len() as u64);
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
        self.file.seek(