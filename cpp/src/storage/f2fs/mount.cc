// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/f2fs/mount.h"




namespace f2fs {

zx::result<size_t> MountOptions::GetValue(const MountOption option) const {
  if (option >= MountOption::kMaxNum)
    return zx::error(ZX_ERR_INVALID_ARGS);
  return zx::ok(opt_[static_cast<size_t>(option)]);
}

zx_status_t MountOptions::SetValue(const MountOption option, const size_t value) {
  if (option == MountOption::kActiveLogs && value != 2 && value != 4 && value != 6) {
    FX_LOGS(WARNING) << " active_logs can be set only to 2, 4, or 6.";
    return ZX_ERR_INVALID_ARGS;
  };
  opt_[static_cast<size_t>(option)] = value;
  return ZX_OK;
}

}  // namespace f2fs
