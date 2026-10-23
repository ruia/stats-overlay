//! Logitech HID++ 2.0: battery level and charging state of devices behind a
//! receiver (Unifying, Bolt, Lightspeed: indexes 1-6), on Bluetooth or on a cable
//! (index 0xFF).
//!
//! Only read queries are sent: IRoot ping and getFeature, DEVICE_NAME and the
//! battery feature's status. Requests carry software id 0xA; replies meant for
//! G HUB or Logi Options+, which share the device, are ignored.
//!
//! Devices that only have BATTERY_VOLTAGE (0x1001, older G mice) aren't shown
//! yet: turning millivolts into % needs a per-device discharge curve.

use super::{Battery, ChargeState, Link};
use crate::util::wide;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetHidGuid, HidD_GetPreparsedData,
    HidP_GetCaps, HIDD_ATTRIBUTES, HIDP_CAPS, PHIDP_PREPARSED_DATA,
};
use windows::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_OVERLAPPED,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

const SOFTWARE_ID: u8 = 0xA;
const TIMEOUT_MS: u64 = 1000;
const SHORT: u8 = 0x10;
const LONG: u8 = 0x11;

const ROOT: u8 = 0x00;
/// HID++ 1.0 device connection notification, sent by receivers.
const CONNECTION: u8 = 0x41;
const SET_REGISTER: u8 = 0x80;
const GET_REGISTER: u8 = 0x81;
const DEVICE_NAME: u16 = 0x0005;
const BATTERY_STATUS: u16 = 0x1000;
const UNIFIED_BATTERY: u16 = 0x1004;

/// Why a request got no answer.
#[derive(Debug, PartialEq)]
enum Fail {
    /// HID++ error code. From a receiver, 0x08 (unknown device) is what an
    /// asleep or switched-off device looks like.
    Error(u8),
    Timeout,
    Io,
}

/// One device's HID++ interface: a long-report collection and, on receivers and
/// cables, the short one HID++ 1.0 errors come back on.
#[derive(Clone)]
struct Port {
    /// The device path minus its collection part, shared by both collections.
    id: String,
    long: String,
    short: Option<String>,
    pid: u16,
    bluetooth: bool,
    address: Option<u64>,
}

/// What a device answered once; kept so a poll is one or two requests.
struct Known {
    port: String,
    index: u8,
    name: String,
    /// (feature id, its index on this device); None: no battery feature we read.
    battery: Option<(u16, u8)>,
    last: Option<Battery>,
    /// Look the name and features up again at the next answer.
    recheck: bool,
}

/// What `Hidpp::wait` returns on.
pub enum Wake {
    /// The time is up, or `stop` was signalled.
    Timeout,
    /// A device sent new battery figures or lost its link: show `batteries()`.
    Changed,
    /// A device connected to a receiver: poll now.
    Poll,
}

#[derive(Default)]
pub struct Hidpp {
    ports: Vec<Port>,
    /// Port id → it's a receiver (else a device at 0xFF).
    receivers: Vec<(String, bool)>,
    known: Vec<Known>,
    /// Open collections, kept between polls so the reads stay pending and
    /// notifications come in as they're sent.
    open: Vec<(String, Io)>,
}

impl Hidpp {
    pub fn batteries(&self) -> Vec<Battery> {
        self.known.iter().filter_map(|k| k.last.clone()).collect()
    }

