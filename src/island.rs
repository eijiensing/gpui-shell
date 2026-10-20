use std::time::{Duration, Instant};

use chrono::Local;
use gpui::{
    AnyElement, Context, FocusHandle, KeyDownEvent, Rems, Render, ScrollHandle, Window, div,
    prelude::*, px,
};

use crate::animation::Animation;
use crate::appearance::{Appearance, IslandMode, Settings, launcher_size};
use crate::audio::{self, Volume};
use crate::launcher::Catalog;

/// How long the volume stays on screen after the last change before the island
/// shows the clock again.
const VOLUME_HOLD: Duration = Duration::from_millis(2000);

/// Fraction of the travel between the normal and expanded sizes at which the
/// launcher content is considered hidden (below) or shown (above). Deriving
/// visibility from the animated size keeps the content switch tied to the
/// resize animation rather than to a separate timer.
const REVEAL_EDGE: f32 = 0.35;

/// Normal-mode content: the clock, or a transient volume indicator.
enum NormalIndicator {
    Clock,
    Volume(Volume),
}

/// Launcher-specific interaction state, owned per window.
struct Launcher {
    /// Focus target for the search input.
    focus: FocusHandle,
    /// What the user has typed.
    query: String,
    /// Whether Ctrl/Cmd+A has selected the whole query. The field has no
    /// movable caret, so selection is all-or-nothing.
    select_all: bool,
    /// Catalog indices matching `query`, in display order.
    results: Vec<usize>,
    /// Index into `results` of the highlighted row.
    selected: usize,
    /// Keeps the selected row in view.
    scroll: ScrollHandle,
    /// Message shown when launching fails.
    error: Option<String>,
}

impl Launcher {
    fn new(focus: FocusHandle) -> Self {
        Self {
            focus,
            query: String::new(),
            select_all: false,
            results: Vec::new(),
            selected: 0,
            scroll: ScrollHandle::new(),
            error: None,
        }
    }
}

pub struct Island {
    /// What this surface is for.
    role: IslandRole,
    /// Current wall-clock time, refreshed once a second.
    time: String,
    /// What normal mode shows right now.
    indicator: NormalIndicator,
    /// Bumped on every volume change so a superseded revert timer is ignored.
    generation: u64,
    launcher: Launcher,
}

/// Which kind of surface an [`Island`] drives.
///
/// [`IslandRole::Bar`] surfaces exist for every display and only ever show the
/// compact clock. [`IslandRole::Launcher`] is a transient surface created on the
/// compositor's focused output while the launcher is open, so it can expand and
/// take keyboard focus without disturbing the bars.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IslandRole {
    Bar,
    Launcher,
}

impl Island {
    pub fn new(role: IslandRole, cx: &mut Context<Self>) -> Self {
        let island = Self {
            role,
            time: Local::now().format("%H:%M").to_string(),
            indicator: NormalIndicator::Clock,
            generation: 0,
            launcher: Launcher::new(cx.focus_handle()),
        };

        island.start_timer(cx);
        island.watch_volume(cx);

        island
    }

    pub fn role(&self) -> IslandRole {
        self.role
    }

    /// Resets the launcher for a fresh search. Called on the transient launcher
    /// surface when the launcher mode is requested.
    pub fn prepare_launcher(&mut self, cx: &mut Context<Self>) {
        self.launcher.query.clear();
        self.launcher.select_all = false;
        self.launcher.selected = 0;
        self.launcher.scroll = ScrollHandle::new();
        self.launcher.error = None;
        self.launcher.results = cx.global::<Catalog>().search("");
        cx.notify();
    }

    /// Asks the server to leave launcher mode.
    ///
    /// Deferred because leaving launcher mode re-creates the layer surface (to
    /// hand keyboard interactivity back), and that must not run while this
    /// window is borrowed.
    fn request_close(&mut self, cx: &mut Context<Self>) {
        self.launcher.error = None;
        cx.defer(crate::server::close_launcher);
    }

    /// Recomputes the filtered results for the current query and selects the
    /// first match.
    fn refilter(&mut self, cx: &mut Context<Self>) {
        self.launcher.results = cx.global::<Catalog>().search(&self.launcher.query);
        self.launcher.selected = 0;
        self.launcher.scroll = ScrollHandle::new();
        cx.notify();
    }

    /// Moves the highlighted row by `delta`, clamped to the result list.
    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.launcher.results.len();
        if len == 0 {
            return;
        }

