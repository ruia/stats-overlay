//! Device batteries (Cargo feature `batteries`): Logitech mice and keyboards over
//! HID++, and any Bluetooth device Windows reports a battery level for.
//!
//! Off by default. While it's off no thread runs and no device is opened; turning
//! it on from Display > Device batteries starts a thread that polls once a minute.

mod bluetooth;
mod hidpp;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// Batteries change slowly; HID++ devices answer in a few ms.
const INTERVAL: Duration = Duration::from_secs(60);
/// Every this many polls, look for new receivers, devices and pairings.
const RESCAN_EVERY: u32 = 5;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ChargeState {
    Discharging,
    Charging,
    Full,
    Error,
}

/// How the device reaches the PC.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Link {
    /// A USB receiver (Logitech Unifying, Bolt, Lightspeed).
    Receiver,
    Bluetooth,
    Wired,
}

#[derive(Clone, Debug)]
pub struct Battery {
    /// Name plus where it's connected; what Display > Device batteries hides by.
    pub key: String,
    pub name: String,
    pub percent: u8,
    /// None when the source only gives a level (Windows' Bluetooth property).
    pub charging: Option<ChargeState>,
    /// Plugged in, whether or not it's charging right now.
    pub external: bool,
    pub link: Link,
    /// Didn't answer this time (asleep, switched off): the last value read.
    pub stale: bool,
    /// Bluetooth address, to drop the generic entry for a device read over HID++.
    pub address: Option<u64>,
}

impl Battery {
    /// Show the charging bolt.
    pub fn plugged(&self) -> bool {
        self.external
            || matches!(
                self.charging,
                Some(ChargeState::Charging | ChargeState::Full)
            )
    }

    /// The name's first word, for the strip: "MX", "G515".
    pub fn short_name(&self) -> &str {
        self.name.split_whitespace().next().unwrap_or(&self.name)
    }
}

/// Every device found, hidden ones included, in a stable order.
pub static LATEST: Mutex<Vec<Battery>> = Mutex::new(Vec::new());

static ENABLED: AtomicBool = AtomicBool::new(false);
static RESCAN: AtomicBool = AtomicBool::new(false);
static THREAD: Mutex<Option<std::thread::Thread>> = Mutex::new(None);

/// Starts or stops polling. `notify` gets WM_APP_SAMPLE after each poll.
pub fn enable(on: bool, notify: HWND) {
    let mut thread = THREAD.lock().unwrap();
    ENABLED.store(on, Ordering::Relaxed);
    match (&*thread, on) {
        (Some(t), false) => t.unpark(),
        (None, true) => {
            let notify = notify.0 as usize;
            *thread = std::thread::Builder::new()
                .stack_size(128 * 1024)
                .spawn(move || run(HWND(notify as _)))
                .ok()
                .map(|h| h.thread().clone());
        }
        _ => {}
    }
}

/// A device arrived or left (WM_DEVICECHANGE): look again now.
pub fn rescan() {
    if let Some(t) = THREAD.lock().unwrap().as_ref() {
        RESCAN.store(true, Ordering::Relaxed);
        t.unpark();
    }
}

fn run(notify: HWND) {
    let mut logitech = hidpp::Hidpp::default();
    let mut polls = 0u32;
    loop {
        {
            // Checked under the lock so a quick off-on can't leave no thread running.
            let mut thread = THREAD.lock().unwrap();
            if !ENABLED.load(Ordering::Relaxed) {
                *thread = None;
                LATEST.lock().unwrap().clear();
                break;
            }
        }
        let full = polls.is_multiple_of(RESCAN_EVERY) || RESCAN.swap(false, Ordering::Relaxed);
        polls = polls.wrapping_add(1);
        let mut list = logitech.poll(full);
        // A mouse moved from its receiver to Bluetooth (or a cable) answers on
        // the new link; drop the old link's stale entry rather than list it twice.
        let live: Vec<String> = list
            .iter()
            .filter(|b| !b.stale)
            .map(|b| b.name.clone())
            .collect();
        list.retain(|b| !b.stale || !live.iter().any(|n| same_name(n, &b.name)));
        let read = list.clone();
        // A Logitech device on Bluetooth also has Windows' property: keep the
        // HID++ entry, which knows about charging.
        list.extend(bluetooth::read().into_iter().filter(|b| {
            !read.iter().any(|l| {
                l.address.is_some() && l.address == b.address
                    || l.link == Link::Bluetooth && same_name(&l.name, &b.name)
            })
        }));
        *LATEST.lock().unwrap() = list;
        unsafe {
            let _ = PostMessageW(
                Some(notify),
                crate::sampler::WM_APP_SAMPLE,
                WPARAM(0),
                LPARAM(0),
            );
        }
        std::thread::park_timeout(INTERVAL);
    }
    unsafe {
        let _ = PostMessageW(
            Some(notify),
            crate::sampler::WM_APP_SAMPLE,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

fn same_name(a: &str, b: &str) -> bool {
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    norm(a) == norm(b)
}

/// The 48-bit address in hex at the end of a Bluetooth device path or instance
/// id ("..._d541b5ff70b6"), if `s` ends with one.
fn address(s: &str) -> Option<u64> {
    let tail = s.rsplit(['_', '\\', '&']).next()?;
    (tail.len() == 12)
        .then(|| u64::from_str_radix(tail, 16).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(
            address("{00001812-0000-1000-8000-00805f9b34fb}_dev_vid&02046d_pid&b034_rev&0003_d541b5ff70b6"),
            Some(0xd541b5ff70b6)
        );
        assert_eq!(address(r"BTHENUM\DEV_2CBEEB6EF626"), Some(0x2cbeeb6ef626));
        assert_eq!(address("vid_046d&pid_c548&mi_02"), None);
    }

    #[test]
    fn names() {
        assert!(same_name("MX Master 3S", "mx master 3s"));
        assert!(!same_name("MX Master 3S", "MX Anywhere 3S"));
    }
}
