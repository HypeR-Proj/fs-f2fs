// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#include "bridge.h"
#include "src/storage/f2fs/f2fs.h"
#include "src/storage/f2fs/file.h"
#include "src/storage/f2fs/dir.h"
#include "src/storage/f2fs/mkfs.h"
#include <cstdarg>
#include <cstdio>
#ifdef HYPER_F2FS_NATIVE
extern "C" [[noreturn]] void hyper_cxx_fatal(const char *);

extern "C" void __zx_panic(const char *format, ...)
{
	hyper_cxx_fatal(format);
}
#else
extern "C" void __zx_panic(const char *format, ...)
{
	va_list args;
	va_start(args, format);
	vfprintf(stderr, format, args);
	va_end(args);
	abort();
}
#endif

struct Mounted {
	f2fs::PlatformVfs platform;
	std::unique_ptr<f2fs::F2fs> fs;
};

static zx::result<fbl::RefPtr<f2fs::VnodeF2fs>> Lookup(Mounted &m, std::string_view path)
{
	auto root = m.fs->GetRootVnode();
	if (root.is_error())
		return root.take_error();
	auto node = *root;
	while (!path.empty()) {
		size_t slash = path.find('/');
		auto part = path.substr(0, slash);
		if (!fs::IsValidName(part) || part == "." || part == "..")
			return zx::error(ZX_ERR_INVALID_ARGS);
		if (!node->IsDir())
			return zx::error(ZX_ERR_NOT_DIR);
		fbl::RefPtr<fs::Vnode> next;
		auto status = static_cast<f2fs::Dir *>(node.get())->Lookup(part, &next);
		if (status != ZX_OK)
			return zx::error(status);
		node = fbl::RefPtr<f2fs::VnodeF2fs>::Downcast(std::move(next));
		if (slash == path.npos)
			break;
		path.remove_prefix(slash + 1);
	}
	return zx::ok(std::move(node));
}

extern "C" int32_t hyper_f2fs_format(const f2fs::DeviceCallbacks *cb)
{
	f2fs::MkfsOptions options;
	auto result = f2fs::Mkfs(options, std::make_unique<f2fs::BcacheMapper>(*cb));
	return result.status_value();
}

extern "C" int32_t hyper_f2fs_mount(const f2fs::DeviceCallbacks *cb, void **output)
{
	*output = nullptr;
	auto mount = std::make_unique<Mounted>();
	f2fs::MountOptions options;
	options.SetValue(f2fs::MountOption::kDiscard, 0);
	auto fs = f2fs::F2fs::Create(nullptr, std::make_unique<f2fs::BcacheMapper>(*cb), options,
				     &mount->platform);
	if (fs.is_error())
		return fs.error_value();
	mount->fs = *std::move(fs);
	*output = mount.release();
	return ZX_OK;
}

extern "C" int32_t hyper_f2fs_sync(void *context)
{
	return static_cast<Mounted *>(context)->fs->SyncFs(false);
}

extern "C" int32_t hyper_f2fs_unmount(void *context)
{
	auto *m = static_cast<Mounted *>(context);
	int status = m->fs->SyncFs(true);
	if (status != ZX_OK)
		return status;
	m->fs->PutSuper();
	delete m;
	return ZX_OK;
}

extern "C" int32_t hyper_f2fs_create(void *context, const char *path, size_t n, uint8_t directory)
{
	auto &m = *static_cast<Mounted *>(context);
	std::string_view p(path, n);
	size_t slash = p.rfind('/');
	auto parent = Lookup(m, slash == p.npos ? std::string_view{} : p.substr(0, slash));
	if (parent.is_error())
		return parent.error_value();
	if (!(*parent)->IsDir())
		return ZX_ERR_NOT_DIR;
	auto result = static_cast<f2fs::Dir *>((*parent).get())
			      ->CreateWithMode(p.substr(slash == p.npos ? 0 : slash + 1),
					       (directory ? S_IFDIR : S_IFREG) | 0777);
	if (result.is_error())
		return result.error_value();
	return (*result)->Close();
}

extern "C" int32_t hyper_f2fs_read(void *context, const char *path, size_t n, uint64_t offset,
				   void *buffer, size_t length, size_t *actual)
{
	auto found = Lookup(*static_cast<Mounted *>(context), {path, n});
	if (found.is_error())
		return found.error_value();
	if (!S_ISREG((*found)->GetMode()))
		return ZX_ERR_NOT_FILE;
	return static_cast<f2fs::File *>((*found).get())
		->ReadNative(buffer, length, offset, actual);
}

