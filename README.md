<!-- SPDX-FileCopyrightText: 2026 roolrz -->
<!-- SPDX-License-Identifier: Apache-2.0 -->

# F2FS service

This repository owns the checked-in Fuchsia-derived C++ media implementation,
its Rust adapter and the Native filesystem worker. There is one Cargo package;
`src/media/` is an internal module, while `src/runtime.rs` adapts the common
filesystem service protocol. The kernel has no C++ or F2FS dependency.
The worker is enabled by default (`SYSTEM_APP_FS_F2FS_SYSTEM=y`). New ordinary
persistent images use F2FS. Existing FAT media is not automatically converted.
The host formatter uses the same checked-in media core and seeds initial files;
no Fuchsia download or Linux `mkfs.f2fs` installation is required.

The C++ core retains Fuchsia's inode, directory, NAT/SIT, segment allocation,
checkpoint, garbage collection and writeback code. The patch removes Zircon
VFS, pager, component, Inspect and memory-pressure dependencies. Explicit file
reads and writes use the core's page/node cache and block allocation routines.
The private page backing is heap memory, not a Zircon VMO emulation service.

Rust owns the device for the entire mount. Public operations require exclusive
access. Background C++ callbacks borrow a stable mutex-protected device; teardown
joins the executor before releasing that device. A successful `close` flushes
and checkpoints; dropping an unsuccessful mount abandons it without claiming
successful synchronization.

## I/O and durability

The Native adapter uses the existing `BlockQueue`, including its admission and
backpressure rules. Each submission is at most 512 KiB, with at most four writes
outstanding. Consecutive independent read operations also use a bounded four-ticket
pipeline. Writeback merges adjacent filesystem blocks before submission.
The callback drains accepted reads and writes before returning, including after a
later admission failure. A transport failure instead closes the queue: the kernel
retains its own request storage until physical retirement, and no callback buffer
remains borrowed. No source pointer survives a Native submission.
The `io-statistics` build feature reports logical read/write ticket peaks separately
from the queue's physical in-flight command peak; ticket counts are not device depth.

A preflush completes before checkpoint commit I/O. A FUA operation writes its
payload, joins it, and completes an explicit queue flush before returning.
Normal `sync` flushes payload before publishing a live checkpoint; unmount writes
a clean shutdown checkpoint. Any failed write sets the core's checkpoint error
flag and fails synchronization instead of retrying data writes indefinitely.
Successful page-cache writes alone do not imply durability. Synchronization trusts
successful device WRITE/FLUSH responses; it does not read back physical media or
guarantee hardware behavior beyond the reported protocol result. Reported errors
are never treated as successful checkpoints.

The ordered service owner synchronizes the volume every 30 seconds, including
when requests keep arriving. `HYPER_F2FS_SYNC_INTERVAL_SECONDS` in the worker's
startup environment selects 1–3600 seconds. The interval schedules a checkpoint;
its completion additionally takes I/O time. A successful pass covers mutations
accepted before the pass starts. Pressure-driven writeback remains active and
can run earlier. Slow passes coalesce missed ticks rather than building a backlog.
Periodic passes inspect the core's dirty vnodes, metadata, orphan state and pending
writeback under the filesystem lock. Clean passes perform no I/O and do not advance
the checkpoint version. Explicit sync and shutdown retain their full barriers;
the clean fast path never hides an existing checkpoint error.
Handoff and unmount retain explicit synchronization and retire the timer with the
service owner. There is no second kernel dirty-data cache.

Recovery starts from a valid checkpoint and replays eligible fsync records.
The Native sync path currently checkpoints the volume; a lightweight per-file
fsync API is not exposed. Recovery is not an application transaction or an exact
snapshot rollback: unsynchronized changes may be lost or partly survive. Failed
replay or checkpoint I/O prevents a successful mount/sync. An empty replay list
is distinct from an inode lookup failure. Failed mount teardown joins writers
before releasing page, callback and metadata owners.

Native reads use at most 512 KiB of temporary file pages per batch. Once copied,
clean inactive pages are evicted from the C++ file cache; dirty and writeback
pages remain owned by the core. The kernel file cache serves repeated reads.

The dirty-page threshold is 2,048 pages (8 MiB), using upstream background
writeback. This is not a hard total-memory quota: inode, directory and metadata
caches have additional costs. The worker retains the ordinary
128 MiB committed-page ceiling and allows 1,024 mappings/objects for its heap
regions and staging pools. Exceeding those quotas currently fails the process. Reads do not export physical extents because that
would bypass dirty data held by the media implementation.

## Fit for HypeR

