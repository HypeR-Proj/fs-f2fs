#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0
"""Exercise the public file-sync path with a volatile device cache and power loss."""
import ctypes as c
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile

COMPONENT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('f2fs_host', COMPONENT / 'host.py')
api = importlib.util.module_from_spec(spec)
spec.loader.exec_module(api)


class CachedImage(api.Image):
    def __init__(self, library, path):
        self.pending = {}
        self.order_error = False
        self.persist_marker = False
        self.flush_attempt = 0
        self.fail_at = None
        super().__init__(library, path)

    def _read(self, context, offset, buffer, size):
        # Device reads observe its volatile cache until a simulated power cut.
        result = super()._read(context, offset, buffer, size)
        if result == 0:
            for start, data in self.pending.items():
                lo, hi = max(start, offset), min(start + len(data), offset + size)
                if lo < hi:
                    c.memmove(buffer + lo - offset, data[lo-start:hi-start], hi-lo)
        return result

    def _write(self, _, offset, buffer, size):
        if self.fail_write:
            return -40
        # Keep block granularity so overlapping writes have last-write semantics.
        if offset % 4096 or size % 4096 or offset + size > len(self.mapping):
            return -14
        for index in range(0, size, 4096):
            self.pending[offset + index] = c.string_at(buffer + index, 4096)
        self.counts['write'] += 1
        return 0

    def _run(self, _, operations, count):
        for operation in operations[:count]:
            if operation.kind == 2 and self.pending:
                # A controller may persist the marker immediately. All earlier
                # nodes must already be durable, not merely in this I/O batch.
                self.order_error = True
                return -40
            if operation.kind == 0:
                result = self._read(None, operation.offset, operation.data, operation.length)
            else:
                result = self._write(None, operation.offset, operation.data, operation.length)
                if result == 0 and operation.kind == 2:
                    if self.persist_marker:
                        # Power may fail after the marker reached media but
                        # before its final flush reports success.
                        start = operation.offset
                        self.mapping[start:start + operation.length] = c.string_at(operation.data, operation.length)
                    result = self._flush(None)
            if result:
                return result
        return 0

    def _flush(self, _):
        self.flush_attempt += 1
        if self.flush_attempt == self.fail_at:
            return -40
        for offset, data in self.pending.items():
            self.mapping[offset:offset + len(data)] = data
        self.pending.clear()
        return super()._flush(None)


def suite(library, case):
    with tempfile.TemporaryDirectory() as directory:
        disk = Path(directory) / 'disk.img'
        with disk.open('xb') as output:
            output.truncate(128 * 1024**2)
        image = CachedImage(library, disk)
        lib = image.library
        lib.hyper_f2fs_sync_file.argtypes = [c.c_void_p, c.c_char_p, c.c_size_t, c.c_uint8]
        lib.hyper_f2fs_enable_vfs_writeback.argtypes = [c.c_void_p]
        lib.hyper_f2fs_test_state.argtypes = [c.c_void_p, c.c_uint]
        lib.hyper_f2fs_test_state.restype = c.c_uint64
        mount = c.c_void_p()

        def write(path, payload, offset=0):
            actual = c.c_size_t()
            api.check(lib.hyper_f2fs_write(mount, path, len(path), offset,
                                         payload, len(payload), c.byref(actual)))
            assert actual.value == len(payload), actual.value

        def read(path, expected):
            data = c.create_string_buffer(len(expected) + 4096)
            actual = c.c_size_t()
            api.check(lib.hyper_f2fs_read(mount, path, len(path), 0,
                                        data, len(data), c.byref(actual)))
            assert actual.value == len(expected), (actual.value, len(expected))
            assert data.raw[:actual.value] == expected

        try:
            api.check(lib.hyper_f2fs_format(c.byref(image.device)))
            api.check(lib.hyper_f2fs_mount(c.byref(image.device), c.byref(mount)))
            api.check(lib.hyper_f2fs_enable_vfs_writeback(mount))
            api.check(lib.hyper_f2fs_create(mount, b'file', 4, 0))
            old = b'A' * 16384
            write(b'file', old)
            api.check(lib.hyper_f2fs_sync(mount))
            version = lib.hyper_f2fs_test_state(mount, 1)
            # Span multiple dnodes and append to exercise both contents and size.
            new = bytes(index % 251 for index in range(512 * 1024)) * 16
            for offset in range(0, len(new), 512 * 1024):
                write(b'file', new[offset:offset + 512 * 1024], offset)
            if case == 'truncate':
                api.check(lib.hyper_f2fs_resize(mount, b'file', 4, 7000))
                new = new[:7000]
            if case == 'namespace':
                api.check(lib.hyper_f2fs_rename(mount, b'file', 4, b'renamed', 7))
                path = b'renamed'
            else:
                path = b'file'
            if case == 'node-write':
                image.fail_write = True
            if case in ('preflush', 'commit-flush', 'commit-early'):
                image.fail_at = image.flush_attempt + (1 if case == 'preflush' else 2)
            image.persist_marker = case == 'commit-early'
            failed = case in ('node-write', 'preflush', 'commit-flush', 'commit-early')
            status = lib.hyper_f2fs_sync_file(mount, path, len(path), int(case == 'data'))
            assert not image.order_error, "commit marker preceded durable recovery nodes"
            assert (status != 0) == failed, (case, status)
            if failed:
                assert lib.hyper_f2fs_sync_file(mount, path, len(path), 0) != 0
            else:
                advanced = lib.hyper_f2fs_test_state(mount, 1) != version
                assert advanced == (case == 'namespace'), (case, advanced)
                read(path, new)
                if case == 'all':
                    counts = dict(image.counts)
                    api.check(lib.hyper_f2fs_sync_file(mount, path, len(path), 0))
                    assert image.counts == counts, 'clean file sync repeated I/O'
                if case == 'repeat':
                    new = b'B' * len(new)
                    for offset in range(0, len(new), 512 * 1024):
                        write(path, new[offset:offset + 512 * 1024], offset)
                    api.check(lib.hyper_f2fs_sync_file(mount, path, len(path), 0))
                    assert lib.hyper_f2fs_test_state(mount, 1) == version
                if case == 'periodic':
                    lib.hyper_f2fs_sync_if_dirty.argtypes = [c.c_void_p]
                    api.check(lib.hyper_f2fs_sync_if_dirty(mount))
                    assert lib.hyper_f2fs_test_state(mount, 1) != version
            assert lib.hyper_f2fs_test_state(mount, 6) == 0
            # No unmount/checkpoint. Lose every unflushed device write.
            lib.hyper_f2fs_destroy(mount)
            mount = c.c_void_p()
            image.pending.clear()
            image.fail_write = False
            image.fail_at = None
            image.persist_marker = False
            api.check(lib.hyper_f2fs_mount(c.byref(image.device), c.byref(mount)))
            read(path, old if failed and case != 'commit-early' else new)
            print(f'F2FS file sync {case}: PASS', flush=True)
        finally:
            if mount.value:
                lib.hyper_f2fs_destroy(mount)
            image.close()


if __name__ == '__main__':
    if len(sys.argv) == 3:
        suite(Path(sys.argv[1]), sys.argv[2])
    else:
        for case in ('all', 'data', 'repeat', 'truncate', 'namespace', 'periodic',
                     'node-write', 'preflush', 'commit-flush', 'commit-early'):
            subprocess.run([sys.executable, __file__, sys.argv[1], case], check=True, timeout=90)
