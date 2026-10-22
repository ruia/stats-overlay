//! A small modal "number of GB" dialog for the custom WSL memory limit. The
//! template is built in memory, so there is no resource script to maintain.

use crate::util::wide;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

const ID_EDIT: i32 = 100;
const EM_SETSEL: u32 = 0x00B1;
const ES_NUMBER: u32 = 0x2000;
const BUTTON: u16 = 0x0080;
const EDIT: u16 = 0x0081;
const STATIC: u16 = 0x0082;

/// Asks for a whole number of GB in 1..=max, starting at `current`. None if cancelled.
pub fn memory_gb(owner: HWND, current: u32, max: u32) -> Option<u32> {
    let label = format!("Memory limit for the WSL VM, in GB (1 to {max}):");
    let mut t = Template::new("WSL memory limit", 186, 66);
    t.item(STATIC, &label, 0, 7, 8, 172, 10, 0);
    t.item(
        EDIT,
        "",
        ID_EDIT,
        7,
        22,
        50,
        14,
        (WS_BORDER | WS_TABSTOP).0 | ES_NUMBER,
    );
    t.item(STATIC, "GB", -1, 62, 25, 20, 10, 0);
    t.item(
        BUTTON,
        "OK",
        IDOK.0,
        75,
        45,
        50,
        14,
        WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32,
    );
    t.item(BUTTON, "Cancel", IDCANCEL.0, 129, 45, 50, 14, WS_TABSTOP.0);
    let words = t.finish();
    let init = (current as isize) | ((max as isize) << 16);
    let r = unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        DialogBoxIndirectParamW(
            Some(instance.into()),
            words.as_ptr() as *const DLGTEMPLATE,
            Some(owner),
            Some(proc),
            LPARAM(init),
        )
    };
    (r > 0).then_some(r as u32)
}

extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> isize {
    unsafe {
        match msg {
            WM_INITDIALOG => {
                let current = (lp.0 & 0xffff) as u32;
                let max = ((lp.0 >> 16) & 0xffff) as u32;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, max as isize);
                if current > 0 {
                    let _ = SetDlgItemInt(hwnd, ID_EDIT, current, false);
                }
                let _ = SendDlgItemMessageW(hwnd, ID_EDIT, EM_SETSEL, WPARAM(0), LPARAM(-1));
                let _ = SetForegroundWindow(hwnd);
                1 // focus the first tab stop (the edit box)
            }
            WM_COMMAND => {
                match (wp.0 & 0xffff) as i32 {
                    id if id == IDOK.0 => {
                        let max = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as u32;
                        let mut ok = windows::core::BOOL(0);
                        let gb = GetDlgItemInt(hwnd, ID_EDIT, Some(&mut ok), false);
                        if ok.as_bool() && (1..=max).contains(&gb) {
                            let _ = EndDialog(hwnd, gb as isize);
                        } else {
                            let text = wide(&format!("Enter a whole number from 1 to {max}."));
                            MessageBoxW(
                                Some(hwnd),
                                windows::core::PCWSTR(text.as_ptr()),
                                windows::core::w!("WSL memory limit"),
                                MB_OK | MB_ICONWARNING,
                            );
                        }
                    }
                    id if id == IDCANCEL.0 => {
                        let _ = EndDialog(hwnd, 0);
                    }
                    _ => {}
                }
                1
            }
            _ => 0,
        }
    }
}

/// DLGTEMPLATE + DLGITEMTEMPLATEs, as UTF-16 words.
struct Template {
    w: Vec<u16>,
    count_at: usize,
}

impl Template {
    fn new(title: &str, cx: i16, cy: i16) -> Template {
        let style = (WS_POPUP | WS_CAPTION | WS_SYSMENU).0
            | (DS_MODALFRAME | DS_CENTER | DS_SETFONT) as u32;
        let mut w = Vec::new();
        push32(&mut w, style);
        push32(&mut w, 0); // extended style
        let count_at = w.len();
        w.extend([0u16, 0, 0, cx as u16, cy as u16]); // items, x, y, cx, cy
        w.extend([0u16, 0]); // no menu, default class
        w.extend(wide(title));
        w.push(9); // point size
        w.extend(wide("Segoe UI"));
        Template { w, count_at }
    }

    #[allow(clippy::too_many_arguments)]
    fn item(
        &mut self,
        class: u16,
        text: &str,
        id: i32,
        x: i16,
        y: i16,
        cx: i16,
        cy: i16,
        style: u32,
    ) {
        if self.w.len() % 2 == 1 {
            self.w.push(0); // DWORD alignment
        }
        push32(&mut self.w, (WS_CHILD | WS_VISIBLE).0 | style);
        push32(&mut self.w, 0);
        self.w
            .extend([x as u16, y as u16, cx as u16, cy as u16, id as u16]);
        self.w.extend([0xffff, class]);
        self.w.extend(wide(text));
        self.w.push(0); // no creation data
        self.w[self.count_at] += 1;
    }

    /// Copied into u32s so the template starts DWORD-aligned.
    fn finish(self) -> Vec<u32> {
        let mut out = vec![0u32; self.w.len().div_ceil(2)];
        for (i, word) in self.w.iter().enumerate() {
            out[i / 2] |= (*word as u32) << (16 * (i % 2));
        }
        out
    }
}

fn push32(w: &mut Vec<u16>, v: u32) {
    w.push(v as u16);
    w.push((v >> 16) as u16);
}
