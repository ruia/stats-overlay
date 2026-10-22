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

mod config;
mod diag;
mod disk;
mod metrics;
mod procs;
mod prompt;
mod render;
mod sampler;
mod startup;
mod taskbar;
mod theme;
mod tray;
mod util;
mod wsl;
mod wslconfig;

use config::{Config, Layout, Show, Toggle};
use disk::DiskSnap;
use render::{bar, seg, Column, Renderer, Seg, Style};
use sampler::{Snapshot, WslState, WM_APP_SAMPLE};
use std::cell::{Cell, OnceCell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use theme::Palette;
use util::{gib, wide, GIB};
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
use wslconfig::Reclaim;

const ID_TASKMGR: usize = 1;
const ID_EXIT: usize = 2;
const ID_RECLAIM: usize = 3;
const ID_SHUTDOWN: usize = 4;
const ID_STARTUP: usize = 5;
const ID_WSLCONFIG_OPEN: usize = 6;
const ID_MEMORY_CUSTOM: usize = 7;
const ID_WSL_SETTINGS_APP: usize = 8;
const ID_TRAY_PROMOTE: usize = 9;
/// + index into Show::ALL.
const ID_SHOW: usize = 90;
const NIN_SELECT: u32 = WM_USER;
const NIN_KEYSELECT: u32 = WM_USER + 1;
const NIN_POPUPOPEN: u32 = WM_USER + 6;
const NIN_POPUPCLOSE: u32 = WM_USER + 7;
/// + index into Layout::ALL.
const ID_LAYOUT: usize = 100;
/// + index into Toggle::ALL.
const ID_TOGGLE: usize = 110;
/// + index into RECLAIM_CHOICES.
const ID_RECLAIM_MODE: usize = 120;
/// + index into MEMORY_CHOICES.
const ID_MEMORY: usize = 130;

const RECLAIM_CHOICES: [(Reclaim, &str); 4] = [
    (Reclaim::NotSet, "Not set (WSL default)"),
    (Reclaim::Gradual, "Gradual"),
    (Reclaim::DropCache, "Drop cache when idle"),
    (Reclaim::Disabled, "Off"),
];
/// GB; None removes the setting (WSL's default, 50% of RAM).
const MEMORY_CHOICES: [Option<u32>; 6] = [None, Some(8), Some(16), Some(24), Some(32), Some(48)];
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
    config: Cell<Config>,
    tray: RefCell<Option<tray::Tray>>,
    /// The panel is open for the tray icon (else for the strip).
    hover_tray: Cell<bool>,
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
            config: Cell::new(Config::load()),
            tray: RefCell::new(None),
            hover_tray: Cell::new(false),
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
        *app.tray.borrow_mut() = Some(tray::Tray::new(host));
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
        WM_COMMAND => {
            command(hwnd, wp.0 & 0xffff);
            // Dialogs and message boxes leave pages resident too.
            trim();
        }
        tray::WM_APP_TRAY => match (lp.0 & 0xffff) as u32 {
            WM_CONTEXTMENU => {
                app.hovering.set(false);
                hide_panel();
                show_menu();
            }
            NIN_SELECT | NIN_KEYSELECT => open_task_manager(),
            NIN_POPUPOPEN => {
                app.hovering.set(true);
                app.hover_tray.set(true);
                update_panel();
            }
            NIN_POPUPCLOSE => {
                app.hovering.set(false);
                hide_panel();
            }
            _ => {}
        },
        WM_DESTROY => unsafe { PostQuitMessage(0) },
        m if m == app.msg_taskbar_created && m != 0 => {
            // Explorer restarted: the old taskbar (and our child) are gone or stale.
            hide_panel();
            destroy_overlay();
            if let Some(t) = app.tray.borrow_mut().as_mut() {
                t.reset();
            }
            refresh();
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    LRESULT(0)
}

fn command(hwnd: HWND, id: usize) {
    let app = app();
    match id {
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
        ID_WSLCONFIG_OPEN => open_wslconfig(),
        ID_WSL_SETTINGS_APP => {
            if let Some(exe) = wslconfig::settings_app() {
                let e = wide(&exe.to_string_lossy());
                unsafe {
                    ShellExecuteW(
                        None,
                        w!("open"),
                        util::pcwstr(&e),
                        None,
                        None,
                        SW_SHOWNORMAL,
                    );
                }
            }
        }
        ID_MEMORY_CUSTOM => {
            let total = latest().map_or(0, |s| s.total);
            let max = ((total as f64 / GIB).round() as u32).max(1);
            let current = wslconfig::read()
                .memory
                .map_or(max / 2, |b| (b as f64 / GIB).round() as u32);
            unsafe {
                let _ = SetForegroundWindow(hwnd);
            }
            if let Some(gb) = prompt::memory_gb(hwnd, current, max) {
                wslconfig_changed(wslconfig::set_memory(Some(gb)));
            }
        }
        id @ ID_LAYOUT..ID_TOGGLE => {
            if let Some(&l) = Layout::ALL.get(id - ID_LAYOUT) {
                let mut c = app.config.get();
                c.set_layout(l);
                set_config(c);
            }
        }
        id @ ID_TOGGLE..ID_RECLAIM_MODE => {
            if let Some(&t) = Toggle::ALL.get(id - ID_TOGGLE) {
                let mut c = app.config.get();
                c.flip(t);
                set_config(c);
            }
        }
        id @ ID_RECLAIM_MODE..ID_MEMORY => {
            if let Some(&(r, _)) = RECLAIM_CHOICES.get(id - ID_RECLAIM_MODE) {
                wslconfig_changed(wslconfig::set_reclaim(r));
            }
        }
        id @ ID_MEMORY..140 => {
            if let Some(&gb) = MEMORY_CHOICES.get(id - ID_MEMORY) {
                wslconfig_changed(wslconfig::set_memory(gb));
            }
        }
        id @ ID_SHOW..ID_LAYOUT => {
            if let Some(&show) = Show::ALL.get(id - ID_SHOW) {
                let mut c = app.config.get();
                c.set_show(show);
                set_config(c);
            }
        }
        ID_TRAY_PROMOTE => {
            let on = tray::promoted() != Some(true);
            if !tray::promote(on) {
                unsafe {
                    let _ = SetForegroundWindow(hwnd);
                    MessageBoxW(
                        Some(hwnd),
                        w!("Windows hasn't registered the tray icon yet. Try again in a few seconds, or use Settings > Personalization > Taskbar > Other system tray icons."),
                        w!("StatsOverlay"),
                        MB_OK | MB_ICONINFORMATION,
                    );
                }
            }
        }
        ID_EXIT => unsafe {
            hide_panel();
            destroy_overlay();
            if let Some(t) = app.tray.borrow_mut().as_mut() {
                t.remove();
            }
            let _ = DestroyWindow(hwnd);
        },
        _ => {}
    }
}

extern "system" fn panel_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let app = app();
    match msg {
        WM_MOUSEMOVE => {
            if !app.hovering.replace(true) {
                app.hover_tray.set(false);
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
    let show = app.config.get().show;
    if show.tray() {
        update_tray(&snap);
    } else if let Some(t) = app.tray.borrow_mut().as_mut() {
        t.remove();
    }
    if !show.strip() {
        destroy_overlay();
        if app.hovering.get() {
            update_panel();
        }
        return;
    }
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
        &strip_columns(&snap, disk_snap().as_ref(), &app.config.get(), &palette()),
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

/// "412/931G"
fn of(used: u64, total: u64) -> String {
    format!("{}/{}", gib(used).trim_end_matches('G'), gib(total))
}

fn disk_snap() -> Option<DiskSnap> {
    sampler::DISK.lock().unwrap().clone()
}

/// Lines of the strip, per layout. Hidden or unavailable metrics drop out, and so
/// do columns left empty.
///
/// ```text
/// Grid:      WIN 20.8G   ALL 50.0/63.9G   CPU 12%   C: 412/931G
///            WSL 29.2G   CMT 94%          GPU  4%   VHD 86G ↓31G
/// By owner:  WIN  20.8G ram   326G disk   8% cpu    CMT 94%
///            WSL  29.2G ram   86G ↓31G    4% cpu    GPU  4%
/// Bars:      RAM 45.0/63.9G   C: 692/838G   CMT 92%   CPU 14%
///            [win|wsl---]     [win|wsl--]   [####-]   GPU  0%
/// Compact:   WIN 20.8G
///            WSL 29.2G
/// ```
fn strip_columns(s: &Snapshot, d: Option<&DiskSnap>, c: &Config, p: &Palette) -> Vec<Column> {
    let label = |t: &str| seg(t, p.label, false);
    let none = || seg("–", p.label, false);
    let frac = |a: u64, b: u64| a as f64 / b.max(1) as f64;
    let wsl_ram = match s.vm {
        Some(_) => seg(gib(s.vm_ram()), p.value, true),
        None => seg("off", p.label, false),
    };
    let commit = || match s.commit {
        Some((u, l)) => seg(pct(frac(u, l) * 100.0), theme::level(p, frac(u, l)), true),
        None => none(),
    };
    let percent = |v: Option<f64>| match v {
        Some(v) => seg(pct(v), theme::level(p, v / 100.0), true),
        None => none(),
    };
    let gpu = || percent(s.gpu.map(|g| g.percent));
    let system = d.and_then(DiskSnap::system).filter(|_| c.disk);
    let vhd = |line: &mut Vec<Seg>| {
        if let Some(d) = d {
            line.push(seg(gib(d.vhd_total()), p.value, true));
            if let Some(r) = d.reclaimable().filter(|&r| r >= 1 << 30) {
                line.push(label(&format!(" ↓{}", gib(r))));
            }
        }
    };

    let mut cols: Vec<Column> = match c.layout {
        Layout::Compact => vec![vec![
            vec![label("WIN "), seg(gib(s.windows()), p.value, true)],
            vec![label("WSL "), wsl_ram],
        ]],
        Layout::Grid => {
            let mut cols = vec![
                vec![
                    vec![label("WIN "), seg(gib(s.windows()), p.value, true)],
                    vec![label("WSL "), wsl_ram],
                ],
                vec![vec![
                    label("ALL "),
                    seg(
                        of(s.used, s.total),
                        theme::level(p, frac(s.used, s.total)),
                        true,
                    ),
                ]],
            ];
            if c.commit {
                cols[1].push(vec![label("CMT "), commit()]);
            }
            let mut cg = Vec::new();
            if c.cpu {
                cg.push(vec![label("CPU "), percent(s.cpu)]);
            }
            if c.gpu {
                cg.push(vec![label("GPU "), gpu()]);
            }
            cols.push(cg);
            if let Some(sys) = system {
                let mut v = vec![label("VHD ")];
                vhd(&mut v);
                cols.push(vec![
                    vec![
                        label(&format!("{}: ", sys.letter)),
                        seg(
                            of(sys.used(), sys.total),
                            theme::level(p, frac(sys.used(), sys.total)),
                            true,
                        ),
                    ],
                    v,
                ]);
            }
            cols
        }
        Layout::Bars => {
            let f = |a: u64, b: u64| frac(a, b) as f32;
            let mut cols = vec![vec![
                vec![
                    label("RAM "),
                    seg(
                        of(s.used, s.total),
                        theme::level(p, frac(s.used, s.total)),
                        true,
                    ),
                ],
                bar(
                    vec![
                        (f(s.windows(), s.total), p.win),
                        (f(s.vm_ram(), s.total), p.wsl),
                    ],
                    p.track,
                ),
            ]];
            if let (Some(sys), Some(d)) = (system, d) {
                let vhds: u64 = d
                    .vhds
                    .iter()
                    .filter(|v| v.drive() == Some(sys.letter))
                    .map(|v| v.on_disk)
                    .sum();
                cols.push(vec![
                    vec![
                        label(&format!("{}: ", sys.letter)),
                        seg(
                            of(sys.used(), sys.total),
                            theme::level(p, frac(sys.used(), sys.total)),
                            true,
                        ),
                    ],
                    bar(
                        vec![
                            (f(d.windows_used(&sys), sys.total), p.win),
                            (f(vhds, sys.total), p.wsl),
                        ],
                        p.track,
                    ),
                ]);
            }
            if let (true, Some((u, l))) = (c.commit, s.commit) {
                cols.push(vec![
                    vec![label("CMT "), commit()],
                    bar(vec![(f(u, l), theme::level(p, frac(u, l)))], p.track),
                ]);
            }
            let mut cg = Vec::new();
            if c.cpu {
                cg.push(vec![label("CPU "), percent(s.cpu)]);
            }
            if c.gpu {
                cg.push(vec![label("GPU "), gpu()]);
            }
            cols.push(cg);
            cols
        }
        Layout::ByOwner => {
            let owner = |t: &str, color| seg(t, color, true);
            let mut cols = vec![
                vec![vec![owner("WIN", p.win)], vec![owner("WSL", p.wsl)]],
                vec![
                    vec![seg(gib(s.windows()), p.value, true), label(" ram")],
                    vec![wsl_ram, label(" ram")],
                ],
            ];
            if let (Some(sys), Some(d)) = (system, d) {
                let mut v = Vec::new();
                vhd(&mut v);
                cols.push(vec![
                    vec![
                        seg(gib(d.windows_used(&sys)), p.value, true),
                        label(" disk"),
                    ],
                    v,
                ]);
            }
            if c.cpu {
                let win = s.cpu.map(|t| (t - s.wsl_cpu.unwrap_or(0.0)).max(0.0));
                let wsl = s.wsl_cpu.or(s.vm.is_none().then_some(0.0));
                cols.push(vec![
                    vec![percent(win), label(" cpu")],
                    vec![percent(wsl), label(" cpu")],
                ]);
            }
            let mut extra = Vec::new();
            if c.commit {
                extra.push(vec![label("CMT "), commit()]);
            }
            if c.gpu {
                extra.push(vec![label("GPU "), gpu()]);
            }
            cols.push(extra);
            cols
        }
    };
    cols.retain(|c| !c.is_empty());
    cols
}

/// The hover breakdown in sections; a label column and a value column.
/// Display > CPU / GPU / Commit / Disk apply here as well as to the strip.
fn panel_columns(
    s: &Snapshot,
    d: Option<&DiskSnap>,
    c: &Config,
    p: &Palette,
    embedded: bool,
) -> Vec<Column> {
    let mut labels: Column = Vec::new();
    let mut values: Column = Vec::new();
    let mut row = |label: Seg, value: Vec<Seg>| {
        labels.push(vec![label]);
        values.push(value);
    };
    let l = |t: &str| seg(t, p.label, false);
    let v = |t: String| seg(t, p.value, true);
    let dim = |t: String| seg(t, p.label, false);
    let g = |b: u64| format!("{:.1}G", b as f64 / GIB);
    let gd = |b: u64| gib(b);
    let head = |t: &str| seg(t, p.label, true);

    row(head("MEMORY"), vec![]);
    let ram_frac = s.used as f64 / s.total.max(1) as f64;
    row(
        l("In use"),
        vec![
            seg(
                format!("{} / {}", g(s.used), g(s.total)),
                theme::level(p, ram_frac),
                true,
            ),
            dim(format!("  {}", pct(ram_frac * 100.0))),
        ],
    );
    row(l("   Windows"), vec![v(g(s.windows()))]);
    match s.vm {
        Some(vm) => row(
            l("   WSL VM"),
            vec![
                v(g(s.vm_ram())),
                dim(format!(
                    "  {} RAM + {} compressed",
                    g(vm.working_set),
                    g(s.vm_compressed())
                )),
            ],
        ),
        None => row(l("   WSL VM"), vec![dim("off".into())]),
    }
    if let (true, Some((c, lim))) = (c.commit, s.commit) {
        let f = c as f64 / lim.max(1) as f64;
        let mut val = vec![
            seg(format!("{} / {}", g(c), g(lim)), theme::level(p, f), true),
            dim(format!("  {}", pct(f * 100.0))),
        ];
        if let Some(vm) = s.vm {
            val.push(dim(format!(" · VM holds {}", g(vm.private))));
        }
        row(l("Commit"), val);
    }

    row(l(""), vec![]);
    row(head("WSL"), vec![]);
    let linux = match (s.wsl_state, s.linux) {
        (WslState::Running, Some(m)) => vec![
            v(format!("used {}", g(m.used()))),
            dim(format!("  cache {}", g(m.cache))),
        ],
        (WslState::Running, None) => vec![dim("meminfo unavailable".into())],
        (WslState::NoDistro, _) => vec![dim("no distro running (not polled)".into())],
        (WslState::Off, _) => vec![dim("VM off".into())],
    };
    row(l("Linux"), linux);
    let wc = s.wslconfig;
    row(
        l("Memory reclaim"),
        vec![
            v(wc.reclaim.label().into()),
            dim("  (right-click to change)".into()),
        ],
    );
    row(
        l("Memory limit"),
        match wc.memory {
            Some(m) => vec![v(gd(m))],
            None => vec![
                v("none".into()),
                dim(format!("  VM may grow to {}", g(s.total / 2))),
            ],
        },
    );
    if let (true, Some(w)) = (c.cpu, s.wsl_cpu) {
        row(l("CPU"), vec![v(pct(w))]);
    }

    if let (true, Some(d)) = (c.disk, d) {
        row(l(""), vec![]);
        row(head("DISK"), vec![]);
        for drive in &d.drives {
            let f = drive.used() as f64 / drive.total.max(1) as f64;
            row(
                l(&format!("{}:", drive.letter)),
                vec![
                    seg(
                        format!("{} / {}", gd(drive.used()), gd(drive.total)),
                        theme::level(p, f),
                        true,
                    ),
                    dim(format!(
                        "  {} free · Windows {}",
                        gd(drive.free),
                        gd(d.windows_used(drive))
                    )),
                ],
            );
            for vhd in d.vhds.iter().filter(|x| x.drive() == Some(drive.letter)) {
                let mut detail = String::new();
                if let Some(u) = vhd.linux_used {
                    detail.push_str(&format!("  Linux {}", gd(u)));
                }
                if let Some(r) = vhd.reclaimable() {
                    detail.push_str(&format!(" · ↓{}", gd(r)));
                }
                detail.push_str(if vhd.sparse {
                    " · sparse"
                } else {
                    " · not sparse"
                });
                row(
                    l(&format!("   {}", vhd.name)),
                    vec![v(gd(vhd.on_disk)), dim(detail)],
                );
            }
        }
    }

    if c.cpu || c.gpu {
        row(l(""), vec![]);
        row(
            head(match (c.cpu, c.gpu) {
                (true, true) => "CPU · GPU",
                (true, false) => "CPU",
                _ => "GPU",
            }),
            vec![],
        );
    }
    if c.cpu {
        row(l("CPU"), vec![v(s.cpu.map_or("–".into(), pct))]);
    }
    if c.gpu {
        let mut gpu = vec![v(s.gpu.map_or("–".into(), |x| pct(x.percent)))];
        if let Some(vram) = s.gpu.and_then(|x| x.vram) {
            gpu.push(dim(format!("  VRAM {}", g(vram))));
        }
        row(l("GPU"), gpu);
    }
    if !embedded {
        row(
            l("Mode"),
            vec![dim("overlay (taskbar embedding failed)".into())],
        );
    }
    vec![labels, values]
}

/// The tray icon's second bar: commit, else the system drive, as Display allows.
fn second_bar(s: &Snapshot) -> Option<f32> {
    let c = app().config.get();
    let frac = |a: u64, b: u64| a as f32 / b.max(1) as f32;
    if c.commit {
        if let Some((u, l)) = s.commit {
            return Some(frac(u, l));
        }
    }
    if c.disk {
        let d = disk_snap()?;
        let sys = d.system()?;
        return Some(frac(sys.used(), sys.total));
    }
    None
}

fn update_tray(s: &Snapshot) {
    let frac = |a: u64, b: u64| a as f32 / b.max(1) as f32;
    let bars = tray::Bars {
        win: frac(s.windows(), s.total),
        wsl: frac(s.vm_ram(), s.total),
        second: second_bar(s),
    };
    let mut tip = format!(
        "WIN {}  WSL {}  ALL {}",
        gib(s.windows()),
        if s.vm.is_some() {
            gib(s.vm_ram())
        } else {
            "off".into()
        },
        of(s.used, s.total)
    );
    if let Some((u, l)) = s.commit {
        tip.push_str(&format!("\nCMT {}", pct(frac(u, l) as f64 * 100.0)));
    }
    if let Some(c) = s.cpu {
        tip.push_str(&format!("  CPU {}", pct(c)));
    }
    if let Some(t) = app().tray.borrow_mut().as_mut() {
        t.update(bars, &tip, &palette());
    }
}

/// Where the panel hangs from: the strip, or the tray icon.
fn panel_anchor() -> Option<RECT> {
    let app = app();
    if app.hover_tray.get() || !app.config.get().show.strip() {
        return app.tray.borrow().as_ref().and_then(tray::Tray::rect);
    }
    let strip = app.overlay.get()?;
    let mut r = RECT::default();
    unsafe { GetWindowRect(strip, &mut r).ok()? };
    Some(r)
}

/// The overflow flyout's rectangle when `icon` is in it rather than on the taskbar.
fn overflow_flyout(icon: &RECT, taskbar: &RECT) -> Option<RECT> {
    let on_taskbar = icon.top >= taskbar.top && icon.bottom <= taskbar.bottom;
    if on_taskbar {
        return None;
    }
    let center = POINT {
        x: (icon.left + icon.right) / 2,
        y: (icon.top + icon.bottom) / 2,
    };
    unsafe {
        let h = WindowFromPoint(center);
        let root = GetAncestor(h, GA_ROOT);
        let mut r = RECT::default();
        GetWindowRect(root, &mut r).ok()?;
        // Not the flyout if it spans the screen (e.g. the point hit the desktop).
        (r.right - r.left < (taskbar.right - taskbar.left) / 2).then_some(r)
    }
}

fn update_panel() {
    let app = app();
    let (Some(snap), Some(sr), Some(tb)) = (latest(), panel_anchor(), taskbar::find()) else {
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
        &panel_columns(
            &snap,
            disk_snap().as_ref(),
            &app.config.get(),
            &p,
            app.hover_tray.get() || app.parent.get().is_some(),
        ),
        scale,
        None,
    ) else {
        return;
    };
    let tbr = taskbar::rect(tb);
    let gap = (6.0 * scale) as i32;
    let at_top = taskbar::at_top(tb);
    let mut x = sr.left.clamp(tbr.left, (tbr.right - w).max(tbr.left));
    // Above the taskbar, or below it when the taskbar is at the top of its screen.
    let mut y = if at_top {
        tbr.bottom + gap
    } else {
        tbr.top - h - gap
    };
    // An icon in the ^ overflow flyout sits outside the taskbar: put the panel
    // beside the flyout so it never covers the icons you're pointing at.
    if let Some(f) = overflow_flyout(&sr, &tbr) {
        x = if f.left - gap - w >= tbr.left {
            f.left - gap - w
        } else {
            f.right + gap
        };
        y = if at_top { f.top } else { f.bottom - h };
    }
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
        // ShowWindow returns whether it was visible.
        if unsafe { ShowWindow(h, SW_HIDE) }.as_bool() {
            trim();
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

fn set_config(c: Config) {
    app().config.set(c);
    if let Some(r) = app().strip.borrow_mut().as_mut() {
        r.reset_widths();
    }
    refresh();
}

/// After .wslconfig was written: re-read it, and offer to restart WSL so it applies.
fn wslconfig_changed(result: std::io::Result<()>) {
    let host = app().host.get();
    sampler::request_wsl_recheck();
    unsafe {
        let _ = SetForegroundWindow(host);
        if let Err(e) = result {
            let text = wide(&format!("Couldn't update .wslconfig:\n\n{e}"));
            MessageBoxW(
                Some(host),
                util::pcwstr(&text),
                w!("StatsOverlay"),
                MB_OK | MB_ICONERROR,
            );
            return;
        }
        let vm_up = latest().is_some_and(|s| s.vm.is_some());
        if !vm_up || ACTION_BUSY.load(Ordering::Acquire) {
            return; // Applies next time WSL starts.
        }
        let restart = MessageBoxW(
            Some(host),
            w!("Saved to .wslconfig. It applies the next time WSL starts.\n\nRestart WSL now? This stops every running distro, including Docker Desktop's engine."),
            w!("StatsOverlay"),
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
        ) == IDYES;
        if restart {
            run_action(|| {
                wsl::shutdown();
            });
        }
    }
}

fn open_wslconfig() {
    let Some(path) = wslconfig::path() else {
        return;
    };
    let p = wide(&path.to_string_lossy());
    unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            w!("notepad.exe"),
            util::pcwstr(&p),
            None,
            SW_SHOWNORMAL,
        );
    }
}

fn menu_item(menu: HMENU, on: bool, checked: bool, id: usize, text: &str) {
    let mut flags = MF_STRING;
    if !on {
        flags |= MF_GRAYED;
    }
    if checked {
        flags |= MF_CHECKED;
    }
    let t = wide(text);
    unsafe {
        let _ = AppendMenuW(menu, flags, id, util::pcwstr(&t));
    }
}

fn submenu(menu: HMENU, sub: HMENU, text: &str) {
    let t = wide(text);
    unsafe {
        let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, util::pcwstr(&t));
    }
}

fn separator(menu: HMENU) {
    unsafe {
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    }
}

fn display_menu() -> Option<HMENU> {
    let c = app().config.get();
    let m = unsafe { CreatePopupMenu().ok()? };
    for (i, s) in Show::ALL.iter().enumerate() {
        menu_item(m, true, c.show == *s, ID_SHOW + i, s.label());
    }
    menu_item(
        m,
        c.show.tray(),
        tray::promoted() == Some(true),
        ID_TRAY_PROMOTE,
        "Keep tray icon visible (not under ^)",
    );
    separator(m);
    for (i, l) in Layout::ALL.iter().enumerate() {
        menu_item(m, true, c.layout == *l, ID_LAYOUT + i, l.label());
    }
    separator(m);
    for (i, t) in Toggle::ALL.iter().enumerate() {
        // Compact shows memory only.
        menu_item(
            m,
            c.layout != Layout::Compact,
            c.shows(*t),
            ID_TOGGLE + i,
            t.label(),
        );
    }
    Some(m)
}

fn wsl_settings_menu() -> Option<HMENU> {
    let wc = wslconfig::read();
    let m = unsafe { CreatePopupMenu().ok()? };
    menu_item(m, false, false, 0, "Memory reclaim");
    for (i, (r, label)) in RECLAIM_CHOICES.iter().enumerate() {
        menu_item(
            m,
            true,
            wc.reclaim == *r,
            ID_RECLAIM_MODE + i,
            &format!("    {label}"),
        );
    }
    separator(m);
    menu_item(m, false, false, 0, "Memory limit");
    let gb = wc.memory.map(|b| (b as f64 / GIB).round() as u32);
    let standard = MEMORY_CHOICES.contains(&gb);
    for (i, choice) in MEMORY_CHOICES.iter().enumerate() {
        let label = match choice {
            None => "    Default (50% of RAM)".to_string(),
            Some(g) => format!("    {g} GB"),
        };
        menu_item(m, true, gb == *choice, ID_MEMORY + i, &label);
    }
    let custom = match (standard, wc.memory) {
        (false, Some(b)) => format!("    Custom ({})…", gib(b)),
        _ => "    Custom…".to_string(),
    };
    menu_item(m, true, !standard, ID_MEMORY_CUSTOM, &custom);
    separator(m);
    menu_item(m, true, false, ID_WSLCONFIG_OPEN, "Open .wslconfig");
    if wslconfig::settings_app().is_some() {
        menu_item(m, true, false, ID_WSL_SETTINGS_APP, "Open WSL Settings app");
    }
    menu_item(m, false, false, 0, "Changes apply after WSL restarts");
    Some(m)
}

fn show_menu() {
    let host = app().host.get();
    let snap = latest();
    let busy = ACTION_BUSY.load(Ordering::Acquire);
    let running = snap.is_some_and(|s| s.wsl_state == WslState::Running);
    let vm_up = snap.is_some_and(|s| s.vm.is_some());
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        menu_item(
            menu,
            running && !busy,
            false,
            ID_RECLAIM,
            "Reclaim WSL cache",
        );
        if let Some(sub) = wsl_settings_menu() {
            submenu(menu, sub, "WSL settings");
        }
        menu_item(menu, vm_up && !busy, false, ID_SHUTDOWN, "Shut down WSL…");
        separator(menu);
        if let Some(sub) = display_menu() {
            submenu(menu, sub, "Display");
        }
        menu_item(menu, true, false, ID_TASKMGR, "Open Task Manager");
        menu_item(
            menu,
            true,
            startup::enabled(),
            ID_STARTUP,
            "Start with Windows",
        );
        separator(menu);
        menu_item(menu, true, false, ID_EXIT, "Exit");
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let vertical = match taskbar::find() {
            Some(tb) if taskbar::at_top(tb) => TPM_TOPALIGN,
            _ => TPM_BOTTOMALIGN,
        };
        // Standard dance so the menu closes when clicking elsewhere.
        let _ = SetForegroundWindow(host);
        let _ = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | vertical,
            pt.x,
            pt.y,
            None,
            host,
            None,
        );
        let _ = PostMessageW(Some(host), WM_NULL, WPARAM(0), LPARAM(0));
        // Destroys the submenus too.
        let _ = DestroyMenu(menu);
    }
    trim();
}

/// Gives back the pages that menus, dialogs and the panel pulled in (themed menu
/// rendering alone adds ~3 MB that would otherwise stay resident).
fn trim() {
    use windows::Win32::System::ProcessStatus::K32EmptyWorkingSet;
    use windows::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let _ = K32EmptyWorkingSet(GetCurrentProcess());
    }
}
