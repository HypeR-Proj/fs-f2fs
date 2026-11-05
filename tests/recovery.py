#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0
"""Real-core roll-forward, failed mount cleanup and checkpoint error fixtures."""
import ctypes as c
import importlib.util
import os
from pathlib import Path
import subprocess
import sys
import tempfile

COMPONENT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('f2fs_host', COMPONENT / 'host.py')
api = importlib.util.module_from_spec(spec)
spec.loader.exec_module(api)


class FaultImage(api.Image):
    def __init__(self, *args):
        self.read_fault = None
        self.write_fault = None
        self.injected = 0
        self.fail_write_at = None
        self.fail_flush_at = None
        self.write_attempts = 0
        self.flush_attempts = 0
        super().__init__(*args)
        self.library.hyper_f2fs_test_state.argtypes = [c.c_void_p, c.c_uint]
        self.library.hyper_f2fs_test_state.restype = c.c_uint64
        self.library.hyper_f2fs_test_pending_writer.argtypes = [c.c_void_p]
        self.library.hyper_f2fs_test_writeback_nat.argtypes = [c.c_void_p, c.c_uint32]
        self.library.hyper_f2fs_test_node.argtypes = [c.c_void_p, c.c_uint32, c.c_bool]
        self.library.hyper_f2fs_test_fsync.argtypes = [c.c_void_p, c.c_char_p, c.c_size_t]
        self.library.hyper_f2fs_sync_if_dirty.argtypes = [c.c_void_p]

    def _read(self, ctx, offset, buffer, size):
        if self.read_fault is not None and offset <= self.read_fault < offset + size:
            self.injected += 1
            return -40
        return super()._read(ctx, offset, buffer, size)

    def _write(self, ctx, offset, buffer, size):
        self.write_attempts += 1
        if self.write_attempts == self.fail_write_at:
            self.injected += 1
            return -40
        if self.write_fault is not None and offset <= self.write_fault < offset + size:
            self.injected += 1
            return -40
        return super()._write(ctx, offset, buffer, size)

    def _flush(self, ctx):
        self.flush_attempts += 1
        if self.flush_attempts == self.fail_flush_at:
            self.injected += 1
            return -40
        return super()._flush(ctx)


def mount(image):
    result = c.c_void_p()
    api.check(image.library.hyper_f2fs_mount(c.byref(image.device), c.byref(result)))
    return result


def write(image, mounted, payload):
    actual = c.c_size_t()
    api.check(image.library.hyper_f2fs_write(mounted, b'data', 4, 0, payload,
                                            len(payload), c.byref(actual)))
    if actual.value != len(payload):
        raise RuntimeError('short fixture write')


