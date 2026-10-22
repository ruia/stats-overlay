//! StatsOverlay v0: proves taskbar placement with a two-line RAM readout.
//!
//! Windows:
//! - host: hidden, unowned top-level window. It receives the broadcasts a child
//!   window never sees (TaskbarCreated, WM_SETTINGCHANGE) and the sampler's pokes.
//! - overlay: layered child of Shell_TrayWnd, or a topmost tool window when
//!   embedding fails (or with `--popup`).
//!
//! A child window in Explorer's taskbar shares its input queue with Explorer, so a
//! stall on this thread can stall the taskbar. All sampling happens on the sampler
//! thread; this thread only lays out text and blits.

#![windows_subsystem = "windows"]

mod diag;
mod procs;
mod render;
mod sampler;
mod taskbar;
mod theme;
mod util;
mod wsl;

use render::{Renderer, Seg};
use sampler::{Snapshot, WslState, WM_APP_SAMPLE};
use std::cell::{Cell, OnceCell, RefCell};
use util::{gib, pcwstr, wide};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, ShellExecuteW, QUNS_BUSY, QUNS_PRESENTATION_MODE,
    QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const ID_TASKMGR: usize = 1;
const ID_EXIT: usize = 2;
const WM_DPICHANGED_AFTERPARENT: u32 = 0x02E3;

struct Opts {
    popup: bool,
    offset: Option<i32>,
}

struct App {
    host: Cell<HWND>,
    overlay: Cell<Option<HWND>>,
    /// The taskbar the overlay is a child of; None in popup mode.
    parent: Cell<Option<HWND>>,
    light: Cell<bool>,
    msg_taskbar_created: u32,
    opts: Opts,
    renderer: RefCell<Option<Renderer>>,
}

thread_local! {
    static APP: OnceCell<&'static App> = const { OnceCell::new() };
}

fn app() -> &'static App {
    APP.with(|a| *a.get().expect("app initialised"))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--diag") {
        diag::run();
        return;
    }
    let offset = args
        .iter()
        .position(|a| a == "--offset")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok());
    let opts = Opts {
        popup: args.iter().any(|a| a == "--popup"),
        offset,
    };

    unsafe {
        // Single instance. The handle is intentionally kept for the process lifetime.
        let _mutex = CreateMutexW(None, false, w!("Local\\StatsOverlay.SingleInstance"));
        if windows::Win32::Foundation::GetLastError()
            == windows::Win32::Foundation::ERROR_ALREADY_EXISTS
        {
            return;
        }
        // The manifest already asks for this; this covers builds without it.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        let instance = GetModuleHandleW(None).unwrap_or_default();
        let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
        for (name, proc) in [
            (
                w!("StatsOverlayHost"),
                Some(host_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT),
            ),
            (w!("StatsOverlay"), Some(overlay_proc as _)),
        ] {
            let wc = WNDCLASSW {
                lpfnWndProc: proc,
                hInstance: instance.into(),
                lpszClassName: name,
                hCursor: cursor,
                ..Default::default()
            };
            RegisterClassW(&wc);
        }

        let app: &'static App = Box::leak(Box::new(App {
            host: Cell::new(HWND::default()),
            overlay: Cell::new(None),
            parent: Cell::new(None),
            light: Cell::new(theme::system_light()),
            msg_taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            opts,
            renderer: RefCell::new(Renderer::new().ok()),
        }));
        APP.with(|a| a.set(app).ok());

        // Unowned and not message-only, so it gets HWND_BROADCAST messages.
        let Ok(host) = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("StatsOverlayHost"),
            w!("StatsOverlay"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            return;
        };
        app.host.set(host);
        sampler::spawn(host);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

