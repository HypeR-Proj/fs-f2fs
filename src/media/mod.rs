// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Experimental Fuchsia F2FS media core. A mount serializes public operations;
//! the original C++ writer may call the owned block device on a background thread.
mod callbacks;
mod ffi;
#[cfg(target_os = "hyper")]
pub mod native;
use std::{ffi::c_void, marker::PhantomData, ptr::NonNull, rc::Rc, sync::Mutex};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error(pub i32);
pub const IO: Error = Error(-40);
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
pub trait Device: Send {
    fn blocks(&self) -> u64;
    fn read(&mut self, offset: u64, output: &mut [u8]) -> Result<(), Error>;
    fn write(&mut self, offset: u64, input: &[u8]) -> Result<(), Error>;
    fn flush(&mut self) -> Result<(), Error>;
    /// Complete all accepted I/O before returning, including the failure path.
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
    pub fn sync(&mut self) -> Result<(), Error> {
        // SAFETY: Exclusive public access; mount and callback owner remain live.
        status(unsafe { ffi::hyper_f2fs_sync(self.raw()?) })
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
