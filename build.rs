// SPDX-FileCopyrightText: 2026 Sash contributors
// SPDX-License-Identifier: MIT

use std::path::{Path, PathBuf};

/// Ask git for a short answer, or give up when the tree has no repository.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "macos" {
        panic!("sash is macOS-only for now (target OS: {os})");
    }

    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // Keep in step with scripts/fetch-libghostty.sh.
    let lib = root.join("vendor/ghostty-vt/1.0.2");
    let headers = lib.join("include");
    assert!(
        headers.join("ghostty/vt.h").exists(),
        "libghostty-vt headers missing at {}; run scripts/fetch-libghostty.sh",
        headers.display()
    );

    println!(
        "cargo:rustc-link-search=native={}",
        lib.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=ghostty-vt");
    println!(
        "cargo:rerun-if-changed={}",
        lib.join("lib/libghostty-vt.a").display()
    );
    println!("cargo:rerun-if-changed=build.rs");

    // Build identity for the Settings footer, the menu-bar menu and bug reports.
    // Scripts pass SASH_BUILD_SHA so a packaged build names its exact revision.
    println!("cargo:rerun-if-env-changed=SASH_BUILD_SHA");
    let sha = std::env::var("SASH_BUILD_SHA")
        .ok()
        .filter(|sha| !sha.trim().is_empty())
        .or_else(|| git(&root, &["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());
    let date = git(&root, &["log", "-1", "--format=%cs"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=SASH_GIT_SHA={sha}");
    println!("cargo:rustc-env=SASH_BUILD_DATE={date}");

    let bindings = bindgen::Builder::default()
        .header(headers.join("ghostty/vt.h").to_string_lossy())
        .clang_arg(format!("-I{}", headers.display()))
        .allowlist_function("ghostty_.*")
        .allowlist_type("Ghostty.*")
        .allowlist_var("GHOSTTY_.*")
        .default_enum_style(bindgen::EnumVariation::Consts)
        .prepend_enum_name(false)
        .derive_default(true)
        .layout_tests(false)
        .generate()
        .expect("bindgen failed on Ghostty headers");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("ghostty_vt.rs"))
        .expect("write bindings");
}
