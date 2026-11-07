// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0
use super::{Device, Error, INVALID, IO, Operation, ffi};
use std::{ffi::c_void, sync::Mutex};
fn code(result: Result<(), Error>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => e.0,
    }
}
pub(super) fn descriptor<D: Device>(
    owner: &Mutex<D>,
    uuid: [u8; 16],
) -> Result<ffi::Device, Error> {
    let blocks = owner.lock().map_err(|_| IO)?.blocks();
    if !(32768..u32::MAX as u64).contains(&blocks) {
        return Err(INVALID);
    }
    Ok(ffi::Device {
        context: std::ptr::from_ref(owner).cast_mut().cast(),
        blocks,
        read: read::<D>,
        write: write::<D>,
        flush: flush::<D>,
        uuid,
        run: run::<D>,
        submit: submit::<D>,
        complete,
    })
}
fn range<D: Device>(device: &D, offset: u64, length: usize) -> Result<(), Error> {
    let capacity = device.blocks().checked_mul(4096).ok_or(INVALID)?;
    if !offset.is_multiple_of(4096)
        || !length.is_multiple_of(4096)
        || offset
            .checked_add(length as u64)
            .is_none_or(|end| end > capacity)
    {
        return Err(INVALID);
    }
    Ok(())
}
unsafe extern "C" fn read<D: Device>(
    context: *mut c_void,
    offset: u64,
    output: *mut u8,
    length: usize,
) -> i32 {
    // SAFETY: The mount retains this stable Mutex<D>; its destructor joins all callers.
    let owner = unsafe { &*context.cast::<Mutex<D>>() };
    let Ok(mut device) = owner.lock() else {
        return IO.0;
    };
    if let Err(e) = range(&*device, offset, length) {
        return e.0;
    }
    if length == 0 {
        return 0;
    }
    // SAFETY: Bcache lends one exclusively writable, initialized C++ block buffer for this call.
    code(device.read(offset, unsafe {
        std::slice::from_raw_parts_mut(output, length)
    }))
}
unsafe extern "C" fn write<D: Device>(
    context: *mut c_void,
    offset: u64,
    input: *const u8,
    length: usize,
) -> i32 {
    // SAFETY: The mount retains this stable Mutex<D>; its destructor joins all callers.
    let owner = unsafe { &*context.cast::<Mutex<D>>() };
    let Ok(mut device) = owner.lock() else {
        return IO.0;
    };
    if let Err(e) = range(&*device, offset, length) {
        return e.0;
    }
    if length == 0 {
        return 0;
    }
    // SAFETY: Bcache retains the initialized immutable source buffer through callback completion.
    code(device.write(offset, unsafe { std::slice::from_raw_parts(input, length) }))
}
unsafe extern "C" fn flush<D: Device>(context: *mut c_void) -> i32 {
    // SAFETY: The mount retains this stable Mutex<D>; its destructor joins all callers.
    let owner = unsafe { &*context.cast::<Mutex<D>>() };
    let Ok(mut device) = owner.lock() else {
        return IO.0;
    };
    code(device.flush())
}
unsafe extern "C" fn run<D: Device>(
    context: *mut c_void,
    requests: *const ffi::Io,
    count: usize,
) -> i32 {
    if count == 0 {
        return 0;
    }
    // SAFETY: The mount owns the context; Bcache's vector retains count initialized ABI records.
    let (owner, requests) = unsafe {
        (
            &*context.cast::<Mutex<D>>(),
            std::slice::from_raw_parts(requests, count),
        )
    };
    let Ok(mut device) = owner.lock() else {
        return IO.0;
    };
    let mut operations = Vec::new();
    if operations.try_reserve_exact(count).is_err() {
        return -4;
    }
    for request in requests {
        if let Err(e) = range(&*device, request.offset, request.length) {
            return e.0;
        }
        if request.length == 0 {
            continue;
        }
        // SAFETY: The upstream storage pool reserves disjoint output ranges for reads. Write
        // batches borrow immutable owned staging ranges. Neither pool reuses a range until
        // run returns; the private C++ bridge never mixes aliased reads/writes in one batch.
        let operation = unsafe {
            match request.kind {
                0 => Operation::Read {
                    offset: request.offset,
                    output: std::slice::from_raw_parts_mut(request.data, request.length),
                },
                1 | 2 => Operation::Write {
                    offset: request.offset,
                    input: std::slice::from_raw_parts(request.data, request.length),
                    fua: request.kind == 2,
                },
                _ => return INVALID.0,
            }
        };
        operations.push(operation);
    }
    code(device.run(&mut operations))
}

// A thin allocation makes the private C pointer independent of Rust trait-object
// layout. It is allocated before submission and owns all accepted completions.
struct CompletionOwner(Option<Box<dyn super::DeviceCompletion>>);
unsafe extern "C" fn submit<D: Device>(
    context: *mut c_void,
    requests: *const ffi::Io,
    count: usize,
    output: *mut *mut c_void,
) -> i32 {
    // SAFETY: Bcache supplies one live output slot for this synchronous callback.
    unsafe {
        *output = std::ptr::null_mut();
    }
    let mut completion = match super::try_box(CompletionOwner(None)) {
        Ok(completion) => completion,
        Err(error) => return error.0,
    };
    // SAFETY: The mount owns the stable callback context and count ABI records.
    let (owner, requests) = unsafe {
        (
            &*context.cast::<Mutex<D>>(),
            std::slice::from_raw_parts(requests, count),
        )
    };
    let Ok(mut device) = owner.lock() else {
        return IO.0;
    };
    let mut operations = Vec::new();
    if operations.try_reserve_exact(count).is_err() {
        return -4;
    }
    for request in requests {
        if request.kind != 1 {
            return INVALID.0;
        }
        if let Err(e) = range(&*device, request.offset, request.length) {
            return e.0;
        }
        if request.length == 0 {
            continue;
        }
        // SAFETY: Ordinary write sources remain immutable through submission;
        // the device consumes/copies them before returning its completion owner.
        let input = unsafe { std::slice::from_raw_parts(request.data, request.length) };
        operations.push(Operation::Write {
            offset: request.offset,
            input,
            fua: false,
        });
    }
    completion.0 = match device.submit(&mut operations) {
        Ok(owner) => Some(owner),
        Err(error) => return error.0,
    };
    // SAFETY: The caller takes exclusive ownership and consumes via complete.
    unsafe {
        *output = Box::into_raw(completion).cast();
    }
    0
}
unsafe extern "C" fn complete(context: *mut c_void) -> i32 {
    // SAFETY: Bcache passes exactly once the live allocation returned by submit.
    let mut completion = unsafe { Box::from_raw(context.cast::<CompletionOwner>()) };
    match completion.0.take() {
        Some(owner) => code(owner.wait()),
        None => INVALID.0,
    }
}
