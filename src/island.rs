use std::time::Duration;

use chrono::Local;
use gpui::{Context, Rems, Render, Window, div, prelude::*};

use crate::appearance::Settings;
use crate::audio::{self, Volume};

/// How long the volume stays on screen after the last change before the island
/// shows the clock again.
const VOLUME_HOLD: Duration = Duration::from_millis(2000);

pub struct Island {
    time: String,
    /// Latest sink state, shown in place of the clock while `Some`.
    volume: Option<Volume>,
    /// Bumped on every change so a superseded revert timer can be ignored.
    generation: u64,
}

impl Island {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let island = Self {
            time: Local::now().format("%H:%M").to_string(),
            volume: None,
            generation: 0,
        };

        island.start_timer(cx);
        island.watch_volume(cx);

        island
    }

    /// Shows the default sink's volume, reverting to the clock shortly after
    /// the last change.
    fn watch_volume(&self, cx: &mut Context<Self>) {
        let volume_rx = audio::subscribe();

        cx.spawn(async move |island, cx| {
            while let Ok(volume) = volume_rx.recv().await {
                let Ok(generation) = island.update(cx, |island, cx| {
                    island.volume = Some(volume);
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
                                    island.volume = None;
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
        let settings = cx.global::<Settings>();
        let window_size = settings.window_size;
        let island = settings.current;

        // The surface is transparent around the island, so we fill it with a
        // transparent layout and draw the (animated) island inside it, centered
        // horizontally and pinned to the top.
        div()
            .w(window_size.width)
            .h(window_size.height)
            .flex()
            .justify_center()
            .items_start()
            .child(
                div()
                    .w(island.size.width)
                    .h(island.size.height)
                    .bg(island.background)
                    .text_color(island.text_color)
                    .text_size(Rems(0.5))
                    .rounded_b_lg()
                    .font_family("CaskaydiaMono")
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(match self.volume {
                        Some(Volume::Muted) => "muted".to_string(),
                        Some(Volume::Percent(percent)) => format!("{percent}%"),
                        None => self.time.clone(),
                    }),
            )
    }
}
