#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0
"""Exercise persistent payload, sparse tails, directory mutation and I/O failures."""
import ctypes as c
import importlib.util
from pathlib import Path
import sys
import subprocess
import tempfile
ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('f2fs_host', ROOT / 'svc/fs-f2fs/host.py')
api = importlib.util.module_from_spec(spec)
spec.loader.exec_module(api)


def suite(library):
    with tempfile.TemporaryDirectory() as temporary:
        path = Path(temporary) / 'disk.img'
        with path.open('xb') as output:
            output.truncate(128 * 1024 * 1024)
        image = api.Image(library, path)
        fs = image.library
        api.check(fs.hyper_f2fs_format(c.byref(image.device)))
        mount = c.c_void_p()
        api.check(fs.hyper_f2fs_mount(c.byref(image.device), c.byref(mount)))
        def call(operation, path, *args):
            encoded = path.encode()
            api.check(getattr(fs, 'hyper_f2fs_' + operation)(mount, encoded, len(encoded), *args))
        def read(path, offset, length):
            output = c.create_string_buffer(length)
            actual = c.c_size_t()
            call('read', path, offset, output, length, c.byref(actual))
            return output.raw[:actual.value]
        def write(path, offset, payload):
            actual = c.c_size_t()
            call('write', path, offset, payload, len(payload), c.byref(actual))
            if actual.value != len(payload):
                raise RuntimeError('short write')
        try:
            call('create', 'test', 1)
            call('create', 'test/data', 0)
            payload = bytes(n % 251 for n in range(1024 * 1024))
            for start in range(0, len(payload), 128 * 1024):
                write('test/data', start, payload[start:start + 128 * 1024])
            if read('test/data', 0, len(payload)) != payload:
                raise RuntimeError('dirty cache mismatch')
            api.check(fs.hyper_f2fs_sync(mount))
            write('test/data', 123, b'updated after sync')
            payload = payload[:123] + b'updated after sync' + payload[141:]
            call('rename', 'test/data', b'test/renamed', 12)
            api.check(fs.hyper_f2fs_unmount(mount))
            mount = c.c_void_p()
            api.check(fs.hyper_f2fs_mount(c.byref(image.device), c.byref(mount)))
            if read('test/renamed', 0, len(payload)) != payload:
                raise RuntimeError('remount data mismatch')
            call('resize', 'test/renamed', 17)
            call('resize', 'test/renamed', 9000)
            expected = payload[:17] + bytes(9000 - 17)
            if read('test/renamed', 0, 9000) != expected:
                raise RuntimeError('truncate/regrow exposed stale bytes')
            write('test/renamed', 16389, b'sparse')
            expected += bytes(16389 - 9000) + b'sparse'
            api.check(fs.hyper_f2fs_unmount(mount))
            mount = c.c_void_p()
            api.check(fs.hyper_f2fs_mount(c.byref(image.device), c.byref(mount)))
            if read('test/renamed', 0, len(expected)) != expected:
                raise RuntimeError('sparse remount mismatch')
            for index in range(80):
                call('create', f'test/file-{index}', 0)
            cursor = c.c_uint64()
            names = set()
            for _ in range(100):
                buffer = c.create_string_buffer(300)
                actual = c.c_size_t()
                call('readdir', 'test', c.byref(cursor), buffer, len(buffer), c.byref(actual))
                if not actual.value:
                    break
                offset = 0
                while offset < actual.value:
                    length = buffer.raw[offset + 8]
                    names.add(buffer.raw[offset + 10:offset + 10 + length])
                    offset += 10 + length
            if not {f'file-{index}'.encode() for index in range(80)}.issubset(names):
                raise RuntimeError('directory cursor lost entries')
            for index in range(80):
                call('remove', f'test/file-{index}')
            call('remove', 'test/renamed')
            call('remove', 'test')
            api.check(fs.hyper_f2fs_unmount(mount))
            mount = c.c_void_p()
            if image.counts['max_write'] < 128 * 1024:
                raise RuntimeError('writeback did not merge payload blocks')
            print('F2FS host persistence: PASS', image.counts)
        finally:
            if mount.value:
                fs.hyper_f2fs_destroy(mount)
            image.close()


def failure(library, fault):
    with tempfile.TemporaryDirectory() as temporary:
        path = Path(temporary) / 'disk.img'
        with path.open('xb') as output:
            output.truncate(128 * 1024 * 1024)
        image = api.Image(library, path)
        fs = image.library
        mount = c.c_void_p()
        try:
            api.check(fs.hyper_f2fs_format(c.byref(image.device)))
            if fault == 'geometry':
                image.device.blocks //= 2
            elif fault == 'extension':
                for block in [0, 1]:
                    image.mapping[block * 4096 + 4095] = 1
            result = fs.hyper_f2fs_mount(c.byref(image.device), c.byref(mount))
            if fault in ('geometry', 'extension'):
                if result == 0 or mount.value:
                    raise RuntimeError(f'unsupported {fault} was mounted')
                return
            api.check(result)
            api.check(fs.hyper_f2fs_create(mount, b'data', 4, 0))
            data = bytes(1024 * 1024)
            actual = c.c_size_t()
            api.check(fs.hyper_f2fs_write(mount, b'data', 4, 0, data, len(data), c.byref(actual)))
            image.fail_write = fault == 'write'
            image.fail_flush = fault == 'flush'
            if fs.hyper_f2fs_sync(mount) == 0:
                raise RuntimeError(f'failed {fault} incorrectly reported durable sync')
            image.fail_write = image.fail_flush = False
            if fs.hyper_f2fs_sync(mount) == 0:
                raise RuntimeError('failed checkpoint state was silently cleared')
        finally:
            if mount.value:
                fs.hyper_f2fs_destroy(mount)
            image.close()


if __name__ == '__main__':
    if len(sys.argv) == 3:
        failure(sys.argv[1], sys.argv[2])
    else:
        suite(sys.argv[1])
        for fault in ['geometry', 'extension', 'write', 'flush']:
            subprocess.run([sys.executable, __file__, sys.argv[1], fault], check=True, timeout=30)
            print(f'F2FS {fault} rejection: PASS', flush=True)
