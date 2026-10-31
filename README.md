<!-- SPDX-FileCopyrightText: 2026 roolrz -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# Experimental F2FS worker

This Native service runs the Fuchsia media core through
[lib-fs-f2fs](https://github.com/HypeR-Proj/lib-fs-f2fs/tree/feat/fuchsia-f2fs).
The kernel-facing filesystem protocol remains unchanged. The worker is disabled
by default; select `SYSTEM_APP_FS_F2FS_SYSTEM=y` to include it in a product.

Use the [working manifest](https://github.com/HypeR-Proj/manifest/tree/feat/fuchsia-f2fs)
and [disposable-image acceptance guide](https://github.com/HypeR-Proj/system-tests/blob/feat/fuchsia-f2fs/fs-f2fs/README.md).
The guide builds this service through the public SDK and exercises write,
checkpoint, complete QEMU restart, readback, recursive deletion and writeback.
The `io-statistics` feature prints queue submission counts at unmount.

Only images produced by the pinned formatter are supported. This is a working
integration branch; crash recovery, sustained GC/full-disk pressure and hardware
performance remain unqualified. Normal system images continue to use FAT.
