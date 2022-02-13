// Copyright (c) 2017-present, PingCAP, Inc. Licensed under Apache-2.0.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use log::{info, warn};
use rayon::prelude::*;

use crate::config::{Config, RecoveryMode};
use crate::event_listener::EventListener;
use crate::file_system::FileSystem;
use crate::log_batch::LogItemBatch;
use crate::pipe_log::{FileId, FileSeq, LogQueue};
use crate::Result;

use super::format::FileNameExt;
use super::log_file::build_file_reader;
use super::pipe::{DualPipes, SinglePipe};
use super::reader::LogItemBatchFileReader;
use crate::file_system::Handle;

pub trait ReplayMachine: Send + Default {
    fn replay(&mut self, item_batch: LogItemBatch, file_id: FileId) -> Result<()>;

    fn merge(&mut self, rhs: Self, queue: LogQueue) -> Result<()>;
}

struct FileToRecover<F: FileSystem> {
    seq: FileSeq,
    path: PathBuf,
    fd: Arc<F::Handle>,
}

pub struct DualPipesBuilder<F: FileSystem> {
    cfg: Config,
    file_system: Arc<F>,
    listeners: Vec<Arc<dyn EventListener>>,

    append_files: Vec<FileToRecover<F>>,
    rewrite_files: Vec<FileToRecover<F>>,
}

impl<F: FileSystem> DualPipesBuilder<F> {
    pub fn new(cfg: Config, file_system: Arc<F>, listeners: Vec<Arc<dyn EventListener>>) -> Self {
        Self {
            cfg,
            file_system,
            listeners,
            append_files: Vec::new(),
            rewrite_files: Vec::new(),
        }
    }

    pub fn scan(&mut self) -> Result<()> {
        let dir = &self.cfg.dir;
        let path = Path::new(dir);
        if !path.exists() {
            info!("Create raft log directory: {}", dir);
            fs::create_dir(dir)?;
            return Ok(());
        }
        if !path.is_dir() {
            return Err(box_err!("Not directory: {}", dir));
        }

        let (mut min_append_id, mut max_append_id) = (u64::MAX, 0);
        let (mut min_rewrite_id, mut max_rewrite_id) = (u64::MAX, 0);
        fs::read_dir(path)?.for_each(|e| {
            if let Ok(e) = e {
                let p = e.path();
                if p.is_file() {
                    match FileId::parse_file_name(p.file_name().unwrap().to_str().unwrap()) {
                        Some(FileId {
                            queue: LogQueue::Append,
                            seq,
                        }) => {
                            min_append_id = std::cmp::min(min_append_id, seq);
                            max_append_id = std::cmp::max(max_append_id, seq);
                        }
                        Some(FileId {
                            queue: LogQueue::Rewrite,
                            seq,
                        }) => {
                            min_rewrite_id = std::cmp::min(min_rewrite_id, seq);
                            max_rewrite_id = std::cmp::max(max_rewrite_id, seq);
                        }
                        _ => {}
                    }
                }
            }
        });

        for (queue, min_id, max_id, files) in [
            (
                LogQueue::Append,
                min_append_id,
                max_append_id,
                &mut self.append_files,
            ),
            (
                LogQueue::Rewrite,
                min_rewrite_id,
                max_rewrite_id,
                &mut self.rewrite_files,
            ),
        ] {
            if max_id > 0 {
                for seq in min_id..=max_id {
                    let file_id = FileId { queue, seq };
                    let path = file_id.build_file_path(dir);
                    if !path.exists() {
                        warn!(
                            "Detected a hole when scanning directory, discarding files before {:?}.",
                            file_id,
                        );
                        files.clear();
                    } else {
                        let fd = Arc::new(self.file_system.open(&path)?);
                        files.push(FileToRecover { seq, path, fd });
                    }
                }
            }
        }
        Ok(())
    }

