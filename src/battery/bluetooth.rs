//! Battery levels Windows already knows for Bluetooth devices (the % shown in
//! Settings > Bluetooth & devices): DEVPKEY_Bluetooth_Battery, set on one of the
//! device's nodes by the hands-free profile or the LE Battery Service.
//!
//! The level is all there is: no charging state. The property keeps its last
//! value after a device disconnects, so only connected devices are listed.

use super::{Battery, Link};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDevicePropertyW, DIGCF_ALLCLASSES, DIGCF_PRESENT,
    HDEVINFO, SP_DEVINFO_DATA,
};
use windows::Win32::Devices::Properties::DEVPROPTYPE;
use windows::Win32::Foundation::DEVPROPKEY;

const BATTERY: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x104ea319_6ee2_4701_bd47_8ddbf425bbe5),
    pid: 2,
};
/// DEVPKEY_Device_FriendlyName.
const FRIENDLY_NAME: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};
/// The Bluetooth device node's "connected" flag (what Settings shows).
const CONNECTED: DEVPROPKEY = DEVPROPKEY {
    fmtid: GUID::from_u128(0x83da6326_97a6_4088_9453_a1923f573b29),
    pid: 15,
};

struct Device {
    address: u64,
    /// The address as it appears in instance ids.
    hex: String,
    name: String,
    connected: bool,
}

pub fn read() -> Vec<Battery> {
    let mut devices: Vec<Device> = Vec::new();
    // (instance id, level) of every node that has the property.
    let mut levels: Vec<(String, u8)> = Vec::new();
    unsafe {
        let Ok(set) =
            SetupDiGetClassDevsW(None, PCWSTR::null(), None, DIGCF_ALLCLASSES | DIGCF_PRESENT)
        else {
            return Vec::new();
        };
        for i in 0.. {
            let mut info = SP_DEVINFO_DATA {
                cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInfo(set, i, &mut info).is_err() {
                break;
            }
            let mut buf = [0u16; 512];
            if SetupDiGetDeviceInstanceIdW(set, &info, Some(&mut buf), None).is_err() {
                continue;
            }
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let id = String::from_utf16_lossy(&buf[..len]).to_uppercase();
            if !id.starts_with("BTH") {
                continue;
            }
            // The device itself: BTHENUM\DEV_<address> (classic), BTHLE\DEV_<address>.
            if let Some(rest) = id
                .strip_prefix(r"BTHENUM\DEV_")
                .or_else(|| id.strip_prefix(r"BTHLE\DEV_"))
            {
                let hex: String = rest.chars().take(12).collect();
                if let Ok(address) = u64::from_str_radix(&hex, 16) {
                    devices.push(Device {
                        address,
                        hex,
                        name: string(set, &info, &FRIENDLY_NAME).unwrap_or_default(),
                        connected: bytes(set, &info, &CONNECTED)
                            .is_some_and(|b| b.first() == Some(&0xFF) || b.first() == Some(&1)),
                    });
                }
            }
            if let Some(&level) = bytes(set, &info, &BATTERY).as_ref().and_then(|b| b.first()) {
                levels.push((id, level));
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    let mut out: Vec<Battery> = Vec::new();
    for (id, level) in levels {
        let Some(d) = devices.iter().find(|d| id.contains(&d.hex)) else {
            continue;
        };
        if !d.connected || d.name.is_empty() || out.iter().any(|b| b.address == Some(d.address)) {
            continue;
        }
        out.push(Battery {
            key: format!("{}@BT:{:012X}", d.name, d.address),
            name: d.name.clone(),
            percent: level.min(100),
            charging: None,
            external: false,
            link: Link::Bluetooth,
            stale: false,
            address: Some(d.address),
        });
    }
    out
}

unsafe fn bytes(set: HDEVINFO, info: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<Vec<u8>> {
    let mut kind = DEVPROPTYPE::default();
    let mut buf = vec![0u8; 512];
    let mut size = 0u32;
    SetupDiGetDevicePropertyW(
        set,
        info,
        key,
        &mut kind,
        Some(&mut buf),
        Some(&mut size),
        0,
    )
    .ok()?;
    buf.truncate(size as usize);
    Some(buf)
}

unsafe fn string(set: HDEVINFO, info: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<String> {
    let b = bytes(set, info, key)?;
    let w: Vec<u16> = b
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&c| c != 0)
        .collect();
    Some(String::from_utf16_lossy(&w))
}
