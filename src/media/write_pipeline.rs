// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded ticket admission and complete retirement, shared by Native and tests.
use super::{Error, INVALID};
use hyper_os::block::WriteRequest;
use std::collections::VecDeque;

pub(super) trait Queue {
    type Ticket;
    /// None is a rejected admission: no range or byte has been accepted.
    fn submit_writes(&mut self, ranges: &[WriteRequest<'_>])
    -> Result<Option<Self::Ticket>, Error>;
    fn wait_space(&mut self, bytes: usize) -> Result<(), Error>;
    /// Consumes ticket ownership even when returning a device error. A backend
    /// unable to prove reap/retirement must fail-stop or transfer ownership to a
    /// guaranteed retirement owner; an observation error cannot simply return.
    fn finish(&mut self, ticket: Self::Ticket, expected: usize) -> Result<(), Error>;
}

/// Reserve descriptor storage before use. All accepted tickets remain in pending
/// until finish consumes them; admission failure leaves prior tickets for drain.
pub(super) fn admit<Q: Queue>(
    queue: &mut Q,
    pending: &mut VecDeque<(Q::Ticket, usize)>,
    maximum: usize,
    ranges: &[WriteRequest<'_>],
    bytes: usize,
) -> Result<(), Error> {
    if maximum == 0 || maximum > 4 || pending.capacity() < maximum || pending.len() > maximum {
        return Err(INVALID);
    }
    let total = ranges
        .iter()
        .try_fold(0usize, |total, range| total.checked_add(range.bytes.len()));
    if ranges.is_empty()
        || ranges.len() > 16
        || bytes == 0
        || bytes > 512 * 1024
        || total != Some(bytes)
    {
        return Err(INVALID);
    }
    if pending.len() == maximum {
        let (ticket, expected) = pending.pop_front().ok_or(INVALID)?;
        queue.finish(ticket, expected)?;
    }
    loop {
        match queue.submit_writes(ranges)? {
            Some(ticket) => {
                // Capacity was checked before admission; this push cannot allocate.
                pending.push_back((ticket, bytes));
                return Ok(());
            }
            None => {
                if let Some((ticket, expected)) = pending.pop_front() {
                    queue.finish(ticket, expected)?;
                } else {
                    // Other accepted groups have independent completion owners;
                    // this wait must not hold a lock needed by their retirement.
                    queue.wait_space(bytes)?;
                }
            }
        }
    }
}

/// Continue retiring after failure. A failed completion does not cancel the
/// remaining accepted tickets or permit their owned storage to be reused early.
pub(super) fn drain<Q: Queue>(
    queue: &mut Q,
    pending: &mut VecDeque<(Q::Ticket, usize)>,
) -> Result<(), Error> {
    let mut result = Ok(());
    while let Some((ticket, expected)) = pending.pop_front() {
        let completed = queue.finish(ticket, expected);
        if result.is_ok() {
            result = completed;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::IO;

    #[derive(Default)]
    struct FakeQueue {
        replies: VecDeque<Result<Option<u64>, Error>>,
        submissions: usize,
        accepted: Vec<u64>,
        finished: Vec<(u64, usize)>,
        waits: Vec<usize>,
        fail_completion: Option<u64>,
    }
    impl Queue for FakeQueue {
        type Ticket = u64;
        fn submit_writes(&mut self, _: &[WriteRequest<'_>]) -> Result<Option<u64>, Error> {
            self.submissions += 1;
            let result = self.replies.pop_front().ok_or(INVALID)?;
            if let Ok(Some(ticket)) = result {
                self.accepted.push(ticket);
            }
            result
        }
        fn wait_space(&mut self, bytes: usize) -> Result<(), Error> {
            self.waits.push(bytes);
            Ok(())
        }
        fn finish(&mut self, ticket: u64, expected: usize) -> Result<(), Error> {
            self.finished.push((ticket, expected));
            if self.fail_completion == Some(ticket) {
                Err(IO)
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn full_window_retires_one_before_accepting_more() -> Result<(), Error> {
        let mut queue = FakeQueue {
            replies: [Ok(Some(5))].into(),
            ..FakeQueue::default()
        };
        let mut pending = VecDeque::with_capacity(4);
        pending.extend((1..=4).map(|ticket| (ticket, 512)));
        let input = [0; 512];
        admit(
            &mut queue,
            &mut pending,
            4,
            &[WriteRequest {
                first: 8,
                bytes: &input,
            }],
            input.len(),
        )?;
        assert_eq!(queue.finished, [(1, 512)]);
        assert_eq!(
            pending.iter().copied().collect::<Vec<_>>(),
            [(2, 512), (3, 512), (4, 512), (5, 512)]
        );
        Ok(())
    }

    #[test]
    fn rejection_retries_identical_batch_without_duplicate_admission() -> Result<(), Error> {
        let mut queue = FakeQueue {
            replies: [Ok(None), Ok(None), Ok(Some(2))].into(),
            ..FakeQueue::default()
        };
        let mut pending = VecDeque::with_capacity(4);
        pending.push_back((1, 512));
        let input = [0; 512];
        admit(
            &mut queue,
            &mut pending,
            4,
            &[WriteRequest {
                first: 8,
                bytes: &input,
            }],
            512,
        )?;
        assert_eq!(queue.submissions, 3);
        assert_eq!(queue.accepted, [2]);
        assert_eq!(queue.finished, [(1, 512)]);
        assert_eq!(queue.waits, [512]);
        assert_eq!(pending.iter().copied().collect::<Vec<_>>(), [(2, 512)]);
        Ok(())
    }

    #[test]
    fn submission_error_keeps_prior_work_and_drain_continues_after_late_failure() {
        let mut queue = FakeQueue {
            replies: [Err(INVALID)].into(),
            fail_completion: Some(1),
            ..FakeQueue::default()
        };
        let mut pending = VecDeque::with_capacity(4);
        pending.extend([(1, 512), (2, 1024)]);
        let input = [0; 512];
        assert_eq!(
            admit(
                &mut queue,
                &mut pending,
                4,
                &[WriteRequest {
                    first: 8,
                    bytes: &input
                }],
                512
            ),
            Err(INVALID)
        );
        assert_eq!(pending.len(), 2);
        assert_eq!(drain(&mut queue, &mut pending), Err(IO));
        assert_eq!(queue.finished, [(1, 512), (2, 1024)]);
        assert!(pending.is_empty());
    }

    #[test]
    fn failed_window_retirement_does_not_accept_new_work() {
        let mut queue = FakeQueue {
            fail_completion: Some(1),
            ..FakeQueue::default()
        };
        let mut pending = VecDeque::with_capacity(4);
        pending.extend((1..=4).map(|ticket| (ticket, 512)));
        let input = [0; 512];
        assert_eq!(
            admit(
                &mut queue,
                &mut pending,
                4,
                &[WriteRequest {
                    first: 8,
                    bytes: &input
                }],
                512
            ),
            Err(IO)
        );
        assert_eq!(queue.submissions, 0);
        assert_eq!(drain(&mut queue, &mut pending), Ok(()));
        assert_eq!(queue.finished.len(), 4);
        assert!(pending.is_empty());
    }

    #[test]
    fn descriptor_reservation_and_byte_bounds_precede_admission() {
        let mut queue = FakeQueue::default();
        let input = [0; 512];
        let ranges = [WriteRequest {
            first: 0,
            bytes: &input,
        }];
        let mut empty = VecDeque::new();
        assert_eq!(admit(&mut queue, &mut empty, 4, &ranges, 512), Err(INVALID));
        let mut pending = VecDeque::with_capacity(4);
        assert_eq!(
            admit(&mut queue, &mut pending, 4, &ranges, 1024),
            Err(INVALID)
        );
        assert_eq!(admit(&mut queue, &mut pending, 4, &[], 0), Err(INVALID));
        assert_eq!(queue.submissions, 0);
        assert!(pending.is_empty());
    }
}
