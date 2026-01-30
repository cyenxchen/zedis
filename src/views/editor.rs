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
    assets::CustomIconName,
    components::{EditValueDialogParams, SelectableTextState, open_edit_value_dialog},
    helpers::{EditorAction, format_duration, humanize_keystroke, validate_ttl},
    states::{KeyType, ServerEvent, ZedisGlobalStore, ZedisServerState, i18n_common, i18n_editor},
    views::{ZedisBytesEditor, ZedisHashEditor, ZedisListEditor, ZedisSetEditor, ZedisZsetEditor},
};
use gpui::{App, ClipboardItem, Entity, FocusHandle, SharedString, Subscription, Window, div, prelude::*, px};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, WindowExt,
    button::Button,
    h_flex,
    input::{Input, InputEvent, InputState},
    label::Label,
    notification::Notification,
    scroll::ScrollableElement,
    v_flex,
};
use humansize::{DECIMAL, format_size};
use rust_i18n::t;
use std::time::{Duration, Instant};
use tracing::{debug, info};

// Constants
const RECENTLY_SELECTED_THRESHOLD_MS: u64 = 300;
const TTL_INPUT_MAX_WIDTH: f32 = 100.0;

/// Main editor component for displaying and editing Redis key values
/// Supports different key types (String, List, etc.) with type-specific editors
pub struct ZedisEditor {
    /// Reference to the server state containing Redis connection and data
    server_state: Entity<ZedisServerState>,

    /// Type-specific editors for different Redis data types
    list_editor: Option<Entity<ZedisListEditor>>,
    bytes_editor: Option<Entity<ZedisBytesEditor>>,
    set_editor: Option<Entity<ZedisSetEditor>>,
    zset_editor: Option<Entity<ZedisZsetEditor>>,
    hash_editor: Option<Entity<ZedisHashEditor>>,

    /// Selectable text state for key name display
    key_text_state: Entity<SelectableTextState>,

    /// TTL editing state
    should_enter_ttl_edit_mode: Option<bool>,
    ttl_edit_mode: bool,
    ttl_input_state: Entity<InputState>,

    /// Track when a key was selected to handle loading states smoothly
    selected_key_at: Option<Instant>,

    /// Focus handle for tracking focus within this component
    focus_handle: FocusHandle,

    /// Event subscriptions for reactive updates
    _subscriptions: Vec<Subscription>,
}

fn format_ttl_string(ttl: &str) -> String {
    let trimmed = ttl.trim();

    let ends_with_digit = trimmed.chars().last().is_some_and(|c| c.is_ascii_digit());

    if ends_with_digit {
        return format!("{}s", trimmed);
    }
    trimmed.to_string()
}

impl ZedisEditor {
    /// Create a new editor instance with event subscriptions
    pub fn new(server_state: Entity<ZedisServerState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = vec![];

        // Initialize TTL input field with placeholder
        let ttl_input_state = cx.new(|cx| {
            InputState::new(window, cx)
                .validate(|s, _cx| {
                    if s.is_empty() {
                        return true;
                    }
                    validate_ttl(&format_ttl_string(s))
                })
                .clean_on_escape()
                .placeholder(i18n_common(cx, "ttl_placeholder"))
        });

        // Subscribe to server events to track when keys are selected
        subscriptions.push(cx.subscribe_in(
            &server_state,
            window,
            |this, server_state, event, window, cx| match event {
                ServerEvent::KeySelected(key) => {
                    this.selected_key_at = Some(Instant::now());
                    // Update the key text state with selected key
                    this.key_text_state.update(cx, |state, _cx| {
                        state.set_text(key.clone());
                    });
                }
                ServerEvent::EditonActionTriggered(action) => match action {
                    EditorAction::UpdateTtl => {
                        this.should_enter_ttl_edit_mode = Some(true);
                        cx.notify();
                    }
                    EditorAction::Reload => {
                        this.reload(cx);
                    }
                    _ => {}
                },
                ServerEvent::ListEditDialogReady(index, bytes) => {
                    this.handle_list_edit_dialog_ready(*index, bytes, server_state, window, cx);
                }
                _ => {}
            },
        ));

        // Subscribe to TTL input events for Enter key and blur
        subscriptions.push(cx.subscribe_in(
            &ttl_input_state,
            window,
            |view, _state, event, window, cx| match &event {
                InputEvent::PressEnter { .. } => {
                    view.handle_update_ttl(window, cx);
                }
                InputEvent::Blur => {
                    view.ttl_edit_mode = false;
                    cx.notify();
                }
                _ => {}
            },
        ));

        info!("Creating new editor view");

        // Initialize selectable text state for key name
        let key_text_state = cx.new(|cx| SelectableTextState::new("", cx));
        let focus_handle = cx.focus_handle();

        Self {
            server_state,
            list_editor: None,
            bytes_editor: None,
            set_editor: None,
            zset_editor: None,
            hash_editor: None,
            key_text_state,
            ttl_edit_mode: false,
            ttl_input_state,
            should_enter_ttl_edit_mode: None,
            focus_handle,
            _subscriptions: subscriptions,
            selected_key_at: None,
        }
    }

