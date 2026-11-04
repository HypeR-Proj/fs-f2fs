#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0
"""VFS-owned data: bounded staging, no per-batch checkpoint, and cold recovery."""
import ctypes as c
import importlib.util
from pathlib import Path
import sys
import tempfile

COMPONENT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('f2fs_host', COMPONENT / 'host.py')
api = importlib.util.module_from_spec(spec)
spec.loader.exec_module(api)


def suite(library, failure=False):
    with tempfile.TemporaryDirectory() as temporary:
        disk = Path(temporary) / 'disk.img'
        with disk.open('xb') as output:
            output.truncate(256 * 1024 ** 2)
        image = api.Image(library, disk)
        lib = image.library
        lib.hyper_f2fs_enable_vfs_writeback.argtypes = [c.c_void_p]
        lib.hyper_f2fs_enable_vfs_writeback.restype = c.c_int32
        lib.hyper_f2fs_test_state.argtypes = [c.c_void_p, c.c_uint]
        lib.hyper_f2fs_test_state.restype = c.c_uint64
        mount = c.c_void_p()
        try:
            api.check(lib.hyper_f2fs_format(c.byref(image.device)))
            api.check(lib.hyper_f2fs_mount(c.byref(image.device), c.byref(mount)))
            api.check(lib.hyper_f2fs_enable_vfs_writeback(mount))
            api.check(lib.hyper_f2fs_create(mount, b'payload', 7, 0))
            checkpoint = lib.hyper_f2fs_test_state(mount, 1)
            flushes = image.counts['flush']
            payload = bytes(n % 251 for n in range(512 * 1024))
            if failure:
                image.fail_write = True
            for index in range(32):
                actual = c.c_size_t()
                result = lib.hyper_f2fs_write(mount, b'payload', 7, index * len(payload),
                                             payload, len(payload), c.byref(actual))
                if failure:
                    if result == 0 or lib.hyper_f2fs_sync(mount) == 0:
                        raise RuntimeError('data writeback failure was acknowledged as success')
                    if lib.hyper_f2fs_test_state(mount, 6) != 0:
                        raise RuntimeError('failed request left unretired data I/O')
                    print('F2FS VFS writeback failure: PASS')
                    return
                api.check(result)
                if actual.value != len(payload):
                    raise RuntimeError('short accepted VFS batch')
                if any(lib.hyper_f2fs_test_state(mount, field) for field in (5, 6)):
                    raise RuntimeError('VFS batch left a second dirty data cache')
            reads = image.counts['read']
            actual = c.c_size_t()
            api.check(lib.hyper_f2fs_write(mount, b'payload', 7, 0, payload,
                                         len(payload), c.byref(actual)))
            if actual.value != len(payload) or image.counts['read'] != reads:
                raise RuntimeError('whole-page writeback fetched old data blocks')
            if lib.hyper_f2fs_test_state(mount, 1) != checkpoint or image.counts['flush'] != flushes:
                raise RuntimeError('a data batch introduced a checkpoint/device flush')
            api.check(lib.hyper_f2fs_sync(mount))
            api.check(lib.hyper_f2fs_unmount(mount))
            mount = c.c_void_p()
            api.check(lib.hyper_f2fs_mount(c.byref(image.device), c.byref(mount)))
            for index in range(32):
                output = c.create_string_buffer(len(payload))
                actual = c.c_size_t()
                api.check(lib.hyper_f2fs_read(mount, b'payload', 7, index * len(payload),
                                             output, len(payload), c.byref(actual)))
                if actual.value != len(payload) or output.raw != payload:
                    raise RuntimeError('cold VFS writeback mismatch')
            print('F2FS VFS writeback persistence: PASS')
        finally:
            if mount.value:
                lib.hyper_f2fs_destroy(mount)
            image.close()


if __name__ == '__main__':
    for failed in (False, True):
        suite(Path(sys.argv[1]), failed)
