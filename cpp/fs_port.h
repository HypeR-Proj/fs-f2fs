// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#pragma once
#include "portability.h"
#include <set>
#include <lib/fit/defer.h>
#include <fbl/ref_ptr.h>
#include <fbl/recycler.h>
#include "src/storage/lib/vfs/cpp/ref_counted.h"

namespace fuchsia_io
{
enum class NodeProtocolKinds { kFile, kDirectory };
enum class DirentType : uint8_t { kUnknown = 0 };

namespace wire
{
enum class WatchEvent { kDeleted, kAdded, kRemoved };
}
} // namespace fuchsia_io

namespace fs
{
struct VnodeAttributes {
	uint64_t mode = 0, id = 0, content_size = 0, storage_size = 0, link_count = 0,
		 creation_time = 0, modification_time = 0, change_time = 0, access_time = 0;
};
enum class VnodeAttributesQuery : uint32_t { kCreationTime = 1, kModificationTime = 2 };

inline VnodeAttributesQuery operator|(VnodeAttributesQuery a, VnodeAttributesQuery b)
{
	return VnodeAttributesQuery(uint32_t(a) | uint32_t(b));
}

struct VnodeAttributesUpdate {
	std::optional<uint64_t> creation_time, modification_time, access_time;
};
enum class CreationType { kFile, kDirectory };

struct VdirCookie {
	uint64_t position = 0;
};

class DirentFiller
{
	char *data_;
	size_t capacity_, used_ = 0;

      public:
	DirentFiller(void *p, size_t n) : data_((char *)p), capacity_(n)
	{
	}

	bool Next(std::string_view name, fuchsia_io::DirentType kind, uint64_t ino)
	{
		if (name.size() > 255 || capacity_ - used_ < 10 + name.size())
			return false;
		memcpy(data_ + used_, &ino, 8);
		data_[used_ + 8] = name.size();
		data_[used_ + 9] = uint8_t(kind);
		memcpy(data_ + used_ + 10, name.data(), name.size());
		used_ += 10 + name.size();
		return true;
	}

	size_t BytesFilled() const
	{
		return used_;
	}
};

inline bool IsValidName(std::string_view s)
{
	return !s.empty() && s.size() <= 255 && s.find('/') == s.npos && s.find('\0') == s.npos;
}

class Vnode : public VnodeRefCounted<Vnode>, public fbl::Recyclable<Vnode>
{
      public:
	using SyncCallback = fit::function<void(zx_status_t)>;
	virtual ~Vnode() = default;

	void fbl_recycle()
	{
		RecycleNode();
	}

	uint64_t open_count() const
	{
		return opens_;
	}

	int32_t Open(void *)
	{
		++opens_;
		return ZX_OK;
	}

	int32_t Close()
	{
		if (!opens_)
			return ZX_ERR_BAD_STATE;
		--opens_;
		return CloseNode();
	}

	virtual void Sync(SyncCallback) = 0;
	virtual zx::result<VnodeAttributes> GetAttributes() const = 0;
	virtual VnodeAttributesQuery SupportedMutableAttributes() const = 0;
	virtual zx::result<> UpdateAttributes(const VnodeAttributesUpdate &) = 0;
	virtual fuchsia_io::NodeProtocolKinds GetProtocols() const = 0;

	virtual zx_status_t Lookup(std::string_view, fbl::RefPtr<Vnode> *)
	{
		return ZX_ERR_NOT_SUPPORTED;
	}

	virtual zx_status_t Readdir(VdirCookie *, void *, size_t, size_t *)
	{
		return ZX_ERR_NOT_SUPPORTED;
	}

	virtual zx_status_t Rename(fbl::RefPtr<Vnode>, std::string_view, std::string_view, bool,
				   bool)
	{
		return ZX_ERR_NOT_SUPPORTED;
	}

	virtual zx_status_t Link(std::string_view, fbl::RefPtr<Vnode>)
	{
		return ZX_ERR_NOT_SUPPORTED;
	}

	virtual zx::result<fbl::RefPtr<Vnode>> Create(std::string_view, CreationType)
	{
		return zx::error(ZX_ERR_NOT_SUPPORTED);
	}

	virtual zx_status_t Unlink(std::string_view, bool)
	{
		return ZX_ERR_NOT_SUPPORTED;
	}

	virtual zx_status_t Truncate(size_t)
	{
		return ZX_ERR_NOT_SUPPORTED;
	}

	virtual void Notify(std::string_view, fuchsia_io::wire::WatchEvent)
	{
	}

      protected:
	mutable std::shared_mutex mutex_;
	uint64_t opens_ = 0;

	virtual zx_status_t CloseNode()
	{
		return ZX_OK;
	}

	virtual void RecycleNode()
	{
		delete this;
	}
};
} // namespace fs
