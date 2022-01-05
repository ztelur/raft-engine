// Copyright (c) 2017-present, PingCAP, Inc. Licensed under Apache-2.0.

use std::io::{Read, Result, Seek, Write};
use std::path::Path;
use std::sync::Arc;

/// FileSystem
pub trait FileSystem: Send + Sync {
    type Handle: Clone + Send + Sync;
    type Reader: Seek + Read + Send;
    type Writer: Seek + Write + Send + WriteExt;
    
    fn create<P: AsRef<Path>>(&self, path: P) -> Result<Self::Handle>;
    fn open<P: AsRef<Path>>(&self, path: P) -> Result<Self::Handle>;
    fn new_reader(&self, handle: Arc<Self::Handle>) -> Result<Self::Reader>;
    fn new_writer(&self, handle: Arc<Self::Handle>) -> Result<Self::Writer>;
    fn file_size(&self, handle: Arc<Self::Handle>) -> Result<usize>;
}

pub trait WriteExt {
    fn finish(&self) -> IoResult<()>;
}