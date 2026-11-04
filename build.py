#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Build the checked-in F2FS media core for Native service or host tests."""

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import fcntl
import json
import os
import shutil
from pathlib import Path
import subprocess
import sys

HERE = Path(__file__).resolve().parent


def build(sdk, output):
    output.mkdir(parents=True, exist_ok=True)
    with (output / '.build.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        return build_locked(sdk, output)


def build_locked(sdk, output):
    environment = dict(os.environ)
    if sdk is None:
        flags = [os.environ.get('HYPER_HOST_CXX', '/usr/bin/clang++' if sys.platform == 'darwin' else 'clang++'), '-std=c++20', '-g', '-O1',
                 '-fno-exceptions', '-fno-rtti', '-fPIC']
        if sys.platform == 'darwin':
            host_sdk = subprocess.check_output(['xcrun', '--show-sdk-path'], text=True).strip()
            flags += ['-isysroot', host_sdk, '-isystem', str(Path(host_sdk) / 'usr/include/c++/v1')]
        cpp_archive = None
    else:
        sdk = sdk.resolve()
        environment['HYPER_SYSROOT'] = str(sdk)
        flags = [str(sdk / 'bin/hyper-clang++'), '-O1', '-g', '-D_GNU_SOURCE=1',
                 '-fno-c++-static-destructors']
        cpp_archive = sdk / 'lib/libc++.a'
        for required in [cpp_archive, sdk / 'lib/libc.a', sdk / 'lib/libsystem.so']:
            if not required.is_file():
                raise FileNotFoundError(f'{required}: build the public Native SDK first')
    tree = HERE / 'cpp'
    digest = hashlib.sha256()
    for path in sorted(tree.rglob('*')):
        if path.is_file():
            digest.update(str(path.relative_to(tree)).encode())
            digest.update(path.read_bytes())
    include_paths = ['', 'zircon/system/public', 'sdk/lib/zircon-assert',
                     'zircon/system/ulib/fbl/include', 'zircon/system/ulib/zx/include',
                     'zircon/system/ulib/zircon-internal/include',
                     'zircon/third_party/ulib/safemath/include', 'sdk/lib/fit/include',
                     'sdk/lib/fit-promise/include', 'sdk/lib/stdcompat/include',
                     'zircon/system/ulib/bitmap/include']
    flags += ([] if sdk is None else ['-DHYPER_F2FS_NATIVE=1']) + [arg for path in include_paths for arg in ('-I', str(tree / path))]
    excluded = {'bcache', 'component_runner', 'inspect', 'memory_watcher', 'vmo_manager', 'fsck'}
    files = [p for p in (tree / 'src/storage/f2fs').glob('*.cc') if p.stem not in excluded]
    files += list((tree / 'sdk/lib/fit-promise').glob('*.cc'))
    files += [tree / path for path in ['src/storage/f2fs/third_party/ext2_hash/hash.cc',
        'bridge.cc', 'src/storage/lib/vfs/cpp/journal/background_executor.cc',
        'src/storage/lib/vfs/cpp/transaction/buffered_operations_builder.cc',
        'zircon/system/ulib/bitmap/raw-bitmap.cc', 'zircon/system/ulib/fbl/alloc_checker.cc',
        'src/storage/lib/buffer/block_buffer.cc']]
    digest.update(json.dumps(flags).encode())
    digest.update(subprocess.check_output([flags[0], "-c", "--version"], env=environment))
    if sdk is not None:
        digest.update((sdk / 'share/hyper/manifest').read_bytes())
        for path in sorted((sdk / 'include').rglob('*')):
            if path.is_file():
                digest.update(str(path.relative_to(sdk)).encode())
                digest.update(path.read_bytes())
        for name in ('libc++.a', 'libc.a'):
            digest.update((sdk / 'lib' / name).read_bytes())
    digest.update(Path(__file__).read_bytes())
    stamp = output / 'build.sha256'
    archive = output / ('static/libhyper-f2fs.a' if sdk is None else 'libhyper-f2fs.a')
    shared = output / 'libhyper-f2fs.so'
    if (not archive.exists() or (sdk is None and not shared.exists())
            or not stamp.exists() or stamp.read_text() != digest.hexdigest()):
        objects = output / 'objects'
        objects.mkdir(exist_ok=True)
        def compile(source):
            obj = objects / (str(source.relative_to(tree)).replace('/', '_') + '.o')
            subprocess.run(flags + ['-c', str(source), '-o', str(obj)], env=environment, check=True)
            return str(obj)
        with ThreadPoolExecutor(max_workers=3) as pool:
            built = list(pool.map(compile, files))
        temporary = output / 'libhyper-f2fs.tmp'
        temporary.unlink(missing_ok=True)
        archiver = os.environ.get('LLVM_AR') or shutil.which('llvm-ar') or shutil.which('ar')
        if not archiver:
            raise FileNotFoundError('LLVM_AR, llvm-ar or ar is required to archive the F2FS core')
        subprocess.run([archiver, 'rcs', str(temporary)] + built,
                       check=True)
        archive.parent.mkdir(parents=True, exist_ok=True)
        temporary.replace(archive)
        if sdk is None:
            subprocess.run(flags + ['-dynamiclib' if sys.platform == 'darwin' else '-shared',
                                   '-o', str(temporary)] + built, check=True)
            temporary.replace(shared)
        stamp.write_text(digest.hexdigest())
    return shared if sdk is None else archive, cpp_archive


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--sdk', type=Path)
    mode.add_argument('--host', action='store_true')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    build(args.sdk, args.output.resolve())
