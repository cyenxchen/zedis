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

//! Edit value dialog component for editing Redis bytes values.
//!
//! This component provides:
//! - Modal dialog for editing binary/text values
//! - Format switching (Text, JSON, Hex, MessagePack)
//! - Compression format selection (None, Gzip, Zstd, Snappy, LZ4)
//! - Real-time validation with error display
//! - Save/Cancel actions

use crate::helpers::codec::{CompressionFormat, EditFormat};
use crate::helpers::get_font_family;
use crate::helpers::is_windows;
use crate::states::edit_session::EditSession;
use crate::states::{ZedisServerState, i18n_common};
use bytes::Bytes;
use gpui::{App, Entity, SharedString, Window, prelude::*, px};
use gpui_component::highlighter::Language;
use gpui_component::input::{Input, InputEvent, InputState, TabSize};
use gpui_component::label::Label;
use gpui_component::{
    ActiveTheme, Disableable, Sizable, WindowExt,
    button::{Button, ButtonVariants},
    h_flex, v_flex,
};
use std::cell::Cell;
use std::rc::Rc;

// Constants
const DEFAULT_TAB_SIZE: usize = 2;

/// Configuration for the edit value dialog
#[allow(clippy::type_complexity)]
pub struct EditValueDialogParams {
    /// The Redis key being edited
    pub key: SharedString,
    /// Original bytes value
    pub bytes: Bytes,
    /// Server state for saving
    pub server_state: Entity<ZedisServerState>,
    /// Custom save handler (optional)
    /// If provided, this callback will be used instead of the default save_bytes_value
    pub on_save: Option<Rc<dyn Fn(Bytes, &mut Window, &mut App) -> bool>>,
}

