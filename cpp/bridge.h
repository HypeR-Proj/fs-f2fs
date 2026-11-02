// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0
#pragma once
#include <stddef.h>
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
// Private FFI, not a kernel ABI. Buffers are borrowed until the callback returns.
// run() must finish every accepted operation, including on error. kind: read=0, write=1, FUA=2.
struct HyperF2fsIo {
	uint64_t offset;
	void *data;
	size_t length;
	uint32_t kind;
};

struct HyperF2fsDevice {
	void *context;
	uint64_t blocks;
	int32_t (*read)(void *, uint64_t, void *, size_t);
	int32_t (*write)(void *, uint64_t, const void *, size_t);
	int32_t (*flush)(void *);
	uint8_t uuid[16];
	int32_t (*run)(void *, const struct HyperF2fsIo *, size_t);
};

struct HyperF2fsStat {
	uint64_t size, created, accessed, modified;
	uint32_t mode;
};

int32_t hyper_f2fs_format(const struct HyperF2fsDevice *);
int32_t hyper_f2fs_mount(const struct HyperF2fsDevice *, void **);
int32_t hyper_f2fs_sync(void *);
int32_t hyper_f2fs_unmount(void *);
// destroy abandons an unsuccessful mount without claiming durable synchronization.
void hyper_f2fs_destroy(void *);
int32_t hyper_f2fs_create(void *, const char *, size_t, uint8_t);
int32_t hyper_f2fs_read(void *, const char *, size_t, uint64_t, void *, size_t, size_t *);
int32_t hyper_f2fs_write(void *, const char *, size_t, uint64_t, const void *, size_t, size_t *);
int32_t hyper_f2fs_stat(void *, const char *, size_t, struct HyperF2fsStat *);
int32_t hyper_f2fs_readdir(void *, const char *, size_t, uint64_t *, void *, size_t, size_t *);
int32_t hyper_f2fs_remove(void *, const char *, size_t);
int32_t hyper_f2fs_rename(void *, const char *, size_t, const char *, size_t);
int32_t hyper_f2fs_resize(void *, const char *, size_t, uint64_t);
int32_t hyper_f2fs_times(void *, const char *, size_t, const int64_t *, const uint32_t *, uint32_t);
#ifdef __cplusplus
}
#endif