The initial Native run demonstrates the property missing from a purely serial
sector writer: four outstanding queue writes and aggregation of a 4 MiB payload
plus 80 small files into 23 write submissions. See the [acceptance test](https://github.com/HypeR-Proj/system-tests/blob/main/fs-f2fs/README.md).
This makes the port a plausible writeback experiment for the I/O VM transport.

The remaining performance risks are substantial: dirty file data and metadata
still consume C++ heap alongside the kernel cache; payload still crosses cache,
staging and copied queue buffers;
GC and checkpoint latency have not been characterized under sustained pressure.
The port also introduces a C++ runtime and replaces Fuchsia's pager with explicit
heap-backed pages. These costs remain qualification work even though new deployment images use
F2FS by default.

## Supported contract and limits

The worker supports regular files, directories, paginated listing,
read/write, rename, removal, truncate, timestamps, sync and orderly handoff.
New nodes use mode `0777`, matching the current FAT worker's writable Native
mode projection. Read-only mounts, symlinks, hard links, extended attributes,
compression, encryption and discard are not exposed by this adapter.

Use images produced by the formatter from this checkout. The upstream superblock
structure ends at `cp_payload`; this adapter rejects nonzero trailing superblock
fields rather than guessing support for modern F2FS extensions. It also checks
that the media geometry covers the recorded block count. This is deliberately
narrower than accepting arbitrary Linux `mkfs.f2fs` output. Times before the Unix
epoch or beyond the signed nanosecond range are rejected.

The default image format does not imply exhaustive media qualification.
Long-running GC/full-disk workloads, exhaustive allocation and I/O fault injection,
unclean shutdown/power-loss recovery, Linux interoperability, hard cache limits,
and real SD/NVMe performance still need qualification. QEMU timing is not a
hardware performance result.

## Sources and licenses

`cpp/` contains the maintained fork of Fuchsia revision
[`fff7033e9cc337f2ab4fe8629a61c0c66bdb5cff`](https://fuchsia.googlesource.com/fuchsia/+/fff7033e9cc337f2ab4fe8629a61c0c66bdb5cff/src/storage/f2fs/),
including the support subtrees needed by the port. [UPSTREAM.json](UPSTREAM.json)
records provenance for the original imported trees. The unmodified baseline is
commit `275781642d6e1770a73af7b89fb631636d130b84`; compare changes beneath `cpp/`
against that commit when reviewing the adaptation. Original upstream build files
and tests remain as reference material; `build.py` defines the supported build.

Builds compile the checked-in files directly. They do not download Fuchsia,
expand source archives or apply patches. Upstream updates are explicit source
changes reviewed and committed in this repository. The C++ build cache includes
all checked-in media files, compiler options and the installed SDK identity.
The repository Rustdoc driver sets `HYPER_DOCUMENTATION=1` to avoid compiling or
linking media code while producing documentation; executable builds leave it unset.

The media core uses the Fuchsia BSD license; ext2 hashing retains its FreeBSD
BSD-2-Clause notice, and safemath retains its BSD notice. The Native C++ support
uses LLVM libc++ 21.1.8 (Apache-2.0 with LLVM exception) and the public musl 1.2.5 port (MIT), linked against `libsystem.so`.
No private C++ runtime or Linux filesystem code is linked.
Complete notices are retained beside this file and under `licenses/`, and are
included under `share/licenses/fs-f2fs` in system images. Google's upstream
[PATENTS grant](FUCHSIA-PATENTS) is preserved; it is not a general clearance of
third-party F2FS patent claims.

## Build and test

Start with an assembled SDK and the Clang/LLVM tools used by the Native build.
`HYPER_CXX`, `HYPER_CLANG` and `LLVM_AR` can select explicit tool paths.
Native compilation uses installed `hyper-clang++`, `libc++.a` and `libc.a`.
The host test uses the host C++ standard library; on macOS it selects Apple Clang
and its installed SDK. `HYPER_HOST_CXX` overrides that selection.

```sh
export OUT="$PWD/out-f2fs"
python3 project.py test fs-f2fs
make -C svc/fs-f2fs ARCH=aarch64 OUT="$OUT"
python3 svc/fs-f2fs/host.py \
  --library "$OUT/build/fs-f2fs/host/libhyper-f2fs.so" \
  --output "$OUT/new-volume.img" --size-mib 128
```

The formatter exclusively creates a new image; it never overwrites an existing
file. The host test exercises real media algorithms, including dirty reads,
checkpoint/remount, overwrite, truncate/regrow, sparse data and directory paging.

Enable `SYSTEM_APP_FS_F2FS_SYSTEM` in an alternate product configuration to deploy
`/svc/fs-f2fs`. Both mount managers recognize `f2fs`, but bootstrap never links
this formal implementation. To build it independently, run `make -C svc/fs-f2fs ARCH=aarch64 OUT="$PWD/out"`
from the workspace root. The optional `io-statistics` feature reports queue counts at
unmount for test evidence.

`tests/app/f2fs-smoke` is a system-test application using only ordinary file
interfaces. Its `write ROOT`, `verify ROOT` and `writeback ROOT` modes exercise
the real worker through Native VFS. It does not link the filesystem core.
`tests/qemu/verify-f2fs.py` runs these workloads in separate QEMU instances,
including a 12 MiB background-writeback workload and explicit unmounts.
See [the fixture instructions](https://github.com/HypeR-Proj/system-tests/blob/main/fs-f2fs/README.md).

Module tests: `python3 project.py test fs-f2fs`. The host media suite covers
persistence, sparse files, directory paging and write/flush failures. The Rust
module test in `src/media/tests.rs` owns a 128 MiB RAM device and checks the FFI
boundary, format, mount, 4 MiB payload, rename, remount and truncate/regrow
zeroing. These internal tests belong to the service repository and never format
physical media. The system suite builds the host formatter but does not rerun
module tests.

Build metadata owns the service artifact, Cargo package and installed notices.
The product switch is generated from its artifact declaration. Host objects,
SDK and Cargo outputs use `OUT`; no Fuchsia source cache is needed.
