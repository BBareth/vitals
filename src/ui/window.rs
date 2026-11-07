//! The overlay window itself.
//!
//! It is a layered, topmost, tool window painted with Direct2D into a DIB and
//! pushed to the screen with UpdateLayeredWindow. That combination is what
//! gives real per-pixel transparency (rounded corners, soft edges) over
//! whatever is underneath, without compositing a full GPU swapchain for a
//! panel that changes once a second.

use std::ffi::c_void;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::{
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE, D2D1_ROUNDED_RECT, D2D1CreateFactory,
    ID2D1DCRenderTarget, ID2D1Factory, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::{DWRITE_FACTORY_TYPE_SHARED, DWriteCreateFactory, IDWriteFactory};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP, HDC, HGDIOBJ, HMONITOR, MONITOR_DEFAULTTOPRIMARY,
    MONITORINFO, MONITORINFOEXW, MonitorFromWindow, ReleaseDC, SelectObject,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result, w};

use super::render::{self, Fonts, Painter};
use super::theme::*;
use super::tray;
use crate::config::Config;
use crate::sensors::{Sensors, Snapshot};

const TIMER_ID: usize = 1;
const HOTKEY_ID: i32 = 0xB01D;

/// Posted by a second launch to the instance that is already running.
const WM_VITALS_TOGGLE: u32 = WM_APP + 2;

pub struct App {
    hwnd: HWND,
    cfg: Config,
    sensors: Sensors,
    snap: Snapshot,

    /// Retained purely to keep the Direct2D factory alive for as long as the
    /// render target created from it.
    _d2d: ID2D1Factory,
    dwrite: IDWriteFactory,
    rt: ID2D1DCRenderTarget,
    brush: Option<ID2D1SolidColorBrush>,
    fonts: Fonts,

    hdc_mem: HDC,
    hbmp: Option<HBITMAP>,
    old_obj: HGDIOBJ,
    surface: (i32, i32),

    icon: Option<HICON>,
    visible: bool,
    dragging: bool,
    scale: f32,
    dpi: u32,
    ticks: u64,
    hotkey_ok: bool,
    /// Set when the user drags the panel, so painting stops re-anchoring it.
    manual_pos: Option<(i32, i32)>,
}

pub fn run() -> Result<()> {
    unsafe {
        // Per-monitor v2 so the panel is crisp on a scaled display and reacts
        // when it is moved between monitors with different scaling.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // A second instance cannot register the hotkey the first one holds, and
    // would sit there silently doing nothing. Treat launching again as a
    // request to toggle the panel that is already up.
    if let Some(existing) = find_running_instance() {
        unsafe {
            let _ = PostMessageW(Some(existing), WM_VITALS_TOGGLE, WPARAM(0), LPARAM(0));
        }
        return Ok(());
    }

    let (cfg, warn) = Config::load();
    if let Some(w) = warn {
        eprintln!("vitals: {w}");
        crate::diag::log(&w);
    }

    let hinstance = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None)? };
    let class = w!("VitalsOverlayWindow");

    unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: class,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }
    }

    let mut ex_style = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
    if cfg.window.click_through {
        ex_style |= WS_EX_TRANSPARENT;
    }

    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            class,
            w!("Vitals"),
            WS_POPUP,
            0,
            0,
            10,
            10,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?
    };

    let d2d: ID2D1Factory = unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
    let dwrite: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };

    let props = D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        // Fixed at 96: the layout already multiplies every length by the
        // monitor scale, so letting D2D scale again would double-apply it.
        dpiX: 96.0,
        dpiY: 96.0,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    };
    let rt = unsafe { d2d.CreateDCRenderTarget(&props)? };

    let dpi = monitor_dpi_for(hwnd);
    let scale = (dpi as f32 / 96.0) * cfg.window.scale;
    let fonts = Fonts::new(&dwrite, scale)?;

    let hdc_mem = unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        ReleaseDC(None, screen);
        dc
    };

    let icon = tray::make_icon();
    let sensors = Sensors::new(&cfg.bridge, cfg.update.interval_ms);
    register_adapter_notifications(hwnd);

    let app = Box::new(App {
        hwnd,
        visible: !cfg.window.start_hidden,
        cfg,
        sensors,
        snap: Snapshot::default(),
        _d2d: d2d,
        dwrite,
        rt,
        brush: None,
        fonts,
        hdc_mem,
        hbmp: None,
        old_obj: HGDIOBJ::default(),
        surface: (0, 0),
        icon,
        dragging: false,
        scale,
        dpi,
        ticks: 0,
        hotkey_ok: false,
        manual_pos: None,
    });
    let ptr = Box::into_raw(app);
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize) };

    let app = unsafe { &mut *ptr };
    app.hotkey_ok = app.register_hotkey();
    let tray_ok = tray::add(hwnd, app.icon);
    #[cfg(debug_assertions)]
    eprintln!(
        "[startup] icon={} tray={} hotkey={} monitors={}",
        app.icon.is_some(),
        tray_ok,
        app.hotkey_ok,
        enumerate_monitors().len()
    );
    let _ = tray_ok;

    // First sample immediately so the panel has real numbers on screen rather
    // than a frame of zeroes.
    app.tick();
    if app.visible {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    }

    unsafe {
        SetTimer(Some(hwnd), TIMER_ID, app.cfg.update.interval_ms.max(200) as u32, None);
    }

    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