extern "C" int32_t hyper_f2fs_write(void *context, const char *path, size_t n, uint64_t offset,
				    const void *buffer, size_t length, size_t *actual)
{
	auto found = Lookup(*static_cast<Mounted *>(context), {path, n});
	if (found.is_error())
		return found.error_value();
	if (!S_ISREG((*found)->GetMode()))
		return ZX_ERR_NOT_FILE;
	return static_cast<f2fs::File *>((*found).get())
		->WriteNative(buffer, length, offset, actual);
}

const char *zx::result<>::status_string() const
{
	return "F2FS status";
}

extern "C" void hyper_f2fs_destroy(void *context)
{
	auto *m = static_cast<Mounted *>(context);
	m->fs->PutSuper();
	delete m;
}

extern "C" int32_t hyper_f2fs_stat(void *context, const char *path, size_t n, HyperF2fsStat *out)
{
	auto found = Lookup(*static_cast<Mounted *>(context), {path, n});
	if (found.is_error())
		return found.error_value();
	auto a = (*found)->GetAttributes();
	if (a.is_error())
		return a.error_value();
	*out = {a->content_size, a->creation_time, a->access_time, a->modification_time,
		static_cast<uint32_t>(a->mode)};
	return ZX_OK;
}

extern "C" int32_t hyper_f2fs_readdir(void *context, const char *path, size_t n, uint64_t *cursor,
				      void *output, size_t capacity, size_t *actual)
{
	auto found = Lookup(*static_cast<Mounted *>(context), {path, n});
	if (found.is_error())
		return found.error_value();
	if (!(*found)->IsDir())
		return ZX_ERR_NOT_DIR;
	fs::VdirCookie cookie{*cursor};
	auto status = static_cast<f2fs::Dir *>((*found).get())
			      ->Readdir(&cookie, output, capacity, actual);
	if (status == ZX_OK)
		*cursor = cookie.position;
	return status;
}

struct Parent {
	fbl::RefPtr<f2fs::VnodeF2fs> node;
	std::string_view name;
};

static zx::result<Parent> FindParent(Mounted &mount, std::string_view path)
{
	auto slash = path.rfind('/');
	auto name = path.substr(slash == path.npos ? 0 : slash + 1);
	if (!fs::IsValidName(name) || name == "." || name == "..")
		return zx::error(ZX_ERR_INVALID_ARGS);
	auto parent =
		Lookup(mount, slash == path.npos ? std::string_view{} : path.substr(0, slash));
	if (parent.is_error())
		return parent.take_error();
	if (!(*parent)->IsDir())
		return zx::error(ZX_ERR_NOT_DIR);
	return zx::ok(Parent{*parent, name});
}

extern "C" int32_t hyper_f2fs_remove(void *context, const char *path, size_t n)
{
	auto parent = FindParent(*static_cast<Mounted *>(context), {path, n});
	if (parent.is_error())
		return parent.error_value();
	return static_cast<f2fs::Dir *>(parent->node.get())->Unlink(parent->name, false);
}

extern "C" int32_t hyper_f2fs_rename(void *context, const char *path, size_t n, const char *target,
				     size_t length)
{
	auto &m = *static_cast<Mounted *>(context);
	auto from = FindParent(m, {path, n});
	if (from.is_error())
		return from.error_value();
	auto to = FindParent(m, {target, length});
	if (to.is_error())
		return to.error_value();
	return static_cast<f2fs::Dir *>(from->node.get())
		->Rename(to->node, from->name, to->name, false, false);
}

extern "C" int32_t hyper_f2fs_resize(void *context, const char *path, size_t n, uint64_t size)
{
	auto found = Lookup(*static_cast<Mounted *>(context), {path, n});
	if (found.is_error())
		return found.error_value();
	if (!S_ISREG((*found)->GetMode()))
		return ZX_ERR_NOT_FILE;
	return static_cast<f2fs::File *>((*found).get())->Truncate(size);
}

extern "C" int32_t hyper_f2fs_times(void *context, const char *path, size_t n,
				    const int64_t *seconds, const uint32_t *nanos, uint32_t mask)
{
	if (mask & ~3u)
		return ZX_ERR_INVALID_ARGS;
	fs::VnodeAttributesUpdate update;
	for (size_t i = 0; i < 2; ++i)
		if (mask & (1u << i)) {
			if (seconds[i] < 0 || nanos[i] >= 1000000000 ||
			    uint64_t(seconds[i]) > (INT64_MAX - nanos[i]) / 1000000000)
				return ZX_ERR_OUT_OF_RANGE;
			uint64_t ns = uint64_t(seconds[i]) * 1000000000 + nanos[i];
			if (i == 0)
				update.access_time = ns;
			else
				update.modification_time = ns;
		}
	auto found = Lookup(*static_cast<Mounted *>(context), {path, n});
	if (found.is_error())
		return found.error_value();
	fs::SharedLock global(f2fs::GetGlobalLock());
	return (*found)->UpdateAttributes(update).status_value();
}
