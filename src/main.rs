use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

use chrono::Local;
use gpui::layer_shell::{Anchor, LayerShellOptions};
use gpui::{
    App, Context, DisplayId, Global, Hsla, Pixels, Rems, Render, Size, Window, WindowBounds,
    WindowKind, WindowOptions, div, ease_in_out, point, prelude::*, px,
};
use gpui_platform::application;

/// How long a setting change takes to settle, and the tick rate of the animator.
const ANIMATION_DURATION: Duration = Duration::from_millis(220);
const ANIMATION_FRAME: Duration = Duration::from_millis(16);

/// Linear interpolation towards a target value with a normalized progress `t`.
trait Lerp: Copy {
    fn lerp(self, toward: Self, t: f32) -> Self;
}

impl Lerp for f32 {
    fn lerp(self, toward: Self, t: f32) -> Self {
        self + (toward - self) * t
    }
}

impl Lerp for Pixels {
    fn lerp(self, toward: Self, t: f32) -> Self {
        px(f32::from(self).lerp(f32::from(toward), t))
    }
}

impl Lerp for Hsla {
    fn lerp(self, toward: Self, t: f32) -> Self {
        Self {
            h: self.h.lerp(toward.h, t),
            s: self.s.lerp(toward.s, t),
            l: self.l.lerp(toward.l, t),
            a: self.a.lerp(toward.a, t),
        }
    }
}

/// The visual state of the island.
#[derive(Clone, Copy)]
struct Appearance {
    window_size: Size<Pixels>,
    background: Hsla,
    text_color: Hsla,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            window_size: Size::new(px(48.0), px(12.0)),
            background: gpui::black(),
            text_color: gpui::white(),
        }
    }
}

impl Lerp for Appearance {
    fn lerp(self, toward: Self, t: f32) -> Self {
        Self {
            window_size: Size::new(
                self.window_size.width.lerp(toward.window_size.width, t),
                self.window_size.height.lerp(toward.window_size.height, t),
            ),
            background: self.background.lerp(toward.background, t),
            text_color: self.text_color.lerp(toward.text_color, t),
        }
    }
}

/// Current and target appearance, shared by every open window.
///
/// `current` is continuously interpolated towards `target` by the animator task,
/// so command handlers only update `target` and bump `generation`.
struct Settings {
    current: Appearance,
    target: Appearance,
    /// Bumped on every command so the animator can restart its tween.
    generation: u64,
}

impl Global for Settings {}

impl Default for Settings {
    fn default() -> Self {
        let appearance = Appearance::default();
        Self {
            current: appearance,
            target: appearance,
            generation: 0,
        }
    }
}

struct Island {
    time: String,
}

impl Island {
    fn new(cx: &mut Context<Self>) -> Self {
        let island = Self {
            time: Local::now().format("%H:%M").to_string(),
        };

        island.start_timer(cx);

        island
    }

