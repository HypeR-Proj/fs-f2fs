// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded queue adapter. Write submission copies into kernel-owned requests;
//! independently owned completions retire tickets without the device mutex.
use super::{Device, Error, INVALID, IO, Operation};
use hyper_os::block::{BlockQueue, NativeBlock, QueueConfig, Ticket, WriteRequest};
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
struct QueueOwner {
    queue: BlockQueue,
    poisoned: AtomicBool,
    write_tickets: AtomicUsize,
    peak_write_tickets: AtomicUsize,
}
struct QueueAccess {
    owner: Arc<QueueOwner>,
    writes: u64,
    bytes: u64,
}
impl QueueAccess {
    fn new(owner: Arc<QueueOwner>) -> Self {
        Self {
            owner,
            writes: 0,
            bytes: 0,
        }
    }
}
impl super::write_pipeline::Queue for QueueAccess {
    type Ticket = Ticket;
    fn submit_writes(&mut self, ranges: &[WriteRequest<'_>]) -> Result<Option<Ticket>, Error> {
        if self.owner.poisoned.load(Ordering::Acquire) {
            return Err(IO);
        }
        match self.owner.queue.submit_writes(ranges) {
            Ok(ticket) => {
                let count = self.owner.write_tickets.fetch_add(1, Ordering::Relaxed) + 1;
                self.owner
                    .peak_write_tickets
                    .fetch_max(count, Ordering::Relaxed);
                self.writes += 1;
                self.bytes += ranges
                    .iter()
                    .map(|range| range.bytes.len() as u64)
                    .sum::<u64>();
                Ok(Some(ticket))
            }
            Err(hyper_os::Error::Status(hyper_os::Status::WOULD_BLOCK)) => Ok(None),
            Err(e) => Err(error(e)),
        }
    }
    fn wait_space(&mut self, bytes: usize) -> Result<(), Error> {
        if self.owner.poisoned.load(Ordering::Acquire) {
            return Err(IO);
        }
        self.owner.queue.wait_space(bytes, u64::MAX).map_err(error)
    }
    fn finish(&mut self, ticket: Ticket, expected: usize) -> Result<(), Error> {
        finish_write_owned(&self.owner, ticket, expected)
    }
}
struct WriteCompletion {
    owner: Arc<QueueOwner>,
    tickets: VecDeque<(Ticket, usize)>,
    result: Result<(), Error>,
}
impl Drop for WriteCompletion {
    fn drop(&mut self) {
        // Releasing an unused completion owner must still return queue credits.
        let _ = super::write_pipeline::drain(
            &mut QueueAccess::new(self.owner.clone()),
            &mut self.tickets,
        );
    }
}
impl super::DeviceCompletion for WriteCompletion {
    fn wait(mut self: Box<Self>) -> Result<(), Error> {
        let completed = super::write_pipeline::drain(
            &mut QueueAccess::new(self.owner.clone()),
            &mut self.tickets,
        );
        self.result.and(completed)
    }
}
fn finish_write_owned(owner: &QueueOwner, ticket: Ticket, expected: usize) -> Result<(), Error> {
    let result = finish_owned(owner, ticket, &mut [], expected);
    owner.write_tickets.fetch_sub(1, Ordering::Relaxed);
    result
}
struct ObservationQueue<'a>(&'a BlockQueue);
impl super::completion::Queue for ObservationQueue<'_> {
    type Ticket = Ticket;
    fn wait(&mut self, ticket: Ticket) -> hyper_os::Result<hyper_os::Status> {
        Ok(self.0.wait(ticket, u64::MAX)?.status)
    }
    fn reap(
        &mut self,
        ticket: Ticket,
        output: &mut [u8],
    ) -> hyper_os::Result<super::completion::Reaped> {
        let completion = self.0.reap(ticket, output)?;
        Ok(super::completion::Reaped {
            status: completion.status,
            bytes: completion.bytes,
        })
    }
}

