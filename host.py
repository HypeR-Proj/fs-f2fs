#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Host fixture for the same F2FS core; only creates new images when formatting."""
import argparse
import ctypes as c
import mmap
from pathlib import Path
import uuid

READ = c.CFUNCTYPE(c.c_int32, c.c_void_p, c.c_uint64, c.c_void_p, c.c_size_t)
FLUSH = c.CFUNCTYPE(c.c_int32, c.c_void_p)
class Io(c.Structure):
    _fields_ = [('offset', c.c_uint64), ('data', c.c_void_p), ('length', c.c_size_t), ('kind', c.c_uint32)]
RUN = c.CFUNCTYPE(c.c_int32, c.c_void_p, c.POINTER(Io), c.c_size_t)
class Device(c.Structure):
    _fields_ = [('context', c.c_void_p), ('blocks', c.c_uint64), ('read', READ), ('write', READ),
                ('flush', FLUSH), ('uuid', c.c_uint8 * 16), ('run', RUN)]

class Image:
    def __init__(self, library, path):
        self.library = c.CDLL(str(Path(library).resolve()))
        self.file = Path(path).open('r+b')
        self.mapping = mmap.mmap(self.file.fileno(), 0)
        self.counts = dict(read=0, write=0, flush=0, max_write=0)
        self.fail_write = False
        self.fail_flush = False
        self.read = READ(self._read)
        self.write = READ(self._write)
        self.flush = FLUSH(self._flush)
        self.run = RUN(self._run)
        self.device = Device(None, len(self.mapping) // 4096, self.read, self.write, self.flush,
                             (c.c_uint8 * 16).from_buffer_copy(uuid.uuid4().bytes), self.run)
        lib = self.library
        lib.hyper_f2fs_format.argtypes = [c.POINTER(Device)]
        lib.hyper_f2fs_mount.argtypes = [c.POINTER(Device), c.POINTER(c.c_void_p)]
        for name in ('sync', 'unmount', 'destroy'):
            getattr(lib, 'hyper_f2fs_' + name).argtypes = [c.c_void_p]
        lib.hyper_f2fs_create.argtypes = [c.c_void_p, c.c_char_p, c.c_size_t, c.c_uint8]
        for name in ('read', 'write'):
            getattr(lib, 'hyper_f2fs_' + name).argtypes = [c.c_void_p, c.c_char_p, c.c_size_t,
                c.c_uint64, c.c_void_p, c.c_size_t, c.POINTER(c.c_size_t)]
        lib.hyper_f2fs_resize.argtypes = [c.c_void_p, c.c_char_p, c.c_size_t, c.c_uint64]
        lib.hyper_f2fs_remove.argtypes = [c.c_void_p, c.c_char_p, c.c_size_t]
        lib.hyper_f2fs_rename.argtypes = [c.c_void_p, c.c_char_p, c.c_size_t, c.c_char_p, c.c_size_t]
        lib.hyper_f2fs_readdir.argtypes = [c.c_void_p, c.c_char_p, c.c_size_t,
                                         c.POINTER(c.c_uint64), c.c_void_p, c.c_size_t, c.POINTER(c.c_size_t)]

    def _read(self, _, offset, buffer, size):
        if offset + size > len(self.mapping):
            return -14
        c.memmove(buffer, self.mapping[offset:offset + size], size)
        self.counts['read'] += 1
        return 0

    def _write(self, _, offset, buffer, size):
        if self.fail_write:
            return -40
        if offset + size > len(self.mapping):
            return -14
        self.mapping[offset:offset + size] = c.string_at(buffer, size)
        self.counts['write'] += 1
        self.counts['max_write'] = max(self.counts['max_write'], size)
        return 0

    def _flush(self, _):
        if self.fail_flush:
            return -40
        self.mapping.flush()
        self.counts['flush'] += 1
        return 0

    def _run(self, _, operations, count):
        for operation in operations[:count]:
            if operation.kind == 0:
                result = self._read(None, operation.offset, operation.data, operation.length)
            elif operation.kind in (1, 2):
                result = self._write(None, operation.offset, operation.data, operation.length)
                if result == 0 and operation.kind == 2:
                    result = self._flush(None)
            else:
                return -2
            if result:
                return result
        return 0

    def close(self):
        self.mapping.close()
        self.file.close()


def check(status):
    if status:
        raise RuntimeError(f'F2FS status {status}')


def populate(image, root):
    """Seed regular files/directories in bounded chunks, then checkpoint once."""
    root = root.resolve(strict=True)
    if not root.is_dir():
        raise ValueError('payload root must be a directory')
    fs = image.library
    mount = c.c_void_p()
    check(fs.hyper_f2fs_mount(c.byref(image.device), c.byref(mount)))
    try:
        for path in sorted(root.rglob('*')):
            if path.is_symlink() or not (path.is_file() or path.is_dir()):
                raise ValueError(f'unsupported F2FS payload entry: {path}')
            name = path.relative_to(root).as_posix().encode()
            check(fs.hyper_f2fs_create(mount, name, len(name), int(path.is_dir())))
            if path.is_file():
                with path.open('rb') as source:
                    offset = 0
                    while data := source.read(128 * 1024):
                        actual = c.c_size_t()
                        check(fs.hyper_f2fs_write(mount, name, len(name), offset,
                                                 data, len(data), c.byref(actual)))
                        if actual.value != len(data):
                            raise RuntimeError(f'short F2FS payload write: {path}')
                        offset += actual.value
        check(fs.hyper_f2fs_unmount(mount))
        mount = c.c_void_p()
    finally:
        if mount.value:
            fs.hyper_f2fs_destroy(mount)


def format_image(library, output, size_mib, root=None):
    if not 128 <= size_mib < (1 << 24):
        raise ValueError('image must be between 128 MiB and 16 TiB')
    # Exclusive creation protects existing images even on invalid input/failure.
    stream = output.open('xb')
    image = None
    try:
        with stream:
            stream.truncate(size_mib * 1024 * 1024)
        image = Image(library, output)
        check(image.library.hyper_f2fs_format(c.byref(image.device)))
        if root is not None:
            populate(image, root)
        print(f'Formatted {output}: {image.counts}')
    except BaseException:
        output.unlink()
        raise
    finally:
        if image is not None:
            image.close()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--library', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--size-mib', type=int, default=128)
    parser.add_argument('--root', type=Path, help='seed a tree of regular files and directories')
    args = parser.parse_args()
    format_image(args.library, args.output, args.size_mib, args.root)
