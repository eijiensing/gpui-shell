use std::collections::HashSet;
use std::time::{Duration, Instant};

use clap::Parser;
use gpui::layer_shell::{Anchor, LayerShellOptions};
use gpui::{
    App, AppContext, DisplayId, Pixels, Size, WindowBounds, WindowKind, WindowOptions, point, px,
};
use gpui_platform::application;

use crate::animation::Animator;
use crate::appearance::Settings;
use crate::cli::{Cli, Command};
use crate::ipc;
use crate::island::Island;

/// Animator wake-up cadence.
const FRAME: Duration = Duration::from_millis(16);
/// How often to look for newly connected displays.
const DISPLAY_POLL: Duration = Duration::from_millis(500);
/// Safety cap so a spring can never animate forever.
const MAX_ANIMATION: Duration = Duration::from_secs(5);
/// Extra surface size so the island's spring overshoot isn't clipped.
const OVERSHOOT_MARGIN: f32 = 0.35;

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

    application().run(move |cx: &mut App| {
        cx.set_global(Settings::default());

        // Wakes the animator whenever a command changes the target appearance.
        let (wake_tx, wake_rx) = async_channel::bounded::<()>(1);

        // Consume commands, moving the target appearance.
        cx.spawn(async move |cx| {
            while let Ok(command) = command_rx.recv().await {
                cx.update(|cx| apply_command(cx, command));
                // Bounded to one: extra wakes are coalesced.
                wake_tx.try_send(()).ok();
            }
        })
        .detach();

        // Animate `current` towards `target` with springs.
        cx.spawn(async move |cx| {
            let mut animator = cx.update(|cx| Animator::new(cx.global::<Settings>().current));

            while wake_rx.recv().await.is_ok() {
                let started = Instant::now();
                let mut last = Instant::now();

                loop {
                    let dt = last.elapsed().as_secs_f32();
                    last = Instant::now();

                    let target = cx.update(|cx| cx.global::<Settings>().target);
                    let settled = animator.advance(target, dt);
                    let value = if settled { target } else { animator.value() };
                    if settled {
                        animator.snap_to(target);
                    }

                    cx.update(|cx| {
                        cx.global_mut::<Settings>().current = value;
                        cx.refresh_windows();
                    });

                    if settled || started.elapsed() >= MAX_ANIMATION {
                        break;
                    }

                    cx.background_executor().timer(FRAME).await;
                }

                // The surface is kept large enough to fit the island while it
                // animates; shrink it to the target once the island has settled.
                let shrink = cx.update(|cx| {
                    let settings = cx.global_mut::<Settings>();
                    (settings.window_size != settings.target.size).then(|| {
                        settings.window_size = settings.target.size;
                        settings.target.size
                    })
                });
                if let Some(size) = shrink {
                    cx.update(|cx| resize_windows(cx, size));
                }
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
                            open_bar_window(cx, Some(display_id));
                        }

                        active_displays = current;
                    });
                }
            }
        })
        .detach();
    });
}

/// Updates the target appearance, growing the surface immediately for a resize.
fn apply_command(cx: &mut App, command: Command) {
    match command {
        Command::Resize { width, height } => {
            let new_size = Size::new(px(width), px(height));

            // Size the surface now (a single resize call), with headroom for the
            // spring overshoot, so the animating island is never clipped.
            let (surface, changed) = {
                let settings = cx.global_mut::<Settings>();
                let surface = surface_size(settings.current.size, new_size);
                let changed = surface != settings.window_size;
                settings.window_size = surface;
                (surface, changed)
            };
            if changed {
                resize_windows(cx, surface);
            }

            cx.global_mut::<Settings>().target.size = new_size;
        }
        Command::Color { color } => cx.global_mut::<Settings>().target.background = color,
        Command::TextColor { color } => cx.global_mut::<Settings>().target.text_color = color,
    }
}

/// Surface size big enough to hold the island as it springs from `current` to
/// `target`, including the overshoot the spring adds on the way.
fn surface_size(current: Size<Pixels>, target: Size<Pixels>) -> Size<Pixels> {
    let headroom = 1.0 + OVERSHOOT_MARGIN;
    Size::new(
        px(f32::from(current.width).max(f32::from(target.width) * headroom)),
        px(f32::from(current.height).max(f32::from(target.height) * headroom)),
    )
}

/// Resizes every window surface to `size` in one call and requests a redraw.
fn resize_windows(cx: &mut App, size: Size<Pixels>) {
    for handle in cx.windows() {
        if let Some(handle) = handle.downcast::<Island>() {
            handle
                .update(cx, move |_island, window, _cx| window.resize(size))
                .ok();
        }
    }

    cx.refresh_windows();
}

fn open_bar_window(cx: &mut App, display_id: Option<DisplayId>) {
    let window_size = cx.global::<Settings>().window_size;

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
