#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0
"""Bound dirty plus in-flight pages when the backing device is slower than writers."""
import ctypes as c
import importlib.util
from pathlib import Path
import sys
import tempfile
import time

COMPONENT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('f2fs_host', COMPONENT / 'host.py')
api = importlib.util.module_from_spec(spec)
spec.loader.exec_module(api)


class SlowImage(api.Image):
    slow = False
    attempts = 0
    fail_at = None

    def _write(self, context, offset, buffer, size):
        if self.slow:
            self.attempts += 1
            time.sleep(size / (16 * 1024 * 1024))
            if self.attempts == self.fail_at:
                return -40
        return super()._write(context, offset, buffer, size)


def suite(library, failure):
    with tempfile.TemporaryDirectory() as directory:
        disk = Path(directory) / 'disk.img'
        with disk.open('xb') as output:
            output.truncate(2 * 1024 ** 3)
        image = SlowImage(library, disk)
        lib = image.library
        lib.hyper_f2fs_test_state.argtypes = [c.c_void_p, c.c_uint]
        lib.hyper_f2fs_test_state.restype = c.c_uint64
        mount = c.c_void_p()
        try:
            api.check(lib.hyper_f2fs_format(c.byref(image.device)))
            api.check(lib.hyper_f2fs_mount(c.byref(image.device), c.byref(mount)))
            api.check(lib.hyper_f2fs_create(mount, b'payload', 7, 0))
            checkpoint = lib.hyper_f2fs_test_state(mount, 1)
            flushes = image.counts['flush']
            image.slow = True
            image.fail_at = 3 if failure else None
            peak = 0
            failed = False
            for index in range(512):
                chunk = bytes([index % 251]) * (128 * 1024)
                actual = c.c_size_t()
                result = lib.hyper_f2fs_write(mount, b'payload', 7, index * len(chunk),
                                             chunk, len(chunk), c.byref(actual))
                if result:
                    if not failure or actual.value:
                        raise RuntimeError('unexpected failure or accepted bytes after pressure error')
                    failed = True
                    break
                if actual.value != len(chunk):
                    raise RuntimeError('short write')
                pending = sum(lib.hyper_f2fs_test_state(mount, field) for field in (5, 6))
                peak = max(peak, pending)
                # Admission is checked before each 128 KiB request. Include its
                # possible overshoot; moving pages to writeback must not hide them.
                if pending > 2048 + len(chunk) // 4096:
                    raise RuntimeError(f'unbounded slow-device backlog: {pending} pages')
            if failed != failure:
                raise RuntimeError('injected writeback error was not observed by the producer')
            if failure:
                if lib.hyper_f2fs_sync(mount) == 0:
                    raise RuntimeError('sync hid a failed pressure-driven write')
                actual = c.c_size_t()
                if lib.hyper_f2fs_write(mount, b'payload', 7, 0, b'x', 1, c.byref(actual)) == 0 or actual.value:
                    raise RuntimeError('producer accepted data after the writeback error')
            else:
                if lib.hyper_f2fs_test_state(mount, 1) != checkpoint or image.counts['flush'] != flushes:
                    raise RuntimeError('data pressure forced an unnecessary checkpoint/device flush')
                api.check(lib.hyper_f2fs_unmount(mount))
                mount = c.c_void_p()
                api.check(lib.hyper_f2fs_mount(c.byref(image.device), c.byref(mount)))
                for index in range(512):
                    chunk = bytes([index % 251]) * (128 * 1024)
                    output = c.create_string_buffer(len(chunk))
                    actual = c.c_size_t()
                    api.check(lib.hyper_f2fs_read(mount, b'payload', 7, index * len(chunk),
                                                 output, len(chunk), c.byref(actual)))
                    if actual.value != len(chunk) or output.raw != chunk:
                        raise RuntimeError(f'cold payload mismatch at chunk {index}')
                api.check(lib.hyper_f2fs_unmount(mount))
                mount = c.c_void_p()
            print(f'F2FS slow-device {"failure" if failure else "persistence"}: PASS (peak={peak} pages)')
        finally:
            image.slow = False
            if mount.value:
                lib.hyper_f2fs_destroy(mount)
            image.close()


if __name__ == '__main__':
    for failure in (False, True):
        suite(sys.argv[1], failure)
