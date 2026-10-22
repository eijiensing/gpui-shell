use clap::{Parser, Subcommand};
use gpui::Hsla;

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
}

/// Parses `#rgb`, `#rgba`, `#rrggbb`, or `#rrggbbaa` into a color.
fn parse_color(value: &str) -> Result<Hsla, String> {
    gpui::Rgba::try_from(value)
        .map(Hsla::from)
        .map_err(|err| err.to_string())
}