    /// Reads every device; with `full`, first looks for receivers and devices
    /// that came or went. Receiver devices that don't answer keep their last
    /// reading, marked stale; a device whose own link (Bluetooth, cable) went
    /// away is dropped.
    pub fn poll(&mut self, full: bool) -> Vec<Battery> {
        if full {
            self.ports = ports();
            let ids: Vec<String> = self.ports.iter().map(|p| p.id.clone()).collect();
            self.known.retain(|k| ids.contains(&k.port));
            self.open.retain(|(id, _)| ids.contains(id));
            self.receivers.retain(|(id, _)| ids.contains(id));
        }
        for port in self.ports.clone() {
            let mut io = match self.open.iter().position(|(id, _)| *id == port.id) {
                Some(i) => self.open.swap_remove(i).1,
                None => match Io::open(&port) {
                    Some(io) => io,
                    None => continue,
                },
            };
            let receiver = match self.receivers.iter().find(|(id, _)| *id == port.id) {
                Some(&(_, r)) => r,
                None => {
                    // A receiver answers a HID++ 2.0 ping at 0xFF with a HID++ 1.0
                    // error; a device on a cable or Bluetooth answers it.
                    let r = !port.bluetooth && io.request(0xFF, ROOT, 1, &[]).is_err();
                    if r {
                        io.wireless_notifications();
                    }
                    self.receivers.push((port.id.clone(), r));
                    r
                }
            };
            // Empty slots answer at once with an error, so every slot is asked:
            // a device switched back to this receiver shows up at the next poll.
            let indexes: Vec<u8> = if receiver {
                (1..=6).collect()
            } else {
                vec![0xFF]
            };
            for index in indexes {
                self.read(&mut io, &port, receiver, index);
            }
            self.open.push((port.id.clone(), io));
        }
        self.batteries()
    }

