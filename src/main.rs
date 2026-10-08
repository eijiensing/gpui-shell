use std::collections::HashSet;
use std::time::Duration;

use chrono::Local;
use gpui::layer_shell::{Anchor, LayerShellOptions};
use gpui::{
    App, Context, DisplayId, Rems, Render, Window, WindowBounds, WindowKind, WindowOptions, div,
    point, prelude::*, px,
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
            .w(px(48.0))
            .h(px(12.0))
            .bg(gpui::black())
            .text_color(gpui::white())
            .text_size(Rems(0.5))
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

fn open_bar_window(cx: &mut App, display_id: Option<DisplayId>) {
    let _ = cx.open_window(
        WindowOptions {
            display_id,
            window_bounds: Some(WindowBounds::Windowed(gpui::Bounds::new(
                point(px(0.0), px(0.0)),
                gpui::size(px(48.0), px(12.0)),
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
