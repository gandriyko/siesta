use gpui::*;
use gpui_component::Root;

struct Siesta;

impl Render for Siesta {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full()
    }
}

fn main() {
    gpui_platform::application().with_assets(gpui_kit_assets::Assets).run(|cx: &mut App| {
        gpui_component::init(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1440.), px(880.)), cx)),
            titlebar: Some(TitlebarOptions { title: Some("Siesta".into()), ..Default::default() }),
            ..Default::default()
        };
        cx.open_window(options, |window, cx| {
            let view = cx.new(|_| Siesta);
            cx.new(|cx| Root::new(view, window, cx))
        })
        .unwrap();
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.activate(true);
    });
}