impl App {
    fn register_hotkey(&self) -> bool {
        let Some((mods, vk)) = parse_hotkey(&self.cfg.hotkey.toggle) else {
            return false;
        };
        unsafe { RegisterHotKey(Some(self.hwnd), HOTKEY_ID, mods | MOD_NOREPEAT, vk).is_ok() }
    }

    fn tick(&mut self) {
        self.snap = self.sensors.sample();
        self.ticks += 1;
        if self.visible && !self.dragging {
            self.paint();
        }
        if self.ticks % 10 == 1 {
            let g = self.snap.gpu.as_ref();
            let deg = |v: Option<f32>| v.map(|x| format!(" {x:.0}°")).unwrap_or_default();
            let mut tip = format!(
                "Vitals · {} to toggle
CPU {:.0}%{}   GPU {:.0}%{}
RAM {:.0}%",
                self.cfg.hotkey.toggle,
                self.snap.cpu.total,
                deg(self.snap.cpu.temp_c),
                g.map(|g| g.util_percent).unwrap_or(0.0),
                deg(g.and_then(|g| g.temp_c)),
                self.snap.mem.percent,
            );
            // Which sensor supplies the CPU temperature matters for trusting
            // the number, but not enough to spend a row of a small panel.
            if let Some(label) = &self.snap.cpu_temp_label {
                tip.push_str(&format!(
                    "
temp via {} · {label}",
                    self.snap.bridge_source.label()
                ));
            }
            tray::update_tip(self.hwnd, self.icon, &tip);
        }
        // Startup touches a lot of pages that are never read again (font
        // enumeration, NVML init, D2D setup). This app then idles at ~0.01% CPU,
        // so handing those pages back costs nothing and keeps the resident set
        // proportional to what a once-a-second repaint actually needs.
        if self.ticks == 3 || self.ticks.is_multiple_of(600) {
            unsafe {
                let _ = windows::Win32::System::ProcessStatus::EmptyWorkingSet(
                    windows::Win32::System::Threading::GetCurrentProcess(),
                );
            }
        }

        // Fullscreen apps can displace a topmost window; periodically reclaim
        // the position without stealing focus.
        if self.visible && self.ticks.is_multiple_of(5) {
            unsafe {
                let _ =
                    SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            }
        }
    }

    fn ensure_surface(&mut self, w: i32, h: i32) -> bool {
        if self.surface == (w, h) && self.hbmp.is_some() {
            return true;
        }
        unsafe {
            if let Some(old) = self.hbmp.take() {
                SelectObject(self.hdc_mem, self.old_obj);
                let _ = DeleteObject(HGDIOBJ(old.0));
            }
            let bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down, matching D2D's origin
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let screen = GetDC(None);
            let bmp = CreateDIBSection(Some(screen), &bi, DIB_RGB_COLORS, &mut bits, None, 0);
            ReleaseDC(None, screen);
            let Ok(bmp) = bmp else { return false };
            self.old_obj = SelectObject(self.hdc_mem, HGDIOBJ(bmp.0));
            self.hbmp = Some(bmp);
            self.surface = (w, h);
        }
        true
    }