    /// Waits up to `timeout` for a notification from any open collection, or for
    /// `stop`. Replies to G HUB's and Options+' requests come in too and are
    /// skipped.
    pub fn wait(&mut self, timeout: Duration, stop: HANDLE) -> Wake {
        let deadline = Instant::now() + timeout;
        loop {
            // Notifications that came in while we were waiting for a reply.
            for i in 0..self.open.len() {
                for r in std::mem::take(&mut self.open[i].1.queued) {
                    if let Some(w) = self.notice(i, &r) {
                        return w;
                    }
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Wake::Timeout;
            }
            let mut handles = vec![stop];
            let mut from = Vec::new();
            for (i, (_, io)) in self.open.iter().enumerate() {
                for (short, r) in [(false, Some(&io.long)), (true, io.short.as_ref())] {
                    if let Some(r) = r.filter(|r| r.pending) {
                        if handles.len() < 64 {
                            handles.push(r.ov.hEvent);
                            from.push((i, short));
                        }
                    }
                }
            }
            let ms = left.as_millis().clamp(1, u32::MAX as u128) as u32;
            let w = unsafe { WaitForMultipleObjects(&handles, false, ms) };
            if w == WAIT_TIMEOUT {
                return Wake::Timeout;
            }
            let n = w.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
            if n == 0 {
                return Wake::Timeout;
            }
            let Some(&(i, short)) = from.get(n - 1) else {
                // The wait itself failed; don't spin.
                std::thread::sleep(left);
                return Wake::Timeout;
            };
            let io = &mut self.open[i].1;
            let report = if short {
                io.short.as_mut().and_then(Reader::take)
            } else {
                io.long.take()
            };
            if let Some(w) = report.and_then(|r| self.notice(i, &r)) {
                return w;
            }
        }
    }

    /// A report nobody asked for, from open collection `i`.
    fn notice(&mut self, i: usize, r: &[u8]) -> Option<Wake> {
        if r.len() < 7 {
            return None;
        }
        let port = &self.open[i].0;
        let index = r[1];
        let known = self
            .known
            .iter_mut()
            .find(|k| k.port == *port && k.index == index);
        // HID++ 1.0 device connection, from a receiver; bit 6 of the flags set
        // means the link was lost (asleep, switched off or to another host).
        if r[2] == CONNECTION {
            if r[4] & 0x40 != 0 {
                let last = known?.last.as_mut()?;
                return (!last.stale).then(|| {
                    last.stale = true;
                    Wake::Changed
                });
            }
            // Maybe a different device in that slot: look it up again.
            if let Some(k) = known {
                k.recheck = true;
            }
            return Some(Wake::Poll);
        }
        // A battery event: the battery feature's event 0, software id 0. Both
        // features' events carry the same bytes as their get_status reply.
        let k = known?;
        let (feature, fi) = k.battery?;
        if r[0] != LONG || r[2] != fi || r[3] != 0 {
            return None;
        }
        let (percent, charging, external) = if feature == UNIFIED_BATTERY {
            unified(&r[4..])
        } else {
            status(&r[4..])
        };
        let last = k.last.as_mut()?;
        (last.percent, last.charging, last.external, last.stale) =
            (percent, Some(charging), external, false);
        Some(Wake::Changed)
    }
    fn read(&mut self, io: &mut Io, port: &Port, receiver: bool, index: u8) {
        let pos = self
            .known
            .iter()
            .position(|k| k.port == port.id && k.index == index);
        let online = io.request(index, ROOT, 1, &[]).is_ok();
        if !online {
            if let Some(last) = pos.and_then(|i| self.known[i].last.as_mut()) {
                last.stale = true;
            }
            return;
        }
        let pos = match pos {
            Some(i) if self.known[i].recheck => {
                let last = self.known[i].last.take();
                self.known[i] = Known {
                    last,
                    ..identify(io, port, index)
                };
                i
            }
            Some(i) => i,
            None => {
                self.known.push(identify(io, port, index));
                self.known.len() - 1
            }
        };
        let k = &mut self.known[pos];
        let Some((feature, fi)) = k.battery else {
            return;
        };
        let Ok(r) = io.request(
            index,
            fi,
            if feature == UNIFIED_BATTERY { 1 } else { 0 },
            &[],
        ) else {
            // Firmware updated or re-paired: look the features up again next
            // time it answers; show the last reading until then.
            if let Some(last) = k.last.as_mut() {
                last.stale = true;
            }
            k.recheck = true;
            return;
        };
        let (percent, charging, external) = if feature == UNIFIED_BATTERY {
            unified(&r)
        } else {
            status(&r)
        };
        let link = match (port.bluetooth, receiver) {
            (true, _) => Link::Bluetooth,
            (false, true) => Link::Receiver,
            (false, false) => Link::Wired,
        };
        k.last = Some(Battery {
            key: format!("{}@{:04X}:{}", k.name, port.pid, index),
            name: k.name.clone(),
            percent,
            charging: Some(charging),
            external,
            link,
            stale: false,
            address: port.address,
        });
    }
}

/// Name and battery feature of a device that just answered.
fn identify(io: &mut Io, port: &Port, index: u8) -> Known {
    let feature = |io: &mut Io, id: u16| -> Option<u8> {
        let r = io.request(index, ROOT, 0, &id.to_be_bytes()).ok()?;
        (r[0] != 0).then_some(r[0])
    };
    let name = feature(io, DEVICE_NAME)
        .and_then(|fi| device_name(io, index, fi))
        .unwrap_or_else(|| format!("Logitech {:04X}", port.pid));
    let battery = [UNIFIED_BATTERY, BATTERY_STATUS]
        .into_iter()
        .find_map(|id| feature(io, id).map(|fi| (id, fi)));
    Known {
        port: port.id.clone(),
        index,
        name,
        battery,
        last: None,
        recheck: false,
    }
}

/// DEVICE_NAME: getCount, then getDeviceName from each offset.
fn device_name(io: &mut Io, index: u8, fi: u8) -> Option<String> {
    let len = io.request(index, fi, 0, &[]).ok()?[0] as usize;
    let mut name = Vec::with_capacity(len);
    while name.len() < len {
        let r = io.request(index, fi, 1, &[name.len() as u8]).ok()?;
        let take = (len - name.len()).min(r.len());
        if take == 0 {
            break;
        }
        name.extend_from_slice(&r[..take]);
    }
    let s = String::from_utf8_lossy(&name);
    let s = s.trim_end_matches('\0').trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// UNIFIED_BATTERY get_status: state of charge %, level flags, charging status,
/// external power.
fn unified(r: &[u8]) -> (u8, ChargeState, bool) {
    let charging = match r[2] {
        0 => ChargeState::Discharging,
        1 | 2 => ChargeState::Charging,
        3 => ChargeState::Full,
        _ => ChargeState::Error,
    };
    (r[0].min(100), charging, r[3] != 0)
}

/// BATTERY_STATUS get_status: discharge level %, next level %, status.
fn status(r: &[u8]) -> (u8, ChargeState, bool) {
    let charging = match r[2] {
        0 => ChargeState::Discharging,
        1 | 2 | 4 => ChargeState::Charging,
        3 => ChargeState::Full,
        _ => ChargeState::Error,
    };
    (r[0].min(100), charging, r[2] != 0 && r[2] < 5)
}

/// Logitech HID++ collections, paired up per device.
fn ports() -> Vec<Port> {
    let mut ports: Vec<Port> = Vec::new();
    for (path, caps, pid) in collections() {
        let vendor = caps.UsagePage == 0xFF00 || caps.UsagePage == 0xFF43;
        let long = caps.InputReportByteLength == 20;
        if !vendor || !(long || caps.InputReportByteLength == 7) {
            continue;
        }
        let id = port_id(&path);
        let i = match ports.iter().position(|p| p.id == id) {
            Some(i) => i,
            None => {
                let first = id.split('#').nth(1).unwrap_or_default();
                ports.push(Port {
                    bluetooth: first.contains("vid&02046d") || first.contains("vid&0002046d"),
                    address: super::address(first),
                    id,
                    long: String::new(),
                    short: None,
                    pid,
                });
                ports.len() - 1
            }
        };
        if long {
            ports[i].long = path;
        } else {
            ports[i].short = Some(path);
        }
    }
    ports.retain(|p| !p.long.is_empty());
    ports
}

/// `\\?\hid#vid_046d&pid_c548&mi_02&col02#7&2335452a&0&0001#{guid}` →
/// `hid#vid_046d&pid_c548&mi_02#7&2335452a&0`: the same for each collection.
fn port_id(path: &str) -> String {
    let parts: Vec<&str> = path.split('#').collect();
    let (Some(hw), Some(inst)) = (parts.get(1), parts.get(2)) else {
        return path.to_string();
    };
    let hw = match hw.find("&col") {
        Some(i) => &hw[..i],
        None => hw,
    };
    let inst = inst.rsplit_once('&').map_or(*inst, |(a, _)| a);
    format!("hid#{hw}#{inst}")
}

/// Present HID collections of vendor 046D: (path, caps, product id).
fn collections() -> Vec<(String, HIDP_CAPS, u16)> {
    let mut out = Vec::new();
    unsafe {
        let guid = HidD_GetHidGuid();
        let Ok(set) = SetupDiGetClassDevsW(
            Some(&guid),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        ) else {
            return out;
        };
        for i in 0.. {
            let mut iface = SP_DEVICE_INTERFACE_DATA {
                cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInterfaces(set, None, &guid, i, &mut iface).is_err() {
                break;
            }
            // Room for the struct header and a long path, u32-aligned.
            let mut buf = vec![0u32; 1024];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(
                set,
                &iface,
                Some(detail),
                (buf.len() * 4) as u32,
                None,
                None,
            )
            .is_err()
            {
                continue;
            }
            let p = std::ptr::addr_of!((*detail).DevicePath) as *const u16;
            let len = (0..).take_while(|&n| *p.add(n) != 0).count();
            let path = String::from_utf16_lossy(std::slice::from_raw_parts(p, len)).to_lowercase();
            if !(path.contains("vid_046d")
                || path.contains("vid&02046d")
                || path.contains("vid&0002046d"))
            {
                continue;
            }
            // No access rights needed to read the caps, and nothing is sent.
            let w = wide(&path);
            let Ok(h) = CreateFileW(
                PCWSTR(w.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            ) else {
                continue;
            };
            let mut attr = HIDD_ATTRIBUTES {
                Size: size_of::<HIDD_ATTRIBUTES>() as u32,
                ..Default::default()
            };
            let mut pd = PHIDP_PREPARSED_DATA::default();
            if HidD_GetAttributes(h, &mut attr) && HidD_GetPreparsedData(h, &mut pd) {
                let mut caps = HIDP_CAPS::default();
                if HidP_GetCaps(pd, &mut caps).is_ok() {
                    out.push((path, caps, attr.ProductID));
                }
                HidD_FreePreparsedData(pd);
            }
            let _ = CloseHandle(h);
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    out
}

/// An overlapped read kept outstanding on one collection.
struct Reader {
    file: HANDLE,
    ov: Box<OVERLAPPED>,
    buf: Box<[u8; 64]>,
    len: usize,
    pending: bool,
}

impl Reader {
    fn open(path: &str, len: usize) -> Option<Reader> {
        let w = wide(path);
        unsafe {
            let file = CreateFileW(
                PCWSTR(w.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
            .ok()?;
            let Ok(event) = CreateEventW(None, true, false, PCWSTR::null()) else {
                let _ = CloseHandle(file);
                return None;
            };
            let mut ov = Box::new(OVERLAPPED::default());
            ov.hEvent = event;
            Some(Reader {
                file,
                ov,
                buf: Box::new([0; 64]),
                len,
                pending: false,
            })
        }
    }

    fn start(&mut self) {
        if self.pending {
            return;
        }
        unsafe {
            let _ = windows::Win32::System::Threading::ResetEvent(self.ov.hEvent);
            let r = ReadFile(
                self.file,
                Some(&mut self.buf[..self.len]),
                None,
                Some(&mut *self.ov),
            );
            self.pending = r.is_ok()
                || r.is_err_and(|e| {
                    e.code() == windows::Win32::Foundation::ERROR_IO_PENDING.to_hresult()
                });
        }
    }

    /// The report that completed (its event is signalled), and starts the next read.
    fn take(&mut self) -> Option<Vec<u8>> {
        let mut n = 0u32;
        let ok = unsafe { GetOverlappedResult(self.file, &*self.ov, &mut n, false).is_ok() };
        self.pending = false;
        let report = ok.then(|| self.buf[..n as usize].to_vec());
        self.start();
        report
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        unsafe {
            if self.pending {
                let _ = CancelIoEx(self.file, Some(&*self.ov));
                let mut n = 0u32;
                let _ = GetOverlappedResult(self.file, &*self.ov, &mut n, true);
            }
            let _ = CloseHandle(self.ov.hEvent);
            let _ = CloseHandle(self.file);
        }
    }
}

struct Io {
    long: Reader,
    short: Option<Reader>,
    /// Notifications (software id 0, or a receiver's 0x41) read while waiting
    /// for a reply, for `Hidpp::wait`.
    queued: Vec<Vec<u8>>,
}

impl Io {
    fn open(port: &Port) -> Option<Io> {
        let mut long = Reader::open(&port.long, 20)?;
        let mut short = port.short.as_deref().and_then(|p| Reader::open(p, 7));
        long.start();
        if let Some(s) = short.as_mut() {
            s.start();
        }
        Some(Io {
            long,
            short,
            queued: Vec::new(),
        })
    }

    /// Sends feature `feature`, function `function` to device `index` as a long
    /// report and waits for its answer: the reply's 16 parameter bytes.
    fn request(
        &mut self,
        index: u8,
        feature: u8,
        function: u8,
        params: &[u8],
    ) -> Result<[u8; 16], Fail> {
        let mut out = [0u8; 20];
        let fn_sw = (function << 4) | SOFTWARE_ID;
        out[..4].copy_from_slice(&[LONG, index, feature, fn_sw]);
        out[4..4 + params.len()].copy_from_slice(params);
        self.write(&out)?;
        let deadline = Instant::now() + Duration::from_millis(TIMEOUT_MS);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Fail::Timeout);
            }
            let mut events = vec![self.long.ov.hEvent];
            if let Some(s) = &self.short {
                events.push(s.ov.hEvent);
            }
            let w = unsafe { WaitForMultipleObjects(&events, false, left.as_millis() as u32) };
            let which = w.0.wrapping_sub(WAIT_OBJECT_0.0) as usize;
            let report = match which {
                0 => self.long.take(),
                1 => self.short.as_mut().and_then(Reader::take),
                _ => return Err(Fail::Timeout),
            };
            let Some(r) = report.filter(|r| r.len() >= 7) else {
                continue;
            };
            if r[2] == CONNECTION || r[2] < 0x80 && r[3] & 0x0F == 0 {
                if self.queued.len() < 32 {
                    self.queued.push(r.clone());
                }
                continue;
            }
            if r[1] != index {
                continue;
            }
            // HID++ 2.0 error (0xFF) or HID++ 1.0 error from a receiver (0x8F).
            if (r[2] == 0xFF || r[2] == 0x8F) && r[3] == feature && r[4] == fn_sw {
                return Err(Fail::Error(r[5]));
            }
            if r[0] == LONG && r[2] == feature && r[3] == fn_sw {
                let mut p = [0u8; 16];
                p.copy_from_slice(&r[4..20]);
                return Ok(p);
            }
            if r[0] == SHORT && r[2] == feature && r[3] == fn_sw {
                let mut p = [0u8; 16];
                p[..3].copy_from_slice(&r[4..7]);
                return Ok(p);
            }
        }
    }

    /// HID++ 1.0 register 0x00 of a receiver holds its notification flags. Turns
    /// on wireless notifications (device connected / link lost) if they're off,
    /// keeping the other bits. G HUB and Options+ normally have them on already.
    fn wireless_notifications(&mut self) {
        let Some(short) = self.short.as_ref().map(|s| s.file) else {
            return;
        };
        let get = [SHORT, 0xFF, GET_REGISTER, 0x00, 0, 0, 0];
        if write(short, &get).is_err() {
            return;
        }
        let Some(flags) = self.register_reply(GET_REGISTER) else {
            return;
        };
        if flags[1] & 0x01 == 0 {
            let set = [
                SHORT,
                0xFF,
                SET_REGISTER,
                0x00,
                flags[0],
                flags[1] | 0x01,
                flags[2],
            ];
            if write(short, &set).is_ok() {
                let _ = self.register_reply(SET_REGISTER);
            }
        }
    }

    /// The receiver's answer to a register request on register 0x00.
    fn register_reply(&mut self, sub: u8) -> Option<[u8; 3]> {
        let deadline = Instant::now() + Duration::from_millis(TIMEOUT_MS);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let short = self.short.as_mut()?;
            let w = unsafe {
                WaitForMultipleObjects(&[short.ov.hEvent], false, left.as_millis() as u32)
            };
            if left.is_zero() || w != WAIT_OBJECT_0 {
                return None;
            }
            let Some(r) = short.take().filter(|r| r.len() >= 7 && r[1] == 0xFF) else {
                continue;
            };
            if r[2] == sub && r[3] == 0x00 {
                return Some([r[4], r[5], r[6]]);
            }
            if r[2] == 0x8F && r[3] == sub {
                return None;
            }
        }
    }

    fn write(&mut self, report: &[u8; 20]) -> Result<(), Fail> {
        write(self.long.file, report)
    }
}

/// An overlapped write to one collection, waited for.
fn write(file: HANDLE, report: &[u8]) -> Result<(), Fail> {
    unsafe {
        let event = CreateEventW(None, true, false, PCWSTR::null()).map_err(|_| Fail::Io)?;
        let mut ov = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        let started = match WriteFile(file, Some(report), None, Some(&mut ov)) {
            Ok(()) => true,
            Err(e) => e.code() == windows::Win32::Foundation::ERROR_IO_PENDING.to_hresult(),
        };
        let mut n = 0u32;
        let ok = if started {
            let done =
                windows::Win32::System::Threading::WaitForSingleObject(event, TIMEOUT_MS as u32)
                    == WAIT_OBJECT_0;
            if !done {
                let _ = CancelIoEx(file, Some(&ov));
            }
            GetOverlappedResult(file, &ov, &mut n, true).is_ok() && done
        } else {
            false
        };
        let _ = CloseHandle(event);
        ok.then_some(()).ok_or(Fail::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        let a = r"\\?\hid#vid_046d&pid_c548&mi_02&col01#7&2335452a&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        let b = r"\\?\hid#vid_046d&pid_c548&mi_02&col02#7&2335452a&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        assert_eq!(port_id(a), port_id(b));
        assert_eq!(port_id(a), "hid#vid_046d&pid_c548&mi_02#7&2335452a&0");
    }

    #[test]
    fn decode() {
        // Probed: MX Master 3S 20 % discharging, G515 86 %.
        assert_eq!(
            unified(&[0x14, 0x02, 0, 0]),
            (20, ChargeState::Discharging, false)
        );
        assert_eq!(
            unified(&[0x56, 0x08, 1, 1]),
            (86, ChargeState::Charging, true)
        );
        assert_eq!(status(&[50, 30, 3]), (50, ChargeState::Full, true));
        assert_eq!(status(&[50, 30, 0]).1, ChargeState::Discharging);
    }
}
