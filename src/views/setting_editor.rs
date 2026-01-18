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

use crate::{
    helpers::get_or_create_config_dir,
    states::{PresetCredential, ZedisGlobalStore, i18n_settings, update_app_state_and_save},
};
use gpui::{Entity, Subscription, Window, prelude::*};
use gpui_component::{
    form::{field, v_form},
    input::{Input, InputEvent, InputState, NumberInput, NumberInputEvent, StepAction},
    label::Label,
    v_flex,
};
use tracing::info;

pub struct ZedisSettingEditor {
    max_key_tree_depth_state: Entity<InputState>,
    key_separator_state: Entity<InputState>,
    max_truncate_length_state: Entity<InputState>,
    config_dir_state: Entity<InputState>,
    preset_credentials_state: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

/// Convert credentials to display text (newline-separated, format: password or username:password)
fn credentials_to_text(credentials: &[PresetCredential]) -> String {
    credentials
        .iter()
        .map(|c| match &c.username {
            Some(u) => format!("{}:{}", u, c.password),
            None => c.password.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse text to credentials (newline-separated, format: password or username:password)
fn text_to_credentials(text: &str) -> Vec<PresetCredential> {
    text.lines()
        .map(|s| s.trim())
        .filter_map(PresetCredential::from_str)
        .collect()
}

impl ZedisSettingEditor {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let store = cx.global::<ZedisGlobalStore>().read(cx);
        let max_key_tree_depth = store.max_key_tree_depth();
        let key_separator = store.key_separator().to_string();
        let max_truncate_length = store.max_truncate_length();
        let preset_credentials = store.preset_credentials();
        let preset_credentials_text = credentials_to_text(&preset_credentials);

        let max_key_tree_depth_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(i18n_settings(cx, "max_key_tree_depth_placeholder"))
                .default_value(max_key_tree_depth.to_string())
        });
        let key_separator_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(i18n_settings(cx, "key_separator_placeholder"))
                .default_value(key_separator)
        });
        let max_truncate_length_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(i18n_settings(cx, "max_truncate_length_placeholder"))
                .default_value(max_truncate_length.to_string())
        });
        let preset_credentials_state = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(3, 10)
                .placeholder(i18n_settings(cx, "preset_credentials_placeholder"))
                .default_value(preset_credentials_text)
        });

        let config_dir = get_or_create_config_dir().unwrap_or_default();

        let mut subscriptions = Vec::new();
        subscriptions.push(
            cx.subscribe_in(&max_key_tree_depth_state, window, |_view, state, event, _window, cx| {
                if let InputEvent::Blur = &event {
                    let text = state.read(cx).value();
                    let value = text.parse::<i64>().unwrap_or_default();
                    update_app_state_and_save(cx, "save_max_key_tree_depth", move |state, _cx| {
                        state.set_max_key_tree_depth(value as usize);
                    });
                }
            }),
        );
        subscriptions.push(
            cx.subscribe_in(&max_key_tree_depth_state, window, |_view, state, event, window, cx| {
                let NumberInputEvent::Step(action) = event;

                let Ok(current_val) = state.read(cx).value().parse::<u16>() else {
                    return;
                };

                let new_val = match action {
                    StepAction::Increment => current_val.saturating_add(1),
                    StepAction::Decrement => current_val.saturating_sub(1),
                };

                if new_val != current_val {
                    state.update(cx, |input, cx| {
                        input.set_value(new_val.to_string(), window, cx);
                    });
                }
            }),
        );

        subscriptions.push(
            cx.subscribe_in(&key_separator_state, window, |_view, state, event, _window, cx| {
                if let InputEvent::Blur = &event {
                    let text = state.read(cx).value();
                    update_app_state_and_save(cx, "save_key_separator", move |state, _cx| {
                        state.set_key_separator(text.to_string());
                    });
                }
            }),
        );

        subscriptions.push(cx.subscribe_in(
            &max_truncate_length_state,
            window,
            |_view, state, event, _window, cx| {
                if let InputEvent::Blur = &event {
                    let Ok(value) = state.read(cx).value().parse::<usize>() else {
                        return;
                    };
                    if value < 10 {
                        return;
                    };
                    update_app_state_and_save(cx, "save_max_truncate_length", move |state, _cx| {
                        state.set_max_truncate_length(value);
                    });
                }
            },
        ));

        subscriptions.push(cx.subscribe_in(
            &preset_credentials_state,
            window,
            |_view, state: &Entity<InputState>, event: &InputEvent, _window, cx| {
                if let InputEvent::Blur = &event {
                    let text = state.read(cx).value();
                    let credentials = text_to_credentials(&text);
                    info!(
                        "preset_credentials Blur: text_lines={}, parsed_credentials_count={}",
                        text.lines().count(),
                        credentials.len()
                    );
                    for (i, cred) in credentials.iter().enumerate() {
                        info!(
                            "  credential[{}]: username={:?}, password_len={}",
                            i,
                            cred.username,
                            cred.password.len()
                        );
                    }
                    update_app_state_and_save(cx, "save_preset_credentials", move |state, _cx| {
                        state.set_preset_credentials(credentials);
                    });
                }
            },
        ));

        let config_dir_state =
            cx.new(|cx| InputState::new(window, cx).default_value(config_dir.to_string_lossy().to_string()));

        Self {
            _subscriptions: subscriptions,
            config_dir_state,
            max_truncate_length_state,
            key_separator_state,
            max_key_tree_depth_state,
            preset_credentials_state,
        }
    }
}

impl Render for ZedisSettingEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .p_5()
            .child(Label::new(i18n_settings(cx, "title")).text_3xl().mb_2())
            .child(
                v_form()
                    .flex_1()
                    .columns(2)
                    .child(
                        field()
                            .label(i18n_settings(cx, "max_key_tree_depth"))
                            .child(NumberInput::new(&self.max_key_tree_depth_state)),
                    )
                    .child(
                        field()
                            .label(i18n_settings(cx, "key_separator"))
                            .child(Input::new(&self.key_separator_state)),
                    )
                    .child(
                        field()
                            .label(i18n_settings(cx, "max_truncate_length"))
                            .child(Input::new(&self.max_truncate_length_state)),
                    )
                    .child(
                        field()
                            .label(i18n_settings(cx, "config_dir"))
                            .child(Input::new(&self.config_dir_state).disabled(true)),
                    )
                    .child(
                        field()
                            .label(i18n_settings(cx, "preset_credentials"))
                            .child(Input::new(&self.preset_credentials_state)),
                    ),
            )
    }
}
