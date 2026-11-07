// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Observation failures do not prove that a kernel-owned ticket was retired.
use hyper_os::{Error, Status};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Phase {
    Wait,
    Reap,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ObservationFailure {
    pub phase: Phase,
    pub cause: Error,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Reaped {
    pub status: Status,
    pub bytes: usize,
}

pub(super) trait Queue {
    type Ticket: Copy;
    fn wait(&mut self, ticket: Self::Ticket) -> Result<Status, Error>;
    fn reap(&mut self, ticket: Self::Ticket, output: &mut [u8]) -> Result<Reaped, Error>;
}

/// Only Ok proves consumption. Err leaves ownership unresolved; the Native
/// worker must fail-stop and let its retained supervisor revoke/retire the queue.
/// It must not turn this into an ordinary completed-I/O error or lose the ticket
/// and then wait for credits held by the very request it stopped observing.
pub(super) fn retire<Q: Queue>(
    queue: &mut Q,
    ticket: Q::Ticket,
    output: &mut [u8],
) -> Result<Reaped, ObservationFailure> {
    let status = queue.wait(ticket).map_err(|cause| ObservationFailure {
        phase: Phase::Wait,
        cause,
    })?;
    queue
        .reap(
            ticket,
            if status == Status::OK {
                output
            } else {
                &mut []
            },
        )
        .map_err(|cause| ObservationFailure {
            phase: Phase::Reap,
            cause,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        wait: Result<Status, Error>,
        reap: Result<Reaped, Error>,
        outstanding: bool,
        reaps: usize,
        output_bytes: usize,
    }
    impl Queue for Fake {
        type Ticket = u64;
        fn wait(&mut self, _: u64) -> Result<Status, Error> {
            self.wait
        }
        fn reap(&mut self, _: u64, output: &mut [u8]) -> Result<Reaped, Error> {
            self.reaps += 1;
            self.output_bytes = output.len();
            if self.reap.is_ok() {
                self.outstanding = false;
            }
            self.reap
        }
    }
    fn pending() -> Fake {
        Fake {
            wait: Ok(Status::OK),
            reap: Ok(Reaped {
                status: Status::OK,
                bytes: 4096,
            }),
            outstanding: true,
            reaps: 0,
            output_bytes: 0,
        }
    }
    #[test]
    fn interrupted_or_resource_limited_wait_does_not_certify_retirement() {
        for cause in [Status::CANCELLED, Status::RESOURCE_LIMIT, Status::NO_MEMORY] {
            let mut queue = pending();
            queue.wait = Err(Error::Status(cause));
            assert_eq!(
                retire(&mut queue, 1, &mut []),
                Err(ObservationFailure {
                    phase: Phase::Wait,
                    cause: Error::Status(cause)
                })
            );
            assert!(queue.outstanding);
            assert_eq!(queue.reaps, 0);
        }
    }
    #[test]
    fn failed_reap_remains_unresolved_even_after_successful_wait() {
        let mut queue = pending();
        queue.reap = Err(Error::Status(Status::RESOURCE_LIMIT));
        assert_eq!(
            retire(&mut queue, 1, &mut []),
            Err(ObservationFailure {
                phase: Phase::Reap,
                cause: Error::Status(Status::RESOURCE_LIMIT)
            })
        );
        assert!(queue.outstanding);
        assert_eq!(queue.reaps, 1);
    }
    #[test]
    fn device_error_is_consumed_and_keeps_the_ordinary_error_path() -> Result<(), ObservationFailure>
    {
        let mut queue = pending();
        queue.wait = Ok(Status::IO_ERROR);
        queue.reap = Ok(Reaped {
            status: Status::IO_ERROR,
            bytes: 0,
        });
        assert_eq!(
            retire(&mut queue, 1, &mut [0; 16])?,
            Reaped {
                status: Status::IO_ERROR,
                bytes: 0
            }
        );
        assert!(!queue.outstanding);
        assert_eq!(queue.reaps, 1);
        assert_eq!(queue.output_bytes, 0);
        Ok(())
    }
}
