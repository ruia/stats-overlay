use std::ffi::c_void;
use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{RegGetValueW, HKEY, RRF_RT_REG_DWORD, RRF_RT_REG_SZ};

/// NUL-terminated UTF-16 copy of `s`. Keep the Vec alive while the PCWSTR is in use.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn pcwstr(w: &[u16]) -> PCWSTR {
    PCWSTR(w.as_ptr())
}

pub fn reg_dword(root: HKEY, path: &str, name: &str) -> Option<u32> {
    let (p, n) = (wide(path), wide(name));
    let mut v = 0u32;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(
            root,
            pcwstr(&p),
            pcwstr(&n),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut c_void),
            Some(&mut size),
        )
    };
    (r == ERROR_SUCCESS).then_some(v)
}

pub fn reg_string(root: HKEY, path: &str, name: &str) -> Option<String> {
    let (p, n) = (wide(path), wide(name));
    let mut buf = [0u16; 512];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            root,
            pcwstr(&p),
            pcwstr(&n),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    if r != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

pub const GIB: f64 = (1u64 << 30) as f64;

/// "9.1G", or "123G" from 100 GiB up.
pub fn gib(bytes: u64) -> String {
    let g = bytes as f64 / GIB;
    if g >= 99.95 {
        format!("{g:.0}G")
    } else {
        format!("{g:.1}G")
    }
}