    fn paint(&mut self) {
        let s = self.scale;
        let width_px = ((self.cfg.window.width as f32 * s).round() as i32).max(120);
        let pad = PAD * s;

        // Measure pass: no drawing, just cursor advance, to learn the height.
        let height_px = {
            let brush = match self.brush.as_ref() {
                Some(b) => b.clone(),
                None => match self.make_brush() {
                    Some(b) => b,
                    None => return,
                },
            };
            let rt_base: &windows::Win32::Graphics::Direct2D::ID2D1RenderTarget = &self.rt;
            let mut p = Painter {
                rt: rt_base,
                brush: &brush,
                fonts: &self.fonts,
                dry: true,
                s,
                y: 0.0,
                left: pad,
                width: width_px as f32 - pad * 2.0,
            };
            render::layout(&mut p, &self.snap, &self.cfg).ceil() as i32
        };
        let height_px = height_px.max(60);

        if !self.ensure_surface(width_px, height_px) {
            return;
        }

        let bind_rect = RECT { left: 0, top: 0, right: width_px, bottom: height_px };
        if unsafe { self.rt.BindDC(self.hdc_mem, &bind_rect) }.is_err() {
            return;
        }

        // Brushes belong to the render target, so they are created after the
        // first successful BindDC and reused from then on.
        let brush = match self.brush.as_ref() {
            Some(b) => b.clone(),
            None => match self.make_brush() {
                Some(b) => b,
                None => return,
            },
        };

        unsafe {
            self.rt.BeginDraw();
            self.rt.Clear(Some(&D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 }));
            render::set_quality(&self.rt);

            let rt_base: &windows::Win32::Graphics::Direct2D::ID2D1RenderTarget = &self.rt;
            let corner = CORNER * s;
            let body = D2D1_ROUNDED_RECT {
                rect: windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F {
                    left: 0.0,
                    top: 0.0,
                    right: width_px as f32,
                    bottom: height_px as f32,
                },
                radiusX: corner,
                radiusY: corner,
            };
            brush.SetColor(&BG.d2d());
            rt_base.FillRoundedRectangle(&body, &brush);

            let inset = 0.5;
            let outline = D2D1_ROUNDED_RECT {
                rect: windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F {
                    left: inset,
                    top: inset,
                    right: width_px as f32 - inset,
                    bottom: height_px as f32 - inset,
                },
                radiusX: corner,
                radiusY: corner,
            };
            brush.SetColor(&BORDER.d2d());
            rt_base.DrawRoundedRectangle(&outline, &brush, 1.0, None);

            let mut p = Painter {
                rt: rt_base,
                brush: &brush,
                fonts: &self.fonts,
                dry: false,
                s,
                y: 0.0,
                left: pad,
                width: width_px as f32 - pad * 2.0,
            };
            render::layout(&mut p, &self.snap, &self.cfg);

            let _ = self.rt.EndDraw(None, None);
        }

        let (x, y) = match self.manual_pos {
            Some(p) => p,
            None => self.anchor_position(width_px, height_px),
        };

        let alpha = (self.cfg.window.opacity.clamp(0.05, 1.0) * 255.0) as u8;
        let blend = windows::Win32::Graphics::Gdi::BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let pt_dst = POINT { x, y };
        let size = SIZE { cx: width_px, cy: height_px };
        let pt_src = POINT { x: 0, y: 0 };