/// Open the edit value dialog
pub fn open_edit_value_dialog(params: EditValueDialogParams, window: &mut Window, cx: &mut App) {
    // Create edit session
    let mut session = EditSession::new(params.key.clone(), params.bytes);

    // Initialize the session (detect format, decompress, etc.)
    if let Err(e) = session.detect_and_init() {
        // Show error notification and return
        window.push_notification(gpui_component::notification::Notification::error(e.to_string()), cx);
        return;
    }

    // Check if the data is in preview mode (truncated)
    if session.is_preview {
        window.push_notification(
            gpui_component::notification::Notification::warning(
                "Cannot edit truncated data. Please load the full value first.",
            ),
            cx,
        );
        return;
    }

    let session = Rc::new(Cell::new(session));
    let server_state = params.server_state.clone();
    let key = params.key.clone();
    let on_save = params.on_save;

    // Create editor state
    let editor_session = session.clone();
    let initial_session = editor_session.take();
    let initial_text = initial_session.editor_text.clone();
    let initial_format = initial_session.editor_format;
    let initial_compression = initial_session.save_compression;
    editor_session.set(initial_session);

    // Track current format and compression
    let current_format = Rc::new(Cell::new(initial_format));
    let current_compression = Rc::new(Cell::new(initial_compression));
    let has_error = Rc::new(Cell::new(false));
    let error_message: Rc<Cell<Option<String>>> = Rc::new(Cell::new(None));

    // Create input state for editor
    let default_language = Language::from_str(initial_format.language());

    let editor_text = Rc::new(Cell::new(initial_text.clone()));

    // Create editor input state BEFORE open_dialog (only once)
    // This prevents the input from being recreated on every render frame,
    // which was causing set_value/focus to be called repeatedly and
    // resetting the cursor position and triggering validation errors.
    let editor_input = cx.new(|cx| {
        InputState::new(window, cx)
            .code_editor(default_language.name())
            .line_number(true)
            .indent_guides(true)
            .tab_size(TabSize {
                tab_size: DEFAULT_TAB_SIZE,
                hard_tabs: false,
            })
            .searchable(true)
            .soft_wrap(true)
    });

    // Set initial value and focus (only once, before dialog opens)
    editor_input.update(cx, |state, cx| {
        state.set_value(initial_text.clone(), window, cx);
        state.focus(window, cx);
    });

    // Subscribe to editor changes for validation BEFORE open_dialog (only once)
    // This prevents multiple subscriptions from being created on each render frame,
    // which was causing race conditions and session state loss during save.
    {
        let session_for_validation = session.clone();
        let has_error_for_validation = has_error.clone();
        let error_message_for_validation = error_message.clone();
        let editor_text_for_validation = editor_text.clone();

        cx.subscribe(&editor_input, move |_state, event, cx| {
            if let InputEvent::Change = event {
                let text = _state.read(cx).value();
                editor_text_for_validation.set(text.clone());

                let mut s = session_for_validation.take();
                s.set_editor_text(text);
                let valid = s.valid;
                let err = s.error.clone();
                session_for_validation.set(s);

                has_error_for_validation.set(!valid);
                error_message_for_validation.set(err);
            }
        })
        .detach();
    }

    window.open_dialog(cx, move |dialog, _window, cx| {
        // editor_input and subscription are now captured, not recreated each frame

        let title = format!("Edit: {}", key);

        // Clones for save handler
        let session_for_save = session.clone();
        let key_for_save = key.clone();
        let server_state_for_save = server_state.clone();
        let has_error_for_save = has_error.clone();
        let on_save_for_ok = on_save.clone();

        // Clones for footer
        let has_error_for_footer = has_error.clone();
        let session_for_footer_save = session.clone();
        let key_for_footer = key.clone();
        let server_state_for_footer = server_state.clone();
        let on_save_for_footer = on_save.clone();

        // Build format buttons
        let mut format_buttons: Vec<gpui::AnyElement> = Vec::new();
        for (idx, &fmt) in EditFormat::all().iter().enumerate() {
            let is_selected = current_format.get() == fmt;
            let session_clone = session.clone();
            let has_error_clone = has_error.clone();
            let error_message_clone = error_message.clone();
            let editor_input_clone = editor_input.clone();
            let editor_text_clone = editor_text.clone();
            let current_format_clone = current_format.clone();

            let btn = if is_selected {
                Button::new(("format", idx)).primary().xsmall().label(fmt.as_str())
            } else {
                Button::new(("format", idx))
                    .outline()
                    .xsmall()
                    .label(fmt.as_str())
                    .on_click(move |_, window: &mut Window, cx: &mut App| {
                        // Save old format for rollback on failure
                        let old_format = current_format_clone.get();
                        current_format_clone.set(fmt);

                        // Update session format
                        let mut s = session_clone.take();
                        if let Err(e) = s.set_editor_format(fmt) {
                            // Rollback UI state on failure
                            current_format_clone.set(old_format);
                            has_error_clone.set(true);
                            error_message_clone.set(Some(e.to_string()));
                        } else {
                            // Success: clear error state (don't read old error from session)
                            has_error_clone.set(false);
                            error_message_clone.set(None);

                            // Update editor text
                            let new_text = s.editor_text.clone();
                            editor_text_clone.set(new_text.clone());
                            editor_input_clone.update(cx, |state, cx| {
                                state.set_value(new_text, window, cx);
                            });
                        }
                        session_clone.set(s);
                    })
            };
            format_buttons.push(btn.into_any_element());
        }

        // Build compression buttons
        let mut compression_buttons: Vec<gpui::AnyElement> = Vec::new();
        for (idx, &comp) in CompressionFormat::all().iter().enumerate() {
            let is_selected = current_compression.get() == comp;
            let session_clone = session.clone();
            let current_compression_clone = current_compression.clone();

            let btn = if is_selected {
                Button::new(("comp", idx)).primary().xsmall().label(comp.as_str())
            } else {
                Button::new(("comp", idx))
                    .outline()
                    .xsmall()
                    .label(comp.as_str())
                    .on_click(move |_, _window: &mut Window, _cx: &mut App| {
                        current_compression_clone.set(comp);

                        // Update session compression
                        let mut s = session_clone.take();
                        s.set_save_compression(comp);
                        session_clone.set(s);
                    })
            };
            compression_buttons.push(btn.into_any_element());
        }

        dialog
            .title(title)
            .overlay(true)
            .overlay_closable(false)
            .min_w(px(600.0))
            .max_w(px(1200.0))
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        // Format and compression selectors
                        h_flex()
                            .gap_4()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(Label::new("Format:"))
                                    .children(format_buttons),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(Label::new("Compression:"))
                                    .children(compression_buttons),
                            ),
                    )
                    .child(
                        // Editor
                        Input::new(&editor_input)
                            .h(px(400.0))
                            .w_full()
                            .font_family(get_font_family())
                            .bordered(true),
                    )
                    .when_some(error_message.take(), |this, msg| {
                        this.child(Label::new(msg).text_color(cx.theme().danger).text_sm())
                    }),
            )
            .on_ok({
                move |_, window, cx| {
                    if has_error_for_save.get() {
                        return false;
                    }

                    let mut s = session_for_save.take();
                    match s.build_save_bytes() {
                        Ok(bytes) => {
                            let bytes = Bytes::from(bytes);
                            // Use on_save callback if provided, otherwise fallback to save_bytes_value
                            if let Some(ref save_fn) = on_save_for_ok {
                                if save_fn(bytes, window, cx) {
                                    window.close_dialog(cx);
                                    return true;
                                }
                                return false;
                            }
                            // Fallback: save bytes value directly
                            let key = key_for_save.clone();
                            server_state_for_save.update(cx, move |state, cx| {
                                state.save_bytes_value(key, bytes, cx);
                            });
                            window.close_dialog(cx);
                            true
                        }
                        Err(e) => {
                            window.push_notification(
                                gpui_component::notification::Notification::error(e.to_string()),
                                cx,
                            );
                            session_for_save.set(s);
                            false
                        }
                    }
                }
            })
            .on_cancel(|_, window, cx| {
                window.close_dialog(cx);
                true
            })
            .footer({
                move |_, _, _, cx| {
                    let confirm_label = i18n_common(cx, "save");
                    let cancel_label = i18n_common(cx, "cancel");
                    let can_save = !has_error_for_footer.get();

                    // Clone for the save button callback
                    let session_for_btn = session_for_footer_save.clone();
                    let key_for_btn = key_for_footer.clone();
                    let server_state_for_btn = server_state_for_footer.clone();
                    let on_save_for_btn = on_save_for_footer.clone();

                    let mut buttons = vec![
                        Button::new("cancel")
                            .label(cancel_label)
                            .on_click(|_, window: &mut Window, cx: &mut App| {
                                window.close_dialog(cx);
                            }),
                        Button::new("save")
                            .primary()
                            .disabled(!can_save)
                            .label(confirm_label)
                            .on_click(move |_, window: &mut Window, cx: &mut App| {
                                let mut s = session_for_btn.take();
                                match s.build_save_bytes() {
                                    Ok(bytes) => {
                                        let bytes = Bytes::from(bytes);
                                        // Use on_save callback if provided, otherwise fallback to save_bytes_value
                                        if let Some(ref save_fn) = on_save_for_btn {
                                            if save_fn(bytes, window, cx) {
                                                window.close_dialog(cx);
                                            }
                                            return;
                                        }
                                        // Fallback: save bytes value directly
                                        let key = key_for_btn.clone();
                                        server_state_for_btn.update(cx, move |state, cx| {
                                            state.save_bytes_value(key, bytes, cx);
                                        });
                                        window.close_dialog(cx);
                                    }
                                    Err(e) => {
                                        window.push_notification(
                                            gpui_component::notification::Notification::error(e.to_string()),
                                            cx,
                                        );
                                        session_for_btn.set(s);
                                    }
                                }
                            }),
                    ];

                    if is_windows() {
                        buttons.reverse();
                    }
                    buttons
                }
            })
    });
}
