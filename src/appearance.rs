use gpui::{Global, Hsla, Pixels, Size, px};

use crate::glyphs::{FONT_FAMILY, GlyphSlots, Severity};

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

/// Expanded size used by the overview hub.
pub fn overview_size() -> Size<Pixels> {
    Size::new(px(440.0), px(220.0))
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
    /// breathing room but excluding the status dot on the right.
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
    /// Expanded hub linking to the other panels.
    Overview,
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
    /// Compact size used by normal mode, excluding the status dot; the
    /// `resize` command updates this.
    pub normal_size: Size<Pixels>,
    /// Detailed conditions listed on the overview screen.
    pub glyphs: GlyphSlots,
    /// Coarse health dot drawn to the right of the clock.
    pub severity: Severity,
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
    /// Set when an overlay panel opens and cleared once its closing animation
    /// has settled. Bar surfaces stay compact while this is set, so the
    /// overlay's resize never reaches displays that are not hosting it.
    pub overlay_engaged: bool,
    /// Name of the output whose focused window is fullscreen, if any.
    ///
    /// The bar on that output slides away before niri draws the fullscreen
    /// window over the top layer. Bars on other outputs stay visible.
    pub fullscreen_output: Option<String>,
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
            severity: Severity::None,
            toast: None,
            overlay_size: appearance.size,
            overlay_engaged: false,
            fullscreen_output: None,
        }
    }

    /// Compact size plus room for the status dot and for an active notification
    /// toast.
    ///
    /// The dot is only drawn while normal mode shows the clock or the volume
    /// indicator. A toast replaces both, so it hides the dot and the island
    /// only has to fit the measured summary.
    pub fn normal_size_with_status(&self) -> Size<Pixels> {
        let base = f32::from(self.normal_size.width);
        let width = match &self.toast {
            Some(toast) => base.max(f32::from(toast.width)),
            None => base + self.severity.slot_width(),
        };
        Size::new(px(width), self.normal_size.height)
    }

    /// Size the island settles at for the current mode.
    pub fn target_size(&self) -> Size<Pixels> {
        match self.mode {
            IslandMode::Overview => overview_size(),
            IslandMode::Launcher => launcher_size(),
            IslandMode::Tray => tray_size(),
            IslandMode::Notifications => notifications_size(),
            // The calendar is not implemented; it reuses the normal size for now.
            IslandMode::Normal | IslandMode::Calendar => self.normal_size_with_status(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::Glyph;

    #[test]
    fn severity_none_keeps_the_compact_size() {
        let settings = Settings::new();
        assert_eq!(settings.target_size(), settings.normal_size);
    }

    #[test]
    fn an_active_severity_makes_room_for_the_dot() {
        let mut settings = Settings::new();
        let base = settings.normal_size;

        settings.severity = Severity::Warn;
        assert_eq!(
            settings.target_size(),
            Size::new(
                px(f32::from(base.width) + Severity::SLOT_WIDTH),
                base.height
            )
        );

        settings.severity = Severity::None;
        assert_eq!(settings.target_size(), base);
    }

    #[test]
    fn a_toast_fits_its_summary_and_hides_the_dot() {
        let mut settings = Settings::new();
        settings.severity = Severity::Critical;
        let without_toast = settings.target_size();

        settings.toast = Some(Toast {
            summary: "Build finished".to_string(),
            width: px(120.0),
        });
        let with_toast = settings.target_size();

        assert_eq!(with_toast.height, without_toast.height);
        // The dot makes way for the summary, so only the measured text width
        // is used.
        assert_eq!(with_toast.width, px(120.0));

        settings.toast = None;
        assert_eq!(settings.target_size(), without_toast);
    }

    #[test]
    fn detailed_glyphs_no_longer_widen_the_bar() {
        let mut settings = Settings::new();
        settings.glyphs.set(Glyph::BatteryLow);
        settings.glyphs.set(Glyph::Unread);

        assert_eq!(settings.target_size(), settings.normal_size);
    }
}
