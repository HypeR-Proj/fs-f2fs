// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// HypeR platform adapter for the pinned media core.
#pragma once
namespace f2fs {
class F2fs;
class InspectTree {
 public:
  explicit InspectTree(F2fs*) {}
  void Initialize() {}
  void OnOutOfSpace() {}
};
}  // namespace f2fs
