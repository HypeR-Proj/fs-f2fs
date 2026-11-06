// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded queue adapter. Writes copy into kernel-owned requests; the callback
//! waits for the whole batch before C++ may release its staging pages.
use super::{Device, Error, INVALID, IO, Operation};
use hyper_os::block::{BlockQueue, NativeBlock, QueueConfig, Ticket};
use std::collections::VecDeque;
const TRANSFER: usize = 512 * 1024;
const OUTSTANDING: usize = 4;
#[derive(Default, Clone, Copy, Debug)]
pub struct Statistics {
    pub reads: u64,
    pub writes: u64,
    pub flushes: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub peak_writes: usize,
    pub peak_reads: usize,
}
pub struct BlockDevice {
    block: Option<NativeBlock>,
    queue: Option<BlockQueue>,
    blocks: u64,
    pending: VecDeque<(Ticket, usize)>,
    stats: Statistics,
}
impl BlockDevice {
    pub fn new(block: NativeBlock, sectors: u64) -> Result<Self, Error> {
        if !sectors.is_multiple_of(8) {
            return Err(INVALID);
        }
        let queue = block.create_queue(QueueConfig::default()).map_err(error)?;
        let mut device = Self::from_queue(sectors, queue)?;
        device.block = Some(block);
        Ok(device)
    }
    pub fn from_queue(sectors: u64, queue: BlockQueue) -> Result<Self, Error> {
        if !sectors.is_multiple_of(8) {
            return Err(INVALID);
        }
        Ok(Self {
            block: None,
            queue: Some(queue),
            blocks: sectors / 8,
            pending: VecDeque::with_capacity(OUTSTANDING),
            stats: Statistics::default(),
        })
    }
    pub fn statistics(&self) -> Statistics {
        self.stats
    }
    #[cfg(feature = "io-statistics")]
    pub fn queue_statistics(&self) -> Result<hyper_os::block::QueueInfo, Error> {
        self.queue()?.info().map_err(error)
    }
    pub fn into_block(mut self) -> Result<NativeBlock, Error> {
        self.drain()?;
        drop(self.queue.take());
        self.block.take().ok_or(INVALID)
    }
    /// Drain adapter-owned writes before acknowledging worker shutdown.
    pub fn close(mut self) -> Result<(), Error> {
        self.drain()?;
        drop(self.queue.take());
        Ok(())
    }
    fn queue(&self) -> Result<&BlockQueue, Error> {
        self.queue.as_ref().ok_or(IO)
    }
    fn poison(&mut self, e: hyper_os::Error) -> Error {
        // No submitted request retains userspace pointers. Closing the capability
        // leaves kernel request leases alive until physical completion; no later
        // operation is admitted through this failed adapter.
        self.queue.take();
        self.pending.clear();
        error(e)
    }
    fn finish(&mut self, ticket: Ticket, output: &mut [u8], expected: usize) -> Result<(), Error> {
        let completion = match self.queue()?.wait(ticket, u64::MAX) {
            Ok(c) => c,
            Err(e) => return Err(self.poison(e)),
        };
        let bytes = if completion.status == hyper_os::Status::OK {
            output
        } else {
            &mut []
        };
        let completion = match self.queue()?.reap(ticket, bytes) {
            Ok(c) => c,
            Err(e) => return Err(self.poison(e)),
        };
        completion.result().map_err(error)?;
        if completion.bytes != expected {
            return Err(self.poison(hyper_os::Error::InvalidResponse));
        }
        Ok(())
    }
    fn finish_one(&mut self) -> Result<(), Error> {
        let (ticket, bytes) = self.pending.pop_front().ok_or(INVALID)?;
        self.finish(ticket, &mut [], bytes)
    }
    fn drain(&mut self) -> Result<(), Error> {
        let mut result = Ok(());
        while !self.pending.is_empty() {
            if let Err(e) = self.finish_one() {
                result = Err(e)
            }
        }
        result
    }
    fn submit(
        &mut self,
        bytes: usize,
        operation: impl Fn(&BlockQueue) -> hyper_os::Result<Ticket>,
    ) -> Result<Ticket, Error> {
        loop {
            match operation(self.queue()?) {
                Ok(t) => return Ok(t),
                Err(hyper_os::Error::Status(hyper_os::Status::WOULD_BLOCK)) => {
                    if self.pending.is_empty() {
                        self.queue()?.wait_space(bytes, u64::MAX).map_err(error)?;
                    } else {
                        self.finish_one()?;
                    }
                }
                Err(e) => return Err(error(e)),
            }
        }
    }
    fn read_group(&mut self, operations: &mut [Operation<'_>]) -> Result<(), Error> {
        self.drain()?;
        super::read_pipeline::run(self, operations)
    }

    fn write_queued(&mut self, offset: u64, input: &[u8]) -> Result<(), Error> {
        for (index, chunk) in input.chunks(TRANSFER).enumerate() {
            if self.pending.len() == OUTSTANDING {
                self.finish_one()?;
            }
            let ticket = self.submit(chunk.len(), |q| {
                q.submit_write((offset + index as u64 * TRANSFER as u64) / 512, chunk)
            })?;
            self.pending.push_back((ticket, chunk.len()));
            self.stats.writes += 1;
            self.stats.write_bytes += chunk.len() as u64;
            self.stats.peak_writes = self.stats.peak_writes.max(self.pending.len());
        }
        Ok(())
    }
}
impl super::read_pipeline::Queue for BlockDevice {
    type Ticket = Ticket;
    fn submit_read(&mut self, sector: u64, bytes: usize) -> Result<Option<Ticket>, Error> {
        match self.queue()?.submit_read(sector, bytes) {
            Ok(ticket) => {
                self.stats.reads += 1;
                self.stats.read_bytes += bytes as u64;
                Ok(Some(ticket))
            }
            Err(hyper_os::Error::Status(hyper_os::Status::WOULD_BLOCK)) => Ok(None),
            Err(e) => Err(error(e)),
        }
    }
    fn wait_space(&mut self, bytes: usize) -> Result<(), Error> {
        self.queue()?.wait_space(bytes, u64::MAX).map_err(error)
    }
    fn finish_read(&mut self, ticket: Ticket, output: &mut [u8]) -> Result<(), Error> {
        let length = output.len();
        self.finish(ticket, output, length)
    }
    fn admitted(&mut self, pending: usize) {
        self.stats.peak_reads = self.stats.peak_reads.max(pending);
    }
}
impl Device for BlockDevice {
    fn blocks(&self) -> u64 {
        self.blocks
    }
    fn read(&mut self, offset: u64, output: &mut [u8]) -> Result<(), Error> {
        self.read_group(&mut [Operation::Read { offset, output }])
    }
    fn write(&mut self, offset: u64, input: &[u8]) -> Result<(), Error> {
        let result = self.write_queued(offset, input);
        let drained = self.drain();
        result.and(drained)
    }
    fn flush(&mut self) -> Result<(), Error> {
        self.drain()?;
        let ticket = self.submit(0, BlockQueue::submit_flush)?;
        self.finish(ticket, &mut [], 0)?;
        self.stats.flushes += 1;
        Ok(())
    }
    fn run(&mut self, operations: &mut [Operation<'_>]) -> Result<(), Error> {
        let result = (|| {
            let mut remaining = operations;
            while !remaining.is_empty() {
                let reads = remaining
                    .iter()
                    .take_while(|op| matches!(op, Operation::Read { .. }))
                    .count();
                if reads != 0 {
                    let (group, rest) = remaining.split_at_mut(reads);
                    self.read_group(group)?;
                    remaining = rest;
                } else {
                    let (operation, rest) = remaining.split_first_mut().ok_or(INVALID)?;
                    if let Operation::Write { offset, input, fua } = operation {
                        self.write_queued(*offset, input)?;
                        if *fua {
                            self.flush()?;
                        }
                    }
                    remaining = rest;
                }
            }
            Ok(())
        })();
        // Even a later submission error cannot skip completion of already accepted writes.
        let drained = self.drain();
        result.and(drained)
    }
}
fn error(e: hyper_os::Error) -> Error {
    match e {
        hyper_os::Error::Status(hyper_os::Status::READ_ONLY) => Error(-30),
        hyper_os::Error::Status(hyper_os::Status::PEER_CLOSED) => Error(-24),
        hyper_os::Error::Status(hyper_os::Status::NO_MEMORY) => Error(-4),
        hyper_os::Error::Status(hyper_os::Status::INVALID_ARGUMENT) => INVALID,
        _ => IO,
    }
}
