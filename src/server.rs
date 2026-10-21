use std::collections::HashSet;
use std::time::Duration;

use clap::Parser;
use gpui::layer_shell::{Anchor, KeyboardInteractivity, LayerShellOptions};
use gpui::{
    App, AppContext, DisplayId, Pixels, Size, WindowBounds, WindowHandle, WindowKind,
    WindowOptions, point, px,
};
use gpui_platform::application;

use crate::animation::Animation;
use crate::appearance::{IslandMode, Settings};
use crate::cli::{Cli, Command};
use crate::ipc;
use crate::island::{Island, IslandRole};
use crate::launcher::Catalog;

/// How often to look for newly connected displays.
const DISPLAY_POLL: Duration = Duration::from_millis(500);

pub fn run() {
    let (command_tx, command_rx) = async_channel::unbounded::<Command>();

    // Parse each request line with the same clap definition the client used.
    ipc::serve(move |line| {
        let argv = std::iter::once("gpui-shell").chain(line.split_whitespace());
        match Cli::try_parse_from(argv) {
            Ok(cli) => match cli.command {
                Some(command) => command_tx.try_send(command).map_err(|err| err.to_string()),
                None => Err("missing command".to_string()),
            },
            Err(err) => Err(err.to_string()),
        }
    });

    // Own the StatusNotifierWatcher (or attach to an existing one) before any
    // application has a chance to register a tray item.
    crate::status_notifier::start();

    // Walk the desktop-entry locations once, before any window exists, so the
    // launcher never touches the filesystem from the render path.
    let catalog = Catalog::discover();

    application().run(move |cx: &mut App| {
        cx.set_global(Settings::new());
        cx.set_global(Animation::new(cx.global::<Settings>().current));
        cx.set_global(catalog);

        // Consume commands, moving the target appearance or mode.
        cx.spawn(async move |cx| {
            while let Ok(command) = command_rx.recv().await {
                cx.update(|cx| {
                    apply_command(cx, command);
                    start_animation(cx);
                });
            }
        })
        .detach();

        // Reconcile open windows with active displays.
        cx.spawn(|cx: &mut gpui::AsyncApp| {
            let cx = cx.clone();
            async move {
                let mut active_displays: HashSet<DisplayId> = HashSet::new();

                loop {
                    cx.background_executor().timer(DISPLAY_POLL).await;

                    cx.update(|cx: &mut App| {
                        let current: HashSet<DisplayId> =
                            cx.displays().into_iter().map(|d| d.id()).collect();

                        for &display_id in current.difference(&active_displays) {
                            let _ = open_bar_window(cx, Some(display_id));
                        }

                        active_displays = current;
                    });
                }
            }
        })
        .detach();
    });
}

/// Updates the target state in response to a command.
fn apply_command(cx: &mut App, command: Command) {
    match command {
        Command::Resize { width, height } => {
            // `resize` redefines the compact size, not the overlay's.
            let new_size = Size::new(px(width), px(height));
            {
                let settings = cx.global_mut::<Settings>();
                settings.normal_size = new_size;
                let target = settings.target_size();
                settings.target.size = target;
            }
            grow_surface(cx);
        }
        Command::Color { color } => cx.global_mut::<Settings>().target.background = color,
        Command::TextColor { color } => cx.global_mut::<Settings>().target.text_color = color,
        Command::Launcher => open_overlay(cx, IslandMode::Launcher),
        Command::Tray => open_overlay(cx, IslandMode::Tray),
    }
}

/// Switches to an overlay mode and expands the island.
///
/// Requesting the mode that is already open is a no-op, so repeated invocations
/// are predictable. Requesting the other overlay reuses the existing surface and
/// swaps its content.
fn open_overlay(cx: &mut App, mode: IslandMode) {
    if cx.global::<Settings>().mode == mode {
        return;
    }

    {
        let settings = cx.global_mut::<Settings>();
        settings.mode = mode;
        settings.overlay_mode = mode;
        settings.overlay_engaged = true;
        settings.overlay_size = settings.target_size();
        let target = settings.target_size();
        settings.target.size = target;
    }
    grow_surface(cx);

    // A dedicated surface with no output lets the compositor place the overlay
    // on the focused display - the same NULL-output trick fuzzel uses. Re-use
    // it if a previous close is still animating away.
    let window = overlay_window(cx).or_else(|| open_overlay_window(cx));
    if let Some(window) = window {
        window
            .update(cx, |island, _window, cx| match mode {
                IslandMode::Launcher => island.prepare_launcher(cx),
                IslandMode::Tray => island.prepare_tray(cx),
                IslandMode::Normal | IslandMode::Calendar => {}
            })
            .ok();
    }
}

/// Leaves overlay mode and eases back to the normal size.
///
/// Every exit path funnels through here - a successful launch or stop, Escape,
/// or a failed action - so the island can't be left expanded or holding the
/// keyboard. The transient overlay surface is removed by the animator once it
/// has contracted.
pub(crate) fn close_overlay(cx: &mut App) {
    if cx.global::<Settings>().mode == IslandMode::Normal {
        return;
    }

    {
        let settings = cx.global_mut::<Settings>();
        settings.mode = IslandMode::Normal;
        let target = settings.target_size();
        settings.target.size = target;
    }

    start_animation(cx);
}