        unsafe {
            let screen = GetDC(None);
            let _ = UpdateLayeredWindow(
                self.hwnd,
                Some(screen),
                Some(&pt_dst),
                Some(&size),
                Some(self.hdc_mem),
                Some(&pt_src),
                COLORREF(0),
                Some(&blend),
                windows::Win32::UI::WindowsAndMessaging::ULW_ALPHA,
            );
            ReleaseDC(None, screen);
        }
    }

    fn make_brush(&mut self) -> Option<ID2D1SolidColorBrush> {
        // A 1x1 bind is enough to make the target valid for resource creation
        // before the first real paint.
        let r = RECT { left: 0, top: 0, right: 1, bottom: 1 };
        unsafe {
            if self.hbmp.is_none() && !self.ensure_surface(1, 1) {
                return None;
            }
            let _ = self.rt.BindDC(self.hdc_mem, &r);
            let b = self.rt.CreateSolidColorBrush(&D2D1_COLOR_F { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }, None).ok()?;
            self.brush = Some(b.clone());
            Some(b)
        }
    }

    fn anchor_position(&self, w: i32, h: i32) -> (i32, i32) {
        let monitors = enumerate_monitors();
        if monitors.is_empty() {
            return (0, 0);
        }
        let idx = (self.cfg.window.monitor.max(1) as usize - 1).min(monitors.len() - 1);
        let m = monitors[idx].rect;
        let mx = (self.cfg.window.margin_x as f32 * self.scale) as i32;
        let my = (self.cfg.window.margin_y as f32 * self.scale) as i32;

        let (x, y) = match self.cfg.window.anchor.as_str() {
            "top-left" => (m.left + mx, m.top + my),
            "bottom-left" => (m.left + mx, m.bottom - h - my),
            "bottom-right" => (m.right - w - mx, m.bottom - h - my),
            "center" => (m.left + (m.right - m.left - w) / 2, m.top + (m.bottom - m.top - h) / 2),
            _ => (m.right - w - mx, m.top + my), // top-right
        };
        (x, y)
    }

    fn set_visible(&mut self, on: bool) {
        self.visible = on;
        unsafe {
            if on {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                let _ =
                    SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            } else {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
        if on {
            self.paint();
        }
    }

    fn set_click_through(&mut self, on: bool) {
        self.cfg.window.click_through = on;
        unsafe {
            let mut ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) as u32;
            if on {
                ex |= WS_EX_TRANSPARENT.0;
            } else {
                ex &= !WS_EX_TRANSPARENT.0;
            }
            SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex as isize);
        }
        crate::config::patch_scalar("click_through", if on { "true" } else { "false" });
    }

    fn next_monitor(&mut self) {
        let n = enumerate_monitors().len().max(1) as u32;
        self.cfg.window.monitor = self.cfg.window.monitor % n + 1;
        self.manual_pos = None;
        crate::config::patch_scalar("monitor", &self.cfg.window.monitor.to_string());
        self.refresh_scale();
        self.paint();
    }

    fn refresh_scale(&mut self) {
        let monitors = enumerate_monitors();
        if monitors.is_empty() {
            return;
        }
        let idx = (self.cfg.window.monitor.max(1) as usize - 1).min(monitors.len() - 1);
        let dpi = monitors[idx].dpi;
        if dpi != self.dpi {
            self.dpi = dpi;
            self.scale = (dpi as f32 / 96.0) * self.cfg.window.scale;
            // Font sizes are baked into the text formats, so they have to be
            // rebuilt whenever the effective scale changes.
            if let Ok(f) = Fonts::new(&self.dwrite, self.scale) {
                self.fonts = f;
            }
        }
    }

    fn reload_config(&mut self) {
        let (cfg, warn) = Config::load();
        if let Some(w) = warn {
            crate::diag::log(&w);
        }
        let hotkey_changed = cfg.hotkey.toggle != self.cfg.hotkey.toggle;
        let interval_changed = cfg.update.interval_ms != self.cfg.update.interval_ms;
        let click = cfg.window.click_through;
        self.cfg = cfg;
        self.manual_pos = None;

        if hotkey_changed {
            unsafe {
                let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_ID);
            }
            self.hotkey_ok = self.register_hotkey();
        }
        if interval_changed {
            self.sensors.gpu.set_interval(self.cfg.update.interval_ms);
            unsafe {
                SetTimer(Some(self.hwnd), TIMER_ID, self.cfg.update.interval_ms.max(200) as u32, None);
            }
        }
        self.set_click_through(click);
        self.refresh_scale();
        self.surface = (0, 0); // force the DIB to be rebuilt at the new size
        self.paint();
    }

    fn save_dragged_position(&mut self) {
        let mut r = RECT::default();
        if unsafe { GetWindowRect(self.hwnd, &mut r) }.is_err() {
            return;
        }
        self.manual_pos = Some((r.left, r.top));

        // Convert the dropped position back into margins from the configured
        // anchor so the config stays human-readable.
        let monitors = enumerate_monitors();
        if monitors.is_empty() {
            return;
        }
        let idx = (self.cfg.window.monitor.max(1) as usize - 1).min(monitors.len() - 1);
        let m = monitors[idx].rect;
        let w = r.right - r.left;
        let h = r.bottom - r.top;
        let (mx, my) = match self.cfg.window.anchor.as_str() {
            "top-left" => (r.left - m.left, r.top - m.top),
            "bottom-left" => (r.left - m.left, m.bottom - r.top - h),
            "bottom-right" => (m.right - r.left - w, m.bottom - r.top - h),
            "center" => (0, 0),
            _ => (m.right - r.left - w, r.top - m.top),
        };
        let inv = 1.0 / self.scale.max(0.1);
        self.cfg.window.margin_x = (mx as f32 * inv).round() as i32;
        self.cfg.window.margin_y = (my as f32 * inv).round() as i32;
        crate::config::patch_scalar("margin_x", &self.cfg.window.margin_x.to_string());
        crate::config::patch_scalar("margin_y", &self.cfg.window.margin_y.to_string());
        self.manual_pos = None;
    }
}

