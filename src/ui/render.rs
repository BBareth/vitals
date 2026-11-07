//! Direct2D drawing.
//!
//! Layout runs twice per frame: a dry pass that only advances the cursor to
//! find the panel height, then a wet pass that draws. Sharing one code path
//! means the measured height and the painted content can never disagree.

use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_ROUNDED_RECT, ID2D1RenderTarget, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_FONT_WEIGHT_SEMI_BOLD, DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_WORD_WRAPPING_NO_WRAP,
    IDWriteFactory, IDWriteTextFormat,
};
use windows::core::PCWSTR;

use super::theme::*;
use crate::config::Config;
use crate::sensors::{Snapshot, cpu::CoreKind, fmt_bytes, fmt_rate};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

pub struct Fonts {
    pub title: IDWriteTextFormat,
    pub hero: IDWriteTextFormat,
    pub small: IDWriteTextFormat,
    pub tiny: IDWriteTextFormat,
}

impl Fonts {
    pub fn new(dw: &IDWriteFactory, scale: f32) -> windows::core::Result<Self> {
        let make = |size: f32, weight: DWRITE_FONT_WEIGHT| -> windows::core::Result<IDWriteTextFormat> {
            let family: Vec<u16> = "Segoe UI".encode_utf16().chain(std::iter::once(0)).collect();
            let locale: Vec<u16> = "en-us".encode_utf16().chain(std::iter::once(0)).collect();
            let f = unsafe {
                dw.CreateTextFormat(
                    PCWSTR(family.as_ptr()),
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size * scale,
                    PCWSTR(locale.as_ptr()),
                )?
            };
            unsafe {
                // Every string in the panel is a single short line; wrapping
                // would silently push content out of the measured height.
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
            }
            Ok(f)
        };
        Ok(Self {
            title: make(12.0, DWRITE_FONT_WEIGHT_SEMI_BOLD)?,
            hero: make(26.0, DWRITE_FONT_WEIGHT_NORMAL)?,
            small: make(12.5, DWRITE_FONT_WEIGHT_NORMAL)?,
            tiny: make(11.0, DWRITE_FONT_WEIGHT_NORMAL)?,
        })
    }
}

pub struct Painter<'a> {
    pub rt: &'a ID2D1RenderTarget,
    pub brush: &'a ID2D1SolidColorBrush,
    pub fonts: &'a Fonts,
    /// Measure-only pass: advance the cursor but issue no draw calls.
    pub dry: bool,
    pub s: f32,
    pub y: f32,
    pub left: f32,
    pub width: f32,
}

impl<'a> Painter<'a> {
    fn set(&self, c: Color) {
        if !self.dry {
            unsafe { self.brush.SetColor(&c.d2d() as *const D2D1_COLOR_F) };
        }
    }

