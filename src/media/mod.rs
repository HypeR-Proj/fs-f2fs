// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Experimental Fuchsia F2FS media core. A mount serializes public operations;
//! the original C++ writer may call the owned block device on a background thread.
mod callbacks;
#[cfg(any(target_os = "hyper", test))]
mod completion;
mod ffi;
#[cfg(target_os = "hyper")]
pub mod native;
#[cfg(any(target_os = "hyper", test))]
mod read_pipeline;
#[cfg(any(target_os = "hyper", test))]
mod write_batch;
#[cfg(any(target_os = "hyper", test))]
mod write_pipeline;
use std::{ffi::c_void, marker::PhantomData, ptr::NonNull, rc::Rc, sync::Mutex};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error(pub i32);
pub const IO: Error = Error(-40);
pub const NO_MEMORY: Error = Error(-4);

/// Reserve an owned completion before mutation/submission, without an aborting
/// allocation on the asynchronous retirement path.
pub(crate) fn try_box<T>(value: T) -> Result<Box<T>, Error> {
    let layout = std::alloc::Layout::new::<T>();
    if layout.size() == 0 {
        return Ok(Box::new(value));
    }
    // SAFETY: The nonzero layout describes T; failure is checked before writing.
    let allocation = unsafe { std::alloc::alloc(layout) }.cast::<T>();
    let allocation = NonNull::new(allocation).ok_or(NO_MEMORY)?;
    // SAFETY: Fresh uniquely owned aligned storage receives exactly one T. Box
    // uses the same global allocator/layout and takes its sole destruction duty.
    Ok(unsafe {
        allocation.as_ptr().write(value);
        Box::from_raw(allocation.as_ptr())
    })
}
pub const INVALID: Error = Error(-10);
pub const CORRUPT: Error = Error(-42);
pub const UNSUPPORTED: Error = Error(-2);
fn status(code: i32) -> Result<(), Error> {
    if code == 0 { Ok(()) } else { Err(Error(code)) }
}

pub enum Operation<'a> {
    Read {
        offset: u64,
        output: &'a mut [u8],
    },
    Write {
        offset: u64,
        input: &'a [u8],
        fua: bool,
    },
}
/// Owns completion independently of the mutable submitting device. The wait
/// consumes every accepted operation, even if an earlier operation failed.
pub trait DeviceCompletion: Send {
    fn wait(self: Box<Self>) -> Result<(), Error>;
}
struct Completed(Result<(), Error>);
impl DeviceCompletion for Completed {
    fn wait(self: Box<Self>) -> Result<(), Error> {
        self.0
    }
}

