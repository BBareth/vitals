//! Colours and layout metrics.
//!
//! Every length here is in logical pixels and gets multiplied by the monitor
//! scale at paint time, so the panel stays the same physical size on a 4K
//! display as on a 1080p one.

use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;

#[derive(Clone, Copy)]
pub struct Color(pub f32, pub f32, pub f32, pub f32);

impl Color {
    /// Hex is written the way it appears in a design tool: 0xRRGGBB.
    pub const fn hex(rgb: u32, a: f32) -> Self {
        Color(((rgb >> 16) & 0xFF) as f32 / 255.0, ((rgb >> 8) & 0xFF) as f32 / 255.0, (rgb & 0xFF) as f32 / 255.0, a)
    }

    pub fn alpha(self, a: f32) -> Self {
        Color(self.0, self.1, self.2, a)
    }

    pub fn d2d(self) -> D2D1_COLOR_F {
        // Straight alpha, not premultiplied: D2D takes brush and clear colours
        // in straight alpha and premultiplies them itself when writing to a
        // premultiplied target. Scaling here as well would darken everything.
        D2D1_COLOR_F { r: self.0, g: self.1, b: self.2, a: self.3 }
    }

    pub fn mix(self, other: Color, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        Color(
            self.0 + (other.0 - self.0) * t,
            self.1 + (other.1 - self.1) * t,
            self.2 + (other.2 - self.2) * t,
            self.3 + (other.3 - self.3) * t,
        )
    }
}

/// Fully opaque on purpose: the window's own `opacity` setting is the one
/// place translucency is controlled, so the two never multiply together.
pub const BG: Color = Color::hex(0x10131A, 1.0);
pub const BORDER: Color = Color::hex(0xFFFFFF, 0.10);
pub const DIVIDER: Color = Color::hex(0xFFFFFF, 0.06);

pub const TEXT: Color = Color::hex(0xE9ECF4, 1.0);
pub const TEXT_DIM: Color = Color::hex(0x8A92A6, 1.0);
pub const TEXT_FAINT: Color = Color::hex(0x6B7488, 1.0);

pub const TRACK: Color = Color::hex(0xFFFFFF, 0.08);

pub const GREEN: Color = Color::hex(0x4ADE80, 1.0);
pub const AMBER: Color = Color::hex(0xFBBF24, 1.0);
pub const RED: Color = Color::hex(0xF87171, 1.0);
pub const BLUE: Color = Color::hex(0x60A5FA, 1.0);
pub const VIOLET: Color = Color::hex(0xA78BFA, 1.0);
pub const CYAN: Color = Color::hex(0x38BDF8, 1.0);

/// Load colouring: calm until it matters, then unmistakable.
pub fn load_color(percent: f32) -> Color {
    if percent < 55.0 {
        GREEN
    } else if percent < 80.0 {
        GREEN.mix(AMBER, (percent - 55.0) / 25.0)
    } else {
        AMBER.mix(RED, ((percent - 80.0) / 20.0).min(1.0))
    }
}

/// Temperature colouring. The thresholds suit CPU and GPU silicon, which is
/// comfortable to about 70C and throttles somewhere near 90-100C.
pub fn temp_color(c: f32) -> Color {
    if c < 60.0 {
        GREEN
    } else if c < 80.0 {
        GREEN.mix(AMBER, (c - 60.0) / 20.0)
    } else {
        AMBER.mix(RED, ((c - 80.0) / 15.0).min(1.0))
    }
}

/// Drive controllers run cooler and throttle earlier than CPU or GPU dies.
pub fn drive_temp_color(c: f32) -> Color {
    if c < 50.0 {
        GREEN
    } else if c < 65.0 {
        GREEN.mix(AMBER, (c - 50.0) / 15.0)
    } else {
        AMBER.mix(RED, ((c - 65.0) / 15.0).min(1.0))
    }
}

pub const PAD: f32 = 12.0;
pub const CORNER: f32 = 10.0;
pub const SECTION_GAP: f32 = 8.0;
pub const BAR_H: f32 = 8.0;
/// Detail rows ("clock", "vram", ...). Sized for the 12.5pt body font.
pub const ROW_H: f32 = 18.0;
/// The big percentage-and-temperature row that heads each section.
pub const HERO_H: f32 = 33.0;
pub const HEADER_H: f32 = 17.0;
