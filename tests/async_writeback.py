#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0
"""Exercise independent completion, staging reuse and same-page writer progress."""
import ctypes as c
import importlib.util
from pathlib import Path
import sys
import tempfile
import threading

spec = importlib.util.spec_from_file_location('f2fs_host', Path(__file__).resolve().parents[1] / 'host.py')
api = importlib.util.module_from_spec(spec)
spec.loader.exec_module(api)
SUBMIT = c.CFUNCTYPE(c.c_int32, c.c_void_p, c.POINTER(api.Io), c.c_size_t, c.POINTER(c.c_void_p))
COMPLETE = c.CFUNCTYPE(c.c_int32, c.c_void_p)


def suite(library, fail=False, reject=False):
    with tempfile.TemporaryDirectory() as temporary:
        path = Path(temporary) / 'disk.img'
        with path.open('xb') as output:
            output.truncate(128 * 1024 * 1024)
        image = api.Image(library, path)
        gate = threading.Event()
        gate.set()
        condition = threading.Condition()
        pending = {}
        sequence = 0
        completed = 0
        failure = False
        reject_submission = False
        def submit(_, operations, count, output):
            nonlocal sequence
            if reject_submission:
                output[0] = None
                return -4
            copied = []
            for operation in operations[:count]:
                if operation.kind != 1:
                    return -10
                copied.append((operation.offset, c.string_at(operation.data, operation.length)))
            with condition:
                sequence += 1
                pending[sequence] = copied
                output[0] = sequence
                condition.notify_all()
            return 0
        def complete(token):
            nonlocal completed
            if not gate.wait(10):
                return -40
            with condition:
                copied = pending.pop(token)
            if not failure:
                for offset, data in copied:
                    image.mapping[offset:offset + len(data)] = data
            with condition:
                completed += 1
                condition.notify_all()
            return -40 if failure else 0
        submit_cb, complete_cb = SUBMIT(submit), COMPLETE(complete)
        image.device.submit = c.cast(submit_cb, c.c_void_p)
        image.device.complete = c.cast(complete_cb, c.c_void_p)
        fs = image.library
        fs.hyper_f2fs_enable_vfs_writeback.argtypes = [c.c_void_p]
        fs.hyper_f2fs_submit_write.argtypes = [c.c_void_p, c.c_char_p, c.c_size_t,
            c.c_uint64, c.c_void_p, c.c_size_t, c.POINTER(c.c_size_t), c.POINTER(c.c_void_p)]
        fs.hyper_f2fs_wait_write.argtypes = [c.c_void_p]
        fs.hyper_f2fs_test_cached_pages.argtypes = [c.c_void_p, c.c_char_p, c.c_size_t]
        fs.hyper_f2fs_test_cached_pages.restype = c.c_int64
        mount = c.c_void_p()
        receipts = []
        def start(data, offset=0):
            receipt, actual = c.c_void_p(), c.c_size_t()
            api.check(fs.hyper_f2fs_submit_write(mount, b'data', 4, offset, data,
                len(data), c.byref(actual), c.byref(receipt)))
            if actual.value != len(data) or not receipt.value:
                raise RuntimeError('invalid accepted prefix or receipt')
            receipts.append(receipt)
            return receipt
        def wait(receipt, expected=0):
            result = fs.hyper_f2fs_wait_write(receipt)
            receipts.remove(receipt)
            if result != expected:
                raise RuntimeError(f'completion {result}, expected {expected}')
        try:
            api.check(fs.hyper_f2fs_format(c.byref(image.device)))
            api.check(fs.hyper_f2fs_mount(c.byref(image.device), c.byref(mount)))
            api.check(fs.hyper_f2fs_create(mount, b'data', 4, 0))
            api.check(fs.hyper_f2fs_enable_vfs_writeback(mount))
            first = b'A' * (128 * 1024)
            second = b'B' * len(first)
            if reject:
                reject_submission = True
                receipt = start(first)
                wait(receipt, -4)
                if pending or fs.hyper_f2fs_sync(mount) == 0:
                    raise RuntimeError('allocation rejection stranded pages or permitted checkpoint')
                print('F2FS asynchronous completion reservation failure: PASS')
                return
            gate.clear()
            with condition:
                initial_completed = completed
            one = start(first)
            two = start(second, len(first))
            with condition:
                if not condition.wait_for(lambda: len(pending) == 2, timeout=5):
                    raise RuntimeError('second submission waited for first completion')
                if completed != initial_completed:
                    raise RuntimeError('completion escaped controlled device gate')
            failure = fail
            gate.set()
            wait(one, -40 if fail else 0)
            wait(two, -40 if fail else 0)
            if fail:
                if fs.hyper_f2fs_sync(mount) == 0:
                    raise RuntimeError('failed data completion allowed successful checkpoint')
                print('F2FS asynchronous writeback error propagation: PASS')
                return
            if fs.hyper_f2fs_test_cached_pages(mount, b'data', 4) != 0:
                raise RuntimeError('completed temporary data pages accumulated beside VFS')
            output, actual = c.create_string_buffer(len(first) * 2), c.c_size_t()
            api.check(fs.hyper_f2fs_read(mount, b'data', 4, 0, output, len(output), c.byref(actual)))
            if output.raw != first + second:
                raise RuntimeError('staging reuse corrupted retained write data')
            # The actor can block on a page whose earlier writeback is pending.
            # Completing that write must not require the blocked actor to reap.
            gate.clear()
            one = start(b'C' * len(first))
            with condition:
                if not condition.wait_for(lambda: bool(pending), timeout=5):
                    raise RuntimeError('first same-page write was not submitted')
            done, errors, later = threading.Event(), [], []
            def overwrite():
                try:
                    later.append(start(b'D' * len(first)))
                except Exception as error:
                    errors.append(error)
                finally:
                    done.set()
            worker = threading.Thread(target=overwrite)
            worker.start()
            if done.wait(0.05):
                raise RuntimeError('same-page overwrite bypassed writeback protection')
            gate.set()
            if not done.wait(5):
                raise RuntimeError('page completion depended on blocked actor/reap')
            worker.join()
            if errors:
                raise errors[0]
            wait(one)
            wait(later[0])
            # Teardown joins accepted writes even when their public receipts
            # have not been consumed. A receipt remains safe after mount teardown.
            gate.clear()
            final = start(b'E' * len(first))
            closed = []
            closer = threading.Thread(target=lambda: closed.append(fs.hyper_f2fs_unmount(mount)))
            closer.start()
            closer.join(0.05)
            if not closer.is_alive():
                raise RuntimeError('unmount returned before accepted I/O retired')
            gate.set()
            closer.join(5)
            if closer.is_alive():
                raise RuntimeError('teardown could not join independent completion')
            api.check(closed[0])
            mount = c.c_void_p()
            wait(final)
            api.check(fs.hyper_f2fs_mount(c.byref(image.device), c.byref(mount)))
            api.check(fs.hyper_f2fs_read(mount, b'data', 4, 0, output, len(output), c.byref(actual)))
            if output.raw != b'E' * len(first) + second:
                raise RuntimeError('asynchronous overwrite did not survive remount')
            print('F2FS asynchronous writeback ownership/progress: PASS')
        finally:
            gate.set()
            for receipt in receipts:
                fs.hyper_f2fs_wait_write(receipt)
            if mount.value:
                fs.hyper_f2fs_destroy(mount)
            image.close()


