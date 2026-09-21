use gpui::layer_shell::{Anchor, LayerShellOptions};
use gpui::{
    App, Context, Window, WindowBounds, WindowKind, WindowOptions, div, prelude::*, px, rgb, size,
};
use gpui_platform::application;

struct RedSquare;

impl Render for RedSquare {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_8().bg(rgb(0xff0000))
    }
}

fn main() {
    application().run(|cx: &mut App| {
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(gpui::Bounds::new(
                    gpui::point(px(0.0), px(0.0)),
                    size(px(45.0), px(25.0)),
                ))),
                kind: WindowKind::LayerShell(LayerShellOptions {
                    layer: gpui::layer_shell::Layer::Top,
                    anchor: Anchor::TOP,
                    exclusive_zone: Some(px(25.0)),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_window, cx| cx.new(|_| RedSquare),
        )
        .unwrap();

        cx.activate(true);
    });
}
