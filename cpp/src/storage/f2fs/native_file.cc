// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#include "src/storage/f2fs/file.h"
#include "src/storage/f2fs/vmo_manager.h"
#include "src/storage/f2fs/f2fs.h"
#include "src/storage/f2fs/node.h"
#include "src/storage/f2fs/node_page.h"
#include "src/storage/f2fs/superblock_info.h"

namespace f2fs
{
zx::result<std::vector<LockedPage>> File::ReadPagesForNative(pgoff_t first, size_t count)
{
	auto pages = GrabLockedPages(first, first + count);
	if (pages.is_error())
		return pages.take_error();
	auto addresses = FindAddresses(first, count);
	if (addresses.is_error())
		return addresses.take_error();
	for (size_t i = 0; i < count; ++i) {
		auto &page = (*pages)[i];
		page.WaitOnWriteback();
		if (page->IsUptodate()) {
			(*addresses)[i] = kNullAddr;
			continue;
		}
		if (!IsValidBlockAddr((*addresses)[i])) {
			page.Zero();
			page->SetUptodate();
			(*addresses)[i] = kNullAddr;
		}
	}
	auto result = fs()->MakeReadOperations(*pages, *addresses, PageType::kData);
	if (result.is_error())
		return result.take_error();
	return zx::ok(*std::move(pages));
}

zx::result<LockedPage> File::FindDataPageForNative(pgoff_t index)
{
	auto pages = ReadPagesForNative(index, 1);
	if (pages.is_error())
		return pages.take_error();
	return zx::ok(std::move((*pages)[0]));
}

zx_status_t File::ReadNative(void *output, size_t length, size_t offset, size_t *actual)
{
	*actual = 0;
	fs::SharedLock global(f2fs::GetGlobalLock());
	std::lock_guard file(mutex_);
	size_t size = GetSize();
	if (offset >= size || !length)
		return ZX_OK;
	length = std::min(length, size - offset);
	if (TestFlag(InodeInfoFlag::kInlineData)) {
		LockedPage node;
		auto status = fs()->GetNodeManager().GetNodePage(Ino(), &node);
		if (status != ZX_OK)
			return status;
		if (offset + length > MaxInlineData())
			return ZX_ERR_IO_DATA_INTEGRITY;
		status = node->Read(output, InlineDataOffset() + offset, length);
		if (status == ZX_OK)
			*actual = length;
		return status;
	}
	size_t first = offset / kBlockSize,
	       count = (offset % kBlockSize + length + kBlockSize - 1) / kBlockSize;
	auto pages = ReadPagesForNative(first, count);
	if (pages.is_error())
		return pages.error_value();
	for (auto &page : *pages) {
		size_t begin = offset % kBlockSize,
		       n = std::min(length - *actual, kBlockSize - begin);
		auto status = page->Read(static_cast<uint8_t *>(output) + *actual, begin, n);
		if (status != ZX_OK)
			return status;
		offset += n;
		*actual += n;
	}
	return ZX_OK;
}

zx_status_t File::WriteNative(const void *input, size_t length, size_t offset, size_t *actual)
{
	*actual = 0;
	if (!length)
		return ZX_OK;
	if (offset > MaxFileSize() || length > MaxFileSize() - offset)
		return ZX_ERR_OUT_OF_RANGE;
	if (GetSuperblockInfo().TestCpFlags(CpFlag::kCpErrorFlag))
		return ZX_ERR_BAD_STATE;
	fs()->BalanceFs((length + 2 * kBlockSize - 1) / kBlockSize);
	fs::SharedLock global(f2fs::GetGlobalLock());
	if (TestFlag(InodeInfoFlag::kInlineData)) {
		auto status = ConvertInlineData();
		if (status != ZX_OK)
			return status;
	}
	std::lock_guard file(mutex_);
	size_t first = offset / kBlockSize,
	       count = (offset % kBlockSize + length + kBlockSize - 1) / kBlockSize;
	auto pages = ReadPagesForNative(first, count);
	if (pages.is_error())
		return pages.error_value();
	auto addresses = GetAddresses(first, count);
	if (addresses.is_error())
		return addresses.error_value();
	size_t end = offset + length;
	for (auto &page : *pages) {
		size_t begin = offset % kBlockSize,
		       n = std::min(length - *actual, kBlockSize - begin);
		auto status = page->Write(static_cast<const uint8_t *>(input) + *actual, begin, n);
		if (status != ZX_OK)
			return status;
		page.SetDirty();
		offset += n;
		*actual += n;
	}
	if (end > GetSize())
		SetSize(end);
	SetTime<Timestamps::ModificationTime>();
	SetDirty();
	return ZX_OK;
}
} // namespace f2fs
