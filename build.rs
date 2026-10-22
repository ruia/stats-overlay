// Embeds app.manifest (Per-Monitor V2 DPI awareness, Windows 10/11 compatibility).
// MSVC: through linker flags. GNU: compiled with windres. No build dependencies.
use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=app.rc");
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    if os != "windows" {
        return;
    }
    if target_env == "msvc" {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            dir.join("app.manifest").display()
        );
        return;
    }
    // GNU: layered child windows need the manifest's Windows 8+ compatibility entry.
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("manifest.o");
    let compiled = ["x86_64-w64-mingw32-windres", "windres"]
        .iter()
        .any(|windres| {
            Command::new(windres)
                .current_dir(&dir)
                .args(["app.rc", "-O", "coff", "-o"])
                .arg(&out)
                .status()
                .is_ok_and(|s| s.success())
        });
    if compiled {
        println!("cargo:rustc-link-arg-bins={}", out.display());
    } else {
        println!("cargo:warning=windres not found: manifest not embedded; use the MSVC toolchain");
    }
}
