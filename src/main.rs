use std::time::Duration;

use chrono::Local;
use gpui::layer_shell::{Anchor, LayerShellOptions};
use gpui::{
    App, Context, Render, Window, WindowBounds, WindowKind, WindowOptions, div, point, prelude::*,
    px, size,
};
use gpui_platform::application;

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
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(64.0))
            .h(px(16.0))
            .bg(gpui::black())
            .text_color(gpui::white())
            .text_xs()
            .rounded_b_lg()
            .font_family("CaskaydiaMono")
            .flex()
            .items_center()
            .justify_center()
            .child(self.time.clone())
    }
}

fn main() {
    application().run(|cx: &mut App| {
        cx.spawn(|cx: &mut gpui::AsyncApp| {
            let cx = cx.clone();
            async move {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;

                cx.update(|cx: &mut App| {
                    let displays = cx.displays();

                    if displays.is_empty() {
                        open_bar_window(cx, None);
                    } else {
                        for display in displays {
                            open_bar_window(cx, Some(display.id()));
                        }
                    }

                    cx.activate(true);
                })
            }
        })
        .detach();
    });
}

fn open_bar_window(cx: &mut App, display_id: Option<gpui::DisplayId>) {
    cx.open_window(
        WindowOptions {
            display_id,
            window_bounds: Some(WindowBounds::Windowed(gpui::Bounds::new(
                point(px(0.0), px(0.0)),
                size(px(64.0), px(16.0)),
            ))),
            kind: WindowKind::LayerShell(LayerShellOptions {
                layer: gpui::layer_shell::Layer::Top,
                anchor: Anchor::TOP,
                exclusive_zone: Some(px(8.0)),
                ..Default::default()
            }),
            ..Default::default()
        },
        |_window, cx| cx.new(|cx| Island::new(cx)),
    )
    .unwrap();
}