        let current = self.launcher.selected.min(len - 1) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.launcher.selected = next;
        self.launcher.scroll.scroll_to_item(next);
        cx.notify();
    }

    /// Launches the highlighted application and closes the launcher.
    fn launch_selected(&mut self, cx: &mut Context<Self>) {
        let Some(&index) = self.launcher.results.get(self.launcher.selected) else {
            return;
        };

        // Resolve the entry and spawn it while only holding an immutable borrow
        // of the app context, so the launcher can then be closed.
        let outcome = {
            let catalog = cx.global::<Catalog>();
            match catalog.entry(index) {
                Some(entry) => (entry.name.clone(), entry.launch()),
                None => return,
            }
        };

        match outcome.1 {
            Ok(()) => self.request_close(cx),
            Err(err) => {
                eprintln!("gpui-shell: failed to launch {}: {err}", outcome.0);
                self.launcher.error = Some(format!("Couldn't launch {}", outcome.0));
                cx.notify();
            }
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // Keys arriving after the launcher has been closed are ignored.
        if cx.global::<Settings>().mode != IslandMode::Launcher {
            return;
        }

        let keystroke = &event.keystroke;
        let control =
            keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform;

        // Select the whole query so the next keystroke replaces it. `secondary`
        // is Ctrl on Linux and Cmd on macOS.
        if keystroke.modifiers.secondary() && keystroke.key == "a" {
            self.launcher.select_all = true;
            cx.notify();
            return;
        }

        match keystroke.key.as_str() {
            "escape" => return self.request_close(cx),
            "enter" => return self.launch_selected(cx),
            "up" => return self.move_selection(-1, cx),
            "down" => return self.move_selection(1, cx),
            "backspace" => {
                if self.launcher.select_all {
                    self.launcher.select_all = false;
                    self.launcher.query.clear();
                    self.refilter(cx);
                } else if self.launcher.query.pop().is_some() {
                    self.refilter(cx);
                }
                return;
            }
            // Leave Tab to GPUI's focus navigation.
            "tab" => return,
            _ => {}
        }

        // Plain typing only; ignore shortcuts and control characters.
        if control {
            return;
        }
        if let Some(text) = keystroke.key_char.as_deref()
            && !text.is_empty()
            && !text.chars().any(char::is_control)
        {
            if self.launcher.select_all {
                self.launcher.select_all = false;
                self.launcher.query.clear();
            }
            self.launcher.query.push_str(text);
            self.refilter(cx);
        }
    }

    /// Shows the default sink's volume, reverting to the clock shortly after
    /// the last change.
    fn watch_volume(&self, cx: &mut Context<Self>) {
        let volume_rx = audio::subscribe();

        cx.spawn(async move |island, cx| {
            while let Ok(volume) = volume_rx.recv().await {
                let Ok(generation) = island.update(cx, |island, cx| {
                    island.indicator = NormalIndicator::Volume(volume);
                    island.generation = island.generation.wrapping_add(1);
                    cx.notify();
                    island.generation
                }) else {
                    break;
                };

                // Each change schedules its own revert; the generation check
                // makes sure only the newest one hides the volume.
                let scheduled = island.update(cx, |_island, cx| {
                    cx.spawn(async move |island, cx| {
                        cx.background_executor().timer(VOLUME_HOLD).await;
                        island
                            .update(cx, |island, cx| {
                                if island.generation == generation {
                                    island.indicator = NormalIndicator::Clock;
                                    cx.notify();
                                }
                            })
                            .ok();
                    })
                    .detach();
                });
                if scheduled.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn start_timer(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |island, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;

                // A window may be re-created (e.g. when keyboard interactivity
                // changes); stop ticking once this island's entity is gone.
                let updated = island.update(cx, |island, cx| {
                    island.time = Local::now().format("%H:%M").to_string();
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// The compact clock/volume view.
    fn render_normal(&self) -> AnyElement {
        let text = match &self.indicator {
            NormalIndicator::Clock => self.time.clone(),
            NormalIndicator::Volume(Volume::Muted) => "muted".to_string(),
            NormalIndicator::Volume(Volume::Percent(percent)) => format!("{percent}%"),
        };

        div().child(text).into_any_element()
    }

    /// The expanded search view.
    fn render_launcher(
        &self,
        progress: f32,
        island: Appearance,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Fade the whole view in and out with the resize so the content appears
        // in step with the growing island.
        let alpha = progress.clamp(0.0, 1.0);
        let text = island.text_color.opacity(alpha);
        let muted = island.text_color.opacity(alpha * 0.5);
        let border = island.text_color.opacity(alpha * 0.25);
        let selection = island.text_color.opacity(alpha * 0.15);

        let catalog = cx.global::<Catalog>();
        let has_query = !self.launcher.query.is_empty();

        let query = div()
            .text_sm()
            .whitespace_nowrap()
            .text_color(if has_query { text } else { muted })
            .min_w_0()
            .overflow_hidden()
            .when(has_query && self.launcher.select_all, |query| {
                query.bg(selection).rounded_xs()
            })
            .child(if has_query {
                self.launcher.query.clone()
            } else {
                "Search applications…".to_string()
            });

        let caret = div().flex_shrink_0().w(px(1.5)).h(px(16.0)).bg(text);

        // With a placeholder, put the caret before the text so it sits at the
        // start of the field instead of being pushed to the end of the hint.
        let field = div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .min_w_0()
            .overflow_hidden();
        let field = if has_query {
            field.child(query).child(caret.ml(px(1.0)))
        } else {
            field.child(caret).child(query)
        };

        let input = div()
            .id("launcher-input")
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(px(36.0))
            .px(px(10.0))
            .flex_shrink_0()
            .rounded_md()
            .border_1()
            .border_color(border)
            .track_focus(&self.launcher.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .child(field);

        let status = |message: String, cx_muted: gpui::Hsla| -> AnyElement {
            div()
                .flex()
                .items_center()
                .justify_center()
                .flex_1()
                .w_full()
                .text_sm()
                .text_color(cx_muted)
                .child(message)
                .into_any_element()
        };

        let body: AnyElement =
            if let Some(error) = self.launcher.error.clone() {
                status(error, muted)
            } else if self.launcher.results.is_empty() {
                let message = if has_query {
                    "No matching applications".to_string()
                } else {
                    "No applications found".to_string()
                };
                status(message, muted)
            } else {
                let rows =
                    self.launcher
                        .results
                        .iter()
                        .enumerate()
                        .map(|(row, &index)| {
                            let Some(entry) = catalog.entry(index) else {
                                return div().into_any_element();
                            };
                            let selected = row == self.launcher.selected;

                            div()
                                .flex()
                                .flex_col()
                                .w_full()
                                .px(px(10.0))
                                .py(px(6.0))
                                .flex_shrink_0()
                                .rounded_md()
                                .when(selected, |row| row.bg(selection))
                                .child(div().text_sm().text_color(text).child(entry.name.clone()))
                                .children(entry.comment.clone().map(|comment| {
                                    div().text_xs().text_color(muted).child(comment)
                                }))
                                .into_any_element()
                        });

                div()
                    .id("launcher-results")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .w_full()
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.launcher.scroll)
                    .children(rows)
                    .into_any_element()
            };

        div()
            .flex()
            .flex_col()
            .gap_2()
            .size_full()
            .p(px(10.0))
            .child(input)
            .child(body)
            .into_any_element()
    }
}

/// How far the island has travelled from the normal size towards the expanded
/// launcher size, in `0..=1`.
fn launcher_progress(settings: &Settings) -> f32 {
    let normal = f32::from(settings.normal_size.height);
    let expanded = f32::from(launcher_size().height);
    let current = f32::from(settings.current.size.height);

    if expanded <= normal {
        return 0.0;
    }

    ((current - normal) / (expanded - normal)).clamp(0.0, 1.0)
}

impl Render for Island {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Advance the shared animation before building the tree so this frame
        // draws the new position. While it is in flight, request the next frame
        // from the compositor; this paces the animation on frame callbacks
        // instead of a fixed timer, keeping it in step with presentation.
        let target = cx.global::<Settings>().target;
        let (animated, settled) = cx.global_mut::<Animation>().tick(target, Instant::now());
        cx.global_mut::<Settings>().current = animated;

        if settled {
            // Surface bookkeeping must not mutate windows while this one is
            // being drawn, so hand it to the next effect cycle.
            cx.defer(crate::server::finish_animation);
        } else if cx.global::<Animation>().running() {
            window.request_animation_frame();
        }

        let (window_size, island, mode, normal_size, progress, launcher_engaged) = {
            let settings = cx.global::<Settings>();
            (
                settings.window_size,
                settings.current,
                settings.mode,
                settings.normal_size,
                launcher_progress(settings),
                settings.launcher_engaged,
            )
        };

        // `true` from the moment the launcher starts opening until the island
        // has contracted back past the reveal edge.
        let launcher_active = mode == IslandMode::Launcher || progress > REVEAL_EDGE;

        // Only the transient launcher surface is interactive.
        if self.role == IslandRole::Launcher
            && mode == IslandMode::Launcher
            && !self.launcher.focus.is_focused(window)
        {
            let focus = self.launcher.focus.clone();
            window.defer(cx, move |window, cx| focus.focus(window, cx));
        }

        // Bar surfaces stay compact and keep showing the clock while another
        // display hosts the launcher. The launcher surface follows the animated
        // size and swaps back to the clock at the end of the closing animation.
        let (surface_size, box_size, content): (_, _, AnyElement) = match self.role {
            IslandRole::Bar => {
                // Bars never follow a launcher resize. They stay compact until
                // the closing animation has fully settled and the surface is
                // handed back, otherwise they would briefly jump to the
                // expanded surface size as the launcher shrinks past the
                // reveal edge.
                let (surface, box_size) = if launcher_engaged {
                    (normal_size, normal_size)
                } else {
                    (window_size, island.size)
                };
                (surface, box_size, self.render_normal())
            }
            IslandRole::Launcher => {
                let content = if launcher_active {
                    self.render_launcher(progress, island, cx)
                } else {
                    self.render_normal()
                };
                (window_size, island.size, content)
            }
        };

        // The surface is transparent around the island, so we fill it with a
        // transparent layout and draw the (animated) island inside it, centered
        // horizontally and pinned to the top.
        div()
            .w(surface_size.width)
            .h(surface_size.height)
            .flex()
            .justify_center()
            .items_start()
            .child(
                div()
                    .w(box_size.width)
                    .h(box_size.height)
                    .bg(island.background)
                    .text_color(island.text_color)
                    .text_size(Rems(0.5))
                    .rounded_b_lg()
                    .font_family("CaskaydiaMono")
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(content),
            )
    }
}