    fn start_timer(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |island, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;

                island
                    .update(cx, |island, cx| {
                        island.time = Local::now().format("%H:%M").to_string();
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }
}

impl Render for Island {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let appearance = cx.global::<Settings>().current;

        div()
            .w(appearance.window_size.width)
            .h(appearance.window_size.height)
            .bg(appearance.background)
            .text_color(appearance.text_color)
            .text_size(Rems(0.5))
            .rounded_b_lg()
            .font_family("CaskaydiaMono")
            .flex()
            .items_center()
            .justify_center()
            .child(self.time.clone())
    }
}

/// A command sent from a client invocation to the running bar server.
#[derive(Debug, Clone, Copy)]
enum Command {
    Resize { width: f32, height: f32 },
    Background(Hsla),
    TextColor(Hsla),
}

impl Command {
    /// Parses a single command line, e.g. `resize 120 24` or `color #00ff88`.
    fn parse(line: &str) -> Result<Self, String> {
        let mut parts = line.split_whitespace();
        let name = parts.next().ok_or_else(|| "empty command".to_string())?;

        match name {
            "resize" => {
                let width = parts
                    .next()
                    .ok_or_else(|| "resize: missing width".to_string())?
                    .parse::<f32>()
                    .map_err(|_| "resize: invalid width".to_string())?;
                let height = parts
                    .next()
                    .ok_or_else(|| "resize: missing height".to_string())?
                    .parse::<f32>()
                    .map_err(|_| "resize: invalid height".to_string())?;
                if parts.next().is_some() {
                    return Err("resize: too many arguments".to_string());
                }
                Ok(Command::Resize { width, height })
            }
            "color" => {
                let value = parts
                    .next()
                    .ok_or_else(|| "color: missing value".to_string())?;
                let color = gpui::Rgba::try_from(value).map_err(|err| err.to_string())?;
                Ok(Command::Background(color.into()))
            }
            "text-color" => {
                let value = parts
                    .next()
                    .ok_or_else(|| "text-color: missing value".to_string())?;
                let color = gpui::Rgba::try_from(value).map_err(|err| err.to_string())?;
                Ok(Command::TextColor(color.into()))
            }
            other => Err(format!("unknown command: {other}")),
        }
    }
}

/// Path of the Unix domain socket used to talk to the running bar server.
fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir).join("gpui-shell.sock");
        }
    }
    PathBuf::from("/tmp/gpui-shell.sock")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // With arguments we act as a client and hand the command to the server.
    if !args.is_empty() {
        run_client(&args);
    }

    run_server();
}

/// Sends a single command to the running server, then exits.
fn run_client(args: &[String]) -> ! {
    let line = args.join(" ");

    // Validate locally so the user gets a useful error without a round trip.
    if let Err(err) = Command::parse(&line) {
        eprintln!("gpui-shell: {err}");
        eprintln!("usage: gpui-shell [resize <width> <height> | color <#hex> | text-color <#hex>]");
        std::process::exit(2);
    }

    let path = socket_path();
    let mut stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!(
                "gpui-shell: cannot reach a running instance at {}: {err}",
                path.display()
            );
            eprintln!(
                "gpui-shell: start the bar server first by running `gpui-shell` without arguments."
            );
            std::process::exit(1);
        }
    };

    if let Err(err) = writeln!(stream, "{line}") {
        eprintln!("gpui-shell: failed to send command: {err}");
        std::process::exit(1);
    }

    let mut response = String::new();
    let _ = BufReader::new(&stream).read_line(&mut response);
    print!("{response}");

    if response.starts_with("ok") {
        std::process::exit(0);
    } else {
        std::process::exit(1);
    }
}

fn run_server() {
    let (command_tx, command_rx) = async_channel::unbounded::<Command>();
    start_ipc_server(command_tx);

    application().run(move |cx: &mut App| {
        cx.set_global(Settings::default());

        // Wakes the animator whenever a command changes the target.
        let (wake_tx, wake_rx) = async_channel::bounded::<()>(1);

        // Consume commands, moving the target appearance and waking the animator.
        cx.spawn(async move |cx| {
            while let Ok(command) = command_rx.recv().await {
                cx.update(|cx| set_target(cx, command));
                // Bounded to one: extra wakes are coalesced.
                wake_tx.try_send(()).ok();
            }
        })
        .detach();

        // Interpolate `current` towards `target`, one tween per wake.
        cx.spawn(async move |cx| {
            while wake_rx.recv().await.is_ok() {
                let (mut from, mut generation) = cx.update(|cx| {
                    let settings = cx.global::<Settings>();
                    (settings.current, settings.generation)
                });

                let mut elapsed = 0.0_f32;
                loop {
                    elapsed += ANIMATION_FRAME.as_secs_f32();
                    let t = (elapsed / ANIMATION_DURATION.as_secs_f32()).min(1.0);

                    let next_generation = cx.update(|cx| {
                        let target = cx.global::<Settings>().target;
                        let current = from.lerp(target, ease_in_out(t));
                        cx.global_mut::<Settings>().current = current;
                        apply_appearance(cx, current);
                        cx.global::<Settings>().generation
                    });

                    if next_generation != generation {
                        // A newer command arrived; continue from where we are now.
                        from = cx.update(|cx| cx.global::<Settings>().current);
                        generation = next_generation;
                        elapsed = 0.0;
                        continue;
                    }

                    if t >= 1.0 {
                        break;
                    }

                    cx.background_executor().timer(ANIMATION_FRAME).await;
                }
            }
        })
        .detach();

        // Run a polling loop to reconcile open windows with active displays
        cx.spawn(|cx: &mut gpui::AsyncApp| {
            let cx = cx.clone();
            async move {
                let mut active_displays: HashSet<DisplayId> = HashSet::new();

                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(500))
                        .await;

                    cx.update(|cx: &mut App| {
                        let current_displays: HashSet<DisplayId> =
                            cx.displays().into_iter().map(|d| d.id()).collect();

                        // Open windows on newly connected displays
                        for &display_id in current_displays.difference(&active_displays) {
                            open_bar_window(cx, Some(display_id));
                        }

                        active_displays = current_displays;
                    });
                }
            }
        })
        .detach();
    });
}