/// Marks the shared animation as running and asks every island window to draw.
///
/// The redraw gets us into `Render`, where `request_animation_frame` can
/// legally be called (it is restricted to the draw phase); from then on each
/// rendered frame requests the next compositor frame until the animation
/// settles. That pacing is what bypasses gpui's 30 fps throttle for inactive
/// windows.
fn start_animation(cx: &mut App) {
    cx.global_mut::<Animation>().request();

    for handle in island_windows(cx) {
        // `on_next_frame` is safe outside the draw phase (unlike
        // `request_animation_frame`) and guarantees an unthrottled frame. The
        // render that follows schedules the rest of the animation from inside
        // `Render`.
        handle
            .update(cx, |_island, window, _cx| window.on_next_frame(|_, _| {}))
            .ok();
    }

    cx.refresh_windows();
}

/// Settles the surfaces once the shared animation reaches its target.
///
/// The overlay surface is kept large enough to hold the island while it
/// animates; this shrinks it back once the island has come to rest. A closing
/// overlay's transient surface is then removed entirely so the bar underneath
/// takes over.
pub(crate) fn finish_animation(cx: &mut App) {
    let (shrink, remove_overlay) = {
        let settings = cx.global_mut::<Settings>();
        let closing = settings.mode == IslandMode::Normal;
        if closing {
            // The closing animation has settled, so the bars own the
            // appearance again.
            settings.overlay_engaged = false;
            settings.overlay_mode = IslandMode::Normal;
        }
        let shrink = (settings.window_size != settings.target.size).then(|| {
            settings.window_size = settings.target.size;
            settings.target.size
        });
        (shrink, closing)
    };

    if remove_overlay {
        remove_overlay_window(cx);
    }
    if let Some(size) = shrink {
        resize_windows(cx, size);
    }
}

/// All open island windows: the per-display bars and, while open, the overlay.
fn island_windows(cx: &App) -> Vec<WindowHandle<Island>> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<Island>())
        .collect()
}

/// The transient overlay surface, if it exists.
fn overlay_window(cx: &App) -> Option<WindowHandle<Island>> {
    island_windows(cx).into_iter().find(|window| {
        window
            .read(cx)
            .map(|island| island.role() == IslandRole::Overlay)
            .unwrap_or(false)
    })
}

/// Removes the transient overlay surface once it is no longer needed.
fn remove_overlay_window(cx: &mut App) {
    if let Some(window) = overlay_window(cx) {
        window
            .update(cx, |_island, window, _cx| window.remove_window())
            .ok();
    }
}

/// Sizes the transparent surface for the island's upcoming animation.
fn grow_surface(cx: &mut App) {
    let (surface, changed) = {
        let settings = cx.global_mut::<Settings>();
        let surface = surface_size(settings.current.size, settings.target.size);
        let changed = surface != settings.window_size;
        settings.window_size = surface;
        (surface, changed)
    };
    if changed {
        resize_windows(cx, surface);
    }
}

/// Surface size big enough to hold the island as it eases from `current` to
/// `target`.
fn surface_size(current: Size<Pixels>, target: Size<Pixels>) -> Size<Pixels> {
    Size::new(
        px(f32::from(current.width).max(f32::from(target.width))),
        px(f32::from(current.height).max(f32::from(target.height))),
    )
}

/// Resizes each island surface for its role and requests a redraw.
fn resize_windows(cx: &mut App, size: Size<Pixels>) {
    let (normal_size, overlay_engaged) = {
        let settings = cx.global::<Settings>();
        (settings.normal_size, settings.overlay_engaged)
    };

    for handle in island_windows(cx) {
        let role = handle.read(cx).map(|island| island.role()).ok();
        let target = match role {
            // The overlay surface follows the animated size.
            Some(IslandRole::Overlay) => size,
            // Bars stay compact while an overlay owns the shared appearance.
            Some(IslandRole::Bar) => {
                if overlay_engaged {
                    normal_size
                } else {
                    size
                }
            }
            None => continue,
        };

        handle
            .update(cx, move |_island, window, _cx| window.resize(target))
            .ok();
    }

    cx.refresh_windows();
}

fn open_bar_window(cx: &mut App, display_id: Option<DisplayId>) -> Option<WindowHandle<Island>> {
    let window_size = cx.global::<Settings>().window_size;

    cx.open_window(
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
                // The bar never competes for the keyboard.
                keyboard_interactivity: KeyboardInteractivity::None,
                ..Default::default()
            }),
            ..Default::default()
        },
        |_window, cx| cx.new(|cx| Island::new(IslandRole::Bar, cx)),
    )
    .ok()
}

/// Creates the transient overlay surface on the compositor's focused output.
///
/// Passing no output to the layer surface lets the compositor choose one; niri
/// resolves that to its active output, matching fuzzel's behaviour.
fn open_overlay_window(cx: &mut App) -> Option<WindowHandle<Island>> {
    let window_size = cx.global::<Settings>().window_size;

    cx.open_window(
        WindowOptions {
            display_id: None,
            window_bounds: Some(WindowBounds::Windowed(gpui::Bounds::new(
                point(px(0.0), px(0.0)),
                window_size,
            ))),
            kind: WindowKind::LayerShell(LayerShellOptions {
                layer: gpui::layer_shell::Layer::Overlay,
                anchor: Anchor::TOP,
                exclusive_zone: None,
                // Hold the keyboard even when focus-follows-mouse points at
                // another window, like fuzzel's exclusive mode.
                keyboard_interactivity: KeyboardInteractivity::Exclusive,
                ..Default::default()
            }),
            ..Default::default()
        },
        |_window, cx| cx.new(|cx| Island::new(IslandRole::Overlay, cx)),
    )
    .ok()
}
