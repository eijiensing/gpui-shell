//! Glyph vocabulary for the compact status dot and the overview screen.
//!
//! The compact island shows a single [`Severity`] dot to the right of the
//! clock. Detailed conditions ([`Glyph`]) are listed on the overview screen,
//! where there is enough room to render legible icons and labels.
//!
//! Several detailed glyphs can be active at once; [`GlyphSlots::active`]
//! exposes all of them for the overview.

use clap::ValueEnum;

/// Font carrying the island's text and the Nerd Font private-use glyphs.
///
/// Only the Nerd Font patched build of Caskaydia Mono is installed; asking for
/// the plain `CaskaydiaMono` family silently falls back to a proportional face
/// (DejaVu Sans), which breaks the bar's even character spacing. The "Mono"
/// variant keeps text and icons on a single advance width, so it is also the
/// right choice for the clock, notification summaries, and icons.
pub const FONT_FAMILY: &str = "CaskaydiaMono Nerd Font Mono";

/// Coarse machine health, drawn as one dot to the right of the clock.
///
/// The stages are ordered from calm to urgent so the highest one can be picked
/// with a simple maximum.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum Severity {
    /// No dot is drawn.
    #[default]
    None,
    /// Something worth knowing, but not a problem yet.
    Info,
    /// Something needs attention.
    Warn,
    /// Something is broken or dangerous.
    Critical,
}

impl Severity {
    /// Horizontal room the dot claims on the right of the clock, including its
    /// gap. The clock is centered in the remaining width, so the island only
    /// grows when this is non-zero.
    pub const SLOT_WIDTH: f32 = 12.0;

    /// Nerd Font private-use codepoint for the dot, or `None` when hidden.
    pub fn icon(self) -> Option<&'static str> {
        match self {
            Severity::None => None,
            Severity::Info => Some("\u{eb8a}"), // nf-cod-circle_small_filled
            Severity::Warn => Some("\u{eabc}"), // nf-cod-circle
            Severity::Critical => Some("\u{ea71}"), // nf-cod-circle_filled
        }
    }

    /// Width this stage adds to the compact island.
    pub fn slot_width(self) -> f32 {
        if self.icon().is_some() {
            Self::SLOT_WIDTH
        } else {
            0.0
        }
    }
}

/// Which side of the inbox a detailed glyph belongs to.
///
/// This no longer affects where icons are drawn (the bar shows only the status
/// dot), but it keeps the `glyph` command's vocabulary stable and groups the
/// conditions on the overview.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn name(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
        }
    }
}

/// A detailed condition listed on the overview screen.
///
/// The names are also the CLI values (`battery-low`, `wifi-down`, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Glyph {
    /// Battery is low. Left side.
    #[value(name = "battery-low")]
    BatteryLow,
    /// Wifi is unavailable. Left side.
    #[value(name = "wifi-down")]
    WifiDown,
    /// Bluetooth is off or unavailable. Left side.
    #[value(name = "bluetooth-off")]
    BluetoothOff,
    /// There are unread notifications. Right side.
    #[value(name = "unread")]
    Unread,
    /// Do not disturb is enabled. Right side.
    #[value(name = "dnd")]
    Dnd,
}

impl Glyph {
    /// Every glyph, in overview order: machine state first, then the inbox.
    pub const ALL: [Glyph; 5] = [
        Glyph::BatteryLow,
        Glyph::WifiDown,
        Glyph::BluetoothOff,
        Glyph::Unread,
        Glyph::Dnd,
    ];

    /// The fixed side this glyph belongs to.
    pub fn side(self) -> Side {
        match self {
            Glyph::BatteryLow | Glyph::WifiDown | Glyph::BluetoothOff => Side::Left,
            Glyph::Unread | Glyph::Dnd => Side::Right,
        }
    }

