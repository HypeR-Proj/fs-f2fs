// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#pragma once
#include <algorithm>
#include <array>
#include <atomic>
#include <condition_variable>
#include <cstdlib>
#include <cstring>
#include <map>
#include <memory>
#include <mutex>
#include <optional>
#include <shared_mutex>
#include <string>
#include <string_view>
#include <vector>
#include <sys/stat.h>
#include <lib/zx/result.h>
#include <fbl/algorithm.h>

struct PortLog {
	template <class T> PortLog &operator<<(const T &)
	{
		return *this;
	}
};

#define FX_LOGS(level) PortLog()
#define TRACE_DURATION(...)

inline const char *zx_status_get_string(zx_status_t)
{
	return "F2FS error";
}

#include <bitset>
#include <lib/zx/time.h>
#include <fbl/intrusive_double_list.h>

struct sync_completion_t {
	std::mutex mutex;
	std::condition_variable cv;
	bool done = false;
};

inline void sync_completion_signal(sync_completion_t *c)
{
	std::lock_guard l(c->mutex);
	c->done = true;
	c->cv.notify_all();
}

inline zx_status_t sync_completion_wait(sync_completion_t *c, int64_t timeout)
{
	std::unique_lock l(c->mutex);
	if (timeout == ZX_TIME_INFINITE) {
		c->cv.wait(l, [&] { return c->done; });
		return ZX_OK;
	}
	return c->cv.wait_for(l, std::chrono::nanoseconds(timeout), [&] { return c->done; })
		       ? ZX_OK
		       : ZX_ERR_TIMED_OUT;
}

inline uint32_t zx_system_get_page_size()
{
	return 4096;
}
