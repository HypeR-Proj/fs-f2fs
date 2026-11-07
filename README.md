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
access. Submission callbacks borrow a stable mutex-protected device. Owned block
completions wait and reap independently of that mutex; teardown joins both the
submission and completion executors before releasing the device. A successful `close` flushes
and checkpoints; dropping an unsuccessful mount abandons it without claiming
successful synchronization.
The Native worker requires five threads: its service actor, runtime thread reaper,
two F2FS executors and independent completion owner. The mount manager grants this
bounded budget when launching F2FS; host tests do not enforce Native domain quotas.

## I/O and durability

The Native adapter uses the existing `BlockQueue`, including its admission and
backpressure rules. Already available ordinary writes are grouped into at most
sixteen disjoint ranges totaling 512 KiB per ticket, with at most four write
tickets retained per submitted group. Grouping preserves input order, splits at
overlapping ranges and never crosses read or FUA boundaries; it adds no delay to
fill a batch. Scatter ranges reduce ticket overhead without turning disjoint disk
ranges into one physical command. Consecutive independent read operations also use a bounded four-ticket
pipeline. Writeback merges adjacent filesystem blocks before submission.
Ordinary data-write submission consumes its input into kernel-owned requests and
returns an independently owned completion. The ordered C++ submitter reuses its
staging buffer after those copies, while a separate completion executor retires
exact page lists and publishes errors. At most two groups of 128 pages are in
flight through this path. Its fixed completion ring reserves a slot before physical
submission; publication and retirement require no new task allocation. Rust ticket
and receipt allocations are fallible and precede kernel admission. Clean temporary
data pages are evicted after completion, preserving newer dirty/active pages. Reads and FUA operations retain their synchronous
completion contract. Every accepted ticket is retired even after a later admission
failure; failed queues reject further work and remain owned until their completion
owners retire. The kernel retains physical request storage until safe retirement.
No source pointer survives a Native submission. A wait/reap API failure does not
prove ticket retirement: the worker emits a best-effort nonblocking critical
report and terminates instead of dropping the ticket and potentially wedging
credit waiters. The supervisor retains the queue and quarantines the failed volume;
explicit failed-provider recovery revokes the queue and proves physical retirement
before reuse. Successfully reaped device errors retain normal sticky error handling.
The `io-statistics` build feature reports logical read/write ticket peaks separately
from the queue's physical in-flight command peak. Write ticket accounting spans all
accepted asynchronous groups; neither a local group limit nor aggregate ticket
count is physical device depth.

A preflush completes before checkpoint commit I/O. A FUA operation writes its
payload, joins it, and completes an explicit queue flush before returning.
Volume `sync` flushes payload before publishing a live checkpoint; unmount writes
a clean shutdown checkpoint. Any failed write sets the core's checkpoint error
flag and fails synchronization instead of retrying data writes indefinitely.
Successful page-cache writes alone do not imply durability. Synchronization trusts
successful device WRITE/FLUSH responses; it does not read back physical media or
guarantee hardware behavior beyond the reported protocol result. Reported errors
are never treated as successful checkpoints.

The kernel negotiates VFS-owned file-data writeback at mount. Accepted writes
populate the common VFS page cache, so reads through independent opens see dirty
bytes without another filesystem RPC. VFS batches adjacent pages (up to 512 KiB)
and bounds dirty ownership to 8 MiB per mount within its common cache budget.
At 6 MiB, background data drain starts and continues toward 4 MiB, releasing the
volume gate between completed batches. This pressure drain is separate from the
periodic checkpoint. Foreground writes also release that gate between bounded
chunks while preserving same-file ordering.
F2FS separately advertises asynchronous writeback. Its explicit submit operation
copies input and selects the exact page list before acknowledging acceptance; an
owned receipt reports completion later. Per-vnode preparation/enqueue serialization
prevents that receipt from overtaking pages already captured by pressure writeback
but not yet queued. Completion progresses even if the service
actor blocks overwriting a page still under writeback. Ordinary Write replies keep
their synchronous data-completion contract. Neither path issues a checkpoint or
device-cache flush per data batch.
Node/metadata caches, block allocation, GC and recovery remain owned by F2FS.

