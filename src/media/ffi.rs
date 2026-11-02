// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0
use std::ffi::{c_char, c_void};
#[repr(C)]
pub(super) struct Io {
    pub offset: u64,
    pub data: *mut u8,
    pub length: usize,
    pub kind: u32,
}
#[repr(C)]
pub(super) struct Device {
    pub context: *mut c_void,
    pub blocks: u64,
    pub read: unsafe extern "C" fn(*mut c_void, u64, *mut u8, usize) -> i32,
    pub write: unsafe extern "C" fn(*mut c_void, u64, *const u8, usize) -> i32,
    pub flush: unsafe extern "C" fn(*mut c_void) -> i32,
    pub uuid: [u8; 16],
    pub run: unsafe extern "C" fn(*mut c_void, *const Io, usize) -> i32,
}
#[repr(C)]
#[derive(Default)]
pub(super) struct Stat {
    pub size: u64,
    pub created: u64,
    pub accessed: u64,
    pub modified: u64,
    pub mode: u32,
}
unsafe extern "C" {
    pub fn hyper_f2fs_format(cb: *const Device) -> i32;
    pub fn hyper_f2fs_mount(cb: *const Device, out: *mut *mut c_void) -> i32;
    pub fn hyper_f2fs_unmount(m: *mut c_void) -> i32;
    pub fn hyper_f2fs_sync(m: *mut c_void) -> i32;
    pub fn hyper_f2fs_destroy(m: *mut c_void);
    pub fn hyper_f2fs_create(m: *mut c_void, p: *const c_char, n: usize, directory: u8) -> i32;
    pub fn hyper_f2fs_write(
        m: *mut c_void,
        p: *const c_char,
        n: usize,
        o: u64,
        b: *const u8,
        l: usize,
        a: *mut usize,
    ) -> i32;
    pub fn hyper_f2fs_read(
        m: *mut c_void,
        p: *const c_char,
        n: usize,
        o: u64,
        b: *mut u8,
        l: usize,
        a: *mut usize,
    ) -> i32;
    pub fn hyper_f2fs_stat(m: *mut c_void, p: *const c_char, n: usize, s: *mut Stat) -> i32;
    pub fn hyper_f2fs_readdir(
        m: *mut c_void,
        p: *const c_char,
        n: usize,
        c: *mut u64,
        b: *mut u8,
        l: usize,
        a: *mut usize,
    ) -> i32;
    pub fn hyper_f2fs_remove(m: *mut c_void, p: *const c_char, n: usize) -> i32;
    pub fn hyper_f2fs_rename(
        m: *mut c_void,
        p: *const c_char,
        n: usize,
        t: *const c_char,
        l: usize,
    ) -> i32;
    pub fn hyper_f2fs_resize(m: *mut c_void, p: *const c_char, n: usize, size: u64) -> i32;
    pub fn hyper_f2fs_times(
        m: *mut c_void,
        p: *const c_char,
        n: usize,
        seconds: *const i64,
        nanos: *const u32,
        mask: u32,
    ) -> i32;
}
