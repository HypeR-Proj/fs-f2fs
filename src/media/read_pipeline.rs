// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Borrow retirement and bounded read admission, shared by Native and fault tests.
use super::{Error, INVALID, Operation};
use std::collections::VecDeque;

pub(super) trait Queue {
    type Ticket;
    /// None means credits are temporarily unavailable.
    fn submit_read(&mut self, sector: u64, bytes: usize) -> Result<Option<Self::Ticket>, Error>;
    fn wait_space(&mut self, bytes: usize) -> Result<(), Error>;
    /// Returning, including an error, retires request ownership and this borrow.
    /// A lost wait/reap observation cannot be reported as an ordinary completion.
    fn finish_read(&mut self, ticket: Self::Ticket, output: &mut [u8]) -> Result<(), Error>;
    fn admitted(&mut self, pending: usize);
}

pub(super) fn run<Q: Queue>(queue: &mut Q, operations: &mut [Operation<'_>]) -> Result<(), Error> {
    const TRANSFER: usize = 512 * 1024;
    const OUTSTANDING: usize = 4;
    let mut pending: VecDeque<(Q::Ticket, &mut [u8])> = VecDeque::with_capacity(OUTSTANDING);
    let result = (|| {
        for operation in operations {
            let Operation::Read { offset, output } = operation else {
                return Err(INVALID);
            };
            for (index, chunk) in output.chunks_mut(TRANSFER).enumerate() {
                if pending.len() == OUTSTANDING {
                    let (ticket, output) = pending.pop_front().ok_or(INVALID)?;
                    queue.finish_read(ticket, output)?;
                }
                let ticket = loop {
                    match queue.submit_read(
                        (*offset + index as u64 * TRANSFER as u64) / 512,
                        chunk.len(),
                    )? {
                        Some(ticket) => break ticket,
                        None => {
                            if let Some((ticket, output)) = pending.pop_front() {
                                queue.finish_read(ticket, output)?;
                            } else {
                                queue.wait_space(chunk.len())?;
                            }
                        }
                    }
                };
                pending.push_back((ticket, chunk));
                queue.admitted(pending.len());
            }
        }
        Ok(())
    })();
    // Keep draining after the first error. Transport poison may retire requests
    // by closing their kernel-owned storage instead, but never retains a borrow.
    let mut drained = Ok(());
    while let Some((ticket, output)) = pending.pop_front() {
        if let Err(error) = queue.finish_read(ticket, output) {
            drained = Err(error);
        }
    }
    result.and(drained)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::IO;

    struct FaultQueue {
        live: Vec<usize>,
        next: usize,
        capacity: usize,
        admission_error: Option<usize>,
        completion_error: Option<usize>,
        poison: bool,
        closed: bool,
        external_pressure: bool,
        waits: usize,
        finishes: usize,
        peak: usize,
    }
    impl FaultQueue {
        fn new() -> Self {
            Self {
                live: vec![],
                next: 0,
                capacity: 4,
                admission_error: None,
                completion_error: None,
                poison: false,
                closed: false,
                external_pressure: false,
                waits: 0,
                finishes: 0,
                peak: 0,
            }
        }
    }
    impl Queue for FaultQueue {
        type Ticket = usize;
        fn submit_read(&mut self, _: u64, _: usize) -> Result<Option<usize>, Error> {
            if self.closed || self.admission_error == Some(self.next) {
                return Err(IO);
            }
            if self.external_pressure || self.live.len() == self.capacity {
                return Ok(None);
            }
            let id = self.next;
            self.next += 1;
            self.live.push(id);
            Ok(Some(id))
        }
        fn wait_space(&mut self, _: usize) -> Result<(), Error> {
            // Waiting with our own unreaped reads would deadlock this device.
            assert!(self.live.is_empty());
            self.waits += 1;
            self.external_pressure = false;
            Ok(())
        }
        fn finish_read(&mut self, ticket: usize, output: &mut [u8]) -> Result<(), Error> {
            self.finishes += 1;
            if self.closed {
                return Err(IO);
            }
            let index = self
                .live
                .iter()
                .position(|id| *id == ticket)
                .ok_or(INVALID)?;
            self.live.remove(index);
            if self.completion_error == Some(ticket) {
                if self.poison {
                    self.closed = true;
                    self.live.clear();
                }
                return Err(IO);
            }
            output.fill(ticket as u8 + 1);
            Ok(())
        }
        fn admitted(&mut self, pending: usize) {
            self.peak = self.peak.max(pending);
        }
    }
    fn exercise(queue: &mut FaultQueue) -> Result<(), Error> {
        let mut buffers = [[0; 512]; 6];
        let mut operations: Vec<_> = buffers
            .iter_mut()
            .enumerate()
            .map(|(i, b)| Operation::Read {
                offset: (i * 4096) as u64,
                output: b,
            })
            .collect();
        let result = run(queue, &mut operations);
        drop(operations);
        if result.is_ok() {
            for (i, buffer) in buffers.iter().enumerate() {
                assert_eq!(*buffer, [i as u8 + 1; 512]);
            }
        }
        assert!(queue.live.is_empty());
        result
    }
    #[test]
    fn bounded_pipeline_and_credit_recovery() -> Result<(), Error> {
        let mut q = FaultQueue::new();
        exercise(&mut q)?;
        assert_eq!(q.peak, 4);
        q = FaultQueue::new();
        q.capacity = 1;
        q.external_pressure = true;
        exercise(&mut q)?;
        assert_eq!(q.waits, 1);
        assert_eq!(q.finishes, 6);
        Ok(())
    }
    #[test]
    fn later_admission_failure_retires_all_accepted_reads() {
        let mut q = FaultQueue::new();
        q.admission_error = Some(3);
        assert_eq!(exercise(&mut q), Err(IO));
        assert_eq!(q.finishes, 3);
    }
    #[test]
    fn completion_failure_and_poison_retire_the_rest() {
        for poison in [false, true] {
            let mut q = FaultQueue::new();
            q.completion_error = Some(0);
            q.poison = poison;
            assert_eq!(exercise(&mut q), Err(IO));
            assert_eq!(q.next, 4);
            assert_eq!(q.finishes, 4);
            assert_eq!(q.closed, poison);
        }
    }
}
