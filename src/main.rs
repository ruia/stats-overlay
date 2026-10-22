//! StatsOverlay: a tiny resource readout embedded in the Windows 11 taskbar.
//!
//! Windows:
//! - host: hidden, unowned top-level window. It receives the broadcasts a child
//!   window never sees (TaskbarCreated, WM_SETTINGCHANGE), the sampler's pokes, and
//!   owns the right-click menu.
//! - overlay: the strip, a layered child of Shell_TrayWnd, or a topmost tool window
//!   when embedding fails (or with `--popup`).
//! - panel: the hover breakdown, a click-through topmost layered popup.
//!
//! A child window in Explorer's taskbar shares its input queue with Explorer, so a
//! stall on this thread can stall the taskbar. Sampling happens on the sampler
//! thread and WSL actions on short-lived worker threads; this thread only lays out
//! text and blits.

#![windows_subsystem = "windows"]

mod diag;
mod metrics;
mod procs;
mod render;
mod sampler;
mod startup;
mod taskbar;
mod theme;
mod util;
mod wsl;

use render::{seg, Column, Renderer, Seg, Style};
use sampler::{Snapshot, WslState, WM_APP_SAMPLE};
use std::cell::{Cell, OnceCell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use theme::Palette;
use util::{gib, GIB};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT};
use windows::Win32::UI::Shell::{
    SHQueryUserNotificationState, ShellExecuteW, QUNS_BUSY, QUNS_PRESENTATION_MODE,
    QUNS_RUNNING_D3D_FULL_SCREEN,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const ID_TASKMGR: usize = 1;
const ID_EXIT: usize = 2;
const ID_RECLAIM: usize = 3;
const ID_SHUTDOWN: usize = 4;
const ID_STARTUP: usize = 5;
const WM_DPICHANGED_AFTERPARENT: u32 = 0x02E3;
const WM_MOUSELEAVE: u32 = 0x02A3;

/// A WSL action (reclaim, shutdown) is running on a worker thread.
static ACTION_BUSY: AtomicBool = AtomicBool::new(false);

struct Opts {
    popup: bool,
    offset: Option<i32>,
}

struct App {
    host: Cell<HWND>,
    overlay: Cell<Option<HWND>>,
    /// The taskbar the overlay is a child of; None in popup mode.
    parent: Cell<Option<HWND>>,
    panel: Cell<Option<HWND>>,
    hovering: Cell<bool>,
    light: Cell<bool>,
    msg_taskbar_created: u32,
    opts: Opts,
    strip: RefCell<Option<Renderer>>,
    panel_renderer: RefCell<Option<Renderer>>,
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
        type Proc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;
        for (name, proc) in [
            (w!("StatsOverlayHost"), host_proc as Proc),
            (w!("StatsOverlay"), overlay_proc as Proc),
            (w!("StatsOverlayPanel"), panel_proc as Proc),
        ] {
            let wc = WNDCLASSW {
                lpfnWndProc: Some(proc),
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
            panel: Cell::new(None),
            hovering: Cell::new(false),
            light: Cell::new(theme::system_light()),
            msg_taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            opts,
            strip: RefCell::new(
                Renderer::new(Style {
                    font_dip: 12.0,
                    pad_dip: 6.0,
                    gap_dip: 14.0,
                    background: None,
                    sticky_widths: true,
                })
                .ok(),
            ),
            panel_renderer: RefCell::new(None),
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
            ID_RECLAIM => run_action(|| {
                wsl::reclaim_cache(&wsl::running_distros());
            }),
            ID_SHUTDOWN => {
                if confirm_shutdown() {
                    run_action(|| {
                        wsl::shutdown();
                    });
                }
            }
            ID_STARTUP => {
                startup::set(!startup::enabled());
            }
            ID_EXIT => unsafe {
                hide_panel();
                destroy_overlay();
                let _ = DestroyWindow(hwnd);
            },
            _ => {}
        },
        WM_DESTROY => unsafe { PostQuitMessage(0) },
        m if m == app.msg_taskbar_created && m != 0 => {
            // Explorer restarted: the old taskbar (and our child) are gone or stale.
            hide_panel();
            destroy_overlay();
            refresh();
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}

extern "system" fn panel_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let app = app();
    match msg {
        WM_MOUSEMOVE => {
            if !app.hovering.replace(true) {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                unsafe {
                    let _ = TrackMouseEvent(&mut tme);
                }
                update_panel();
            }
        }
        WM_MOUSELEAVE => {
            app.hovering.set(false);
            hide_panel();
        }
        WM_LBUTTONUP => open_task_manager(),
        WM_RBUTTONUP => {
            app.hovering.set(false);
            hide_panel();
            show_menu();
        }
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_DPICHANGED | WM_DPICHANGED_AFTERPARENT => unsafe {
            let _ = PostMessageW(Some(app.host.get()), WM_APP_SAMPLE, WPARAM(0), LPARAM(0));
        },
        WM_DESTROY => {
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

fn create_window(
    class: PCWSTR,
    ex: WINDOW_EX_STYLE,
    style: WINDOW_STYLE,
    parent: Option<HWND>,
) -> Option<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        CreateWindowExW(
            ex,
            class,
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
    if !app.opts.popup {
        // Created directly as a child of the taskbar (same effect as SetParent).
        let child = create_window(
            w!("StatsOverlay"),
            WS_EX_LAYERED | WS_EX_NOPARENTNOTIFY,
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS,
            Some(tb),
        );
        if let Some(h) = child {
            app.overlay.set(Some(h));
            app.parent.set(Some(tb));
            return Some(h);
        }
    }
    let h = create_window(
        w!("StatsOverlay"),
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        WS_POPUP | WS_VISIBLE,
        None,
    )?;
    app.overlay.set(Some(h));
    Some(h)
}

fn latest() -> Option<Snapshot> {
    *sampler::LATEST.lock().unwrap()
}

fn palette() -> Palette {
    theme::palette(app().light.get())
}

fn refresh() {
    let Some(snap) = latest() else { return };
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
    let r = taskbar::rect(tb);
    let mut strip = app.strip.borrow_mut();
    let Some(renderer) = strip.as_mut() else {
        return;
    };
    let Ok((width, height)) = renderer.prepare(
        &strip_columns(&snap, &palette()),
        scale,
        Some(r.bottom - r.top),
    ) else {
        return;
    };
    let p = taskbar::place(tb, width, app.opts.offset);

    unsafe {
        if app.parent.get().is_some() {
            // Child coordinates are relative to the taskbar's client area.
            let mut pt = POINT { x: p.x, y: p.y };
            let _ = ScreenToClient(tb, &mut pt);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                pt.x,
                pt.y,
                width,
                height,
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
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }
    let _ = renderer.present(hwnd, palette().value);
    drop(strip);
    if app.hovering.get() {
        update_panel();
    }
}

fn pct(v: f64) -> String {
    format!("{v:.0}%")
}

/// ```text
/// WIN 20.8G   WSL 29.2G   CPU 12%
/// ALL 50.0/63.9G  CMT 94%   GPU  4%
/// ```
fn strip_columns(s: &Snapshot, p: &Palette) -> Vec<Column> {
    let ram_frac = s.used as f64 / s.total.max(1) as f64;
    let wsl = match s.vm {
        Some(_) => seg(gib(s.vm_ram()), p.value, true),
        None => seg("off", p.label, false),
    };
    let commit = match s.commit {
        Some((c, l)) => seg(
            pct(c as f64 / l.max(1) as f64 * 100.0),
            theme::level(p, c as f64 / l.max(1) as f64),
            true,
        ),
        None => seg("–", p.label, false),
    };
    let percent = |v: Option<f64>| match v {
        Some(v) => seg(pct(v), theme::level(p, v / 100.0), true),
        None => seg("–", p.label, false),
    };
    vec![
        vec![
            vec![
                seg("WIN ", p.label, false),
                seg(gib(s.windows()), p.value, true),
            ],
            vec![
                seg("ALL ", p.label, false),
                seg(
                    format!("{}/{}", gib(s.used).trim_end_matches('G'), gib(s.total)),
                    theme::level(p, ram_frac),
                    true,
                ),
            ],
        ],
        vec![
            vec![seg("WSL ", p.label, false), wsl],
            vec![seg("CMT ", p.label, false), commit],
        ],
        vec![
            vec![seg("CPU ", p.label, false), percent(s.cpu)],
            vec![
                seg("GPU ", p.label, false),
                percent(s.gpu.map(|g| g.percent)),
            ],
        ],
    ]
}

/// The hover breakdown: a label column and a value column.
fn panel_columns(s: &Snapshot, p: &Palette, embedded: bool) -> Vec<Column> {
    let mut labels: Column = Vec::new();
    let mut values: Column = Vec::new();
    let mut row = |label: &str, value: Vec<Seg>| {
        labels.push(vec![seg(label, p.label, false)]);
        values.push(value);
    };
    let v = |t: String| seg(t, p.value, true);
    let dim = |t: String| seg(t, p.label, false);
    let g = |b: u64| format!("{:.1}G", b as f64 / GIB);

    let ram_frac = s.used as f64 / s.total.max(1) as f64;
    row(
        "Memory in use",
        vec![
            seg(
                format!("{} / {}", g(s.used), g(s.total)),
                theme::level(p, ram_frac),
                true,
            ),
            dim(format!("  {}", pct(ram_frac * 100.0))),
        ],
    );
    row("   Windows", vec![v(g(s.windows()))]);
    match s.vm {
        Some(vm) => {
            row(
                "   WSL VM",
                vec![
                    v(g(s.vm_ram())),
                    dim(format!(
                        "  {} in RAM + {} compressed",
                        g(vm.working_set),
                        g(s.vm_compressed())
                    )),
                ],
            );
            row(
                "   VM committed",
                vec![
                    v(g(vm.private)),
                    dim("  RAM, compressed and page file".into()),
                ],
            );
        }
        None => row("   WSL VM", vec![dim("off".into())]),
    }
    let linux = match (s.wsl_state, s.linux) {
        (WslState::Running, Some(l)) => vec![
            v(format!("used {}", g(l.used()))),
            dim(format!("  cache {}", g(l.cache))),
        ],
        (WslState::Running, None) => vec![dim("meminfo unavailable".into())],
        (WslState::NoDistro, _) => vec![dim("no distro running (not polled)".into())],
        (WslState::Off, _) => vec![dim("VM off".into())],
    };
    row("Linux", linux);
    row("autoMemoryReclaim", vec![v(s.reclaim.label().into())]);
    if let Some((c, l)) = s.commit {
        let f = c as f64 / l.max(1) as f64;
        row(
            "Commit charge",
            vec![
                seg(format!("{} / {}", g(c), g(l)), theme::level(p, f), true),
                dim(format!("  {}", pct(f * 100.0))),
            ],
        );
    }
    let mut cpu = vec![v(s.cpu.map_or("–".into(), pct))];
    if let Some(w) = s.wsl_cpu {
        cpu.push(dim(format!("  WSL {}", pct(w))));
    }
    row("CPU", cpu);
    let mut gpu = vec![v(s.gpu.map_or("–".into(), |x| pct(x.percent)))];
    if let Some(vram) = s.gpu.and_then(|x| x.vram) {
        gpu.push(dim(format!("  VRAM {}", g(vram))));
    }
    row("GPU", gpu);
    row(
        "Mode",
        vec![dim(if embedded {
            "embedded in taskbar".into()
        } else {
            "overlay (fallback)".into()
        })],
    );
    vec![labels, values]
}

fn update_panel() {
    let app = app();
    let (Some(snap), Some(strip), Some(tb)) = (latest(), app.overlay.get(), taskbar::find()) else {
        return;
    };
    let panel = match app.panel.get() {
        Some(h) => h,
        None => {
            let Some(h) = create_window(
                w!("StatsOverlayPanel"),
                WS_EX_LAYERED
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE
                    | WS_EX_TRANSPARENT,
                WS_POPUP,
                None,
            ) else {
                return;
            };
            app.panel.set(Some(h));
            h
        }
    };
    let p = palette();
    let mut slot = app.panel_renderer.borrow_mut();
    if slot.is_none() {
        let style = Style {
            font_dip: 12.0,
            pad_dip: 12.0,
            gap_dip: 16.0,
            background: Some(p.panel),
            sticky_widths: false,
        };
        *slot = Renderer::new(style).ok();
    }
    let Some(r) = slot.as_mut() else { return };
    r.set_background(Some(p.panel));
    let scale = taskbar::dpi(tb) as f32 / 96.0;
    let Ok((w, h)) = r.prepare(
        &panel_columns(&snap, &p, app.parent.get().is_some()),
        scale,
        None,
    ) else {
        return;
    };
    let tbr = taskbar::rect(tb);
    let mut sr = RECT::default();
    unsafe {
        let _ = GetWindowRect(strip, &mut sr);
    }
    let x = sr.left.clamp(tbr.left, (tbr.right - w).max(tbr.left));
    let y = tbr.top - h - (6.0 * scale) as i32;
    unsafe {
        let _ = SetWindowPos(
            panel,
            Some(HWND_TOPMOST),
            x,
            y,
            w,
            h,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
    let _ = r.present(panel, p.value);
}

fn hide_panel() {
    if let Some(h) = app().panel.get() {
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
}

/// Runs a blocking WSL action on a worker thread, then has the sampler re-read WSL.
fn run_action(f: impl FnOnce() + Send + 'static) {
    if ACTION_BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || {
        f();
        ACTION_BUSY.store(false, Ordering::Release);
        sampler::request_wsl_recheck();
    });
}

fn confirm_shutdown() -> bool {
    let host = app().host.get();
    unsafe {
        let _ = SetForegroundWindow(host);
        MessageBoxW(
            Some(host),
            w!("Shut down WSL?\n\nThis stops every running distro, including Docker Desktop's engine, like `wsl --shutdown`."),
            w!("StatsOverlay"),
            MB_OKCANCEL | MB_ICONWARNING | MB_DEFBUTTON2,
        ) == IDOK
    }
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
    let host = app().host.get();
    let snap = latest();
    let busy = ACTION_BUSY.load(Ordering::Acquire);
    let running = snap.is_some_and(|s| s.wsl_state == WslState::Running);
    let vm_up = snap.is_some_and(|s| s.vm.is_some());
    let flag = |on: bool| if on { MF_STRING } else { MF_STRING | MF_GRAYED };
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = AppendMenuW(
            menu,
            flag(running && !busy),
            ID_RECLAIM,
            w!("Reclaim WSL cache"),
        );
        let _ = AppendMenuW(
            menu,
            flag(vm_up && !busy),
            ID_SHUTDOWN,
            w!("Shut down WSL…"),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_TASKMGR, w!("Open Task Manager"));
        let checked = if startup::enabled() {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
        let _ = AppendMenuW(
            menu,
            MF_STRING | checked,
            ID_STARTUP,
            w!("Start with Windows"),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
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
