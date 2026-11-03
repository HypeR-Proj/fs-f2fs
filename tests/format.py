#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0
"""Production image seeding, exclusive creation and failed publication."""
import ctypes as c
from pathlib import Path
import sys
import tempfile
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import host as api

library = Path(sys.argv[1])
with tempfile.TemporaryDirectory() as temporary:
    root = Path(temporary)
    payload = root / 'payload'
    (payload / 'vm').mkdir(parents=True)
    content = bytes(n % 251 for n in range(3 * 1024 * 1024 + 17))
    (payload / 'vm/guest.itb').write_bytes(content)
    (payload / 'empty').touch()
    (payload / 'vms.json').write_text('{"virtual-machines": []}\n')
    output = root / 'data.img'
    api.format_image(library, output, 128, payload)
    image = api.Image(library, output)
    mount = c.c_void_p()
    try:
        api.check(image.library.hyper_f2fs_mount(c.byref(image.device), c.byref(mount)))
        result = c.create_string_buffer(len(content))
        actual = c.c_size_t()
        api.check(image.library.hyper_f2fs_read(mount, b'vm/guest.itb', 12, 0,
                                               result, len(content), c.byref(actual)))
        assert actual.value == len(content) and result.raw == content
        # Refill evicted pages with unaligned endpoints across several read batches.
        size = len(content) - 31
        result = c.create_string_buffer(size)
        api.check(image.library.hyper_f2fs_read(mount, b'vm/guest.itb', 12, 17,
                                               result, size, c.byref(actual)))
        assert actual.value == size and result.raw == content[17:17 + size]
        api.check(image.library.hyper_f2fs_unmount(mount))
        mount = c.c_void_p()
    finally:
        if mount.value:
            image.library.hyper_f2fs_destroy(mount)
        image.close()
    before = output.read_bytes()
    try:
        api.format_image(library, output, 128, payload)
    except FileExistsError:
        pass
    else:
        raise AssertionError('existing media was replaced')
    assert output.read_bytes() == before
    (payload / 'link').symlink_to('vms.json')
    rejected = root / 'invalid.img'
    try:
        api.format_image(library, rejected, 128, payload)
    except ValueError:
        pass
    else:
        raise AssertionError('symlink was silently followed')
    assert not rejected.exists(), 'failed image was published'
print('F2FS production seeding and publication: PASS')
