use gpui::{Global, Hsla, Pixels, Size, px};

use crate::glyphs::{FONT_FAMILY, GlyphSlots, Side};

/// Number of scalar channels the animator drives: the two size components and
/// the hue/saturation/lightness/alpha of each color.
pub const CHANNELS: usize = 10;

/// Font family used for the island's clock and notification text.
///
/// This is the Nerd Font patched build of Caskaydia Mono. The unpatched family
/// name is not installed, and fontconfig would silently substitute a
/// proportional face for it.
pub const TEXT_FONT_FAMILY: &str = FONT_FAMILY;

/// Normal-mode text size in px. The window's default rem is 16 px, so this is
/// the same as `Rems(0.5)` but keeps text measurement and rendering in sync.
pub const NORMAL_TEXT_SIZE: f32 = 8.0;

/// Compact size used by normal mode when `resize` hasn't overridden it.
pub fn default_normal_size() -> Size<Pixels> {
    Size::new(px(40.0), px(12.0))
}

/// Expanded size used by the application launcher.
pub fn launcher_size() -> Size<Pixels> {
    Size::new(px(420.0), px(600.0))
}

/// Expanded size used by the StatusNotifierItem tray.
pub fn tray_size() -> Size<Pixels> {
    Size::new(px(480.0), px(600.0))
}

/// Expanded size used by the notification center.
pub fn notifications_size() -> Size<Pixels> {
    Size::new(px(480.0), px(600.0))
}

/// A notification summary temporarily shown in place of the clock.
///
/// The compact island grows horizontally to fit the summary, so it can be read
/// without changing the bar's height.
#[derive(Clone, Debug)]
pub struct Toast {
    /// Summary text (already flattened to a single line).
    pub summary: String,
    /// Width in px of the summary at the bar's text size, including horizontal
    /// breathing room but excluding any machine glyph on the left.
    pub width: Pixels,
}

/// What the island is showing.
///
/// The mode selects which content is drawn and which target size the animation
/// heads towards. It is intentionally separate from the animation that carries
/// the island between sizes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IslandMode {
    /// Compact clock or volume indicator.
    Normal,
    /// Expanded application search.
    Launcher,
    /// Expanded StatusNotifierItem tray.
    Tray,
    /// Expanded notification center.
    Notifications,
    /// Reserved for a future calendar view; not implemented.
    #[allow(dead_code)]
    Calendar,
}

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
            size: default_normal_size(),
            background: gpui::black(),
            text_color: gpui::white(),
        }
    }
}

impl Appearance {
    /// Flattens the appearance into the scalar channels the animator drives.
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
    /// Appearance the animations are heading towards.
    pub target: Appearance,
    /// Content mode the island is (or is settling towards).
    pub mode: IslandMode,
    /// The overlay that is open, or was open most recently.
    ///
    /// Unlike [`Self::mode`] this is not reset to [`IslandMode::Normal`] when
    /// closing starts, so the overlay content can stay visible while the island
    /// contracts.
    pub overlay_mode: IslandMode,
    /// Compact size used by normal mode, excluding glyph slots; the `resize`
    /// command updates this.
    pub normal_size: Size<Pixels>,
    /// Conditions currently signalled in the two glyph slots.
    pub glyphs: GlyphSlots,
    /// Notification summary currently replacing the clock, if any.
    pub toast: Option<Toast>,
    /// Size the currently open overlay is expanding to.
    ///
    /// Kept while an overlay is open so the content fade can be derived from
    /// the animated height even after the mode has flipped back to
    /// [`IslandMode::Normal`] for the closing animation.
    pub overlay_size: Size<Pixels>,
    /// Whether an overlay currently owns the shared appearance.
    ///
    /// Set when the launcher or tray opens and cleared once its closing
    /// animation has settled. Bar surfaces stay compact while this is set, so
    /// the overlay's resize never reaches displays that are not hosting it.
    pub overlay_engaged: bool,
}

impl Global for Settings {}

impl Settings {
    pub fn new() -> Self {
        let appearance = Appearance::default();
        Self {
            window_size: appearance.size,
            current: appearance,
            target: appearance,
            mode: IslandMode::Normal,
            overlay_mode: IslandMode::Normal,
            normal_size: appearance.size,
            glyphs: GlyphSlots::default(),
            toast: None,
            overlay_size: appearance.size,
            overlay_engaged: false,
        }
    }

    /// Compact size plus room for the glyph slots that are actually visible
    /// and for an active notification toast.
    ///
    /// Inbox glyphs (unread, do-not-disturb) are hidden while a toast is on
    /// screen because the summary already conveys the notification. A machine
    /// glyph on the left keeps its slot, so the summary still fits beside it.
    pub fn normal_size_with_glyphs(&self) -> Size<Pixels> {
        let base = f32::from(self.normal_size.width);
        let width = match &self.toast {
            Some(toast) => {
                let left = self
                    .glyphs
                    .visible(Side::Left)
                    .map_or(0.0, |glyph| glyph.style().slot_width);
                base.max(left + f32::from(toast.width))
            }
            None => base + self.glyphs.extra_width(),
        };
        Size::new(px(width), self.normal_size.height)
    }

    /// Size the island settles at for the current mode.
    pub fn target_size(&self) -> Size<Pixels> {
        match self.mode {
            IslandMode::Launcher => launcher_size(),
            IslandMode::Tray => tray_size(),
            IslandMode::Notifications => notifications_size(),
            // The calendar is not implemented; it reuses the normal size for now.
            IslandMode::Normal | IslandMode::Calendar => self.normal_size_with_glyphs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::Glyph;

    #[test]
    fn each_occupied_side_adds_its_glyphs_slot_width() {
        let mut settings = Settings::new();
        let base = settings.normal_size;
        let battery = Glyph::BatteryLow.style().slot_width;
        let unread = Glyph::Unread.style().slot_width;

        assert_eq!(settings.target_size(), base);

        settings.glyphs.set(Glyph::BatteryLow);
        assert_eq!(
            settings.target_size(),
            Size::new(px(f32::from(base.width) + battery), base.height)
        );

        settings.glyphs.set(Glyph::Unread);
        assert_eq!(
            settings.target_size(),
            Size::new(px(f32::from(base.width) + battery + unread), base.height)
        );

        // A lower-priority glyph on an occupied side does not add width.
        settings.glyphs.set(Glyph::WifiDown);
        assert_eq!(
            settings.target_size(),
            Size::new(px(f32::from(base.width) + battery + unread), base.height)
        );
    }

    #[test]
    fn a_toast_fits_its_summary_and_hides_inbox_glyphs() {
        let mut settings = Settings::new();
        settings.glyphs.set(Glyph::Unread);
        let without_toast = settings.target_size();

        settings.toast = Some(Toast {
            summary: "Build finished".to_string(),
            width: px(120.0),
        });
        let with_toast = settings.target_size();

        assert_eq!(with_toast.height, without_toast.height);
        // The unread glyph makes way for the summary, so only the measured
        // text width (already above the compact clock width) is used.
        assert_eq!(with_toast.width, px(120.0));

        // A machine glyph keeps its slot beside the toast.
        settings.glyphs.set(Glyph::BatteryLow);
        let battery = Glyph::BatteryLow.style().slot_width;
        assert_eq!(settings.target_size().width, px(120.0 + battery));

        settings.toast = None;
        assert_eq!(
            settings.target_size(),
            Size::new(
                px(f32::from(without_toast.width) + battery),
                without_toast.height
            )
        );
    }
}