    /// Check if a key was selected recently (within threshold)
    /// Used to prevent showing loading indicator immediately after selection
    fn is_selected_key_recently(&self) -> bool {
        self.selected_key_at
            .map(|t| t.elapsed() < Duration::from_millis(RECENTLY_SELECTED_THRESHOLD_MS))
            .unwrap_or(false)
    }

    /// Focuses the keyword filter input field in the current type-specific editor.
    /// Only works for List, Set, Zset, and Hash editors (BytesEditor has no filter).
    pub fn focus_keyword(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = &self.list_editor {
            editor.update(cx, |e, cx| e.focus_keyword(window, cx));
        } else if let Some(editor) = &self.set_editor {
            editor.update(cx, |e, cx| e.focus_keyword(window, cx));
        } else if let Some(editor) = &self.zset_editor {
            editor.update(cx, |e, cx| e.focus_keyword(window, cx));
        } else if let Some(editor) = &self.hash_editor {
            editor.update(cx, |e, cx| e.focus_keyword(window, cx));
        }
        // bytes_editor has no keyword filter functionality
    }

    /// Checks if this editor component contains focus.
    pub fn contains_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle.contains_focused(window, cx)
    }

    /// Handle list edit dialog ready event
    /// Opens the edit value dialog with custom save handler for list items
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    fn handle_list_edit_dialog_ready(
        &mut self,
        index: usize,
        bytes: &[u8],
        server_state: &Entity<ZedisServerState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = server_state.read(cx).key().unwrap_or_default();
        let server_state_clone = server_state.clone();

        // Create custom save handler for list item
        let on_save: std::rc::Rc<dyn Fn(bytes::Bytes, &mut Window, &mut gpui::App) -> bool> = std::rc::Rc::new(
            move |new_bytes: bytes::Bytes, _window: &mut Window, cx: &mut gpui::App| {
                server_state_clone.update(cx, |state, cx| {
                    state.update_list_value_bytes(index, new_bytes, cx);
                });
                true
            },
        );

        open_edit_value_dialog(
            EditValueDialogParams {
                key,
                bytes: bytes::Bytes::from(bytes.to_vec()),
                server_state: server_state.clone(),
                on_save: Some(on_save),
            },
            window,
            cx,
        );
    }

    /// Handle TTL update when user submits new value
    fn handle_update_ttl(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let key = self.server_state.clone().read(cx).key().unwrap_or_default();
        if key.is_empty() {
            return;
        }

        self.ttl_edit_mode = false;
        let ttl = format_ttl_string(&self.ttl_input_state.read(cx).value());

        self.server_state.update(cx, move |state, cx| {
            state.update_key_ttl(key, ttl.into(), cx);
        });
        cx.notify();
    }

    /// Delete the currently selected key with confirmation dialog
    fn delete_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.server_state.read(cx).key() else {
            return;
        };

        let server_state = self.server_state.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let locale = cx.global::<ZedisGlobalStore>().read(cx).locale();
            let message = t!("editor.delete_key_prompt", key = key, locale = locale).to_string();
            let server_state = server_state.clone();
            let key = key.clone();

            dialog
                .confirm()
                .child(v_flex().w_full().max_h(px(200.0)).overflow_y_scrollbar().child(message))
                .on_ok(move |_, window, cx| {
                    let key = key.clone();
                    server_state.update(cx, move |state, cx| {
                        state.delete_key(key, cx);
                    });
                    window.close_dialog(cx);
                    true
                })
        });
    }
    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.server_state.read(cx).key() else {
            return;
        };
        self.server_state.update(cx, move |state, cx| {
            state.select_key(key, cx);
        });
    }
    fn save(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let server_state = self.server_state.read(cx);
        let is_busy = server_state.value().map(|v| v.is_busy()).unwrap_or(false);
        if is_busy {
            return;
        }
        let Some(key) = server_state.key() else {
            return;
        };
        let Some(editor) = self.bytes_editor.as_ref() else {
            return;
        };
        editor.clone().update(cx, move |state, cx| {
            let value = state.value(cx);
            self.server_state.update(cx, move |state, cx| {
                state.save_value(key, value, cx);
            });
        });
    }
    fn enter_ttl_edit_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let server_state = self.server_state.read(cx);
        let Some(value) = server_state.value() else {
            return;
        };
        let is_busy = value.is_busy();
        if is_busy {
            return;
        }
        let ttl: SharedString = value.ttl().unwrap_or_default().to_string().into();
        self.ttl_edit_mode = true;
        self.ttl_input_state.update(cx, move |state, cx| {
            // Clear value if permanent, otherwise use current TTL
            let value = if humantime::parse_duration(&ttl).is_err() {
                SharedString::default()
            } else {
                ttl.clone()
            };
            state.set_value(value, window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }
    /// Render the key information bar with actions (copy, save, TTL, delete)
    fn render_select_key(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let server_state = self.server_state.read(cx);
        let Some(key) = server_state.key() else {
            return h_flex();
        };

        let mut is_busy = false;
        let mut btns = vec![];
        let mut ttl = SharedString::default();
        let mut size = SharedString::default();

        // Extract value information if available
        if let Some(value) = server_state.value() {
            is_busy = value.is_busy();

            // Format TTL display
            ttl = if let Some(ttl) = value.ttl() {
                let seconds = ttl.num_seconds();
                if seconds == -2 {
                    i18n_common(cx, "expired")
                } else if seconds < 0 {
                    i18n_common(cx, "permanent")
                } else {
                    format_duration(Duration::from_secs(seconds as u64)).into()
                }
            } else {
                "--".into()
            };

            size = format_size(value.size() as u64, DECIMAL).into();
        }

        // Show loading only if busy and not recently selected (avoid flashing)
        let should_show_loading = is_busy && !self.is_selected_key_recently();
        // Add size label if available
        if !size.is_empty() {
            let size_label = i18n_common(cx, "size");
            btns.push(
                Label::new(format!("{size_label} : {size}"))
                    .ml_2()
                    .text_sm()
                    .into_any_element(),
            );
        }

        // Add save button for string editor if value is modified
        if let Some(bytes_editor) = &self.bytes_editor {
            let state = bytes_editor.read(cx);
            let value_modified = state.is_value_modified();
            let readonly = state.is_readonly();
            let mut tooltip = if readonly {
                i18n_editor(cx, "can_not_edit_value")
            } else {
                i18n_editor(cx, "save_data_tooltip")
            };
            tooltip = format!("{tooltip} ({})", humanize_keystroke("secondary-s")).into();

            btns.push(
                Button::new("zedis-editor-save-key")
                    .ml_2()
                    .disabled(readonly || !value_modified || should_show_loading)
                    .outline()
                    .label(i18n_common(cx, "save"))
                    .tooltip(tooltip)
                    .icon(CustomIconName::FileCheckCorner)
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        this.save(window, cx);
                    }))
                    .into_any_element(),
            );
        }

        // Add TTL button (or input field when in edit mode)
        if !ttl.is_empty() {
            let ttl_btn = if self.ttl_edit_mode {
                // Show input field with confirmation button
                Input::new(&self.ttl_input_state)
                    .ml_2()
                    .max_w(px(TTL_INPUT_MAX_WIDTH))
                    .suffix(
                        Button::new("zedis-editor-ttl-update-btn")
                            .icon(Icon::new(IconName::Check))
                            .on_click(cx.listener(move |this, _event, window, cx| {
                                this.handle_update_ttl(window, cx);
                            })),
                    )
                    .into_any_element()
            } else {
                // Show TTL button that switches to edit mode on click
                let ttl_tooltip: SharedString = format!(
                    "{} ({})",
                    i18n_editor(cx, "update_ttl_tooltip"),
                    humanize_keystroke("secondary-t")
                )
                .into();
                Button::new("zedis-editor-ttl-btn")
                    .ml_2()
                    .outline()
                    .w(px(TTL_INPUT_MAX_WIDTH))
                    .disabled(should_show_loading)
                    .tooltip(ttl_tooltip)
                    .label(ttl.clone())
                    .icon(CustomIconName::Clock3)
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        this.enter_ttl_edit_mode(window, cx);
                    }))
                    .into_any_element()
            };
            btns.push(ttl_btn);
        }

        let reload_tooltip: SharedString = format!(
            "{} ({})",
            i18n_editor(cx, "reload_key_tooltip"),
            humanize_keystroke("secondary-r")
        )
        .into();
        // reload
        btns.push(
            Button::new("zedis-editor-reload-key")
                .ml_2()
                .outline()
                .disabled(should_show_loading)
                .tooltip(reload_tooltip)
                .icon(CustomIconName::RotateCw)
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.reload(cx);
                }))
                .into_any_element(),
        );

        // Add delete button
        btns.push(
            Button::new("zedis-editor-delete-key")
                .ml_2()
                .outline()
                .disabled(should_show_loading)
                .tooltip(i18n_editor(cx, "delete_key_tooltip"))
                .icon(IconName::CircleX)
                .on_click(cx.listener(move |this, _event, window, cx| {
                    if is_busy {
                        return;
                    }
                    this.delete_key(window, cx);
                }))
                .into_any_element(),
        );

        let content = key.clone();
        h_flex()
            .p_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .items_center()
            .w_full()
            .child(
                // Copy key button
                Button::new("zedis-editor-copy-key")
                    .outline()
                    .tooltip(i18n_editor(cx, "copy_key_tooltip"))
                    .loading(should_show_loading)
                    .icon(IconName::Copy)
                    .on_click(cx.listener(move |_this, _event, window, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(content.to_string()));
                        window.push_notification(Notification::info(i18n_editor(cx, "copied_key_to_clipboard")), cx);
                    })),
            )
            .child(
                // Key name display - w_0 prevents long keys from breaking layout
                div()
                    .flex_1()
                    .w_0()
                    .overflow_hidden()
                    .mx_2()
                    .child(self.key_text_state.clone()),
            )
            .children(btns)
    }
    /// Clean up unused editors when switching between key types
    fn reset_editors(&mut self, key_type: KeyType) {
        if key_type != KeyType::String {
            let _ = self.bytes_editor.take();
        }
        if key_type != KeyType::List {
            let _ = self.list_editor.take();
        }
        if key_type != KeyType::Set {
            let _ = self.set_editor.take();
        }
        if key_type != KeyType::Zset {
            let _ = self.zset_editor.take();
        }
        if key_type != KeyType::Hash {
            let _ = self.hash_editor.take();
        }
    }

    /// Render the appropriate editor based on the key type
    fn render_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(value) = self.server_state.read(cx).value() else {
            self.reset_editors(KeyType::Unknown);
            return div().into_any_element();
        };

        // Don't render anything if key type is unknown and still loading
        if value.key_type == KeyType::Unknown && value.is_busy() {
            return div().into_any_element();
        }

        match value.key_type() {
            KeyType::List => {
                self.reset_editors(KeyType::List);
                let editor = self.list_editor.get_or_insert_with(|| {
                    debug!("Creating new list editor");
                    cx.new(|cx| ZedisListEditor::new(self.server_state.clone(), window, cx))
                });
                editor.clone().into_any_element()
            }
            KeyType::Set => {
                self.reset_editors(KeyType::Set);
                let editor = self.set_editor.get_or_insert_with(|| {
                    debug!("Creating new set editor");
                    cx.new(|cx| ZedisSetEditor::new(self.server_state.clone(), window, cx))
                });
                editor.clone().into_any_element()
            }
            KeyType::Zset => {
                self.reset_editors(KeyType::Zset);
                let editor = self.zset_editor.get_or_insert_with(|| {
                    debug!("Creating new zset editor");
                    cx.new(|cx| ZedisZsetEditor::new(self.server_state.clone(), window, cx))
                });
                editor.clone().into_any_element()
            }
            KeyType::Hash => {
                self.reset_editors(KeyType::Hash);
                let editor = self.hash_editor.get_or_insert_with(|| {
                    debug!("Creating new hash editor");
                    cx.new(|cx| ZedisHashEditor::new(self.server_state.clone(), window, cx))
                });
                editor.clone().into_any_element()
            }
            _ => {
                // Default to bytes editor for String type and other types
                self.reset_editors(KeyType::String);

                let editor = self.bytes_editor.get_or_insert_with(|| {
                    debug!("Creating new bytes editor");
                    cx.new(|cx| ZedisBytesEditor::new(self.server_state.clone(), window, cx))
                });
                editor.clone().into_any_element()
            }
        }
    }
}

impl Render for ZedisEditor {
    /// Main render method - displays key info bar and appropriate editor
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let server_state = self.server_state.read(cx);

        // Don't render anything if no key is selected
        if server_state.key().is_none() {
            return v_flex().into_any_element();
        }
        if let Some(true) = self.should_enter_ttl_edit_mode.take() {
            self.enter_ttl_edit_mode(window, cx);
        }

        v_flex()
            .w_full()
            .h_full()
            .track_focus(&self.focus_handle)
            .child(self.render_select_key(cx))
            .child(self.render_editor(window, cx))
            .on_action(cx.listener(move |this, event: &EditorAction, window, cx| match event {
                EditorAction::Save => {
                    this.save(window, cx);
                }
                _ => {
                    cx.propagate();
                }
            }))
            .into_any_element()
    }
}
