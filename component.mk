# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0
$(eval $(call native-rust-app,svc/fs-f2fs,hyper-fs-f2fs-system,fs-f2fs-system,svc/fs-f2fs,development))

$(eval $(call native-file,lib/fs-f2fs/FUCHSIA-LICENSE,share/licenses/fs-f2fs/FUCHSIA-LICENSE,0644,system development))
$(eval $(call native-file,lib/fs-f2fs/FUCHSIA-PATENTS,share/licenses/fs-f2fs/FUCHSIA-PATENTS,0644,system development))
$(foreach notice,ext2-hash safemath libcxx musl-headers,$(eval $(call native-file,lib/fs-f2fs/licenses/$(notice),share/licenses/fs-f2fs/$(notice),0644,system development)))
