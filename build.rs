use std::path::PathBuf;

fn main() {
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "macos" {
        panic!("sash currently links the macOS libghostty-vt build only (target OS: {os})");
    }
    let slice = if arch == "aarch64" {
        "macos-arm64_x86_64"
    } else {
        "macos-arm64_x86_64"
    };

    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let xcf = root.join("vendor/ghostty-vt/ghostty-vt.xcframework").join(slice);
    let headers = xcf.join("Headers");
    assert!(
        headers.join("ghostty/vt.h").exists(),
        "libghostty-vt headers missing at {}; run scripts/fetch-libghostty.sh",
        headers.display()
    );

    println!("cargo:rustc-link-search=native={}", xcf.display());
    println!("cargo:rustc-link-lib=static=ghostty-vt");
    println!("cargo:rerun-if-changed={}", xcf.join("libghostty-vt.a").display());
    println!("cargo:rerun-if-changed=build.rs");

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
