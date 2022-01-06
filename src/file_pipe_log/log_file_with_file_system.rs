// Copyright (c) 2017-present, PingCAP, Inc. Licensed under Apache-2.0.

use std::io::{Read, Result as IoResult, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;


use crate::file_pipe_log::log_file::LogFd;
use crate::file_system::{FileSystem, LowExt, ReadExt, WriteExt};
use crate::metrics::*;
use crate::FileBlockHandle;
use crate::Result;

use super::format::LogFileHeader;

const FILE_ALLOCATE_SIZE: usize = 2 * 1024 * 1024;

/// A `LogFile` is a `LogFd` wrapper that implements `Seek`, `Write` and `Read`.
pub struct LogFileWithFileSystem {
    inner: Arc<LogFd>,
    offset: usize,
}

impl LogFileWithFileSystem {
    pub fn new(fd: Arc<LogFd>) -> Self {
        Self {
            inner: fd,
            offset: 0,
        }
    }
}

impl Write for LogFileWithFileSystem {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        let len = self.inner.write(self.offset, buf)?;
        self.offset += len;
        Ok(len)
    }

    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }
}

impl Read for LogFileWithFileSystem {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        let len = self.inner.read(self.offset, buf)?;
        self.offset += len;
        Ok(len)
    }
}

impl LowExt for LogFileWithFileSystem {
    fn file_size(&self) -> IoResult<usize> {
        self.inner.file_size()
    }
}

impl ReadExt for LogFileWithFileSystem {}

impl WriteExt for LogFileWithFileSystem {
    fn finish(&self) -> IoResult<()> {
        self.inner.sync()
    }

    fn truncate(&self, offset: usize) -> IoResult<()> {
        self.inner.truncate(offset)
    }

    fn sync(&self) -> IoResult<()> {
        self.inner.sync()
    }

    fn allocate(&self, offset: usize, size: usize) -> IoResult<()> {
        self.inner.allocate(offset, size)
    }
}

impl Seek for LogFileWithFileSystem {
    fn seek(&mut self, pos: SeekFrom) -> IoResult<u64> {
        match pos {
            SeekFrom::Start(offset) => self.offset = offset as usize,
            SeekFrom::Current(i) => self.offset = (self.offset as i64 + i) as usize,
            SeekFrom::End(i) => self.offset = (self.inner.file_size()? as i64 + i) as usize,
        }
        Ok(self.offset as u64)
    }
}

pub fn build_file_reader_with_file_system<F: FileSystem>(
    system: &F,
    handle: Arc<F::Handle>,
) -> Result<LogFileReaderWithFileSystem<F>> {
    let reader = system.new_reader(handle.clone())?;
    LogFileReaderWithFileSystem::open(reader)
}

/// Random-access reader for log file.
pub struct LogFileReaderWithFileSystem<F: FileSystem> {
    reader: F::Reader,

    offset: usize,
}

impl<F: FileSystem> LogFileReaderWithFileSystem<F> {
    fn open(reader: F::Reader) -> Result<Self> {
        Ok(Self {
            reader,
            // Set to an invalid offset to force a reseek at first read.
            offset: usize::MAX,
        })
    }

    pub fn read(&mut self, handle: FileBlockHandle) -> Result<Vec<u8>> {
        let mut buf = vec![0; handle.len];
        let size = self.read_to(handle.offset, &mut buf)?;
        buf.truncate(size);
        Ok(buf)
    }

    pub fn read_to(&mut self, offset: u64, buffer: &mut [u8]) -> Result<usize> {
        if offset != self.offset as u64 {
            self.reader.seek(SeekFrom::Start(offset))?;
            self.offset = offset as usize;
        }
        let size = self.reader.read(buffer)?;
        self.offset += size;
        Ok(size)
    }

    #[inline]
    pub fn file_size(&self) -> Result<usize> {
        Ok(self.reader.file_size()?)
    }
}

pub fn build_file_writer_with_file_system<F: FileSystem>(
    system: &F,
    fd: Arc<LogFd>,
    create: bool,
) -> Result<LogFileWriterWithFileSystem<F>> {
    let writer = system.new_writer(fd.clone())?;
    LogFileWriterWithFileSystem::open(writer)
}

/// Append-only writer for log file.
pub struct LogFileWriterWithFileSystem<F: FileSystem> {
    writer: F::Writer,

    offset: usize,

    written: usize,
    capacity: usize,
    last_sync: usize,
}

impl<F: FileSystem> LogFileWriterWithFileSystem<F> {
    fn open(writer: F::Writer) -> Result<Self> {
        let file_size = writer.file_size()?;
        let mut f = Self {
            writer,
            offset: 0,
            written: file_size,
            capacity: file_size,
            last_sync: file_size,
        };
        if file_size < LogFileHeader::len() {
            f.write_header()?;
        } else {
            f.writer.seek(SeekFrom::Start(file_size as u64))?;
        }
        Ok(f)
    }

    fn write_header(&mut self) -> Result<()> {
        self.writer.seek(SeekFrom::Start(0))?;
        self.written = 0;
        let mut buf = Vec::with_capacity(LogFileHeader::len());
        LogFileHeader::default().encode(&mut buf)?;
        self.write(&buf, 0)
    }

    pub fn close(&mut self) -> Result<()> {
        // Necessary to truncate extra zeros from fallocate().
        self.truncate()?;
        self.sync()
    }

    pub fn truncate(&mut self) -> Result<()> {
        if self.written < self.capacity {
            self.writer.truncate(self.written)?;
            self.capacity = self.written;
        }
        Ok(())
    }

    pub fn write(&mut self, buf: &[u8], target_size_hint: usize) -> Result<()> {
        let new_written = self.written + buf.len();
        if self.capacity < new_written {
            let _t = StopWatch::new(&LOG_ALLOCATE_DURATION_HISTOGRAM);
            let alloc = std::cmp::max(
                new_written - self.capacity,
                std::cmp::min(
                    FILE_ALLOCATE_SIZE,
                    target_size_hint.saturating_sub(self.capacity),
                ),
            );
            self.writer.allocate(self.capacity, alloc)?;
            self.capacity += alloc;
        }
        self.writer.write_all(buf)?;
        self.written = new_written;
        Ok(())
    }

    pub fn sync(&mut self) -> Result<()> {
        if self.last_sync < self.written {
            let _t = StopWatch::new(&LOG_SYNC_DURATION_HISTOGRAM);
            self.writer.sync()?;
            self.last_sync = self.written;
        }
        Ok(())
    }

    #[inline]
    pub fn since_last_sync(&self) -> usize {
        self.written - self.last_sync
    }

    #[inline]
    pub fn offset(&self) -> usize {
        self.written
    }
}

pub struct DefaultFileSystem {}

impl FileSystem for DefaultFileSystem {
    type Handle = LogFd;
    type Reader = LogFileWithFileSystem;
    type Writer = LogFileWithFileSystem;

    fn create<P: AsRef<Path>>(&self, path: P) -> IoResult<LogFd> {
        // LogFd::create(&path) error
        todo!()
    }

    fn open<P: AsRef<Path>>(&self, path: P) -> IoResult<LogFd> {
        // LogFd::open(&path)
        todo!()
    }

    fn new_reader(&self, handle: Arc<LogFd>) -> IoResult<LogFileWithFileSystem> {
        Ok(LogFileWithFileSystem::new(handle.clone()))
    }

    fn new_writer(&self, handle: Arc<LogFd>) -> IoResult<LogFileWithFileSystem> {
        Ok(LogFileWithFileSystem::new(handle.clone()))
    }
}
