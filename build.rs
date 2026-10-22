// Embeds app.manifest (Per-Monitor V2 DPI awareness, Windows 10/11 compatibility)
// through the MSVC linker, so no resource-compiler build dependency is needed.
use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" && target_env == "msvc" {
        let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("app.manifest");
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest.display()
        );
    } else if os == "windows" {
        // GNU toolchain: the manifest is not embedded. DPI awareness is still set at
        // runtime, but layered child windows need the Windows 8+ compatibility entry,
        // so prefer the MSVC toolchain.
        println!(
            "cargo:warning=manifest not embedded (non-MSVC toolchain); use x86_64-pc-windows-msvc"
        );
    }
}