/// Binds the command socket and serves incoming client connections on a background thread.
fn start_ipc_server(sender: async_channel::Sender<Command>) {
    let path = socket_path();

    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            // A live server answers on the socket; refuse to start a second one.
            if UnixStream::connect(&path).is_ok() {
                eprintln!("gpui-shell: already running (socket: {})", path.display());
                std::process::exit(1);
            }
            // Otherwise the socket is stale (e.g. a previous crash); replace it.
            let _ = std::fs::remove_file(&path);
            match UnixListener::bind(&path) {
                Ok(listener) => listener,
                Err(err) => {
                    eprintln!("gpui-shell: failed to bind {}: {err}", path.display());
                    std::process::exit(1);
                }
            }
        }
        Err(err) => {
            eprintln!("gpui-shell: failed to bind {}: {err}", path.display());
            std::process::exit(1);
        }
    };

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let sender = sender.clone();
                    std::thread::spawn(move || handle_client(stream, sender));
                }
                Err(err) => eprintln!("gpui-shell: accept failed: {err}"),
            }
        }
    });
}

fn handle_client(mut stream: UnixStream, sender: async_channel::Sender<Command>) {
    let mut line = String::new();

    let response = {
        let mut reader = BufReader::new(&stream);
        match reader.read_line(&mut line) {
            Ok(0) => "error: empty request\n".to_string(),
            Ok(_) => match Command::parse(line.trim()) {
                Ok(command) => match sender.try_send(command) {
                    Ok(()) => "ok\n".to_string(),
                    Err(err) => format!("error: {err}\n"),
                },
                Err(err) => format!("error: {err}\n"),
            },
            Err(err) => format!("error: {err}\n"),
        }
    };

    let _ = stream.write_all(response.as_bytes());
}

/// Updates the target appearance; the animator interpolates towards it.
fn set_target(cx: &mut App, command: Command) {
    let settings = cx.global_mut::<Settings>();

    match command {
        Command::Resize { width, height } => {
            settings.target.window_size = Size::new(px(width), px(height));
        }
        Command::Background(color) => settings.target.background = color,
        Command::TextColor(color) => settings.target.text_color = color,
    }

    settings.generation = settings.generation.wrapping_add(1);
}

/// Resizes every window to `appearance` and requests a redraw.
fn apply_appearance(cx: &mut App, appearance: Appearance) {
    for handle in cx.windows() {
        if let Some(handle) = handle.downcast::<Island>() {
            handle
                .update(cx, move |_island, window, _cx| {
                    window.resize(appearance.window_size);
                })
                .ok();
        }
    }

    cx.refresh_windows();
}

fn open_bar_window(cx: &mut App, display_id: Option<DisplayId>) {
    let window_size = cx.global::<Settings>().current.window_size;

    let _ = cx.open_window(
        WindowOptions {
            display_id,
            window_bounds: Some(WindowBounds::Windowed(gpui::Bounds::new(
                point(px(0.0), px(0.0)),
                window_size,
            ))),
            kind: WindowKind::LayerShell(LayerShellOptions {
                layer: gpui::layer_shell::Layer::Top,
                anchor: Anchor::TOP,
                exclusive_zone: None,
                ..Default::default()
            }),
            ..Default::default()
        },
        |_window, cx| cx.new(|cx| Island::new(cx)),
    );
}