impl Drop for App {
    fn drop(&mut self) {
        unsafe {
            if let Some(b) = self.hbmp.take() {
                SelectObject(self.hdc_mem, self.old_obj);
                let _ = DeleteObject(HGDIOBJ(b.0));
            }
            let _ = DeleteDC(self.hdc_mem);
        }
    }
}

#[derive(Clone, Copy)]
pub struct MonitorEntry {
    pub rect: RECT,
    pub primary: bool,
    pub dpi: u32,
}

/// Monitors ordered as "1 = primary, then left to right". Raw enumeration order
/// is not stable across driver and layout changes, and this ordering is what a
/// person picking a number in a config file expects.
pub fn enumerate_monitors() -> Vec<MonitorEntry> {
    unsafe extern "system" fn cb(h: HMONITOR, _dc: HDC, _r: *mut RECT, data: LPARAM) -> windows::core::BOOL {
        let list = unsafe { &mut *(data.0 as *mut Vec<MonitorEntry>) };
        let mut mi = MONITORINFOEXW::default();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        let ok = unsafe { windows::Win32::Graphics::Gdi::GetMonitorInfoW(h, &mut mi as *mut _ as *mut MONITORINFO) };
        if ok.as_bool() {
            let mut dx = 96u32;
            let mut dy = 96u32;
            let _ = unsafe { GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
            list.push(MonitorEntry {
                rect: mi.monitorInfo.rcMonitor,
                primary: mi.monitorInfo.dwFlags & 1 != 0, // MONITORINFOF_PRIMARY
                dpi: dx.max(48),
            });
        }
        true.into()
    }

    let mut list: Vec<MonitorEntry> = Vec::new();
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::EnumDisplayMonitors(
            None,
            None,
            Some(cb),
            LPARAM(&mut list as *mut _ as isize),
        );
    }
    list.sort_by_key(|m| (!m.primary, m.rect.left, m.rect.top));
    list
}

/// Returns the window of an instance that is already running, if any.
fn find_running_instance() -> Option<HWND> {
    let hwnd = unsafe { FindWindowW(w!("VitalsOverlayWindow"), None) };
    match hwnd {
        Ok(h) if !h.is_invalid() => Some(h),
        _ => None,
    }
}

fn monitor_dpi_for(hwnd: HWND) -> u32 {
    unsafe {
        let h = MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY);
        let mut dx = 96u32;
        let mut dy = 96u32;
        let _ = GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        dx.max(48)
    }
}