def exercise(library, fault):
    output = Path(os.environ.get('OUT', COMPONENT.parents[1] / 'out')).resolve()
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='f2fs-recovery-', dir=output) as directory:
        disk = Path(directory) / 'disk.img'
        with disk.open('xb') as stream:
            stream.truncate(128 * 1024 * 1024)
        image = FaultImage(library, disk)
        fs = image.library
        mounted = c.c_void_p()
        try:
            api.check(fs.hyper_f2fs_format(c.byref(image.device)))
            mounted = mount(image)
            api.check(fs.hyper_f2fs_create(mounted, b'data', 4, 0))
            write(image, mounted, b'A' * 16384)
            api.check(fs.hyper_f2fs_sync(mounted))
            recovery_offset = fs.hyper_f2fs_test_state(mounted, 0) * 4096
            checkpoint = fs.hyper_f2fs_test_state(mounted, 1)
            cp_base = fs.hyper_f2fs_test_state(mounted, 2)
            blocks = fs.hyper_f2fs_test_state(mounted, 3)
            if fault == 'periodic-clean':
                def clean_pass():
                    before = dict(image.counts)
                    version = fs.hyper_f2fs_test_state(mounted, 1)
                    api.check(fs.hyper_f2fs_sync_if_dirty(mounted))
                    if image.counts != before or fs.hyper_f2fs_test_state(mounted, 1) != version:
                        raise RuntimeError('clean periodic sync performed I/O or advanced checkpoint')
                clean_pass()
                # Metadata-only changes must not be hidden by a data-page counter.
                for name, args in [('create', (b'empty', 5, 0)),
                                   ('rename', (b'empty', 5, b'renamed', 7)),
                                   ('remove', (b'renamed', 7))]:
                    api.check(getattr(fs, 'hyper_f2fs_' + name)(mounted, *args))
                    version = fs.hyper_f2fs_test_state(mounted, 1)
                    api.check(fs.hyper_f2fs_sync_if_dirty(mounted))
                    if fs.hyper_f2fs_test_state(mounted, 1) == version:
                        raise RuntimeError('periodic sync skipped metadata mutation')
                    clean_pass()
                write(image, mounted, b'B' * 16384)
                api.check(fs.hyper_f2fs_sync_if_dirty(mounted))
                clean_pass()
                before = image.counts['flush']
                api.check(fs.hyper_f2fs_sync(mounted))
                if image.counts['flush'] <= before:
                    raise RuntimeError('explicit sync lost its flush boundary')
                return
            if fault in ('nat-read', 'writeback-nat-read'):
                # A distant unused nid avoids both the NAT journal and the free-nid
                # scan cache. Its never-written NAT block uses the first copy.
                nid = 100000
                nat_base = fs.hyper_f2fs_test_state(mounted, 4)
                nat_index = nid // 455
                image.read_fault = (nat_base + nat_index // blocks * blocks * 2
                                    + nat_index % blocks) * 4096
                if fault == 'writeback-nat-read':
                    if fs.hyper_f2fs_test_writeback_nat(mounted, nid) != -40:
                        raise RuntimeError('failed NAT lookup allocated an address or lost its dirty page')
                    if fs.hyper_f2fs_sync(mounted) == 0:
                        raise RuntimeError('NAT writeback failure was hidden by volume sync')
                    if image.injected == 0:
                        raise RuntimeError('NAT writeback fault was not reached')
                    return
                for batch in (False, True):
                    if fs.hyper_f2fs_test_node(mounted, nid, batch) != -40:
                        raise RuntimeError('NAT read error was converted to an absent node')
                if image.injected < 2:
                    raise RuntimeError('NAT fault did not reach both node read paths')
                image.read_fault = None
                return
            next_cp = (cp_base + (blocks if checkpoint & 1 else 0)) * 4096
            if fault == 'writer-shutdown':
                fs.hyper_f2fs_test_pending_writer(mounted)
                fs.hyper_f2fs_destroy(mounted)
                mounted = c.c_void_p()
                return
            if fault == 'checkpoint':
                write(image, mounted, b'B' * 16384)
                image.write_fault = next_cp
                if fs.hyper_f2fs_sync(mounted) == 0:
                    raise RuntimeError('checkpoint payload failure reported success')
                if image.injected == 0:
                    raise RuntimeError('checkpoint failure did not reach the device')
                image.write_fault = None
                if fs.hyper_f2fs_sync(mounted) == 0:
                    raise RuntimeError('checkpoint error was forgotten')
                if fs.hyper_f2fs_sync_if_dirty(mounted) == 0:
                    raise RuntimeError('clean fast path hid a checkpoint error')
                return
            if fault != 'scan-read':
                write(image, mounted, b'B' * 16384)
                api.check(fs.hyper_f2fs_test_fsync(mounted, b'data', 4))
                if fs.hyper_f2fs_test_state(mounted, 1) != checkpoint:
                    raise RuntimeError('fixture checkpointed instead of generating replay records')
            # Abandon without a shutdown checkpoint. Only device-acknowledged
            # writes form this fixture; this is not a physical power-loss model.
            fs.hyper_f2fs_destroy(mounted)
            mounted = c.c_void_p()
            saved_inode = None
            if fault == 'missing-inode':
                footer_inode = recovery_offset + 4096 - 24 + 4
                saved_inode = image.mapping[footer_inode:footer_inode + 4]
                image.mapping[footer_inode:footer_inode + 4] = (1000).to_bytes(4, 'little')
            if fault == 'scan-read':
                image.read_fault = recovery_offset
            elif fault == 'replay-checkpoint':
                image.fail_write = True
            elif fault == 'replay-partial-write':
                image.write_attempts = 0
                image.fail_write_at = 2
            elif fault in ('replay-preflush', 'replay-commit-flush'):
                image.flush_attempts = 0
                image.fail_flush_at = 1 if fault == 'replay-preflush' else 2
            writes_before = image.counts['write']
            partial = fault in ('replay-partial-write', 'replay-preflush', 'replay-commit-flush')
            if partial or fault in ('scan-read', 'replay-checkpoint', 'missing-inode'):
                if fs.hyper_f2fs_mount(c.byref(image.device), c.byref(mounted)) == 0 or mounted.value:
                    raise RuntimeError('failed recovery published a mount')
                if fault == 'scan-read' and image.injected == 0:
                    raise RuntimeError('recovery read fault did not fire')
                if partial and (image.injected != 1 or image.counts['write'] <= writes_before):
                    raise RuntimeError('fixture did not fail after successful recovery writes')
                image.read_fault = None
                image.fail_write = False
                image.fail_write_at = None
                image.fail_flush_at = None
                if saved_inode is not None:
                    image.mapping[footer_inode:footer_inode + 4] = saved_inode
            mounted = mount(image)
            data = c.create_string_buffer(16384)
            actual = c.c_size_t()
            api.check(fs.hyper_f2fs_read(mounted, b'data', 4, 0, data, len(data), c.byref(actual)))
            expected = b'A' if fault == 'scan-read' else b'B'
            if actual.value != len(data) or data.raw != expected * len(data):
                raise RuntimeError('checkpoint/replay data mismatch after reopen')
            api.check(fs.hyper_f2fs_unmount(mounted))
            mounted = c.c_void_p()
        finally:
            if mounted.value:
                fs.hyper_f2fs_destroy(mounted)
            image.close()


if __name__ == '__main__':
    if len(sys.argv) == 3:
        exercise(sys.argv[1], sys.argv[2])
    else:
        for fault in ['checkpoint', 'scan-read', 'roll-forward', 'replay-checkpoint',
                      'missing-inode', 'writer-shutdown', 'nat-read', 'writeback-nat-read', 'periodic-clean',
                      'replay-partial-write', 'replay-preflush', 'replay-commit-flush']:
            subprocess.run([sys.executable, __file__, sys.argv[1], fault], check=True, timeout=45)
            print(f'F2FS {fault}: PASS', flush=True)
