//! Embeds the application manifest into `MjolnirVSS.exe`.
//!
//! The manifest is what makes Windows ask for administrator rights when the
//! program starts, what gives the window themed controls, and what tells
//! Windows the process handles display scaling itself.
//!
//! It is embedded by the linker rather than shipped as a separate
//! `MjolnirVSS.exe.manifest` file, because a loose manifest next to the
//! executable is easy to lose when someone copies the program to a USB stick,
//! and losing it would silently drop the elevation request.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=build.rs");

    // Only the MSVC toolchain understands these linker arguments, and only
    // Windows has a manifest to embed.
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target != "windows" || env != "msvc" {
        return;
    }

    let manifest: PathBuf = [
        std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets this"),
        "app.manifest".to_owned(),
    ]
    .iter()
    .collect();

    if !manifest.is_file() {
        // A missing manifest must fail the build rather than quietly produce an
        // executable that never asks for elevation.
        panic!(
            "the application manifest is missing at {}",
            manifest.display()
        );
    }

    // /MANIFEST:EMBED asks the linker to put the manifest inside the binary;
    // /MANIFESTINPUT names the file, and needs a full path.
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );

    // The elevation request goes here rather than in the manifest file. Rust
    // already embeds a default manifest asking for asInvoker, and mt.exe fails
    // the build with "Values of attribute level not equal in different manifest
    // snippets" if a second snippet disagrees. /MANIFESTUAC makes mt.exe
    // generate the trustInfo section itself, so there is only ever one.
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='requireAdministrator'");
}