    fn fill_rrect(&self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Color) {
        if self.dry || w <= 0.0 || h <= 0.0 {
            return;
        }
        self.set(c);
        let rr = D2D1_ROUNDED_RECT {
            rect: D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h },
            radiusX: r,
            radiusY: r,
        };
        unsafe { self.rt.FillRoundedRectangle(&rr, self.brush) };
    }

    fn fill_rect(&self, x: f32, y: f32, w: f32, h: f32, c: Color) {
        if self.dry || w <= 0.0 || h <= 0.0 {
            return;
        }
        self.set(c);
        let r = D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h };
        unsafe { self.rt.FillRectangle(&r, self.brush) };
    }

    /// Draws one line of text inside a band of `h` logical pixels, vertically
    /// centred (the formats all use centre paragraph alignment).
    #[allow(clippy::too_many_arguments)]
    fn text(
        &self,
        s: &str,
        fmt: &IDWriteTextFormat,
        c: Color,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        align: DWRITE_TEXT_ALIGNMENT,
    ) {
        if self.dry || s.is_empty() {
            return;
        }
        self.set(c);
        let buf = wide(s);
        unsafe {
            let _ = fmt.SetTextAlignment(align);
            let rect = D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h };
            self.rt.DrawText(
                &buf,
                fmt,
                &rect,
                self.brush,
                windows::Win32::Graphics::Direct2D::D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    /// Section heading with a hairline rule above it.
    fn section(&mut self, title: &str, accent: Color, first: bool) {
        if !first {
            self.y += SECTION_GAP * self.s;
            self.fill_rect(self.left, self.y, self.width, 1.0, DIVIDER);
            self.y += SECTION_GAP * self.s;
        }
        let h = HEADER_H * self.s;
        // A short accent tab makes each section identifiable at a glance from
        // across the room, which is the whole point of a second-monitor panel.
        self.fill_rrect(self.left, self.y + 4.0 * self.s, 3.0 * self.s, h - 8.0 * self.s, 1.5 * self.s, accent);
        self.text(
            title,
            &self.fonts.title,
            TEXT_DIM,
            self.left + 9.0 * self.s,
            self.y,
            self.width - 9.0 * self.s,
            h,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        );
        self.y += h;
    }

    /// The headline of every section: a large percentage on the left and, where
    /// the component has a sensor for it, a large temperature on the right.
    fn hero(&mut self, value: &str, value_color: Color, right: Option<(String, Color)>) {
        let h = HERO_H * self.s;
        self.text(
            value,
            &self.fonts.hero,
            value_color,
            self.left,
            self.y,
            self.width,
            h,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        );
        if let Some((r, c)) = right {
            self.text(&r, &self.fonts.hero, c, self.left, self.y, self.width, h, DWRITE_TEXT_ALIGNMENT_TRAILING);
        }
        self.y += h;
    }

    fn bar(&mut self, frac: f32, color: Color, height: f32) {
        let h = height * self.s;
        let r = h * 0.5;
        self.fill_rrect(self.left, self.y, self.width, h, r, TRACK);
        let w = (self.width * frac.clamp(0.0, 1.0)).max(if frac > 0.0 { h } else { 0.0 });
        self.fill_rrect(self.left, self.y, w, h, r, color);
        self.y += h;
    }

    /// A row of narrow bars, one per logical processor.
    fn core_bars(&mut self, values: &[f32], kinds: &[CoreKind], height: f32) {
        let h = height * self.s;
        if values.is_empty() {
            return;
        }
        let n = values.len();
        // Hybrid parts get a visual break between the P and E groups.
        let split = kinds.iter().position(|k| *k == CoreKind::Efficiency).filter(|p| *p > 0 && *p < n);
        let gap = 2.0 * self.s;
        let group_gap = if split.is_some() { 7.0 * self.s } else { 0.0 };
        let total_gap = gap * (n as f32 - 1.0) + group_gap;
        let bw = ((self.width - total_gap) / n as f32).max(1.0);

        let mut x = self.left;
        for (i, v) in values.iter().enumerate() {
            if Some(i) == split {
                x += group_gap;
            }
            let fill = (v / 100.0).clamp(0.0, 1.0) * h;
            self.fill_rrect(x, self.y, bw, h, 1.5 * self.s, TRACK);
            if fill > 0.5 {
                self.fill_rrect(x, self.y + h - fill, bw, fill, 1.5 * self.s, load_color(*v));
            }
            x += bw + gap;
        }
        self.y += h;

        if split.is_some() {
            let lh = 12.0 * self.s;
            self.text(
                "P-CORES",
                &self.fonts.tiny,
                TEXT_FAINT,
                self.left,
                self.y + 1.0 * self.s,
                self.width,
                lh,
                DWRITE_TEXT_ALIGNMENT_LEADING,
            );
            self.text(
                "E-CORES",
                &self.fonts.tiny,
                TEXT_FAINT,
                self.left,
                self.y + 1.0 * self.s,
                self.width,
                lh,
                DWRITE_TEXT_ALIGNMENT_TRAILING,
            );
            self.y += lh + self.s;
        }
    }

    fn kv(&mut self, k: &str, v: &str, v_color: Color) {
        let h = ROW_H * self.s;
        self.text(k, &self.fonts.small, TEXT_FAINT, self.left, self.y, self.width, h, DWRITE_TEXT_ALIGNMENT_LEADING);
        self.text(v, &self.fonts.small, v_color, self.left, self.y, self.width, h, DWRITE_TEXT_ALIGNMENT_TRAILING);
        self.y += h;
    }

    fn gap(&mut self, n: f32) {
        self.y += n * self.s;
    }
}

fn pct(v: f32) -> String {
    format!("{v:.0}%")
}

fn degrees(t: f32) -> String {
    format!("{t:.0}\u{00B0}")
}

/// The right-hand side of a hero row: a temperature, or a dash when nothing is
/// supplying one.
fn hero_temp(t: Option<f32>, color: fn(f32) -> Color) -> Option<(String, Color)> {
    match t {
        Some(v) => Some((degrees(v), color(v))),
        None => Some(("\u{2014}".into(), TEXT_FAINT)),
    }
}

/// Draws the whole panel and returns the content height in physical pixels.
pub fn layout(p: &mut Painter, snap: &Snapshot, cfg: &Config) -> f32 {
    p.gap(PAD);
    let mut first = true;

    if cfg.panels.cpu {
        p.section("CPU", BLUE, first);
        first = false;
        p.gap(1.0);
        p.hero(&pct(snap.cpu.total), load_color(snap.cpu.total), hero_temp(snap.cpu.temp_c, temp_color));
        p.gap(2.0);
        p.bar(snap.cpu.total / 100.0, load_color(snap.cpu.total), BAR_H);
        p.gap(7.0);

        if cfg.panels.cpu_cores && !snap.cpu.per_thread.is_empty() {
            p.core_bars(&snap.cpu.per_thread, &snap.cpu.thread_kind, 20.0);
            p.gap(5.0);
        }
        p.kv("clock", &format!("{:.2} GHz", snap.cpu.clock_mhz / 1000.0), TEXT);
        // Shown only while nothing is supplying a temperature, so the dash above
        // has a visible explanation instead of looking broken.
        if snap.cpu.temp_c.is_none() {
            p.kv("temp", "no sensor source", TEXT_FAINT);
        }
        if let Some(b) = snap.board_temp_c {
            p.kv("board", &degrees(b), drive_temp_color(b));
        }
    }

    if cfg.panels.gpu {
        p.section("GPU", VIOLET, first);
        first = false;
        p.gap(1.0);
        match &snap.gpu {
            Some(g) => {
                p.hero(&pct(g.util_percent), load_color(g.util_percent), hero_temp(g.temp_c, temp_color));
                p.gap(2.0);
                p.bar(g.util_percent / 100.0, load_color(g.util_percent), BAR_H);
                p.gap(7.0);

                p.kv("vram", &format!("{} / {}", fmt_bytes(g.mem_used_bytes), fmt_bytes(g.mem_total_bytes)), TEXT);
                p.gap(1.0);
                p.bar(g.mem_percent / 100.0, VIOLET.alpha(0.8), 4.0);
                p.gap(6.0);
                if let Some(c) = g.core_clock_mhz {
                    p.kv("clock", &format!("{:.2} GHz", c as f32 / 1000.0), TEXT);
                }
                match (g.power_w, g.power_limit_w) {
                    (Some(w), Some(lim)) => p.kv("power", &format!("{w:.0} / {lim:.0} W"), TEXT),
                    (Some(w), None) => p.kv("power", &format!("{w:.0} W"), TEXT),
                    _ => {}
                }
                if let Some(f) = g.fan_percent {
                    p.kv("fan", &pct(f), TEXT);
                }
            }
            None => {
                // NVML messages are long; the first clause carries the reason.
                let why = snap
                    .gpu_error
                    .as_deref()
                    .map(|e| e.split([':', '(']).next().unwrap_or(e).trim().to_string())
                    .unwrap_or_else(|| "not detected".into());
                p.kv("nvidia", &why, TEXT_FAINT);
            }
        }
    }

    if cfg.panels.memory {
        p.section("MEMORY", CYAN, first);
        first = false;
        p.gap(1.0);
        p.hero(&pct(snap.mem.percent), load_color(snap.mem.percent), None);
        p.gap(2.0);
        p.bar(snap.mem.percent / 100.0, load_color(snap.mem.percent), BAR_H);
    }

    if cfg.panels.disk {
        p.section("STORAGE", AMBER, first);
        first = false;
        p.gap(1.0);
        // The headline temperature is the hottest drive, since that is the one
        // that would throttle first.
        let hottest = snap
            .disk
            .drives
            .iter()
            .filter_map(|d| d.temp_c)
            .fold(None::<f32>, |acc, t| Some(acc.map_or(t, |a: f32| a.max(t))));
        p.hero(&pct(snap.disk.busy_percent), load_color(snap.disk.busy_percent), hero_temp(hottest, drive_temp_color));
        p.gap(2.0);
        p.bar(snap.disk.busy_percent / 100.0, load_color(snap.disk.busy_percent), BAR_H);

        // With a single drive the headline temperature already says everything.
        if snap.disk.drives.len() > 1 {
            p.gap(7.0);
            for d in &snap.disk.drives {
                let value = d.temp_c.map(degrees).unwrap_or_else(|| "\u{2014}".into());
                p.kv(&short_drive_name(&d.model), &value, d.temp_c.map(drive_temp_color).unwrap_or(TEXT_FAINT));
            }
        }
    }

    if cfg.panels.network {
        p.section("NETWORK", GREEN, first);
        p.gap(3.0);
        p.kv("down", &fmt_rate(snap.net.down_bps), GREEN);
        p.kv("up", &fmt_rate(snap.net.up_bps), BLUE);
    }

    p.gap(PAD);
    p.y
}

fn short_drive_name(model: &str) -> String {
    let m = model.replace("Samsung SSD ", "").replace("SAMSUNG ", "").replace('_', " ");
    let m = m.trim();
    if m.len() > 17 { m[..17].trim_end().to_string() } else { m.to_string() }
}

/// Antialiasing is left on for everything; the panel is small and repaints
/// once a second, so quality costs nothing measurable here.
pub fn set_quality(rt: &ID2D1RenderTarget) {
    unsafe { rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE) };
}
