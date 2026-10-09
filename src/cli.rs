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
    /// Resize the island; the background springs to the new size.
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
}

/// Parses `#rgb`, `#rgba`, `#rrggbb`, or `#rrggbbaa` into a color.
fn parse_color(value: &str) -> Result<Hsla, String> {
    gpui::Rgba::try_from(value)
        .map(Hsla::from)
        .map_err(|err| err.to_string())
}
