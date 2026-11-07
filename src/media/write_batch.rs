// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded scatter submission for an already available group of ordinary writes.
//! Disk order and individual source borrows survive batching; no payload is copied.
use super::{Error, INVALID, Operation};
use hyper_os::block::WriteRequest;

const SECTOR_BYTES: usize = 512;
const MAX_RANGES: usize = 16;
const MAX_BYTES: usize = 512 * 1024;

/// Each callback consumes one batch before its borrowed descriptor array is reused.
/// The caller owns admission retries and retirement of already accepted batches.
/// Read/FUA operations terminate an ordinary group at the caller; they are rejected
/// here before any callback, as are malformed offsets and byte lengths.
pub(super) fn for_each_batch(
    operations: &[Operation<'_>],
    mut submit: impl FnMut(&[WriteRequest<'_>], usize) -> Result<(), Error>,
) -> Result<(), Error> {
    for operation in operations {
        let Operation::Write {
            offset,
            input,
            fua: false,
        } = operation
        else {
            return Err(INVALID);
        };
        if !offset.is_multiple_of(SECTOR_BYTES as u64)
            || !input.len().is_multiple_of(SECTOR_BYTES)
            || offset
                .checked_add(u64::try_from(input.len()).map_err(|_| INVALID)?)
                .is_none()
        {
            return Err(INVALID);
        }
    }
    let mut ranges = [WriteRequest {
        first: 0,
        bytes: &[],
    }; MAX_RANGES];
    let mut count = 0;
    let mut bytes = 0;
    for operation in operations {
        let Operation::Write { offset, input, .. } = operation else {
            return Err(INVALID);
        };
        let mut consumed = 0usize;
        while consumed < input.len() {
            if count == MAX_RANGES || bytes == MAX_BYTES {
                submit(&ranges[..count], bytes)?;
                count = 0;
                bytes = 0;
            }
            let length = (input.len() - consumed).min(MAX_BYTES - bytes);
            let first = offset.checked_add(consumed as u64).ok_or(INVALID)? / SECTOR_BYTES as u64;
            let end = first
                .checked_add((length / SECTOR_BYTES) as u64)
                .ok_or(INVALID)?;
            let mut overlap = false;
            for previous in &ranges[..count] {
                let previous_end = previous
                    .first
                    .checked_add((previous.bytes.len() / SECTOR_BYTES) as u64)
                    .ok_or(INVALID)?;
                overlap |= first < previous_end && previous.first < end;
            }
            if overlap {
                // Never reorder or combine repeated writes to the same sector.
                // The next ticket remains ordered behind this earlier batch.
                submit(&ranges[..count], bytes)?;
                count = 0;
                bytes = 0;
                continue;
            }
            ranges[count] = WriteRequest {
                first,
                bytes: &input[consumed..consumed + length],
            };
            count += 1;
            bytes += length;
            consumed += length;
        }
    }
    if count != 0 {
        submit(&ranges[..count], bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ordinary(offset: u64, input: &[u8]) -> Operation<'_> {
        Operation::Write {
            offset,
            input,
            fua: false,
        }
    }

    #[test]
    fn sixteen_small_disjoint_writes_use_one_batch_with_original_source_bytes() -> Result<(), Error>
    {
        let sources: Vec<_> = (0..16).map(|index| vec![index; 4096]).collect();
        let operations: Vec<_> = sources
            .iter()
            .enumerate()
            .map(|(index, bytes)| ordinary(index as u64 * 8192, bytes))
            .collect();
        let mut callbacks = 0;
        for_each_batch(&operations, |ranges, bytes| {
            callbacks += 1;
            assert_eq!(ranges.len(), 16);
            assert_eq!(bytes, 16 * 4096);
            for (index, range) in ranges.iter().enumerate() {
                assert_eq!(range.first, index as u64 * 16);
                assert_eq!(range.bytes, sources[index]);
                assert!(std::ptr::eq(range.bytes.as_ptr(), sources[index].as_ptr()));
            }
            Ok(())
        })?;
        assert_eq!(callbacks, 1);
        Ok(())
    }

    #[test]
    fn range_limit_and_final_partial_batch_do_not_wait_for_more_work() -> Result<(), Error> {
        let input = [7; 4096];
        let operations: Vec<_> = (0..17)
            .map(|index| ordinary(index * 8192, &input))
            .collect();
        let mut batches = Vec::new();
        for_each_batch(&operations, |ranges, bytes| {
            batches.push((ranges.len(), bytes, ranges[0].first));
            Ok(())
        })?;
        assert_eq!(batches, [(16, 16 * 4096, 0), (1, 4096, 256)]);
        Ok(())
    }

    #[test]
    fn byte_limit_splits_large_inputs_and_preserves_tail_offsets() -> Result<(), Error> {
        let input = vec![3; MAX_BYTES + 4096];
        let tail = [9; 4096];
        let operations = [
            ordinary(4096, &input),
            ordinary(2 * MAX_BYTES as u64, &tail),
        ];
        let mut batches = Vec::new();
        for_each_batch(&operations, |ranges, bytes| {
            batches.push((
                ranges
                    .iter()
                    .map(|range| (range.first, range.bytes.len(), range.bytes[0]))
                    .collect::<Vec<_>>(),
                bytes,
            ));
            Ok(())
        })?;
        assert_eq!(
            batches,
            [
                (vec![(8, MAX_BYTES, 3)], MAX_BYTES),
                (vec![(1032, 4096, 3), (2048, 4096, 9)], 8192),
            ]
        );
        Ok(())
    }

    #[test]
    fn overlapping_ranges_preserve_write_order_across_tickets() -> Result<(), Error> {
        let a = [1; 4096];
        let b = [2; 4096];
        let c = [3; 4096];
        let operations = [ordinary(0, &a), ordinary(2048, &b), ordinary(8192, &c)];
        let mut batches = Vec::new();
        for_each_batch(&operations, |ranges, _| {
            batches.push(
                ranges
                    .iter()
                    .map(|range| (range.first, range.bytes[0]))
                    .collect::<Vec<_>>(),
            );
            Ok(())
        })?;
        assert_eq!(batches, [vec![(0, 1)], vec![(4, 2), (16, 3)]]);
        Ok(())
    }

    #[test]
    fn adjacent_disk_ranges_do_not_invent_contiguous_source_borrows() -> Result<(), Error> {
        let a = [1; 4096];
        let b = [2; 4096];
        let operations = [ordinary(0, &a), ordinary(4096, &b)];
        for_each_batch(&operations, |ranges, bytes| {
            assert_eq!(ranges.len(), 2);
            assert_eq!(bytes, 8192);
            assert_eq!(ranges[0].bytes, a);
            assert_eq!(ranges[1].bytes, b);
            Ok(())
        })
    }

    #[test]
    fn read_fua_and_invalid_geometry_reject_before_any_submission() {
        let input = [0; 4096];
        let mut output = [0; 512];
        let invalid = [
            Operation::Write {
                offset: 4096,
                input: &input,
                fua: true,
            },
            Operation::Read {
                offset: 4096,
                output: &mut output,
            },
            ordinary(1, &input),
            ordinary(0, &input[..513]),
            ordinary(u64::MAX - 511, &input),
        ];
        for operation in invalid {
            let operations = [ordinary(0, &input), operation];
            let mut callbacks = 0;
            assert_eq!(
                for_each_batch(&operations, |_, _| {
                    callbacks += 1;
                    Ok(())
                }),
                Err(INVALID)
            );
            assert_eq!(callbacks, 0);
        }
    }

    #[test]
    fn callback_failure_stops_later_batches_without_replaying_accepted_work() {
        let input = vec![0; MAX_BYTES * 3];
        let mut calls = 0;
        assert_eq!(
            for_each_batch(&[ordinary(0, &input)], |_, _| {
                calls += 1;
                if calls == 2 {
                    Err(super::super::IO)
                } else {
                    Ok(())
                }
            }),
            Err(super::super::IO)
        );
        assert_eq!(calls, 2);
    }

    #[test]
    fn empty_inputs_emit_no_empty_ticket() -> Result<(), Error> {
        let mut calls = 0;
        for_each_batch(&[ordinary(0, &[])], |_, _| {
            calls += 1;
            Ok(())
        })?;
        assert_eq!(calls, 0);
        Ok(())
    }
}