/// Parses strings like "Win+Alt+O" or "Ctrl+Shift+F12".
fn parse_hotkey(spec: &str) -> Option<(HOT_KEY_MODIFIERS, u32)> {
    let mut mods = HOT_KEY_MODIFIERS(0);
    let mut vk: Option<u32> = None;

    for part in spec.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "super" | "meta" => mods |= MOD_WIN,
            key => {
                let k = key.to_ascii_uppercase();
                let code = if k.len() == 1 {
                    let c = k.as_bytes()[0];
                    match c {
                        b'A'..=b'Z' | b'0'..=b'9' => Some(c as u32),
                        _ => None,
                    }
                } else if let Some(n) = k.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
                    (1..=24).contains(&n).then(|| 0x6F + n) // VK_F1 = 0x70
                } else {
                    None
                };
                vk = code.or(vk);
            }
        }
    }
    // A bare key with no modifier would swallow that key system-wide.
    if mods.0 == 0 { None } else { vk.map(|v| (mods, v)) }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut App;
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    }
    let app = unsafe { &mut *ptr };

    match msg {
        WM_TIMER if wp.0 == TIMER_ID => {
            app.tick();
            LRESULT(0)
        }
        WM_HOTKEY if wp.0 as i32 == HOTKEY_ID => {
            let v = !app.visible;
            app.set_visible(v);
            LRESULT(0)
        }
        WM_VITALS_TOGGLE => {
            let v = !app.visible;
            app.set_visible(v);
            LRESULT(0)
        }
        tray::WM_TRAY => {
            let event = (lp.0 as u32) & 0xFFFF;
            match event {
                WM_LBUTTONUP => {
                    let v = !app.visible;
                    app.set_visible(v);
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    let mut pt = POINT::default();
                    let _ = unsafe { GetCursorPos(&mut pt) };
                    let state = tray::MenuState {
                        visible: app.visible,
                        click_through: app.cfg.window.click_through,
                        autostart: tray::autostart_enabled(),
                        hotkey: if app.hotkey_ok { app.cfg.hotkey.toggle.clone() } else { "hotkey unavailable".into() },
                        monitor: app.cfg.window.monitor,
                        bridge: app.snap.bridge_source.label(),
                    };
                    if let Some(id) = tray::show_menu(hwnd, pt.x, pt.y, &state) {
                        handle_command(app, id);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_NCHITTEST => {
            // With click-through off the whole panel acts as a title bar, so it
            // can be dragged anywhere without a visible caption.
            if app.cfg.window.click_through {
                unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
            } else {
                LRESULT(HTCAPTION as isize)
            }
        }
        WM_ENTERSIZEMOVE => {
            app.dragging = true;
            LRESULT(0)
        }
        WM_EXITSIZEMOVE => {
            app.dragging = false;
            app.save_dragged_position();
            LRESULT(0)
        }
        WM_DEVICECHANGE => {
            // A display adapter arriving or leaving is what a driver install
            // or GPU reset looks like here. The NVML helper still has the old
            // driver's DLL mapped, so it is replaced before it polls again.
            let event = wp.0 as u32;
            if (event == DBT_DEVICEARRIVAL || event == DBT_DEVICEREMOVECOMPLETE) && lp.0 != 0 {
                let hdr = unsafe { &*(lp.0 as *const DEV_BROADCAST_HDR) };
                if hdr.dbch_devicetype == DBT_DEVTYP_DEVICEINTERFACE {
                    let what =
                        if event == DBT_DEVICEARRIVAL { "display adapter arrived" } else { "display adapter removed" };
                    app.sensors.gpu.restart(what);
                }
            }
            LRESULT(1)
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            app.refresh_scale();
            app.surface = (0, 0);
            app.paint();
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
                KillTimer(Some(hwnd), TIMER_ID).ok();
            }
            tray::remove(hwnd);
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(ptr));
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Asks for WM_DEVICECHANGE whenever a display adapter interface comes or
/// goes. The registration lives as long as the window, so the handle is never
/// unregistered explicitly.
fn register_adapter_notifications(hwnd: HWND) {
    // GUID_DEVINTERFACE_DISPLAY_ADAPTER
    const DISPLAY_ADAPTER: windows::core::GUID = windows::core::GUID::from_u128(0x5b45201d_f2f2_4f3b_85bb_30ff1f953599);
    let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
        dbcc_size: std::mem::size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
        dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE.0,
        dbcc_classguid: DISPLAY_ADAPTER,
        ..Default::default()
    };
    let r = unsafe {
        RegisterDeviceNotificationW(
            windows::Win32::Foundation::HANDLE(hwnd.0),
            &filter as *const _ as *const core::ffi::c_void,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        )
    };
    if let Err(e) = r {
        crate::diag::log(&format!("RegisterDeviceNotification failed: {e}"));
    }
}

fn handle_command(app: &mut App, id: usize) {
    match id {
        tray::ID_TOGGLE => {
            let v = !app.visible;
            app.set_visible(v);
        }
        tray::ID_CLICKTHRU => {
            let v = !app.cfg.window.click_through;
            app.set_click_through(v);
        }
        tray::ID_NEXT_MON => app.next_monitor(),
        tray::ID_RELOAD => app.reload_config(),
        tray::ID_OPEN_CFG => {
            let path = crate::config::config_path();
            let p: Vec<u16> = path.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
            unsafe {
                windows::Win32::UI::Shell::ShellExecuteW(
                    None,
                    w!("open"),
                    PCWSTR(p.as_ptr()),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    SW_SHOWNORMAL,
                );
            }
        }
        tray::ID_AUTOSTART => {
            let on = !tray::autostart_enabled();
            tray::set_autostart(on);
        }
        tray::ID_EXIT => unsafe {
            let _ = DestroyWindow(app.hwnd);
        },
        _ => {}
    }
}
