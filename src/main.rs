#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod analysis;
mod capture;
mod model;
mod overlay;
mod pattern;
mod ui;
mod workflow;

use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1220.0), px(780.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(1050.0), px(700.0))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Card Reader Comparator".into()),
                    appears_transparent: false,
                    traffic_light_position: None,
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|_| ui::ComparatorApp::new()),
        )
        .expect("failed to open window");
        cx.activate(true);
    });
}