The VFS worker drains dirty data and then requests a volume checkpoint every
30 seconds. `HYPER_F2FS_SYNC_INTERVAL_SECONDS` selects 1–3600 seconds during
negotiation. The deadline is checked once per second; completion additionally
takes I/O time. Busy traffic does not postpone the fixed cadence. Volume sync,
directory sync and unmount use the ordered data-before-checkpoint fence.
Native callers can explicitly select the containing volume through
`FILE_SYNC_VOLUME` on a writable file; ordinary file sync retains the recovery-log
path described below. Worker handoff
preserves VFS ownership instead of restarting an independent service timer.
Before negotiation, standalone core users retain the service's periodic policy.

Recovery starts from a valid checkpoint and replays eligible fsync records.
Native file `sync_data` and `sync_all` first drain only that file's VFS dirty
pages. The core writes its recovery nodes, with a preflush before the final fsync
marker and a device fence after that marker. Both modes include the inode and
metadata needed to recover contents and size; data-only mode may also persist
timestamps. Namespace dependencies, non-regular files, multiple links, disabled
roll-forward or insufficient recovery space require a checkpoint instead.
File sync does not reset or satisfy the periodic volume-checkpoint deadline.
Recovery is not an application transaction or an exact
snapshot rollback: unsynchronized changes may be lost or partly survive. Failed
replay or checkpoint I/O prevents a successful mount/sync. An empty replay list
is distinct from an inode lookup failure. Failed mount teardown joins writers
before releasing page, callback and metadata owners.

Native reads use at most 512 KiB of temporary file pages per batch. Once copied,
clean inactive pages are evicted from the C++ file cache; dirty and writeback
pages remain owned by the core. The kernel file cache serves repeated reads.

The pressure threshold is 2,048 pages (8 MiB), counting dirty data, directory,
node and metadata pages together with pages whose writeback has not completed.
At pressure, the upstream background writer retires a bounded batch before
admitting more data. Waiting for device writes does not force an extra flush or
checkpoint; file sync adds a recovery fence, while volume sync and the periodic
timer retain checkpoint responsibility. This is not a hard total-memory quota: inode, directory and metadata
caches have additional costs. The worker retains the ordinary
128 MiB committed-page ceiling and allows 1,024 mappings/objects for its heap
regions and staging pools. Exceeding those quotas currently fails the process. Reads do not export physical extents because that
would bypass dirty data held by the media implementation.

## Fit for HypeR

The Native acceptance suite exercises block aggregation and persistent payloads;
see the [acceptance test](https://github.com/HypeR-Proj/system-tests/blob/main/fs-f2fs/README.md).
Actual submission counts and in-flight depth depend on range geometry and device
completion timing. The bounded submission/completion split allows write overlap;
its physical throughput and tail-latency benefit require Pi5 measurements.

The remaining performance risks include metadata heap growth and bounded data
staging in the C++ core; payload still crosses cache,
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
File-sync tests model a volatile device cache and power loss, verify checkpoint-free
recovery (including size and repeated sync), namespace fallback, and failures at
node writes, preflush and the final commit fence. The marker must be submitted
only after all preceding recovery nodes have crossed a successful flush.

Enable `SYSTEM_APP_FS_F2FS_SYSTEM` in an alternate product configuration to deploy
`/svc/fs-f2fs`. Both mount managers recognize `f2fs`, but bootstrap never links
this formal implementation. To build it independently, run `make -C svc/fs-f2fs ARCH=aarch64 OUT="$PWD/out"`
from the workspace root. The optional `io-statistics` feature reports queue counts at
unmount for test evidence.

`tests/app/f2fs-smoke` is a system-test application using only ordinary file
interfaces. Its write/verify, fsync-write/fsync-verify and writeback modes exercise
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
