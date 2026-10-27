//! The two glyph slots that flank the island's normal-mode text.
//!
//! Each side owns exactly one visible slot, but several conditions can be
//! active at once. The active glyph with the lowest [`Glyph::priority`] wins,
//! so independent producers can set and clear their own state without having
//! to coordinate with each other.

use clap::ValueEnum;

/// Font carrying the island's text and the Nerd Font private-use glyphs.
///
/// Only the Nerd Font patched build of Caskaydia Mono is installed; asking for
/// the plain `CaskaydiaMono` family silently falls back to a proportional face
/// (DejaVu Sans), which breaks the bar's even character spacing. The "Mono"
/// variant keeps text and icons on a single advance width, so it is also the
/// right choice for the clock and notification summaries.
pub const FONT_FAMILY: &str = "CaskaydiaMono Nerd Font Mono";

/// Which side of the text a glyph can occupy.
///
/// Left is fixed to machine state (hardware and connectivity); right is fixed
/// to the inbox (unread notifications and do-not-disturb).
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

/// A condition the normal-mode island can signal.
///
/// The names are also the CLI values (`battery-low`, `wifi-down`, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Glyph {
    /// Battery is low. Left slot; outranks wifi and bluetooth.
    #[value(name = "battery-low")]
    BatteryLow,
    /// Wifi is unavailable. Left slot; shown when battery is fine.
    #[value(name = "wifi-down")]
    WifiDown,
    /// Bluetooth is off or unavailable. Left slot; lowest machine priority.
    #[value(name = "bluetooth-off")]
    BluetoothOff,
    /// There are unread notifications. Right slot; shown when DND is off.
    #[value(name = "unread")]
    Unread,
    /// Do not disturb is enabled. Right slot; outranks unread.
    #[value(name = "dnd")]
    Dnd,
}

/// Per-glyph rendering adjustments.
///
/// Icon outlines fill very different amounts of their em box: a battery is
/// wide and short, bluetooth is tall and narrow, and a dot is small all over.
/// One global text size cannot make all of them legible in a 12 px island, so
/// each glyph carries its own optical configuration.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GlyphStyle {
    /// Horizontal room this glyph claims on the island.
    ///
    /// The clock is centered by the flexible space that remains between the
    /// slots, so adding a single occupied side moves it by half this value.
    /// Widths differ per glyph because icon outlines do: a battery is much
    /// wider than a notification dot.
    pub slot_width: f32,
    /// Empty strip between the island edge and the glyph.
    ///
    /// Anchoring the glyph inward, instead of centering it in the slot, keeps
    /// the outer margin consistent and leaves the remaining slack on the clock
    /// side.
    pub inset: f32,
    /// Font size in px. Text glyphs use it directly; rotated glyphs use it as
    /// the edge of their square SVG box.
    pub size: f32,
    /// How far to raise the glyph for optical centering, in px. The slot
    /// applies twice this value as bottom padding.
    pub rise: f32,
    /// Clockwise rotation in degrees. Non-zero values are rendered through an
    /// inline SVG because gpui text elements cannot be transformed.
    pub rotation: f32,
}

impl Glyph {
    /// Every glyph, ordered by side and then visually.
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