def preparation_race(library):
    """A receipt cannot overtake pages owned by an unqueued pressure writer."""
    with tempfile.TemporaryDirectory() as temporary:
        path = Path(temporary) / 'disk.img'
        with path.open('xb') as output:
            output.truncate(128 * 1024 * 1024)
        image = api.Image(library, path)
        fs = image.library
        fs.hyper_f2fs_enable_vfs_writeback.argtypes = [c.c_void_p]
        fs.hyper_f2fs_test_pause_preparation.argtypes = [c.c_bool]
        fs.hyper_f2fs_test_wait_prepared.restype = c.c_bool
        fs.hyper_f2fs_test_dirty.argtypes = [c.c_void_p, c.c_char_p, c.c_size_t, c.c_void_p, c.c_size_t]
        fs.hyper_f2fs_test_snapshot.argtypes = [c.c_void_p, c.c_char_p, c.c_size_t, c.POINTER(c.c_void_p)]
        fs.hyper_f2fs_wait_write.argtypes = [c.c_void_p]
        mount, receipts, workers, errors = c.c_void_p(), [], [], []
        completed = [threading.Event(), threading.Event()]
        def snapshot(index):
            receipt = c.c_void_p()
            try:
                api.check(fs.hyper_f2fs_test_snapshot(mount, b'data', 4, c.byref(receipt)))
                receipts.append(receipt)
            except Exception as error:
                errors.append(error)
            finally:
                completed[index].set()
        try:
            api.check(fs.hyper_f2fs_format(c.byref(image.device)))
            api.check(fs.hyper_f2fs_mount(c.byref(image.device), c.byref(mount)))
            api.check(fs.hyper_f2fs_create(mount, b'data', 4, 0))
            api.check(fs.hyper_f2fs_enable_vfs_writeback(mount))
            payload = b'P' * 4096
            api.check(fs.hyper_f2fs_test_dirty(mount, b'data', 4, payload, len(payload)))
            fs.hyper_f2fs_test_pause_preparation(True)
            workers.append(threading.Thread(target=snapshot, args=(0,)))
            workers[-1].start()
            if not fs.hyper_f2fs_test_wait_prepared():
                raise RuntimeError('pressure writer did not reach the controlled preparation gap')
            workers.append(threading.Thread(target=snapshot, args=(1,)))
            workers[-1].start()
            if completed[1].wait(0.05):
                raise RuntimeError('foreground receipt overtook an unqueued pressure PageList')
            fs.hyper_f2fs_test_pause_preparation(False)
            for worker in workers:
                worker.join(5)
                if worker.is_alive():
                    raise RuntimeError('writeback preparation fence failed to progress')
            if errors:
                raise errors[0]
            for receipt in receipts:
                api.check(fs.hyper_f2fs_wait_write(receipt))
            receipts.clear()
            output, actual = c.create_string_buffer(len(payload)), c.c_size_t()
            api.check(fs.hyper_f2fs_read(mount, b'data', 4, 0, output, len(output), c.byref(actual)))
            if output.raw != payload:
                raise RuntimeError('pressure writer lost accepted payload')
            print('F2FS pressure preparation versus receipt ordering: PASS')
        finally:
            fs.hyper_f2fs_test_pause_preparation(False)
            for worker in workers:
                worker.join(5)
            for receipt in receipts:
                fs.hyper_f2fs_wait_write(receipt)
            if mount.value:
                fs.hyper_f2fs_destroy(mount)
            image.close()


if __name__ == '__main__':
    suite(sys.argv[1])
    suite(sys.argv[1], fail=True)
    suite(sys.argv[1], reject=True)
    preparation_race(sys.argv[1])
