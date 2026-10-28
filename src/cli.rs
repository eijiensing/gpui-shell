use clap::{Parser, Subcommand};
use gpui::Hsla;

use crate::glyphs::{Glyph, Side};

/// A layer-shell island whose appearance can be driven from the command line.
///
/// Running with no command starts the server; running with a command sends it
/// to the already-running server.
#[derive(Parser, Debug)]
#[command(name = "gpui-shell", about, version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Resize the island; it eases to the new size.
    Resize { width: f32, height: f32 },

    /// Set the background color, e.g. `#00ff88`.
    Color {
        #[arg(value_parser = parse_color)]
        color: Hsla,
    },

    /// Set the text color, e.g. `#ffffff`.
    TextColor {
        #[arg(value_parser = parse_color)]
        color: Hsla,
    },

    /// Expand the island into the overview hub.
    ///
    /// Shows the other panels as app-like squares in a row. Use Left/Right to
    /// select a square and Enter to open it, or press `l`, `t`, or `n` to open
    /// the launcher, tray, or notifications directly. Escape closes the hub.
    /// Invoking this while the overview is already open does nothing.
    Overview,

    /// Expand the island into an application launcher.
    ///
    /// Invoking this while the launcher is already open does nothing.
    Launcher,

    /// Expand the island into a StatusNotifierItem system tray.
    ///
    /// Enter activates an item, Right opens its DBusMenu (where applications
    /// usually expose Quit), and Tab switches to a fallback process list for
    /// applications without a tray item. Invoking this while the tray is
    /// already open does nothing.
    Tray,

    /// Expand the island into the notification center.
    ///
    /// Lists unread desktop notifications. Enter dismisses the selected one,
    /// Ctrl+D dismisses all, and Escape closes the panel. Invoking this while
    /// the center is already open does nothing. The shell receives
    /// notifications only when it owns the
    /// `org.freedesktop.Notifications` session-bus name; if another daemon
    /// owns it, the panel reports that.
    Notifications,

    /// Manage the glyphs shown in the island's side slots.
    ///
    /// The left slot is reserved for machine state (battery, wifi, bluetooth)
    /// and the right slot for the inbox (unread, do not disturb). Each side
    /// shows only its highest-priority active glyph; setting another glyph on
    /// the same side leaves the existing one active underneath it.
    ///
    /// Examples:
    ///   gpui-shell glyph set left battery-low
    ///   gpui-shell glyph set right unread
    ///   gpui-shell glyph clear left battery-low
    ///   gpui-shell glyph clear right
    Glyph {
        #[command(subcommand)]
        action: GlyphAction,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum GlyphAction {
    /// Activate a glyph in its side's slot.
    Set {
        /// Side the glyph belongs to.
        side: Side,
        /// Glyph to activate.
        glyph: Glyph,
    },

    /// Deactivate a glyph, or every glyph on a side.
    Clear {
        /// Side to clear.
        side: Side,
        /// Glyph to deactivate; omit to clear the whole side.
        glyph: Option<Glyph>,
    },
}

impl Command {
    /// Rejects a glyph command that names a glyph from the other side.
    pub fn validate(&self) -> Result<(), String> {
        let Command::Glyph { action } = self else {
            return Ok(());
        };

        let (side, glyph) = match action {
            GlyphAction::Set { side, glyph } => (*side, Some(*glyph)),
            GlyphAction::Clear { side, glyph } => (*side, *glyph),
        };

        if let Some(glyph) = glyph
            && glyph.side() != side
        {
            return Err(format!(
                "`{}` belongs in the {} slot, not the {} slot",
                glyph.name(),
                glyph.side().name(),
                side.name()
            ));
        }

        Ok(())
    }
}

/// Parses `#rgb`, `#rgba`, `#rrggbb`, or `#rrggbbaa` into a color.
fn parse_color(value: &str) -> Result<Hsla, String> {
    gpui::Rgba::try_from(value)
        .map(Hsla::from)
        .map_err(|err| err.to_string())
}
