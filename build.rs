// Embeds app.manifest (Per-Monitor V2 DPI awareness, Windows 10/11 compatibility)
// and a version resource made from Cargo.toml's version, which Explorer shows
// under Properties > Details. MSVC: a .res file written here, handed to the
// linker. GNU: compiled with windres. No build dependencies.
use std::{env, fs, path::PathBuf, process::Command};

const NAME: &str = "StatsOverlay";
const DESCRIPTION: &str = "StatsOverlay taskbar resource monitor";

fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=app.rc");
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    if os != "windows" {
        return;
    }
    let version = version();
    if target_env == "msvc" {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            dir.join("app.manifest").display()
        );
        let res = out_dir.join("version.res");
        fs::write(&res, version_res(version)).unwrap();
        println!("cargo:rustc-link-arg-bins={}", res.display());
        return;
    }
    // GNU: layered child windows need the manifest's Windows 8+ compatibility entry.
    let rc = out_dir.join("app.rc");
    // Written to OUT_DIR, so the manifest is referred to by its full path.
    let manifest = dir
        .join("app.manifest")
        .display()
        .to_string()
        .replace('\\', "/");
    let script = fs::read_to_string(dir.join("app.rc"))
        .unwrap()
        .replace("\"app.manifest\"", &format!("\"{manifest}\""));
    fs::write(&rc, format!("{script}\n{}", version_rc(version))).unwrap();
    let out = out_dir.join("manifest.o");
    let compiled = ["x86_64-w64-mingw32-windres", "windres"]
        .iter()
        .any(|windres| {
            Command::new(windres)
                .current_dir(&dir)
                .arg(&rc)
                .args(["-O", "coff", "-o"])
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

/// (major, minor, patch) from CARGO_PKG_VERSION.
fn version() -> [u16; 3] {
    let v = env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts = v
        .split(['.', '-', '+'])
        .map(|p| p.parse::<u16>().unwrap_or(0));
    [(); 3].map(|_| parts.next().unwrap_or(0))
}

fn dotted([a, b, c]: [u16; 3]) -> String {
    format!("{a}.{b}.{c}")
}

/// The strings shown in Explorer, under the US English / Unicode block.
fn strings(v: [u16; 3]) -> [(&'static str, String); 5] {
    [
        ("FileDescription", DESCRIPTION.to_string()),
        ("FileVersion", dotted(v)),
        ("OriginalFilename", "statsoverlay.exe".to_string()),
        ("ProductName", NAME.to_string()),
        ("ProductVersion", dotted(v)),
    ]
}

/// VERSIONINFO in resource-script form, for windres.
fn version_rc(v: [u16; 3]) -> String {
    let [a, b, c] = v;
    let mut s = format!(
        "1 VERSIONINFO\nFILEVERSION {a},{b},{c},0\nPRODUCTVERSION {a},{b},{c},0\n\
         FILEOS 0x40004\nFILETYPE 1\nBEGIN\n  BLOCK \"StringFileInfo\"\n  BEGIN\n    BLOCK \"040904B0\"\n    BEGIN\n"
    );
    for (k, val) in strings(v) {
        s.push_str(&format!("      VALUE \"{k}\", \"{val}\"\n"));
    }
    s.push_str(
        "    END\n  END\n  BLOCK \"VarFileInfo\"\n  BEGIN\n    VALUE \"Translation\", 0x409, 1200\n  END\nEND\n",
    );
    s
}

/// A compiled .res file holding just the version resource (RT_VERSION, id 1).
fn version_res(v: [u16; 3]) -> Vec<u8> {
    let [a, b, c] = v;
    let ms = |x: u16, y: u16| (u32::from(x) << 16) | u32::from(y);
    let mut fixed = Vec::new();
    for d in [
        0xFEEF04BD,  // signature
        0x0001_0000, // structure version
        ms(a, b),    // file version
        ms(c, 0),
        ms(a, b), // product version
        ms(c, 0),
        0x3F,        // flags mask
        0,           // flags
        0x0004_0004, // VOS_NT_WINDOWS32
        1,           // VFT_APP
        0,
        0,
        0,
    ] {
        fixed.extend_from_slice(&u32::to_le_bytes(d));
    }
    let strings: Vec<Vec<u8>> = strings(v)
        .iter()
        .map(|(k, val)| node(k, Value::Text(val), &[]))
        .collect();
    let table = node("040904B0", Value::None, &strings);
    let string_info = node("StringFileInfo", Value::None, &[table]);
    let translation = node("Translation", Value::Binary(&[0x09, 0x04, 0xB0, 0x04]), &[]);
    let var_info = node("VarFileInfo", Value::None, &[translation]);
    let info = node(
        "VS_VERSION_INFO",
        Value::Binary(&fixed),
        &[string_info, var_info],
    );

    // A .res file starts with an empty entry, then one header + data per resource.
    let mut res = Vec::new();
    header(&mut res, 0, 0, 0, 0, 0);
    header(&mut res, info.len() as u32, 16, 1, 0x0030, 0x0409);
    res.extend_from_slice(&info);
    pad(&mut res);
    res
}

fn header(res: &mut Vec<u8>, size: u32, kind: u16, name: u16, flags: u16, lang: u16) {
    let mut h = Vec::new();
    h.extend_from_slice(&size.to_le_bytes()); // DataSize
    h.extend_from_slice(&32u32.to_le_bytes()); // HeaderSize
    for w in [0xFFFF, kind, 0xFFFF, name] {
        h.extend_from_slice(&u16::to_le_bytes(w));
    }
    h.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    h.extend_from_slice(&flags.to_le_bytes());
    h.extend_from_slice(&lang.to_le_bytes());
    h.extend_from_slice(&0u32.to_le_bytes()); // Version
    h.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    res.extend_from_slice(&h);
}

enum Value<'a> {
    None,
    Text(&'a str),
    Binary(&'a [u8]),
}

fn pad(b: &mut Vec<u8>) {
    while !b.len().is_multiple_of(4) {
        b.push(0);
    }
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// One block of a version resource: length, value length, type, key, value,
/// children, each part 32-bit aligned.
fn node(key: &str, value: Value, children: &[Vec<u8>]) -> Vec<u8> {
    let (bytes, value_len, text) = match value {
        Value::None => (Vec::new(), 0, true),
        // For text, the length is in characters, terminator included.
        Value::Text(t) => (utf16z(t), t.encode_utf16().count() as u16 + 1, true),
        Value::Binary(b) => (b.to_vec(), b.len() as u16, false),
    };
    let mut b = vec![0, 0];
    b.extend_from_slice(&value_len.to_le_bytes());
    b.extend_from_slice(&u16::to_le_bytes(text as u16));
    b.extend_from_slice(&utf16z(key));
    pad(&mut b);
    b.extend_from_slice(&bytes);
    for child in children {
        pad(&mut b);
        b.extend_from_slice(child);
    }
    let len = b.len() as u16;
    b[..2].copy_from_slice(&len.to_le_bytes());
    b
}
