use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use chrono::{DateTime, Local};
use clap::Parser;
use gpui::layer_shell::{Anchor, KeyboardInteractivity, LayerShellOptions};
use gpui::{
    App, AppContext, DisplayId, Global, Pixels, Size, WindowBounds, WindowHandle, WindowKind,
    WindowOptions, font, point, px,
};
use gpui_platform::application;

use crate::animation::Animation;
use crate::appearance::{IslandMode, NORMAL_TEXT_SIZE, Settings, TEXT_FONT_FAMILY, Toast};
use crate::battery;
use crate::cli::{Cli, Command, GlyphAction};
use crate::glyphs::Glyph;
use crate::ipc;
use crate::island::{Island, IslandRole};
use crate::launcher::Catalog;
use crate::notifications::{self, Notification, Update};

/// How often to look for newly connected displays.
const DISPLAY_POLL: Duration = Duration::from_millis(500);

/// How long each notification summary stays on the compact island.
const TOAST_HOLD: Duration = Duration::from_secs(4);

/// Horizontal breathing room around a toast summary, in px.
const TOAST_PADDING: f32 = 20.0;

/// Cap on the text area so a very long notification cannot take over the bar.
const TOAST_MAX_WIDTH: f32 = 320.0;

/// Notifications waiting to be shown as toasts, plus the one on screen.
///
/// This lives in an app global rather than in an [`Island`] because every bar
/// shares the clock surface: each display should show the same toast at the
/// same time, and only one timer should drive the queue.
#[derive(Default)]
struct Toasts {
    queue: VecDeque<Notification>,
    current: Option<Notification>,
    /// Last revision of each notification already queued or shown. The store
    /// keeps notifications until they are dismissed, so this is what stops
    /// each arrival from re-queueing every older entry in the snapshot.
    seen: HashMap<u32, DateTime<Local>>,
    /// Bumped whenever a toast starts, so a superseded hold timer does nothing.
    generation: u64,
    /// Notifications have arrived that the user has not opened yet.
    unread: bool,
}

impl Global for Toasts {}

