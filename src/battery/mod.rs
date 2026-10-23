//! Device batteries (Cargo feature `batteries`): Logitech mice and keyboards over
//! HID++, and any Bluetooth device Windows reports a battery level for.
//!
//! Off by default. While it's off no thread runs and no device is opened; turning
//! it on from Display > Device batteries starts a thread that reads every device
//! once a minute and, in between, picks up Logitech devices' battery and
//! connection notifications as they're sent.

mod bluetooth;
mod hidpp;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

/// A full read. Changes in between come as notifications (Logitech) or with a
/// device change (Bluetooth).
const INTERVAL: Duration = Duration::from_secs(60);
const CONNECT_RETRY: Duration = Duration::from_secs(3);
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
static RUNNING: Mutex<bool> = Mutex::new(false);
/// Auto-reset event that wakes the thread: turned off, or a device change.
static WAKE: OnceLock<isize> = OnceLock::new();

fn wake_event() -> HANDLE {
    HANDLE(*WAKE.get_or_init(|| unsafe {
        CreateEventW(None, false, false, PCWSTR::null()).map_or(0, |h| h.0 as isize)
    }) as _)
}

/// Starts or stops polling. `notify` gets WM_APP_SAMPLE after each change.
pub fn enable(on: bool, notify: HWND) {
    let mut running = RUNNING.lock().unwrap();
    ENABLED.store(on, Ordering::Relaxed);
    if *running {
        if !on {
            unsafe {
                let _ = SetEvent(wake_event());
            }
        }
    } else if on {
        let notify = notify.0 as usize;
        *running = std::thread::Builder::new()
            .stack_size(128 * 1024)
            .spawn(move || run(HWND(notify as _)))
            .is_ok();
    }
}

/// A device arrived or left (WM_DEVICECHANGE): look again now.
pub fn rescan() {
    if *RUNNING.lock().unwrap() {
        RESCAN.store(true, Ordering::Relaxed);
        unsafe {
            let _ = SetEvent(wake_event());
        }
    }
}

fn run(notify: HWND) {
    let wake = wake_event();
    let mut logitech = hidpp::Hidpp::default();
    let mut polls = 0u32;
    let mut next = INTERVAL;
    loop {
        {
            // Checked under the lock so a quick off-on can't leave no thread running.
            let mut running = RUNNING.lock().unwrap();
            if !ENABLED.load(Ordering::Relaxed) {
                *running = false;
                LATEST.lock().unwrap().clear();
                break;
            }
        }
        let full = polls.is_multiple_of(RESCAN_EVERY) || RESCAN.swap(false, Ordering::Relaxed);
        polls = polls.wrapping_add(1);
        let list = logitech.poll(full);
        let bt = bluetooth::read();
        publish(list, &bt, notify);
        // Between polls, battery events and receivers' connection notices
        // update the list as they come in.
        let until = Instant::now() + next;
        next = INTERVAL;
        loop {
            match logitech.wait(until.saturating_duration_since(Instant::now()), wake) {
                hidpp::Wake::Changed => publish(logitech.batteries(), &bt, notify),
                hidpp::Wake::Poll => {
                    // A device that just connected may take a moment to answer:
                    // poll again soon.
                    next = CONNECT_RETRY;
                    break;
                }
                hidpp::Wake::Timeout => break,
            }
        }
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

/// Merges the sources into LATEST and tells the UI.
fn publish(mut list: Vec<Battery>, bt: &[Battery], notify: HWND) {
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
    list.extend(
        bt.iter()
            .filter(|b| {
                !read.iter().any(|l| {
                    l.address.is_some() && l.address == b.address
                        || l.link == Link::Bluetooth && same_name(&l.name, &b.name)
                })
            })
            .cloned(),
    );
    *LATEST.lock().unwrap() = list;
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
