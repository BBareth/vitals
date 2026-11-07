//! Tray icon, its menu, and the "start with Windows" registry toggle.

use std::ffi::c_void;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject, GetDC, HGDIOBJ,
    ReleaseDC,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyMenu, HICON, HMENU, ICONINFO, MF_CHECKED, MF_SEPARATOR,
    MF_STRING, MF_UNCHECKED, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_RIGHTALIGN, TrackPopupMenu,
};
use windows::core::PCWSTR;

pub const WM_TRAY: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;

pub const ID_TOGGLE: usize = 100;
pub const ID_RELOAD: usize = 101;
pub const ID_OPEN_CFG: usize = 102;
pub const ID_CLICKTHRU: usize = 103;
pub const ID_NEXT_MON: usize = 104;
pub const ID_AUTOSTART: usize = 105;
pub const ID_EXIT: usize = 199;

fn wz(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Builds a 32x32 bar-chart icon in code, so the binary needs no .rc resource
/// and stays a single self-contained file.
pub fn make_icon() -> Option<HICON> {
    const N: i32 = 32;
    let mut px = vec![0u32; (N * N) as usize];

    // Three ascending bars in the panel's accent colours (BGRA, premultiplied).
    let bars: [(i32, i32, u32); 3] = [(6, 20, 0x4ADE80), (13, 13, 0x60A5FA), (20, 7, 0xA78BFA)];
    for (x0, top, rgb) in bars {
        let (r, g, b) = ((rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF);
        let color = 0xFF00_0000 | (r << 16) | (g << 8) | b;
        for y in top..(N - 5) {
            for x in x0..(x0 + 6) {
                if (0..N).contains(&x) && (0..N).contains(&y) {
                    px[(y * N + x) as usize] = color;
                }
            }
        }
    }

    unsafe {
        let screen = GetDC(None);
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: N,
                biHeight: -N, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let hbmp = CreateDIBSection(Some(screen), &bi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        ReleaseDC(None, screen);
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            return None;
        }
        std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u32, px.len());

        // A 32-bit colour bitmap carries its own alpha; the mask is required by
        // the struct but ignored, so an empty one is fine.
        let hmask = CreateBitmap(N, N, 1, 1, None);
        let ii = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: hmask, hbmColor: hbmp };
        let icon = CreateIconIndirect(&ii).ok();
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteObject(HGDIOBJ(hmask.0));
        icon
    }
}

pub fn add(hwnd: HWND, icon: Option<HICON>) -> bool {
    let nid = base(hwnd, icon);
    unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool()
}

pub fn update_tip(hwnd: HWND, icon: Option<HICON>, tip: &str) {
    let mut nid = base(hwnd, icon);
    let mut t: Vec<u16> = tip.encode_utf16().collect();
    // Shell_NotifyIcon reads szTip as a NUL-terminated string, so a tooltip
    // longer than the field has to be cut short of the terminator, not at it.
    t.truncate(nid.szTip.len() - 1);
    t.push(0);
    nid.szTip[..t.len()].copy_from_slice(&t);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

pub fn remove(hwnd: HWND) {
    let nid = base(hwnd, None);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

fn base(hwnd: HWND, icon: Option<HICON>) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        ..Default::default()
    };
    if let Some(i) = icon {
        nid.hIcon = i;
    }
    let tip = wz("Vitals");
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    nid
}

pub struct MenuState {
    pub visible: bool,
    pub click_through: bool,
    pub autostart: bool,
    pub hotkey: String,
    pub monitor: u32,
    pub bridge: &'static str,
}

pub fn show_menu(hwnd: HWND, x: i32, y: i32, state: &MenuState) -> Option<usize> {
    unsafe {
        let menu: HMENU = CreatePopupMenu().ok()?;
        let flag = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };

        let toggle_label = format!("{}\t{}", if state.visible { "Hide panel" } else { "Show panel" }, state.hotkey);
        let _ = AppendMenuW(menu, MF_STRING, ID_TOGGLE, PCWSTR(wz(&toggle_label).as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(
            menu,
            MF_STRING | flag(state.click_through),
            ID_CLICKTHRU,
            PCWSTR(wz("Click-through").as_ptr()),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            ID_NEXT_MON,
            PCWSTR(wz(&format!("Move to next monitor (now {})", state.monitor)).as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ =
            AppendMenuW(menu, MF_STRING, ID_RELOAD, PCWSTR(wz(&format!("Reload config  ({})", state.bridge)).as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, ID_OPEN_CFG, PCWSTR(wz("Edit config file").as_ptr()));
        let _ = AppendMenuW(
            menu,
            MF_STRING | flag(state.autostart),
            ID_AUTOSTART,
            PCWSTR(wz("Start with Windows").as_ptr()),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, PCWSTR(wz("Exit Vitals").as_ptr()));

        // Required so the menu dismisses when the user clicks elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let picked = TrackPopupMenu(
            menu,
            TPM_RIGHTALIGN | TPM_BOTTOMALIGN | windows::Win32::UI::WindowsAndMessaging::TPM_RETURNCMD,
            x,
            y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        if picked.0 == 0 { None } else { Some(picked.0 as usize) }
    }
}

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "Vitals";

pub fn autostart_enabled() -> bool {
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
    };
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(wz(RUN_KEY).as_ptr()), Some(0), KEY_READ, &mut key).is_err() {
            return false;
        }
        let mut size = 0u32;
        let ok = RegQueryValueExW(key, PCWSTR(wz(RUN_VALUE).as_ptr()), None, None, None, Some(&mut size)).is_ok();
        let _ = RegCloseKey(key);
        ok
    }
}

pub fn set_autostart(enable: bool) {
    use windows::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_SZ, RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW,
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    // Quoted so a path containing spaces still launches.
    let cmd = format!("\"{}\"", exe.display());

    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(wz(RUN_KEY).as_ptr()), Some(0), KEY_WRITE, &mut key).is_err() {
            return;
        }
        if enable {
            let v = wz(&cmd);
            let bytes = std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2);
            let _ = RegSetValueExW(key, PCWSTR(wz(RUN_VALUE).as_ptr()), None, REG_SZ, Some(bytes));
        } else {
            let _ = RegDeleteValueW(key, PCWSTR(wz(RUN_VALUE).as_ptr()));
        }
        let _ = RegCloseKey(key);
    }
}