    /// Lower values win when several glyphs on the same side are active.
    ///
    /// Priorities are only compared within a side. A failing machine signal
    /// outranks a quieter one, and DND outranks unread because it explains why
    /// new notifications may not be surfacing.
    pub fn priority(self) -> u8 {
        match self {
            Glyph::BatteryLow => 0,
            Glyph::WifiDown => 1,
            Glyph::BluetoothOff => 2,
            Glyph::Dnd => 0,
            Glyph::Unread => 1,
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

    /// Per-glyph size, optical offset, and rotation.
    ///
    /// The battery is deliberately large: its outline is wide and short, so it
    /// fills the slot without exceeding the island's height. Squarer and
    /// taller icons have to be smaller to avoid looking oversized. Bluetooth
    /// is a tall rune, so it is rotated to lie across the slot instead.
    pub fn style(self) -> GlyphStyle {
        match self {
            Glyph::BatteryLow => GlyphStyle {
                slot_width: 16.0,
                inset: 8.0,
                size: 18.0,
                rise: 0.0,
                rotation: 0.0,
            },
            Glyph::WifiDown => GlyphStyle {
                slot_width: 16.0,
                inset: 8.0,
                size: 16.0,
                rise: 1.0,
                rotation: 0.0,
            },
            Glyph::BluetoothOff => GlyphStyle {
                // The rotated rune is long and shallow. Give it a wider slot
                // and fill the SVG box more completely so it does not read as
                // small next to the battery.
                slot_width: 24.0,
                inset: 8.0,
                size: 20.0,
                rise: 0.0,
                rotation: 90.0,
            },
            Glyph::Unread => GlyphStyle {
                // A dot needs very little room; a full-width slot would leave
                // it stranded far from the clock.
                slot_width: 10.0,
                inset: 5.0,
                size: 10.0,
                rise: 0.0,
                rotation: 0.0,
            },
            Glyph::Dnd => GlyphStyle {
                slot_width: 16.0,
                inset: 8.0,
                size: 16.0,
                rise: 1.0,
                rotation: 0.0,
            },
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

/// The set of active glyphs, stored as a bitmask so `Settings` stays cheap to
/// copy. Inactive glyphs still count towards the side's priority resolution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GlyphSlots {
    active: u16,
}

impl GlyphSlots {
    /// Activates `glyph` without touching the other glyphs on its side.
    pub fn set(&mut self, glyph: Glyph) {
        self.active |= glyph.bit();
    }

    /// Deactivates `glyph`, revealing the next-highest priority glyph on its
    /// side, if any.
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

    /// The glyph currently occupying `side`, if any.
    pub fn visible(self, side: Side) -> Option<Glyph> {
        Glyph::ALL
            .into_iter()
            .filter(|glyph| glyph.side() == side && self.is_active(*glyph))
            .min_by_key(|glyph| glyph.priority())
    }

    /// Whether `side`'s slot is occupied, and therefore claims width.
    #[allow(dead_code)]
    pub fn occupied(self, side: Side) -> bool {
        self.visible(side).is_some()
    }

    /// Number of occupied slots (0, 1, or 2).
    #[allow(dead_code)]
    pub fn occupied_count(self) -> usize {
        [Side::Left, Side::Right]
            .into_iter()
            .filter(|&side| self.occupied(side))
            .count()
    }

    /// Extra width the occupied slots require on top of the compact size.
    pub fn extra_width(self) -> f32 {
        [Side::Left, Side::Right]
            .into_iter()
            .filter_map(|side| self.visible(side))
            .map(|glyph| glyph.style().slot_width)
            .sum()
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
    fn higher_priority_glyph_wins_within_a_side() {
        let mut slots = GlyphSlots::default();
        slots.set(Glyph::BluetoothOff);
        slots.set(Glyph::WifiDown);
        assert_eq!(slots.visible(Side::Left), Some(Glyph::WifiDown));

        slots.set(Glyph::BatteryLow);
        assert_eq!(slots.visible(Side::Left), Some(Glyph::BatteryLow));

        slots.clear(Glyph::BatteryLow);
        assert_eq!(slots.visible(Side::Left), Some(Glyph::WifiDown));
    }

    #[test]
    fn sides_are_independent() {
        let mut slots = GlyphSlots::default();
        slots.set(Glyph::BatteryLow);
        slots.set(Glyph::Unread);

        assert_eq!(slots.occupied_count(), 2);
        assert_eq!(
            slots.extra_width(),
            Glyph::BatteryLow.style().slot_width + Glyph::Unread.style().slot_width
        );

        slots.clear_side(Side::Left);
        assert_eq!(slots.visible(Side::Left), None);
        assert_eq!(slots.visible(Side::Right), Some(Glyph::Unread));
        assert_eq!(slots.extra_width(), Glyph::Unread.style().slot_width);
    }

    #[test]
    fn clearing_an_inactive_glyph_changes_nothing() {
        let mut slots = GlyphSlots::default();
        slots.set(Glyph::Unread);
        slots.clear(Glyph::Dnd);
        assert_eq!(slots.visible(Side::Right), Some(Glyph::Unread));
    }
}