extern "system" fn host_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let app = app();
    match msg {
        WM_APP_SAMPLE => refresh(),
        WM_SETTINGCHANGE | WM_DISPLAYCHANGE | WM_DPICHANGED => {
            app.light.set(theme::system_light());
            refresh();
        }
        WM_COMMAND => match wp.0 & 0xffff {
            ID_TASKMGR => open_task_manager(),
            ID_EXIT => unsafe {
                destroy_overlay();
                let _ = DestroyWindow(hwnd);
            },
            _ => {}
        },
        WM_DESTROY => unsafe { PostQuitMessage(0) },
        m if m == app.msg_taskbar_created && m != 0 => {
            // Explorer restarted: the old taskbar (and our child) are gone or stale.
            destroy_overlay();
            refresh();
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}

extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONUP => open_task_manager(),
        WM_RBUTTONUP => show_menu(),
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_DPICHANGED | WM_DPICHANGED_AFTERPARENT => unsafe {
            let _ = PostMessageW(Some(app().host.get()), WM_APP_SAMPLE, WPARAM(0), LPARAM(0));
        },
        WM_DESTROY => {
            let app = app();
            if app.overlay.get() == Some(hwnd) {
                app.overlay.set(None);
                app.parent.set(None);
            }
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}

fn destroy_overlay() {
    if let Some(h) = app().overlay.take() {
        app().parent.set(None);
        unsafe {
            let _ = DestroyWindow(h);
        }
    }
}

/// Makes sure the overlay exists and is attached to the current taskbar.
fn ensure_overlay(tb: HWND) -> Option<HWND> {
    let app = app();
    if let Some(h) = app.overlay.get() {
        let alive = unsafe { IsWindow(Some(h)).as_bool() };
        let attached = match app.parent.get() {
            None => true,
            Some(p) => p == tb && unsafe { GetParent(h) }.ok() == Some(tb),
        };
        if alive && attached {
            return Some(h);
        }
        destroy_overlay();
    }
    let instance = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let create = |ex: WINDOW_EX_STYLE, style: WINDOW_STYLE, parent: Option<HWND>| unsafe {
        CreateWindowExW(
            ex,
            w!("StatsOverlay"),
            w!("StatsOverlay"),
            style,
            0,
            0,
            1,
            1,
            parent,
            None,
            Some(instance.into()),
            None,
        )
        .ok()
    };
    if !app.opts.popup {
        // Created directly as a child of the taskbar (same effect as SetParent).
        if let Some(h) = create(
            WS_EX_LAYERED | WS_EX_NOPARENTNOTIFY,
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
            Some(tb),
        ) {
            app.overlay.set(Some(h));
            app.parent.set(Some(tb));
            return Some(h);
        }
    }
    let h = create(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        WS_POPUP | WS_VISIBLE,
        None,
    )?;
    app.overlay.set(Some(h));
    Some(h)
}

fn refresh() {
    let Some(snap) = *sampler::LATEST.lock().unwrap() else {
        return;
    };
    let app = app();
    let Some(tb) = taskbar::find() else {
        // Explorer is down. TaskbarCreated will bring us back.
        if let Some(h) = app.overlay.get() {
            unsafe {
                let _ = ShowWindow(h, SW_HIDE);
            }
        }
        return;
    };
    let Some(hwnd) = ensure_overlay(tb) else {
        return;
    };

    let scale = taskbar::dpi(tb) as f32 / 96.0;
    let height = taskbar::rect(tb).bottom - taskbar::rect(tb).top;
    let lines = build_lines(&snap, &theme::palette(app.light.get()));
    let mut renderer = app.renderer.borrow_mut();
    let Some(r) = renderer.as_mut() else { return };
    let Ok(width) = r.prepare(&lines, scale, height) else {
        return;
    };
    let p = taskbar::place(tb, width, app.opts.offset);

    unsafe {
        if app.parent.get().is_some() {
            // Child coordinates are relative to the taskbar's client area.
            let mut pt = POINT { x: p.x, y: p.y };
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(tb, &mut pt);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                pt.x,
                pt.y,
                width,
                p.height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        } else {
            let fullscreen = matches!(
                SHQueryUserNotificationState(),
                Ok(QUNS_BUSY | QUNS_RUNNING_D3D_FULL_SCREEN | QUNS_PRESENTATION_MODE)
            );
            if fullscreen {
                let _ = ShowWindow(hwnd, SW_HIDE);
                return;
            }
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                p.x,
                p.y,
                width,
                p.height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }
    let _ = r.present(hwnd, theme::palette(app.light.get()).value);
}

fn build_lines(s: &Snapshot, p: &theme::Palette) -> Vec<Vec<Seg>> {
    let seg = |text: String, color, bold| Seg { text, color, bold };
    let win = s.windows();
    let frac = if s.total > 0 {
        s.used as f64 / s.total as f64
    } else {
        0.0
    };
    let wsl = match s.vm {
        Some(_) => seg(gib(s.vm_ram()), p.value, true),
        None => seg("off".into(), p.label, false),
    };
    vec![
        vec![
            seg("WIN ".into(), p.label, false),
            seg(gib(win), p.value, true),
            seg("   WSL ".into(), p.label, false),
            wsl,
        ],
        vec![
            seg("ALL ".into(), p.label, false),
            seg(
                format!("{}/{}", gib(s.used).trim_end_matches('G'), gib(s.total)),
                theme::level(p, frac),
                true,
            ),
        ],
    ]
}

fn open_task_manager() {
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            w!("taskmgr.exe"),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

fn show_menu() {
    let app = app();
    let host = app.host.get();
    let mode = if app.parent.get().is_some() {
        "Mode: embedded in taskbar"
    } else {
        "Mode: overlay (fallback)"
    };
    let mode_w = wide(mode);
    let snap = *sampler::LATEST.lock().unwrap();
    let info: Vec<Vec<u16>> = snap
        .map(|s| wsl_summary(&s))
        .unwrap_or_default()
        .iter()
        .map(|l| wide(l))
        .collect();
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, pcwstr(&mode_w));
        for line in &info {
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, pcwstr(line));
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_TASKMGR, w!("Open Task Manager"));
        let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("Exit"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Standard dance so the menu closes when clicking elsewhere.
        let _ = SetForegroundWindow(host);
        let _ = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            None,
            host,
            None,
        );
        let _ = PostMessageW(Some(host), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
    }
}

/// Menu lines until v1's hover panel exists.
fn wsl_summary(s: &Snapshot) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(vm) = s.vm {
        lines.push(format!(
            "WSL VM: {} in RAM + {} compressed, {} committed",
            gib(vm.working_set),
            gib(s.vm_compressed()),
            gib(vm.private)
        ));
    }
    lines.push(match (s.wsl_state, s.linux) {
        (WslState::Off, _) => "WSL: off".into(),
        (WslState::NoDistro, _) => "WSL: VM up, no distro running (not polled)".into(),
        (WslState::Running, Some(l)) => {
            format!("Linux: used {} (cache {})", gib(l.used()), gib(l.cache))
        }
        (WslState::Running, None) => "WSL: distro running, meminfo unavailable".into(),
    });
    lines
}