#[cold]
fn completion_observation_lost(
    ticket: Ticket,
    failure: super::completion::ObservationFailure,
) -> ! {
    // This worker holds only a delegated queue alias. It cannot revoke the
    // parent's alias or prove DMA retirement after losing wait/reap ownership.
    // Do not unwind into an apparently completed receipt, or deadlock shutdown
    // behind another producer waiting for credits that the lost ticket retains.
    struct Diagnostic {
        bytes: [u8; 320],
        length: usize,
    }
    impl std::fmt::Write for Diagnostic {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            let count = text.len().min(self.bytes.len() - self.length);
            self.bytes[self.length..self.length + count].copy_from_slice(&text.as_bytes()[..count]);
            self.length += count;
            Ok(())
        }
    }
    use std::fmt::Write;
    let mut report = Diagnostic {
        bytes: [0; 320],
        length: 0,
    };
    let _ = writeln!(
        report,
        "CRITICAL: fs-f2fs block completion observation lost: ticket={:#x} phase={:?} error={:?}; terminating worker; supervisor queue retirement required",
        ticket.get(),
        failure.phase,
        failure.cause
    );
    let _ = hyper_os::thread::set_current_name("f2fs-completion-lost");
    if let Ok(stderr) = hyper_rt::process::stderr() {
        let _ = stderr
            .as_byte_channel()
            .try_send(&report.bytes[..report.length]);
    }
    if let Ok(console) = hyper_rt::process::console() {
        let _ = console
            .as_emergency_console()
            .write_all(&report.bytes[..report.length]);
    }
    // Native abort maps directly to process_exit and skips stdio cleanup locks.
    // fs-backend reports the death and quarantines the volume. Explicit failed
    // recovery waits for ProcessPhase::Retired, then revokes every queue alias
    // and waits for physical retirement before allowing volume reuse.
    std::process::abort()
}

