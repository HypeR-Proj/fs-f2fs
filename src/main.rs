// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0
//! Experimental Fuchsia F2FS worker using the common filesystem RPC service.
#[cfg(target_os = "hyper")]
mod runtime;
fn main() -> std::process::ExitCode {
    #[cfg(target_os = "hyper")]
    match runtime::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fs-f2fs: {e}");
            std::process::ExitCode::FAILURE
        }
    }
    #[cfg(not(target_os = "hyper"))]
    std::process::ExitCode::FAILURE
}
