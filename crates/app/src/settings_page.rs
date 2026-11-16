//! Settings window (Cmd+,): edits the global settings file, applied live in the main window.
//! Only what the app uses so far; result-table and run options arrive with the features that read them.

use gpui::*;
use gpui_component::{
    ActiveTheme as _, Root,
    button::Button,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    switch::Switch,
    tab::{Tab, TabBar},
};
use serde_json::json;
use siesta_settings::{Appearance, Keymap, Loaded, Split};

use crate::{CloseTab, Siesta, themes::FAMILIES};

const APPEARANCES: [(&str, Appearance); 3] = [("System", Appearance::System), ("Light", Appearance::Light), ("Dark", Appearance::Dark)];
const SPLITS: [(&str, Split); 2] = [("Side by side", Split::Horizontal), ("Stacked", Split::Vertical)];
const KEYMAPS: [(&str, Keymap); 3] = [("Siesta", Keymap::Default), ("VS Code", Keymap::Vscode), ("JetBrains", Keymap::Jetbrains)];

impl Siesta {
    /// Bring the settings window forward, opening it if it is not open.
    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(h) = self.settings_window
            && h.update(cx, |_, w, _| w.activate_window()).is_ok()
        {
            return;
        }
        let (app, main) = (cx.entity().downgrade(), window.window_handle());
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(680.), px(640.)), cx)),
            titlebar: Some(TitlebarOptions { title: Some("Settings".into()), ..Default::default() }),
            is_resizable: false,
            is_minimizable: false,
            ..Default::default()
        };
        // Deferred: the new window reads `Siesta` while building and drawing its first frame,
        // which panics while `Siesta` is still being updated (we are inside its listener).
        cx.defer(move |cx| {
            let weak = app.clone();
            let h = cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| SettingsWindow::new(weak, main, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            });
            match h {
                Ok(h) => _ = app.update(cx, |this, _| this.settings_window = Some(h)),
                Err(e) => eprintln!("siesta: settings window: {e}"),
            }
        });
    }
}

pub(crate) struct SettingsWindow {
    app: WeakEntity<Siesta>,
    /// The main window: settings are applied there (editors, theme).
    main: AnyWindowHandle,
    focus: FocusHandle,
    font_input: Entity<InputState>,
    prod_input: Entity<InputState>,
    _subs: Vec<Subscription>,
}

impl SettingsWindow {
    fn new(app: WeakEntity<Siesta>, main: AnyWindowHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let s = app.upgrade().map(|a| a.read(cx).settings.settings.clone()).unwrap_or_default();
        let font_input = cx.new(|cx| InputState::new(window, cx).placeholder("Default monospace font").default_value(s.font_family.clone()));
        let prod_input = cx.new(|cx| InputState::new(window, cx).placeholder("prod*").default_value(s.connections.prod_pattern.clone()));
        // Only real edits are written: syncing an input from the file must not write it back
        // (a workspace override would land in the global file).
        let mut _subs = vec![
            cx.subscribe(&font_input, |this, input, ev: &InputEvent, cx| {
                let v = input.read(cx).value().trim().to_string();
                if matches!(ev, InputEvent::Change) && Some(&v) != this.current(cx).map(|s| s.font_family).as_ref() {
                    this.set("font_family", v.into(), cx);
                }
            }),
            cx.subscribe(&prod_input, |this, input, ev: &InputEvent, cx| {
                let v = input.read(cx).value().trim().to_string();
                if matches!(ev, InputEvent::Change) && Some(&v) != this.current(cx).map(|s| s.connections.prod_pattern).as_ref() {
                    this.set("connections.prod_pattern", v.into(), cx);
                }
            }),
        ];
        // Settings changed elsewhere (the file on disk): refresh the text inputs not being edited.
        if let Some(entity) = app.upgrade() {
            _subs.push(cx.observe_in(&entity, window, |this, app, window, cx| {
                let s = app.read(cx).settings.settings.clone();
                let inputs = [(&this.font_input, s.font_family), (&this.prod_input, s.connections.prod_pattern)];
                for (input, value) in inputs {
                    if !input.focus_handle(cx).is_focused(window) && input.read(cx).value() != value {
                        input.update(cx, |i, cx| i.set_value(value, window, cx));
                    }
                }
                cx.notify();
            }));
        }
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self { app, main, focus, font_input, prod_input, _subs }
    }

    fn current(&self, cx: &App) -> Option<siesta_settings::Settings> {
        self.app.upgrade().map(|a| a.read(cx).settings.settings.clone())
    }

    fn set(&mut self, key: &str, value: serde_json::Value, cx: &mut Context<Self>) {
        let app = self.app.clone();
        self.main.update(cx, |_, window, cx| app.update(cx, |this, cx| this.set(key, value, window, cx))).ok();
        cx.notify();
    }

    /// Label, control, and a note when the workspace file overrides the key.
    fn row(l: &Loaded, label: &str, key: &str, control: AnyElement, cx: &App) -> Div {
        let note = l.overridden(key).then(|| div().text_xs().text_color(cx.theme().warning).child("overridden by .siesta/settings.json"));
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .py_1p5()
            .child(div().flex().flex_col().child(label.to_string()).children(note))
            .child(control)
    }

    fn switch(key: &'static str, checked: bool, cx: &mut Context<Self>) -> AnyElement {
        Switch::new(key).checked(checked).on_click(cx.listener(move |this, v: &bool, _, cx| this.set(key, json!(*v), cx))).into_any_element()
    }

