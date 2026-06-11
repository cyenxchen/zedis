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
//! - Save-time validation with error display
//! - Save/Cancel actions

use crate::components::SelectableTextState;
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
    ActiveTheme, Sizable, WindowExt,
    button::{Button, ButtonVariants},
    h_flex, v_flex,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

// Constants
const DEFAULT_TAB_SIZE: usize = 2;

fn supports_json_folding(format: EditFormat) -> bool {
    matches!(
        format,
        EditFormat::Json | EditFormat::MessagePack | EditFormat::ProtobufJson
    )
}

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
    let error_message: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    // Create input state for editor
    // TODO: Syntax highlighting doesn't update when format changes because
    // gpui_component's InputState doesn't support set_language() after construction.
    // Would need gpui_component library changes to support dynamic language switching.
    let default_language = Language::from_str(initial_format.language());

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
            .json_folding(supports_json_folding(initial_format))
            .soft_wrap(true)
    });

    // Set initial value and focus (only once, before dialog opens)
    editor_input.update(cx, |state, cx| {
        state.set_value(initial_text.clone(), window, cx);
        state.focus(window, cx);
    });

    // Create selectable title state BEFORE open_dialog (only once)
    let title_text: SharedString = format!("Edit: {}", key).into();
    let selectable_title = cx.new(|cx| SelectableTextState::new(title_text, cx));

    // Subscribe to editor changes BEFORE open_dialog (only once)
    // This prevents multiple subscriptions from being created on each render frame,
    // which was causing race conditions and session state loss during save.
    //
    // NOTE: Do NOT read the editor value here. `InputState::value()` converts
    // the whole Rope to a String, so doing it per keystroke costs a full-text
    // copy each key press on multi-MB values. The text is pulled from the
    // editor once when saving or switching formats (see `sync_session_text`).
    {
        let error_message_for_input = error_message.clone();

        cx.subscribe(&editor_input, move |_state, event, _cx| {
            if let InputEvent::Change = event {
                *error_message_for_input.borrow_mut() = None;
            }
        })
        .detach();
    }

    // Pull the current editor text into the session (one full-text copy).
    let sync_session_text = {
        let session = session.clone();
        let editor_input = editor_input.clone();
        Rc::new(move |cx: &App| {
            let text = editor_input.read(cx).value();
            let mut s = session.take();
            s.set_editor_text(text);
            session.set(s);
        })
    };

    window.open_dialog(cx, move |dialog, _window, cx| {
        // editor_input and subscription are now captured, not recreated each frame

        // Clones for save handler
        let session_for_save = session.clone();
        let key_for_save = key.clone();
        let server_state_for_save = server_state.clone();
        let error_message_for_save = error_message.clone();
        let on_save_for_ok = on_save.clone();
        let sync_session_text_for_ok = sync_session_text.clone();

        // Clones for footer
        let error_message_for_footer_save = error_message.clone();
        let session_for_footer_save = session.clone();
        let key_for_footer = key.clone();
        let server_state_for_footer = server_state.clone();
        let on_save_for_footer = on_save.clone();
        let sync_session_text_for_footer = sync_session_text.clone();

        // Build format buttons
        let mut format_buttons: Vec<gpui::AnyElement> = Vec::new();
        for (idx, &fmt) in EditFormat::all().iter().enumerate() {
            let is_selected = current_format.get() == fmt;
            let session_clone = session.clone();
            let error_message_clone = error_message.clone();
            let editor_input_clone = editor_input.clone();
            let current_format_clone = current_format.clone();
            let sync_session_text_clone = sync_session_text.clone();

            let btn = if is_selected {
                Button::new(("format", idx)).primary().xsmall().label(fmt.as_str())
            } else {
                Button::new(("format", idx))
                    .outline()
                    .xsmall()
                    .label(fmt.as_str())
                    .on_click(move |_, window: &mut Window, cx: &mut App| {
                        // Pull latest editor text into the session before converting
                        sync_session_text_clone(cx);

                        // Save old format for rollback on failure
                        let old_format = current_format_clone.get();
                        current_format_clone.set(fmt);

                        // Update session format
                        let mut s = session_clone.take();
                        if let Err(e) = s.set_editor_format(fmt) {
                            // Rollback UI state on failure
                            current_format_clone.set(old_format);
                            *error_message_clone.borrow_mut() = Some(e.to_string());
                            window.refresh();
                        } else {
                            // Success: clear error state (don't read old error from session)
                            *error_message_clone.borrow_mut() = None;
                            window.refresh();

                            // Update editor text
                            let new_text = s.editor_text.clone();
                            editor_input_clone.update(cx, |state, cx| {
                                state.set_json_folding(supports_json_folding(fmt), window, cx);
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
            .title(selectable_title.clone())
            .overlay(true)
            .overlay_closable(false)
            .min_w(px(700.0))
            .max_w(px(1200.0))
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        // Format and compression selectors (single row layout)
                        h_flex()
                            .gap_4()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .flex_shrink_0()
                                    .child(Label::new("Format:"))
                                    .children(format_buttons),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .flex_shrink_0()
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
                    .when_some(error_message.borrow().clone(), |this, msg| {
                        this.child(Label::new(msg).text_color(cx.theme().danger).text_sm())
                    }),
            )
            .on_ok({
                move |_, window, cx| {
                    // Pull latest editor text into the session before saving
                    sync_session_text_for_ok(cx);

                    let mut s = session_for_save.take();
                    match s.build_save_bytes() {
                        Ok(bytes) => {
                            let bytes = Bytes::from(bytes);
                            // Use on_save callback if provided, otherwise fallback to save_bytes_value
                            if let Some(ref save_fn) = on_save_for_ok {
                                let ok = save_fn(bytes, window, cx);
                                if !ok {
                                    // Restore session on failure so user can retry
                                    session_for_save.set(s);
                                    return false;
                                }
                                window.close_dialog(cx);
                                return true;
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
                            *error_message_for_save.borrow_mut() = Some(e.to_string());
                            session_for_save.set(s);
                            window.refresh();
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

                    // Clone for the save button callback
                    let session_for_btn = session_for_footer_save.clone();
                    let key_for_btn = key_for_footer.clone();
                    let server_state_for_btn = server_state_for_footer.clone();
                    let on_save_for_btn = on_save_for_footer.clone();
                    let error_message_for_btn = error_message_for_footer_save.clone();
                    let sync_session_text_for_btn = sync_session_text_for_footer.clone();

                    let mut buttons = vec![
                        Button::new("cancel")
                            .label(cancel_label)
                            .on_click(|_, window: &mut Window, cx: &mut App| {
                                window.close_dialog(cx);
                            }),
                        Button::new("save").primary().label(confirm_label).on_click(
                            move |_, window: &mut Window, cx: &mut App| {
                                // Pull latest editor text into the session before saving
                                sync_session_text_for_btn(cx);

                                let mut s = session_for_btn.take();
                                match s.build_save_bytes() {
                                    Ok(bytes) => {
                                        let bytes = Bytes::from(bytes);
                                        // Use on_save callback if provided, otherwise fallback to save_bytes_value
                                        if let Some(ref save_fn) = on_save_for_btn {
                                            let ok = save_fn(bytes, window, cx);
                                            if !ok {
                                                // Restore session on failure so user can retry
                                                session_for_btn.set(s);
                                                return;
                                            }
                                            window.close_dialog(cx);
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
                                        *error_message_for_btn.borrow_mut() = Some(e.to_string());
                                        session_for_btn.set(s);
                                        window.refresh();
                                    }
                                }
                            },
                        ),
                    ];

                    if is_windows() {
                        buttons.reverse();
                    }
                    buttons
                }
            })
    });
}