pub fn run() {
    let (command_tx, command_rx) = async_channel::unbounded::<Command>();

    // Parse each request line with the same clap definition the client used.
    ipc::serve(move |line| {
        let argv = std::iter::once("gpui-shell").chain(line.split_whitespace());
        match Cli::try_parse_from(argv) {
            Ok(cli) => match cli.command {
                Some(command) => command
                    .validate()
                    .and_then(|()| command_tx.try_send(command).map_err(|err| err.to_string())),
                None => Err("missing command".to_string()),
            },
            Err(err) => Err(err.to_string()),
        }
    });

    // Own the StatusNotifierWatcher (or attach to an existing one) before any
    // application has a chance to register a tray item.
    crate::status_notifier::start();

    // Become the session's notification server before any application tries
    // to send a notification. If another server owns the name, the panel
    // reports that instead of silently dropping notifications.
    crate::notifications::start();

    // Walk the desktop-entry locations once, before any window exists, so the
    // launcher never touches the filesystem from the render path.
    let catalog = Catalog::discover();

    application().run(move |cx: &mut App| {
        cx.set_global(Settings::new());
        cx.set_global(Animation::new(cx.global::<Settings>().current));
        cx.set_global(Toasts::default());
        cx.set_global(catalog);

        // Show arriving notifications as compact toasts and keep the unread
        // glyph in step with the notification store.
        start_notification_toasts(cx);

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

        // Raise the low-battery glyph whenever the battery drops to the
        // threshold, and clear it again once it recovers. Desktops report
        // `Battery::Absent`, which simply leaves the glyph clear.
        let battery_rx = battery::subscribe();
        cx.spawn(async move |cx| {
            while let Ok(state) = battery_rx.recv().await {
                cx.update(|cx| set_glyph(cx, Glyph::BatteryLow, state.is_low()));
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

/// Shows or hides a glyph, growing the island if the visible slot changed.
fn set_glyph(cx: &mut App, glyph: Glyph, active: bool) {
    {
        let settings = cx.global_mut::<Settings>();
        if settings.glyphs.is_active(glyph) == active {
            return;
        }

        if active {
            settings.glyphs.set(glyph);
        } else {
            settings.glyphs.clear(glyph);
        }

        let target = settings.target_size();
        settings.target.size = target;
    }

    grow_surface(cx);
    start_animation(cx);
}

/// Subscribes to notification changes and drives the toast queue.
fn start_notification_toasts(cx: &mut App) {
    let subscription = notifications::subscribe();
    let updates = subscription.receiver().clone();

    cx.spawn(async move |cx| {
        // Keep the subscription alive for as long as the app runs.
        let _subscription = subscription;

        while let Ok(update) = updates.recv().await {
            cx.update(|cx| handle_notification_update(cx, update));
        }
    })
    .detach();
}

/// Brings the toast queue and the unread glyph up to date after a change in
/// the notification store.
fn handle_notification_update(cx: &mut App, update: Update) {
    let snapshot = notifications::snapshot();
    let ids: HashSet<u32> = snapshot.notifications.iter().map(|n| n.id).collect();
    let viewing = cx.global::<Settings>().mode == IslandMode::Notifications;

    let unread = {
        let toasts = cx.global_mut::<Toasts>();

        // A notification dismissed or expired before its turn should not be
        // toasted later, and one that is dismissed while on screen should give
        // way to the next entry immediately.
        toasts
            .queue
            .retain(|notification| ids.contains(&notification.id));
        toasts.seen.retain(|id, _| ids.contains(id));
        if toasts
            .current
            .as_ref()
            .is_some_and(|current| !ids.contains(&current.id))
        {
            toasts.current = None;
        }

        match update {
            Update::Arrived => {
                // The snapshot is newest-first; queue oldest-first so a burst
                // is shown in arrival order. A replacement keeps its place and
                // refreshes the text instead of being queued twice.
                for notification in snapshot.notifications.iter().rev() {
                    if let Some(current) = toasts
                        .current
                        .as_mut()
                        .filter(|current| current.id == notification.id)
                    {
                        *current = notification.clone();
                    } else if let Some(queued) = toasts
                        .queue
                        .iter_mut()
                        .find(|queued| queued.id == notification.id)
                    {
                        *queued = notification.clone();
                    } else if toasts.seen.get(&notification.id) != Some(&notification.received) {
                        toasts.queue.push_back(notification.clone());
                    }

                    toasts.seen.insert(notification.id, notification.received);
                }

                // Nothing is unread while the user is already looking at the
                // notification center.
                if !viewing {
                    toasts.unread = true;
                }
            }
            Update::Closed => {
                if snapshot.notifications.is_empty() {
                    toasts.unread = false;
                }
            }
        }

        toasts.unread
    };

    set_glyph(cx, Glyph::Unread, unread);

    let (showing, queued) = {
        let toasts = cx.global::<Toasts>();
        (toasts.current.is_some(), !toasts.queue.is_empty())
    };
    if !showing {
        if queued {
            show_next_toast(cx);
        } else {
            hide_toast(cx);
        }
    }
}

/// Moves the queue forward: show the next summary, or return to the clock.
fn show_next_toast(cx: &mut App) {
    let (notification, generation) = {
        let toasts = cx.global_mut::<Toasts>();
        toasts.current = toasts.queue.pop_front();
        toasts.generation = toasts.generation.wrapping_add(1);
        (toasts.current.clone(), toasts.generation)
    };

    let Some(notification) = notification else {
        hide_toast(cx);
        return;
    };

    let summary = toast_summary(&notification);
    let width = px(toast_width(cx, &summary));
    {
        let settings = cx.global_mut::<Settings>();
        settings.toast = Some(Toast { summary, width });
        let target = settings.target_size();
        settings.target.size = target;
    }

    grow_surface(cx);
    start_animation(cx);
    schedule_toast_advance(cx, generation);
}

/// Removes the toast and lets the island contract back to the clock width.
fn hide_toast(cx: &mut App) {
    let had_toast = {
        let settings = cx.global_mut::<Settings>();
        let had_toast = settings.toast.take().is_some();
        let target = settings.target_size();
        settings.target.size = target;
        had_toast
    };

    if had_toast {
        start_animation(cx);
    }
}

/// Retires the current toast after [`TOAST_HOLD`]. The generation check makes
/// sure a timer from a superseded toast cannot advance the queue twice.
fn schedule_toast_advance(cx: &mut App, generation: u64) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(TOAST_HOLD).await;

        cx.update(|cx| {
            if cx.global::<Toasts>().generation != generation {
                return;
            }

            cx.global_mut::<Toasts>().current = None;
            if cx.global::<Toasts>().queue.is_empty() {
                hide_toast(cx);
            } else {
                show_next_toast(cx);
            }
        });
    })
    .detach();
}

/// Marks stored notifications as seen when the notification center opens.
fn mark_notifications_read(cx: &mut App) {
    cx.global_mut::<Toasts>().unread = false;
    set_glyph(cx, Glyph::Unread, false);
}

/// The single line shown in a toast: the summary, falling back to the body's
/// first words and then the application name.
fn toast_summary(notification: &Notification) -> String {
    let text = if notification.summary.trim().is_empty() {
        &notification.body
    } else {
        &notification.summary
    };
    let summary = text.split_whitespace().collect::<Vec<_>>().join(" ");

    if summary.is_empty() {
        notification.app_name.clone()
    } else {
        summary
    }
}

/// Width the summary actually occupies at the bar's font and text size, plus
/// horizontal breathing room. Measuring instead of estimating keeps the grown
/// island close to the text rather than leaving large empty margins.
fn toast_width(cx: &App, summary: &str) -> f32 {
    let text_system = cx.text_system();
    let font_id = text_system.resolve_font(&font(TEXT_FONT_FAMILY));
    let font_size = px(NORMAL_TEXT_SIZE);

    // The bar face is monospace and the summary is a single line, so summing
    // character advances matches what the renderer lays out.
    let text_width: f32 = summary
        .chars()
        .map(|ch| f32::from(text_system.layout_width(font_id, font_size, ch)))
        .sum();

    (text_width + TOAST_PADDING).min(TOAST_MAX_WIDTH)
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
        Command::Glyph { action } => {
            {
                let settings = cx.global_mut::<Settings>();
                match action {
                    GlyphAction::Set { glyph, .. } => settings.glyphs.set(glyph),
                    GlyphAction::Clear { side, glyph } => match glyph {
                        Some(glyph) => settings.glyphs.clear(glyph),
                        None => settings.glyphs.clear_side(side),
                    },
                }
                // Occupied slots widen the compact island; the animator
                // eases to the new target like any other resize.
                let target = settings.target_size();
                settings.target.size = target;
            }
            grow_surface(cx);
        }
        Command::Launcher => open_overlay(cx, IslandMode::Launcher),
        Command::Tray => open_overlay(cx, IslandMode::Tray),
        Command::Notifications => open_overlay(cx, IslandMode::Notifications),
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

    if mode == IslandMode::Notifications {
        mark_notifications_read(cx);
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
                IslandMode::Notifications => island.prepare_notifications(cx),
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
        (settings.normal_size_with_glyphs(), settings.overlay_engaged)
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

#[cfg(test)]
mod tests {
    use chrono::Local;

    use super::*;
    use crate::notifications::Urgency;

    #[test]
    fn toast_summary_is_a_single_line_with_fallbacks() {
        let mut notification = notification("  Build\n finished  ", "ignored");
        assert_eq!(toast_summary(&notification), "Build finished");

        notification.summary.clear();
        notification.body = "Line one\nline two".to_string();
        assert_eq!(toast_summary(&notification), "Line one line two");

        notification.body.clear();
        notification.app_name = "Mail".to_string();
        assert_eq!(toast_summary(&notification), "Mail");
    }

    fn notification(summary: &str, body: &str) -> Notification {
        Notification {
            id: 1,
            app_name: String::new(),
            app_icon: None,
            summary: summary.to_string(),
            body: body.to_string(),
            urgency: Urgency::Normal,
            received: Local::now(),
        }
    }
}