    /// − value + ; integers stay integers in the file.
    fn stepper(key: &'static str, value: f64, step: f64, min: f64, max: f64, cx: &mut Context<Self>) -> AnyElement {
        let button = |label: &'static str, next: f64| {
            Button::new((key, label.len())).label(label).on_click(cx.listener(move |this, _, _, cx| {
                this.set(key, json!(next.clamp(min, max) as i64), cx);
            }))
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(button("−", value - step))
            .child(div().w(px(48.)).flex().justify_center().child(format!("{}", value as i64)))
            .child(button("+", value + step))
            .into_any_element()
    }
}

/// A segmented choice bar over `(label, value)` options.
fn segmented<T: Copy + PartialEq + serde::Serialize + 'static>(
    id: &'static str,
    key: &'static str,
    options: &'static [(&'static str, T)],
    current: T,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let ix = options.iter().position(|(_, v)| *v == current).unwrap_or(0);
    options
        .iter()
        .fold(
            TabBar::new(id).segmented().selected_index(ix).on_click(cx.listener(move |this, ix: &usize, _, cx| {
                this.set(key, json!(options[*ix].1), cx);
            })),
            |bar, (label, _)| bar.child(Tab::new().label(*label)),
        )
        .into_any_element()
}

impl Render for SettingsWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(l) = self.app.upgrade().map(|a| a.read(cx).settings.clone()) else { return div().into_any_element() };
        let s = l.settings.clone();
        let muted = cx.theme().muted_foreground;
        let section = |title: &str| div().mt_6().mb_1().text_xs().font_weight(FontWeight::BOLD).text_color(muted).child(title.to_uppercase());
        let switch = |key: &'static str, on: bool, cx: &mut Context<Self>| Self::switch(key, on, cx);

        let mode = segmented("appearance", "appearance", &APPEARANCES, s.appearance, cx);
        let split = segmented("split", "split", &SPLITS, s.split, cx);
        let keymap = segmented("keymap", "keymap", &KEYMAPS, s.keymap, cx);
        let label = |name: &str, light: &Option<&str>, dark: &Option<&str>| match (light, dark) {
            (None, Some(_)) => format!("{name} (dark only)"),
            (Some(_), None) => format!("{name} (light only)"),
            _ => name.to_string(),
        };
        let current = FAMILIES.iter().find(|f| f.0 == s.theme).map_or(s.theme.clone(), |(n, l, d)| label(n, l, d));
        let view = cx.entity().downgrade();
        let selected = s.theme.clone();
        let families = Button::new("theme").label(current).dropdown_caret(true).outline().w(px(260.)).dropdown_menu(move |menu, _, _| {
            FAMILIES.iter().fold(menu.scrollable(true).max_h(px(420.)), |menu, (name, light, dark)| {
                let view = view.clone();
                menu.item(PopupMenuItem::new(label(name, light, dark)).checked(selected == *name).on_click(move |_, _, cx| {
                    view.update(cx, |this, cx| this.set("theme", json!(name), cx)).ok();
                }))
            })
        });

        let path = l.global_path.as_ref().map_or("—".into(), |p| p.display().to_string());
        let body = div()
            .max_w(px(620.))
            .flex()
            .flex_col()
            .child(section("Appearance"))
            .child(Self::row(&l, "Theme", "theme", families.into_any_element(), cx))
            .child(Self::row(&l, "Appearance", "appearance", mode, cx))
            .child(Self::row(&l, "Interface font size", "ui_font_size", Self::stepper("ui_font_size", s.ui_font_size as f64, 1., 10., 24., cx), cx))
            .child(Self::row(&l, "Editor font", "font_family", Input::new(&self.font_input).w(px(260.)).into_any_element(), cx))
            .child(Self::row(&l, "Editor font size", "font_size", Self::stepper("font_size", s.font_size as f64, 1., 9., 32., cx), cx))
            .child(section("Editor"))
            .child(Self::row(&l, "Line numbers", "editor.line_numbers", switch("editor.line_numbers", s.editor.line_numbers, cx), cx))
            .child(Self::row(&l, "Soft wrap", "editor.soft_wrap", switch("editor.soft_wrap", s.editor.soft_wrap, cx), cx))
            .child(Self::row(
                &l,
                "Highlight the current line",
                "editor.highlight_current_line",
                switch("editor.highlight_current_line", s.editor.highlight_current_line, cx),
                cx,
            ))
            .child(section("Result panel"))
            .child(Self::row(&l, "Layout", "split", split, cx))
            .child(section("Connections"))
            .child(Self::row(&l, "Production pattern", "connections.prod_pattern", Input::new(&self.prod_input).w(px(160.)).into_any_element(), cx))
            .child(div().text_xs().text_color(muted).child("Matching connections get a red banner above the editor. * matches any suffix."))
            .child(section("Keyboard"))
            .child(Self::row(&l, "Extra shortcuts", "keymap", keymap, cx))
            .child(div().text_xs().text_color(muted).child("Your own: keymap.json next to the settings file (Zed's format)."))
            .child(div().mt_8().text_xs().text_color(muted).child(format!("Saved to {path}. A workspace's .siesta/settings.json overrides these values.")));

        div()
            .id("settings")
            .key_context("Siesta")
            .track_focus(&self.focus)
            .on_action(|_: &CloseTab, window, _| window.remove_window())
            .size_full()
            .overflow_y_scroll()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .px_8()
            .pb_6()
            .child(body)
            .into_any_element()
    }
}
