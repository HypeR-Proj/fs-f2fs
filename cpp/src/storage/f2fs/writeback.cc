// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/f2fs/writeback.h"

#include "src/storage/f2fs/bcache.h"
#include "src/storage/f2fs/common.h"
#include "src/storage/f2fs/f2fs.h"
#include "src/storage/f2fs/file_cache.h"
#include "src/storage/f2fs/node_page.h"
#include "src/storage/f2fs/storage_buffer.h"
#include "src/storage/f2fs/superblock_info.h"
#include "src/storage/f2fs/vnode.h"

namespace f2fs {

Writer::Writer(std::unique_ptr<StorageBufferPool> pool)
    : max_block_address_(pool->Block()->Maxblk()), bcache_mapper_(pool->Block()) {
  pool_ = std::move(pool);
  completion_thread_ = std::thread([this] { CompleteLoop(); });
}

Writer::~Writer() { Shutdown(); }

void Writer::Shutdown() {
  if (shutdown_) return;
  // Producers may enqueue I/O while draining their final writeback task.
  // Stop them before the final I/O fence and before terminating its executor.
  writeback_executor_.Terminate();
  Sync();
  executor_.Terminate();
  {
    std::lock_guard lock(completion_mutex_);
    completion_stop_ = true;
  }
  completion_cv_.notify_all();
  completion_thread_.join();
  shutdown_ = true;
}

void Writer::Sync() {
  sync_completion_t completion;
  ScheduleWriteBlocks(&completion, {}, true);
  ZX_ASSERT_MSG(sync_completion_wait(&completion, zx::sec(kWriteTimeOut).get()) == ZX_OK,
                "Writer::Sync() timeout");
}

zx::result<storage::Operation> Writer::PageToOperation(OwnedStorageBuffer &buffer, Page &page,
                                                       bool &needs_preflush) {
  if (!IsValidBlockAddr(page.GetBlockAddr()) || page.GetBlockAddr() >= max_block_address_) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  zx::result index = buffer->Reserve(1);
  if (index.is_error()) {
    return zx::error(ZX_ERR_UNAVAILABLE);
  }

  storage::OperationType type = storage::OperationType::kWrite;
  if (page.IsCommit()) {
    needs_preflush = true;
    type = storage::OperationType::kWriteFua;
    page.ClearCommit();
  } else if (page.IsSync()) {
    type = storage::OperationType::kWriteFua;
    page.ClearSync();
  }

  storage::Operation op = {
      .type = type,
      .vmo_offset = *index,
      .dev_offset = page.GetBlockAddr(),
      .length = 1,
  };
  // Copy data of |page| to |buffer|
  page.Read(buffer->Data(op.vmo_offset));
  return zx::ok(op);
}

std::vector<storage::BufferedOperation> Writer::BuildBufferedOperation(OwnedStorageBuffer &buffer,
                                                                       PageList &pages,
                                                                       PageList &to_submit,
                                                                       bool &needs_preflush) {
  fs::BufferedOperationsBuilder builder;
  NotifyWriteback notifier;
  // Bound data retained by each completion independently of the larger staging
  // allocation; two outstanding groups retain at most 1 MiB of data pages.
  while (!pages.is_empty() && to_submit.size() < 128) {
    auto page = pages.pop_front();
    // A recovery/checkpoint marker starts its own batch. Its preflush must
    // cover every preceding node, including nodes collected in this task.
    // Flushing before a batch that also contains those nodes is too early.
    if (page->IsCommit() && !to_submit.is_empty()) {
      pages.push_front(std::move(page));
      return builder.TakeOperations();
    }
    zx::result operation = PageToOperation(buffer, *page, needs_preflush);
    if (operation.is_error()) {
      if (operation.error_value() == ZX_ERR_UNAVAILABLE) {
        // No available buffers. Need to submit pending StorageOperations to free buffers.
        pages.push_front(std::move(page));
        return builder.TakeOperations();
      }
      // If |page| has an invalid addr, notify waiters.
      if (page->IsUptodate()) {
        FX_LOGS(ERROR) << "(" << page->GetKey() << ": " << page->GetVnode().GetKey()
                       << ") page has an invalid addr: " << page->GetBlockAddr() << " "
                       << operation.status_string();
      }
      notifier.ReserveNotify(std::move(page));
      continue;
    }
    to_submit.push_back(std::move(page));
    builder.Add(*operation, &buffer->GetVmoBuffer());
  }
  return builder.TakeOperations();
}

void Writer::FinishPages(PageList pages, zx_status_t status) {
  if (status != ZX_OK) {
    std::lock_guard lock(completion_mutex_);
    if (io_status_ == ZX_OK) io_status_ = status;
  }
  // Completion must not accumulate a second clean data cache beside VFS.
  // Fixed storage covers the bounded page group without allocating on retirement.
  std::array<fbl::RefPtr<VnodeF2fs>, 128> vnodes;
  size_t vnode_count = 0;
  {
    NotifyWriteback notifier;
    while (!pages.is_empty()) {
      auto page = pages.pop_front();
      if (status != ZX_OK)
        page->fs()->GetSuperblockInfo().SetCpFlags(CpFlag::kCpErrorFlag);
      if (page->GetVnode().IsNode())
        fbl::RefPtr<NodePage>::Downcast(page)->SetFsyncMark(false);
      if (!page->GetVnode().IsNode() && !page->GetVnode().IsMeta() &&
          !page->GetVnode().IsDir()) {
        auto* vnode = &page->GetVnode();
        bool seen = false;
        for (size_t i = 0; i < vnode_count; ++i) seen |= vnodes[i].get() == vnode;
        if (!seen) vnodes[vnode_count++] = page->GetFileCache().GetVnodeRefPtr();
      }
      page->ClearColdData();
      notifier.ReserveNotify(std::move(page));
    }
  }
  // All submitted page refs are now retired. Eviction skips pages made active
  // or dirty by a concurrent writer, including a same-page overwrite.
  for (size_t i = 0; i < vnode_count; ++i) vnodes[i]->GetFileCache().EvictCleanPages();
}

void Writer::DrainCompletions() {
  std::unique_lock lock(completion_mutex_);
  completion_cv_.wait(lock, [this] { return inflight_ == 0; });
}

size_t Writer::ReserveCompletion(bool data) {
  std::unique_lock lock(completion_mutex_);
  completion_cv_.wait(lock, [this, data] {
    return completion_count_ < completions_.size() && (!data || inflight_ < 2);
  });
  const size_t slot = completion_tail_;
  completion_tail_ = (completion_tail_ + 1) % completions_.size();
  ++completion_count_;
  completions_[slot].data = data;
  if (data) ++inflight_;
  return slot;
}

void Writer::PublishCompletion(size_t slot, PageList pages, void* token, zx_status_t status,
                               sync_completion_t* completion, WritebackResult* result) {
  {
    std::lock_guard lock(completion_mutex_);
    auto& task = completions_[slot];
    task.pages = std::move(pages);
    task.token = token;
    task.status = status;
    task.completion = completion;
    task.result = result;
    task.ready = true;
  }
  completion_cv_.notify_all();
}

void Writer::CompleteLoop() {
  for (;;) {
    CompletionTask task;
    {
      std::unique_lock lock(completion_mutex_);
      completion_cv_.wait(lock, [this] {
        return (completion_count_ && completions_[completion_head_].ready) ||
               (completion_stop_ && completion_count_ == 0);
      });
      if (completion_stop_ && completion_count_ == 0) return;
      task = std::move(completions_[completion_head_]);
      completions_[completion_head_] = CompletionTask{};
      completion_head_ = (completion_head_ + 1) % completions_.size();
      --completion_count_;
    }
    completion_cv_.notify_all();
    if (task.data) {
      const auto completed = bcache_mapper_->CompleteRequests(task.token);
      FinishPages(std::move(task.pages), task.status != ZX_OK ? task.status : completed);
      {
        std::lock_guard lock(completion_mutex_);
        --inflight_;
      }
      completion_cv_.notify_all();
    }
    if (task.result) {
      {
        std::lock_guard lock(completion_mutex_);
        task.result->status = io_status_;
      }
      sync_completion_signal(&task.result->done);
    }
    if (task.completion) sync_completion_signal(task.completion);
  }
}

fpromise::promise<> Writer::GetTaskForWriteIO(PageList to_submit, sync_completion_t *completion,
                                             WritebackResult* result) {
  return fpromise::make_promise([this, to_submit = std::move(to_submit), completion, result]() mutable {
    while (!to_submit.is_empty()) {
      PageList submitted;
      void* token = nullptr;
      zx_status_t io_status = ZX_OK;
      bool ordinary = true;
      size_t slot = 0;
      {
        OwnedStorageBuffer buffer = pool_->Get(kDefaultBlocksPerSegment);
        bool needs_preflush = false;
        auto operations = BuildBufferedOperation(buffer, to_submit, submitted, needs_preflush);
        if (operations.empty()) break;
        ordinary = !needs_preflush && std::all_of(operations.begin(), operations.end(),
          [](const auto& op) { return op.op.type == storage::OperationType::kWrite; });
        if (ordinary) {
          // No allocation is needed to enqueue retirement after physical I/O
          // acceptance: reserve the fixed slot first, then publish owned values.
          slot = ReserveCompletion(true);
          io_status = bcache_mapper_->SubmitRequests(operations, &token);
        } else {
          // Recovery/checkpoint markers preserve the existing preflush/FUA
          // boundary. No later ordered submission can overtake this fence.
          DrainCompletions();
          {
            std::lock_guard lock(completion_mutex_);
            io_status = io_status_;
          }
          if (io_status == ZX_OK && needs_preflush) io_status = bcache_mapper_->Flush();
          if (io_status == ZX_OK) io_status = bcache_mapper_->RunRequests(operations);
        }
        // Submission consumed every borrowed byte; completion owns kernel
        // copies and the PageList, so the single staging buffer is reusable.
      }
      if (ordinary)
        PublishCompletion(slot, std::move(submitted), token, io_status);
      else
        FinishPages(std::move(submitted), io_status);
    }
    if (completion || result) {
      // A fixed FIFO marker covers earlier groups. It progresses even when the
      // actor waits for a page; no completion scheduler allocation is required.
      const auto slot = ReserveCompletion(false);
      PublishCompletion(slot, {}, nullptr, ZX_OK, completion, result);
    }
    return fpromise::ok();
  });
}

void Writer::ScheduleWriteback(fpromise::promise<> task) {
  writeback_executor_.schedule_task(writeback_sequencer_.wrap(std::move(task)));
}

void Writer::ScheduleWriteBlocks(sync_completion_t *completion, PageList pages, bool flush,
                                  WritebackResult* result) {
  bool need_flush = completion != nullptr || result != nullptr || flush;
  if (!pages.is_empty() || need_flush) {
    std::lock_guard lock(mutex_);
    pages_.splice(pages_.end(), pages);
    // Submit |pages_| when too many pages are waiting for writeback or |need_flush| is set.
    if (need_flush || pages_.size() >= kDefaultBlocksPerSegment) {
      executor_.schedule_task(sequencer_.wrap(GetTaskForWriteIO(std::move(pages_), completion, result)));
    }
  }
}

void NotifyWriteback::ReserveNotify(fbl::RefPtr<Page> page) {
  VnodeF2fs &vnode = page->GetVnode();
  if (!waiters_.is_empty() && waiters_.front().GetVnode().GetKey() == vnode.GetKey()) {
    waiters_.push_back(std::move(page));
    Notify();
  } else {
    Notify(1);
    waiters_.push_back(std::move(page));
  }
}

void NotifyWriteback::Notify(size_t interval) {
  if (waiters_.size() < interval || waiters_.is_empty()) {
    return;
  }
  auto &page = waiters_.front();
  page.GetFileCache().NotifyWriteback(std::move(waiters_));
}

}  // namespace f2fs
