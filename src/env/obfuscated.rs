// Copyright (c) 2017-present, PingCAP, Inc. Licensed under Apache-2.0.

use std::io::{Read, Result as IoResult, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use crate::env::default::LogFd;
use crate::env::{FileSystem, Handle, WriteExt, DefaultFileSystem};

pub struct ObfuscatedFile {
    inner: Arc<LogFd>,
    offset: usize,
}

impl WriteExt for ObfuscatedFile {
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

impl Write for ObfuscatedFile {
    fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
        let mut new_buf = buf.to_owned();
        for c in &mut new_buf {
            *c = c.wrapping_add(1);
        }
        let len = self.inner.write(self.offset, &new_buf)?;
        self.offset += len;
        Ok(len)
    }

    fn flush(&mut self) -> IoResult<()> {
        Ok(())
    }
}

impl Read for ObfuscatedFile {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        let len = self.inner.read(self.offset, buf)?;
        for c in buf {
            *c = c.wrapping_sub(1);
        }
        self.offset += len;
        Ok(len)
    }
}

impl Seek for ObfuscatedFile {
    fn seek(&mut self, pos: SeekFrom) -> IoResult<u64> {
        match pos {
            SeekFrom::Start(offset) => self.offset = offset as usize,
            SeekFrom::Current(i) => self.offset = (self.offset as i64 + i) as usize,
            SeekFrom::End(i) => self.offset = (self.inner.file_size()? as i64 + i) as usize,
        }
        Ok(self.offset as u64)
    }
}

pub struct ObfuscatedFileSystem;

impl FileSystem for ObfuscatedFileSystem {
    type Handle = LogFd;
    type Reader = ObfuscatedFile;
    type Writer = ObfuscatedFile;

    fn create<P: AsRef<Path>>(&self, path: P) -> IoResult<LogFd> {
        LogFd::create(path.as_ref())
    }

    fn open<P: AsRef<Path>>(&self, path: P) -> IoResult<LogFd> {
        LogFd::open(path.as_ref())
    }

    fn new_reader(&self, inner: Arc<LogFd>) -> IoResult<Self::Reader> {
        Ok(ObfuscatedFile { inner, offset: 0 })
    }

    fn new_writer(&self, inner: Arc<LogFd>) -> IoResult<Self::Writer> {
        Ok(ObfuscatedFile { inner, offset: 0 })
    }
}
