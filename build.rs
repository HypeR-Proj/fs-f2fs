// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0
use std::{env, path::PathBuf, process::Command};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=cpp");
    println!("cargo:rerun-if-changed=build.py");
    for name in [
        "HYPER_SYSROOT",
        "HYPER_HOST_CXX",
        "HYPER_CLANG",
        "LLVM_AR",
        "HYPER_DOCUMENTATION",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    if env::var("HYPER_DOCUMENTATION").as_deref() == Ok("1") {
        return Ok(());
    }
    let target = env::var("CARGO_CFG_TARGET_OS")?;
    let output = PathBuf::from(env::var("OUT_DIR")?).join("f2fs");
    let mut command = Command::new("python3");
    command.arg("build.py");
    if target == "hyper" {
        let sdk = env::var("HYPER_SYSROOT")
            .map_err(|_| "F2FS requires HYPER_SYSROOT and the installed hyper-cargo driver")?;
        for path in [
            "share/hyper/manifest",
            "include",
            "lib/libc++.a",
            "lib/libc.a",
        ] {
            println!("cargo:rerun-if-changed={sdk}/{path}");
        }
        command.args(["--sdk", &sdk]);
        println!("cargo:rustc-link-search=native={sdk}/lib");
    } else {
        command.arg("--host");
    }
    let result = command.arg("--output").arg(&output).output()?;
    if !result.status.success() {
        return Err(format!(
            "F2FS C++ build failed:\n{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        )
        .into());
    }
    let static_directory = if target == "hyper" {
        output
    } else {
        output.join("static")
    };
    println!(
        "cargo:rustc-link-search=native={}",
        static_directory.display()
    );
    println!("cargo:rustc-link-lib=static=hyper-f2fs");
    match target.as_str() {
        "hyper" => {
            println!("cargo:rustc-link-lib=static=c++");
            println!("cargo:rustc-link-lib=static=c");
        }
        "macos" => println!("cargo:rustc-link-lib=dylib=c++"),
        _ => {
            println!("cargo:rustc-link-lib=dylib=stdc++");
            println!("cargo:rustc-link-lib=dylib=pthread");
        }
    }
    Ok(())
}