    /// Nerd Font private-use codepoint for the glyph.
    pub fn icon(self) -> &'static str {
        match self {
            Glyph::BatteryLow => "\u{f242}",
            Glyph::WifiDown => "\u{f05aa}",
            Glyph::BluetoothOff => "\u{f294}",
            Glyph::Unread => "\u{f111}",
            Glyph::Dnd => "\u{f1f7}",
        }
    }

    /// Readable name shown next to the icon on the overview.
    pub fn label(self) -> &'static str {
        match self {
            Glyph::BatteryLow => "Battery low",
            Glyph::WifiDown => "Wifi down",
            Glyph::BluetoothOff => "Bluetooth off",
            Glyph::Unread => "Unread",
            Glyph::Dnd => "Do not disturb",
        }
    }

    /// CLI-facing name, matching [`ValueEnum`].
    pub fn name(self) -> &'static str {
        match self {
            Glyph::BatteryLow => "battery-low",
            Glyph::WifiDown => "wifi-down",
            Glyph::BluetoothOff => "bluetooth-off",
            Glyph::Unread => "unread",
            Glyph::Dnd => "dnd",
        }
    }

    fn index(self) -> u16 {
        match self {
            Glyph::BatteryLow => 0,
            Glyph::WifiDown => 1,
            Glyph::BluetoothOff => 2,
            Glyph::Unread => 3,
            Glyph::Dnd => 4,
        }
    }

    fn bit(self) -> u16 {
        1 << self.index()
    }
}

/// The set of active detailed glyphs, stored as a bitmask so `Settings` stays
/// cheap to copy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GlyphSlots {
    active: u16,
}

impl GlyphSlots {
    /// Activates `glyph` without touching the other glyphs.
    pub fn set(&mut self, glyph: Glyph) {
        self.active |= glyph.bit();
    }

    /// Deactivates `glyph`.
    pub fn clear(&mut self, glyph: Glyph) {
        self.active &= !glyph.bit();
    }

    /// Deactivates every glyph on `side`.
    pub fn clear_side(&mut self, side: Side) {
        for glyph in Glyph::ALL {
            if glyph.side() == side {
                self.clear(glyph);
            }
        }
    }

    pub fn is_active(self, glyph: Glyph) -> bool {
        self.active & glyph.bit() != 0
    }

    /// Every active glyph, in overview order.
    pub fn active(self) -> impl Iterator<Item = Glyph> {
        Glyph::ALL
            .into_iter()
            .filter(move |glyph| self.is_active(*glyph))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_glyph_has_a_fixed_side() {
        assert_eq!(Glyph::BatteryLow.side(), Side::Left);
        assert_eq!(Glyph::WifiDown.side(), Side::Left);
        assert_eq!(Glyph::BluetoothOff.side(), Side::Left);
        assert_eq!(Glyph::Unread.side(), Side::Right);
        assert_eq!(Glyph::Dnd.side(), Side::Right);
    }

    #[test]
    fn active_lists_every_set_glyph() {
        let mut slots = GlyphSlots::default();
        assert_eq!(slots.active().count(), 0);

        slots.set(Glyph::BluetoothOff);
        slots.set(Glyph::Unread);
        assert_eq!(
            slots.active().collect::<Vec<_>>(),
            vec![Glyph::BluetoothOff, Glyph::Unread]
        );

        slots.clear_side(Side::Left);
        assert_eq!(slots.active().collect::<Vec<_>>(), vec![Glyph::Unread]);
    }

    #[test]
    fn clearing_an_inactive_glyph_changes_nothing() {
        let mut slots = GlyphSlots::default();
        slots.set(Glyph::Unread);
        slots.clear(Glyph::Dnd);
        assert_eq!(slots.active().collect::<Vec<_>>(), vec![Glyph::Unread]);
    }

    #[test]
    fn severity_runs_from_none_to_critical() {
        assert!(Severity::None < Severity::Info);
        assert!(Severity::Info < Severity::Warn);
        assert!(Severity::Warn < Severity::Critical);
    }

    #[test]
    fn only_a_visible_severity_claims_width() {
        assert_eq!(Severity::None.slot_width(), 0.0);
        assert_eq!(Severity::Info.slot_width(), Severity::SLOT_WIDTH);
        assert_eq!(Severity::Warn.slot_width(), Severity::SLOT_WIDTH);
        assert_eq!(Severity::Critical.slot_width(), Severity::SLOT_WIDTH);

        assert!(Severity::None.icon().is_none());
        assert!(Severity::Info.icon().is_some());
        assert!(Severity::Warn.icon().is_some());
        assert!(Severity::Critical.icon().is_some());
    }
}
