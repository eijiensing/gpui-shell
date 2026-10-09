use gpui::{Global, Hsla, Pixels, Size, px};

/// Number of scalar channels a spring animates: the two size components and the
/// hue/saturation/lightness/alpha of each color.
pub const CHANNELS: usize = 10;

/// The visual state of the island as it is drawn.
#[derive(Clone, Copy)]
pub struct Appearance {
    pub size: Size<Pixels>,
    pub background: Hsla,
    pub text_color: Hsla,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            size: Size::new(px(48.0), px(12.0)),
            background: gpui::black(),
            text_color: gpui::white(),
        }
    }
}

impl Appearance {
    /// Flattens the appearance into the scalar channels the animator springs.
    pub fn to_channels(self) -> [f32; CHANNELS] {
        [
            f32::from(self.size.width),
            f32::from(self.size.height),
            self.background.h,
            self.background.s,
            self.background.l,
            self.background.a,
            self.text_color.h,
            self.text_color.s,
            self.text_color.l,
            self.text_color.a,
        ]
    }

    /// Rebuilds an appearance from animated channels. Color channels are
    /// wrapped/clamped so a spring overshoot can't produce an invalid color.
    pub fn from_channels(channels: [f32; CHANNELS]) -> Self {
        Self {
            size: Size::new(px(channels[0]), px(channels[1])),
            background: color(channels[2], channels[3], channels[4], channels[5]),
            text_color: color(channels[6], channels[7], channels[8], channels[9]),
        }
    }
}

fn color(h: f32, s: f32, l: f32, a: f32) -> Hsla {
    Hsla {
        h: h.rem_euclid(1.0),
        s: s.clamp(0.0, 1.0),
        l: l.clamp(0.0, 1.0),
        a: a.clamp(0.0, 1.0),
    }
}

/// Island state, shared by every open window.
pub struct Settings {
    /// Size of the Wayland surface. Applied immediately in a single resize call,
    /// sized to fit the island while it animates.
    pub window_size: Size<Pixels>,
    /// Appearance currently drawn (driven by the animator).
    pub current: Appearance,
    /// Appearance the springs are heading towards.
    pub target: Appearance,
}

impl Global for Settings {}

impl Default for Settings {
    fn default() -> Self {
        let appearance = Appearance::default();
        Self {
            window_size: appearance.size,
            current: appearance,
            target: appearance,
        }
    }
}
