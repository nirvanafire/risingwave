// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fs::{File, OpenOptions};
use std::os::unix::prelude::{AsRawFd, FileExt, OpenOptionsExt, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bytes::Buf;
use nix::sys::stat::fstat;

use super::error::{Error, Result};
use super::{asyncify, DioBuffer, DIO_BUFFER_ALLOCATOR, LOGICAL_BLOCK_SIZE};

const ST_BLOCK_SIZE: usize = 512;

const MAGIC: &[u8] = b"hummock-cache-file";
const VERSION: u32 = 1;

pub struct CacheFileOptions {
    pub dir: String,
    pub id: u64,

    pub fs_block_size: usize,
    /// NOTE: `block_size` must be a multiple of `fs_block_size`.
    pub block_size: usize,
    pub meta_blocks: usize,
    pub fallocate_unit: usize,
}

impl CacheFileOptions {
    fn assert(&self) {
        assert_pow2(LOGICAL_BLOCK_SIZE);
        assert_alignment(LOGICAL_BLOCK_SIZE, self.fs_block_size);
        assert_alignment(self.fs_block_size, self.block_size);
    }
}

struct CacheFileCore {
    file: std::fs::File,
    len: AtomicUsize,
    capacity: AtomicUsize,
}

#[derive(Clone)]
pub struct CacheFile {
    dir: String,
    id: u64,

    pub fs_block_size: usize,
    pub block_size: usize,
    pub meta_blocks: usize,
    pub fallocate_unit: usize,

    core: Arc<CacheFileCore>,
}

impl std::fmt::Debug for CacheFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheFile")
            .field(
                "path",
                &PathBuf::from(self.dir.as_str()).join(filename(self.id)),
            )
            .finish()
    }
}

impl CacheFile {
    pub async fn open(options: CacheFileOptions) -> Result<Self> {
        options.assert();

        let path = PathBuf::from(options.dir.as_str()).join(filename(options.id));
        let mut oopts = OpenOptions::new();
        oopts.create(true);
        oopts.read(true);
        oopts.write(true);
        oopts.custom_flags(libc::O_DIRECT);

        let (file, block_size, meta_blocks, len, capacity) = asyncify(move || {
            let file = oopts.open(path)?;
            let stat = fstat(file.as_raw_fd())?;
            if stat.st_blocks == 0 {
                write_header(&file, options.block_size, options.meta_blocks)?;
                // TODO: pre-allocate size
                Ok((
                    file,
                    options.block_size,
                    options.meta_blocks,
                    (options.block_size * (1 + options.meta_blocks)) as usize,
                    0,
                ))
            } else {
                let (block_size, meta_blocks) = read_header(&file)?;
                // TODO: pre-allocate size
                Ok((file, block_size, meta_blocks, stat.st_size as usize, 0))
            }
        })
        .await?;

        // TODO: Pre-allocate file space.

        Ok(Self {
            dir: options.dir,
            id: options.id,

            fs_block_size: options.fs_block_size,
            block_size: block_size,
            meta_blocks: meta_blocks,
            fallocate_unit: options.fallocate_unit,

            core: Arc::new(CacheFileCore {
                file,
                len: AtomicUsize::new(len),
                capacity: AtomicUsize::new(capacity),
            }),
        })
    }

    pub async fn append(&self) -> Result<()> {
        todo!()
    }

    pub async fn write(&self) -> Result<()> {
        todo!()
    }

    pub async fn read(&self) -> Result<()> {
        todo!()
    }

    pub async fn flush(&self) -> Result<()> {
        todo!()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get file length in bytes.
    ///
    /// `len()` stands for the last written byte of the file.
    pub fn len(&self) -> usize {
        self.core.len.load(Ordering::Acquire)
    }

    /// Get file size by `stat.st_blocks * FS_BLOCK_SIZE`.
    ///
    /// `size()` stands for how much space that the file really used.
    ///
    /// `size()` can be different from `len()` because the file is sparse and pre-allocated.
    pub fn size(&self) -> usize {
        fstat(self.fd()).unwrap().st_blocks as usize * ST_BLOCK_SIZE
    }
}

impl CacheFile {
    #[inline(always)]
    fn fd(&self) -> RawFd {
        self.core.file.as_raw_fd()
    }
}

#[inline(always)]
fn filename(id: u64) -> String {
    format!("cf-{:020}", id)
}

fn write_header(file: &File, block_size: usize, meta_blocks: usize) -> Result<()> {
    let mut buf: DioBuffer = Vec::with_capacity_in(LOGICAL_BLOCK_SIZE, &DIO_BUFFER_ALLOCATOR);

    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&VERSION.to_be_bytes());
    buf.extend_from_slice(&block_size.to_be_bytes());
    buf.extend_from_slice(&meta_blocks.to_be_bytes());
    buf.resize(LOGICAL_BLOCK_SIZE, 0);

    file.write_all_at(&buf, 0)?;
    Ok(())
}

fn read_header(file: &File) -> Result<(usize, usize)> {
    let mut buf: DioBuffer = Vec::with_capacity_in(LOGICAL_BLOCK_SIZE, &DIO_BUFFER_ALLOCATOR);
    buf.resize(LOGICAL_BLOCK_SIZE, 0);
    file.read_exact_at(&mut buf, 0)?;
    let mut cursor = 0;

    cursor += MAGIC.len();
    let magic = &buf[cursor - MAGIC.len()..cursor];
    if magic != MAGIC {
        return Err(Error::Other(format!(
            "magic mismatch, expected: {:?}, got: {:?}",
            MAGIC, magic
        )));
    }

    cursor += 4;
    let version = (&buf[cursor - 4..cursor]).get_u32();
    if version != VERSION {
        return Err(Error::Other(format!("unsupported version: {}", version)));
    }

    cursor += 8;
    let block_size = (&buf[cursor - 8..cursor]).get_u64() as usize;

    cursor += 8;
    let meta_blocks = (&buf[cursor - 8..cursor]).get_u64() as usize;

    Ok((block_size, meta_blocks))
}

#[inline(always)]
fn assert_pow2(v: usize) {
    assert_eq!(v & (v - 1), 0);
}

#[inline(always)]
fn assert_alignment(align: usize, v: usize) {
    assert_eq!(v & (align - 1), 0, "align: {}, v: {}", align, v);
}

#[inline(always)]
fn align_up(align: usize, v: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_send_sync_clone<T: Send + Sync + Clone + 'static>() {}

    #[test]
    fn ensure_send_sync_clone() {
        is_send_sync_clone::<CacheFile>();
    }

    #[tokio::test]
    async fn test_file_cache() {
        let tempdir = tempfile::tempdir().unwrap();
        let options = CacheFileOptions {
            dir: tempdir.path().to_str().unwrap().to_string(),
            id: 1,

            fs_block_size: 4096,
            block_size: 4096,
            meta_blocks: 64,
            fallocate_unit: 64 * 1024 * 1024,
        };
        let _cf = CacheFile::open(options).await.unwrap();
    }
}
