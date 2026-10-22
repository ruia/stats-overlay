//! "Start with Windows": a value under HKCU\...\Run.

use crate::util::{pcwstr, wide};
use std::ffi::c_void;
use windows::core::w;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};

const RUN: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const NAME: windows::core::PCWSTR = w!("StatsOverlay");

pub fn enabled() -> bool {
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN,
            NAME,
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        ) == ERROR_SUCCESS
    }
}

/// Registers the exe as it is now; toggling again after moving it updates the path.
pub fn set(on: bool) -> bool {
    unsafe {
        if !on {
            return RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN, NAME) == ERROR_SUCCESS;
        }
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        let cmd = wide(&format!("\"{}\"", exe.display()));
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN,
            NAME,
            REG_SZ.0,
            Some(pcwstr(&cmd).0 as *const c_void),
            (cmd.len() * 2) as u32,
        ) == ERROR_SUCCESS
    }
}
