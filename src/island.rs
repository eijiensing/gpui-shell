use std::time::Duration;

use chrono::Local;
use gpui::{Context, Rems, Render, Window, div, prelude::*};

use crate::appearance::Settings;

pub struct Island {
    time: String,
}

impl Island {
    pub fn new(cx: &mut Context<Self>) -> Self {
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
                    .child(self.time.clone()),
            )
    }
}