    pub fn recover<S: ReplayMachine>(&mut self) -> Result<(S, S)> {
        let threads = self.cfg.recovery_threads;
        let (append_concurrency, rewrite_concurrency) =
            match (self.append_files.len(), self.rewrite_files.len()) {
                (a, b) if a > 0 && b > 0 => {
                    let a_threads = std::cmp::max(1, threads * a / (a + b));
                    let b_threads = std::cmp::max(1, threads.saturating_sub(a_threads));
                    (a_threads, b_threads)
                }
                _ => (threads, threads),
            };

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        let (append, rewrite) = pool.join(
            || {
                Self::recover_queue(
                    LogQueue::Append,
                    self.file_system.clone(),
                    self.cfg.recovery_mode,
                    append_concurrency,
                    self.cfg.recovery_read_block_size.0 as usize,
                    &self.append_files,
                )
            },
            || {
                Self::recover_queue(
                    LogQueue::Rewrite,
                    self.file_system.clone(),
                    self.cfg.recovery_mode,
                    rewrite_concurrency,
                    self.cfg.recovery_read_block_size.0 as usize,
                    &self.rewrite_files,
                )
            },
        );

        Ok((append?, rewrite?))
    }

    pub fn finish(self) -> Result<DualPipes<F>> {
        let appender = self.build_pipe(LogQueue::Append)?;
        let rewriter = self.build_pipe(LogQueue::Rewrite)?;
        DualPipes::open(&self.cfg.dir, appender, rewriter)
    }

    fn recover_queue<S: ReplayMachine>(
        queue: LogQueue,
        file_system: Arc<F>,
        recovery_mode: RecoveryMode,
        concurrency: usize,
        read_block_size: usize,
        files: &[FileToRecover<F>],
    ) -> Result<S> {
        if concurrency == 0 || files.is_empty() {
            return Ok(S::default());
        }
        let max_chunk_size = std::cmp::max((files.len() + concurrency - 1) / concurrency, 1);
        let chunks = files.par_chunks(max_chunk_size);
        let chunk_count = chunks.len();
        debug_assert!(chunk_count <= concurrency);
        let sequential_replay_machine = chunks
            .enumerate()
            .map(|(index, chunk)| {
                let mut reader = LogItemBatchFileReader::new(read_block_size);
                let mut sequential_replay_machine = S::default();
                let file_count = chunk.len();
                for (i, f) in chunk.iter().enumerate() {
                    let is_last_file = index == chunk_count - 1 && i == file_count - 1;
                    reader.open(
                        FileId { queue, seq: f.seq },
                        build_file_reader(file_system.as_ref(), &f.path, f.fd.clone())?,
                    )?;
                    loop {
                        match reader.next() {
                            Ok(Some(item_batch)) => {
                                sequential_replay_machine
                                    .replay(item_batch, FileId { queue, seq: f.seq })?;
                            }
                            Ok(None) => break,
                            Err(e)
                                if recovery_mode == RecoveryMode::TolerateTailCorruption
                                    && is_last_file =>
                            {
                                warn!("The tail of raft log is corrupted but ignored: {}", e);
                                f.fd.truncate(reader.valid_offset())?;
                                break;
                            }
                            Err(e) if recovery_mode == RecoveryMode::TolerateAnyCorruption => {
                                warn!("File is corrupted but ignored: {}", e);
                                f.fd.truncate(reader.valid_offset())?;
                                break;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
                Ok(sequential_replay_machine)
            })
            .try_reduce(
                S::default,
                |mut sequential_replay_machine_left, sequential_replay_machine_right| {
                    sequential_replay_machine_left.merge(sequential_replay_machine_right, queue)?;
                    Ok(sequential_replay_machine_left)
                },
            )?;
        Ok(sequential_replay_machine)
    }

    fn build_pipe(&self, queue: LogQueue) -> Result<SinglePipe<F>> {
        let files = match queue {
            LogQueue::Append => &self.append_files,
            LogQueue::Rewrite => &self.rewrite_files,
        };
        let first_seq = files.first().map(|f| f.seq).unwrap_or(0);
        let files: VecDeque<Arc<F::Handle>> = files.iter().map(|f| f.fd.clone()).collect();
        SinglePipe::open(
            &self.cfg,
            self.file_system.clone(),
            self.listeners.clone(),
            queue,
            first_seq,
            files,
        )
    }
}