pub trait Device: Send {
    fn blocks(&self) -> u64;
    /// Consume ordinary-write input before return. The returned owner must
    /// make progress without reacquiring this device's mutable callback lock.
    fn submit(
        &mut self,
        operations: &mut [Operation<'_>],
    ) -> Result<Box<dyn DeviceCompletion>, Error> {
        let mut completion = try_box(Completed(Ok(())))?;
        completion.0 = self.run(operations);
        Ok(completion)
    }
    fn read(&mut self, offset: u64, output: &mut [u8]) -> Result<(), Error>;
    fn write(&mut self, offset: u64, input: &[u8]) -> Result<(), Error>;
    fn flush(&mut self) -> Result<(), Error>;
    /// Complete accepted I/O before success and retire every buffer borrow on
    /// all paths. A poisoned transport may close its kernel-owned requests on
    /// failure; no further operation may use that transport.
    /// A FUA write must be durable before any later operation in this batch.
    fn run(&mut self, operations: &mut [Operation<'_>]) -> Result<(), Error> {
        for op in operations {
            match op {
                Operation::Read { offset, output } => self.read(*offset, output)?,
                Operation::Write { offset, input, fua } => {
                    self.write(*offset, input)?;
                    if *fua {
                        self.flush()?;
                    }
                }
            }
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Metadata {
    pub size: u64,
    pub mode: u32,
    pub created: u64,
    pub accessed: u64,
    pub modified: u64,
}
impl Metadata {
    pub fn directory(self) -> bool {
        self.mode & 0o170000 == 0o040000
    }
}

/// An owned completion event; it never borrows or accesses the mutable mount.
/// Dropping it joins independently progressing page retirement.
pub struct WriteReceipt(Option<NonNull<c_void>>);
// SAFETY: Only the receipt event/status crosses threads. C++ publishes status
// before signaling; wait consumes its allocation and never invokes mount APIs.
unsafe impl Send for WriteReceipt {}
impl WriteReceipt {
    pub fn wait(mut self) -> Result<(), Error> {
        let receipt = self.0.take().ok_or(INVALID)?;
        // SAFETY: This unique receipt remains allocated until the consuming wait.
        status(unsafe { ffi::hyper_f2fs_wait_write(receipt.as_ptr()) })
    }
}
impl Drop for WriteReceipt {
    fn drop(&mut self) {
        if let Some(receipt) = self.0.take() {
            // SAFETY: The unique event is independent of mount/public-operation
            // locks; consuming it cannot strand writeback on a blocked actor.
            let _ = unsafe { ffi::hyper_f2fs_wait_write(receipt.as_ptr()) };
        }
    }
}

/// C++ retains the stable callback owner until destroy has joined its workers.
/// Public calls require exclusive access; this type deliberately cannot migrate threads.
pub struct FileSystem<D: Device> {
    mount: Option<NonNull<c_void>>,
    device: Option<Box<Mutex<D>>>,
    _ordered: PhantomData<Rc<()>>,
}
impl<D: Device> FileSystem<D> {
    pub fn format(device: &mut D, uuid: [u8; 16]) -> Result<(), Error> {
        // A temporary borrowed device adapter keeps ownership with the caller through mkfs.
        let owner = Mutex::new(Borrowed(device));
        let cb = callbacks::descriptor(&owner, uuid)?;
        // SAFETY: mkfs is synchronous; callbacks and device outlive every operation.
        status(unsafe { ffi::hyper_f2fs_format(&cb) })
    }
    pub fn mount(device: D) -> Result<Self, Error> {
        let owner = Box::new(Mutex::new(device));
        let cb = callbacks::descriptor(&owner, [0; 16])?;
        let mut raw = std::ptr::null_mut();
        // SAFETY: C++ copies the descriptor, retaining only its stable boxed context.
        status(unsafe { ffi::hyper_f2fs_mount(&cb, &mut raw) })?;
        let mount = NonNull::new(raw).ok_or(CORRUPT)?;
        Ok(Self {
            mount: Some(mount),
            device: Some(owner),
            _ordered: PhantomData,
        })
    }
    fn raw(&self) -> Result<*mut c_void, Error> {
        self.mount.map(NonNull::as_ptr).ok_or(INVALID)
    }
    pub fn enable_vfs_writeback(&mut self) -> Result<(), Error> {
        // SAFETY: Exclusive mount access; the mode is retained across handoff.
        status(unsafe { ffi::hyper_f2fs_enable_vfs_writeback(self.raw()?) })
    }
    pub fn sync(&mut self) -> Result<(), Error> {
        // SAFETY: Exclusive public access; mount and callback owner remain live.
        status(unsafe { ffi::hyper_f2fs_sync(self.raw()?) })
    }
    pub fn sync_file(&mut self, path: &str, data_only: bool) -> Result<(), Error> {
        let path = path_bytes(path)?;
        // SAFETY: Exclusive mount access; path and callback owner outlive the call.
        status(unsafe {
            ffi::hyper_f2fs_sync_file(
                self.raw()?,
                path.as_ptr().cast(),
                path.len(),
                data_only.into(),
            )
        })
    }
    pub fn periodic_sync(&mut self) -> Result<(), Error> {
        // SAFETY: Exclusive mount access and callback ownership span the entire pass.
        status(unsafe { ffi::hyper_f2fs_sync_if_dirty(self.raw()?) })
    }
    pub fn close(mut self) -> Result<D, Error> {
        // SAFETY: Successful unmount joins workers and destroys the opaque mount once.
        status(unsafe { ffi::hyper_f2fs_unmount(self.raw()?) })?;
        self.mount = None;
        self.device
            .take()
            .ok_or(INVALID)?
            .into_inner()
            .map_err(|_| IO)
    }
    pub fn stat(&mut self, path: &str) -> Result<Metadata, Error> {
        let path = path_bytes(path)?;
        let mut s = ffi::Stat::default();
        // SAFETY: Path and output are borrowed for this call; the mount remains owned.
        status(unsafe {
            ffi::hyper_f2fs_stat(self.raw()?, path.as_ptr().cast(), path.len(), &mut s)
        })?;
        Ok(Metadata {
            size: s.size,
            mode: s.mode,
            created: s.created,
            accessed: s.accessed,
            modified: s.modified,
        })
    }
    pub fn create(&mut self, path: &str, directory: bool) -> Result<(), Error> {
        let p = path_bytes(path)?;
        // SAFETY: Valid path slice and exclusive mount access last through the call.
        status(unsafe {
            ffi::hyper_f2fs_create(self.raw()?, p.as_ptr().cast(), p.len(), u8::from(directory))
        })
    }
    pub fn read(&mut self, path: &str, offset: u64, output: &mut [u8]) -> Result<usize, Error> {
        let p = path_bytes(path)?;
        let mut actual = 0;
        // SAFETY: C++ only writes the exclusively borrowed output extent before returning.
        status(unsafe {
            ffi::hyper_f2fs_read(
                self.raw()?,
                p.as_ptr().cast(),
                p.len(),
                offset,
                output.as_mut_ptr(),
                output.len(),
                &mut actual,
            )
        })?;
        if actual > output.len() {
            return Err(CORRUPT);
        }
        Ok(actual)
    }
    pub fn write(&mut self, path: &str, offset: u64, input: &[u8]) -> Result<usize, Error> {
        let p = path_bytes(path)?;
        let mut actual = 0;
        // SAFETY: C++ copies the borrowed input into its owned cache before returning.
        status(unsafe {
            ffi::hyper_f2fs_write(
                self.raw()?,
                p.as_ptr().cast(),
                p.len(),
                offset,
                input.as_ptr(),
                input.len(),
                &mut actual,
            )
        })?;
        if actual > input.len() {
            return Err(CORRUPT);
        }
        Ok(actual)
    }
    pub fn submit_writeback(
        &mut self,
        path: &str,
        offset: u64,
        input: &[u8],
    ) -> Result<(usize, WriteReceipt), Error> {
        let p = path_bytes(path)?;
        let mut actual = 0;
        let mut raw = std::ptr::null_mut();
        // SAFETY: C++ consumes input and selects exact pages before returning;
        // the receipt owns only completion storage, never this input borrow.
        status(unsafe {
            ffi::hyper_f2fs_submit_write(
                self.raw()?,
                p.as_ptr().cast(),
                p.len(),
                offset,
                input.as_ptr(),
                input.len(),
                &mut actual,
                &mut raw,
            )
        })?;
        let receipt = WriteReceipt(Some(NonNull::new(raw).ok_or(CORRUPT)?));
        if actual == 0 || actual > input.len() {
            return Err(CORRUPT);
        }
        Ok((actual, receipt))
    }
    pub fn read_directory(
        &mut self,
        path: &str,
        cursor: &mut u64,
        output: &mut [u8],
    ) -> Result<usize, Error> {
        let p = path_bytes(path)?;
        let mut actual = 0;
        // SAFETY: The cursor and output have exclusive, nonoverlapping borrows for this call.
        status(unsafe {
            ffi::hyper_f2fs_readdir(
                self.raw()?,
                p.as_ptr().cast(),
                p.len(),
                cursor,
                output.as_mut_ptr(),
                output.len(),
                &mut actual,
            )
        })?;
        if actual > output.len() {
            return Err(CORRUPT);
        }
        Ok(actual)
    }
    pub fn remove(&mut self, path: &str) -> Result<(), Error> {
        let p = path_bytes(path)?;
        // SAFETY: Borrowed validated path and exclusively owned mount remain live.
        status(unsafe { ffi::hyper_f2fs_remove(self.raw()?, p.as_ptr().cast(), p.len()) })
    }
    pub fn rename(&mut self, path: &str, target: &str) -> Result<(), Error> {
        let p = path_bytes(path)?;
        let t = path_bytes(target)?;
        // SAFETY: Both validated path slices remain borrowed through this call.
        status(unsafe {
            ffi::hyper_f2fs_rename(
                self.raw()?,
                p.as_ptr().cast(),
                p.len(),
                t.as_ptr().cast(),
                t.len(),
            )
        })
    }
    pub fn resize(&mut self, path: &str, length: u64) -> Result<(), Error> {
        let p = path_bytes(path)?;
        // SAFETY: Borrowed validated path and exclusively owned mount remain live.
        status(unsafe { ffi::hyper_f2fs_resize(self.raw()?, p.as_ptr().cast(), p.len(), length) })
    }
    pub fn set_times(&mut self, path: &str, times: [Option<(i64, u32)>; 2]) -> Result<(), Error> {
        let p = path_bytes(path)?;
        let mut seconds = [0; 2];
        let mut nanos = [0; 2];
        let mut mask = 0;
        for (i, t) in times.into_iter().enumerate() {
            if let Some((s, n)) = t {
                seconds[i] = s;
                nanos[i] = n;
                mask |= 1 << i;
            }
        }
        // SAFETY: Both fixed two-element arrays and the path stay borrowed through the call.
        status(unsafe {
            ffi::hyper_f2fs_times(
                self.raw()?,
                p.as_ptr().cast(),
                p.len(),
                seconds.as_ptr(),
                nanos.as_ptr(),
                mask,
            )
        })
    }
}
impl<D: Device> Drop for FileSystem<D> {
    fn drop(&mut self) {
        if let Some(mount) = self.mount.take() {
            // SAFETY: No public call can overlap Drop. Destroy drains workers before the device drops.
            unsafe { ffi::hyper_f2fs_destroy(mount.as_ptr()) }
        }
    }
}
fn path_bytes(path: &str) -> Result<&[u8], Error> {
    if path.len() > 4096
        || (!path.is_empty()
            && path.split('/').any(|c| {
                c.is_empty() || c == "." || c == ".." || c.len() > 255 || c.contains('\0')
            }))
    {
        return Err(INVALID);
    }
    Ok(path.as_bytes())
}
struct Borrowed<'a, D>(&'a mut D);
impl<D: Device> Device for Borrowed<'_, D> {
    fn blocks(&self) -> u64 {
        self.0.blocks()
    }
    fn read(&mut self, o: u64, b: &mut [u8]) -> Result<(), Error> {
        self.0.read(o, b)
    }
    fn write(&mut self, o: u64, b: &[u8]) -> Result<(), Error> {
        self.0.write(o, b)
    }
    fn flush(&mut self) -> Result<(), Error> {
        self.0.flush()
    }
}

#[cfg(test)]
mod tests;
