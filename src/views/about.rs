// Copyright 2026 Tree xie.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::{assets::CustomIconName, states::i18n_about};
use chrono::{Datelike, Local};
use gpui::{App, Bounds, TextAlign, TitlebarOptions, Window, WindowBounds, WindowKind, WindowOptions, div, prelude::*, px, size};
use gpui_component::{ActiveTheme, Icon, Root, h_flex, label::Label, v_flex};
use tracing::info;

struct About;

const VERSION: &str = env!("CARGO_PKG_VERSION");

impl Render for About {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let year = Local::now().year().to_string();
        let years = if year == "2026" {
            "2026".to_string()
        } else {
            format!("2026 - {year}")
        };
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_start()
                    .pt_12()
                    .pb_6()
                    .px_6()
                    .gap_4()
                    // LOGO
                    .child(
                        h_flex().items_center().justify_center().child(
                            Icon::new(CustomIconName::Zap)
                                .size(px(56.))
                                .text_color(cx.theme().primary),
                        ),
                    )
                    .child(Label::new("Zedis").text_xl())
                    .child(
                        Label::new(format!("{} {VERSION}", i18n_about(cx, "version")))
                            .text_sm()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div().w_full().max_w(px(320.)).child(
                            Label::new(i18n_about(cx, "description"))
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .text_align(TextAlign::Center)
                                .whitespace_normal(),
                        ),
                    )
                    .child(
                        div().w_full().max_w(px(320.)).child(
                            Label::new(format!("© {years} Tree xie. All rights reserved."))
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .text_align(TextAlign::Center)
                                .whitespace_normal(),
                        ),
                    ),
            )
    }
}

pub fn open_about_window(cx: &mut App) {
    info!("opening about window");
    let width = px(420.);
    let height = px(300.);
    let window_size = size(width, height);

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, window_size, cx))),
        is_movable: false,
        is_resizable: false,

        titlebar: Some(TitlebarOptions {
            title: Some(i18n_about(cx, "title")),
            appears_transparent: true,
            ..Default::default()
        }),
        focus: true,
        kind: WindowKind::Normal,
        ..Default::default()
    };

    let _ = cx.open_window(options, |window, cx| {
        let dialog = cx.new(|_cx| About);
        cx.new(|cx| Root::new(dialog, window, cx))
    });
}