fn finish_owned(
    owner: &QueueOwner,
    ticket: Ticket,
    output: &mut [u8],
    expected: usize,
) -> Result<(), Error> {
    let completion =
        match super::completion::retire(&mut ObservationQueue(&owner.queue), ticket, output) {
            Ok(completion) => completion,
            Err(failure) => completion_observation_lost(ticket, failure),
        };
    // A device error is an ordinary failure only after reap consumed the ticket.
    let result = if completion.status != hyper_os::Status::OK {
        Err(error(hyper_os::Error::Status(completion.status)))
    } else if completion.bytes != expected {
        Err(IO)
    } else {
        Ok(())
    };
    if result.is_err() {
        owner.poisoned.store(true, Ordering::Release);
    }
    result
}

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
    queue: Option<Arc<QueueOwner>>,
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
            queue: Some(Arc::new(QueueOwner {
                queue,
                poisoned: AtomicBool::new(false),
                write_tickets: AtomicUsize::new(0),
                peak_write_tickets: AtomicUsize::new(0),
            })),
            blocks: sectors / 8,
            pending: VecDeque::with_capacity(OUTSTANDING),
            stats: Statistics::default(),
        })
    }
    pub fn statistics(&self) -> Statistics {
        let mut stats = self.stats;
        if let Some(owner) = &self.queue {
            stats.peak_writes = owner.peak_write_tickets.load(Ordering::Relaxed);
        }
        stats
    }
    #[cfg(feature = "io-statistics")]
    pub fn queue_statistics(&self) -> Result<hyper_os::block::QueueInfo, Error> {
        self.queue()?.info().map_err(error)
    }
    pub fn into_block(mut self) -> Result<NativeBlock, Error> {
        self.drain()?;
        if self
            .queue
            .as_ref()
            .is_some_and(|queue| Arc::strong_count(queue) != 1)
        {
            return Err(INVALID);
        }
        drop(self.queue.take());
        self.block.take().ok_or(INVALID)
    }
    /// Drain adapter-owned writes before acknowledging worker shutdown.
    pub fn close(mut self) -> Result<(), Error> {
        self.drain()?;
        if self
            .queue
            .as_ref()
            .is_some_and(|queue| Arc::strong_count(queue) != 1)
        {
            return Err(INVALID);
        }
        drop(self.queue.take());
        Ok(())
    }
    fn queue(&self) -> Result<&BlockQueue, Error> {
        let owner = self.queue.as_ref().ok_or(IO)?;
        if owner.poisoned.load(Ordering::Acquire) {
            return Err(IO);
        }
        Ok(&owner.queue)
    }
    fn finish(&mut self, ticket: Ticket, output: &mut [u8], expected: usize) -> Result<(), Error> {
        let owner = self.queue.as_ref().ok_or(IO)?;
        // Retirement still runs after poison: only new admission is prohibited.
        finish_owned(owner, ticket, output, expected)
    }
    fn finish_one(&mut self) -> Result<(), Error> {
        let (ticket, bytes) = self.pending.pop_front().ok_or(INVALID)?;
        finish_write_owned(self.queue.as_ref().ok_or(IO)?, ticket, bytes)
    }
    fn drain(&mut self) -> Result<(), Error> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let owner = self.queue.as_ref().cloned().ok_or(IO)?;
        super::write_pipeline::drain(&mut QueueAccess::new(owner), &mut self.pending)
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

    fn write_group(&mut self, operations: &[Operation<'_>]) -> Result<(), Error> {
        let owner = self.queue.as_ref().cloned().ok_or(IO)?;
        let mut queue = QueueAccess::new(owner);
        let result = super::write_batch::for_each_batch(operations, |ranges, bytes| {
            super::write_pipeline::admit(
                &mut queue,
                &mut self.pending,
                OUTSTANDING,
                ranges,
                bytes,
            )?;
            Ok(())
        });
        self.stats.writes += queue.writes;
        self.stats.write_bytes += queue.bytes;
        self.stats.peak_writes = queue.owner.peak_write_tickets.load(Ordering::Relaxed);
        result
    }
    fn write_queued(&mut self, offset: u64, input: &[u8]) -> Result<(), Error> {
        self.write_group(&[Operation::Write {
            offset,
            input,
            fua: false,
        }])
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
    fn submit(
        &mut self,
        operations: &mut [Operation<'_>],
    ) -> Result<Box<dyn super::DeviceCompletion>, Error> {
        let owner = self.queue.as_ref().cloned().ok_or(IO)?;
        // Own the queue and ticket storage before the first kernel submission.
        // Completion never reacquires the outer Mutex<BlockDevice>.
        let mut tickets = VecDeque::new();
        tickets
            .try_reserve_exact(OUTSTANDING)
            .map_err(|_| super::NO_MEMORY)?;
        let mut completion = super::try_box(WriteCompletion {
            owner,
            tickets,
            result: Ok(()),
        })?;
        let mut queue = QueueAccess::new(completion.owner.clone());
        completion.result = self.drain().and_then(|()| {
            super::write_batch::for_each_batch(operations, |ranges, bytes| {
                super::write_pipeline::admit(
                    &mut queue,
                    &mut completion.tickets,
                    OUTSTANDING,
                    ranges,
                    bytes,
                )?;
                Ok(())
            })
        });
        self.stats.writes += queue.writes;
        self.stats.write_bytes += queue.bytes;
        self.stats.peak_writes = queue.owner.peak_write_tickets.load(Ordering::Relaxed);
        Ok(completion)
    }
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
                    let ordinary = remaining
                        .iter()
                        .take_while(|operation| {
                            matches!(operation, Operation::Write { fua: false, .. })
                        })
                        .count();
                    if ordinary != 0 {
                        let (group, rest) = remaining.split_at_mut(ordinary);
                        self.write_group(group)?;
                        remaining = rest;
                    } else {
                        let (operation, rest) = remaining.split_first_mut().ok_or(INVALID)?;
                        let Operation::Write {
                            offset,
                            input,
                            fua: true,
                        } = operation
                        else {
                            return Err(INVALID);
                        };
                        // FUA remains its own write followed by the existing
                        // device-wide flush; later ordinary writes cannot cross it.
                        self.write_queued(*offset, input)?;
                        self.flush()?;
                        remaining = rest;
                    }
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
