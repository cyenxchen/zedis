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

//! Selectable text component - supports mouse drag selection and keyboard copy

use gpui::{
    actions, point, px, quad, App, BorderStyle, Bounds, ClipboardItem, Context, CursorStyle, Edges,
    Element, ElementId, Entity, FocusHandle, Focusable, GlobalElementId, Hitbox, HitboxBehavior,
    InspectorElementId, InteractiveElement, IntoElement, KeyBinding, LayoutId, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Render, SharedString, StyledText,
    TextLayout, Window, div,
};
use gpui_component::ActiveTheme;

actions!(selectable_text, [Copy]);

/// Initialize keyboard shortcuts for selectable text
pub fn init(cx: &mut App) {
    cx.bind_keys(vec![
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-c", Copy, Some("SelectableText")),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-c", Copy, Some("SelectableText")),
    ]);
}

/// Selectable text state - used for tracking selection
pub struct SelectableTextState {
    text: SharedString,
    focus_handle: FocusHandle,
    /// Selection start and end byte indices
    selection: Option<(usize, usize)>,
    /// Whether currently selecting
    is_selecting: bool,
}

impl SelectableTextState {
    pub fn new(text: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            text: text.into(),
            focus_handle: cx.focus_handle(),
            selection: None,
            is_selecting: false,
        }
    }

    /// Update the text content
    pub fn set_text(&mut self, text: impl Into<SharedString>) {
        self.text = text.into();
        self.selection = None;
        self.is_selecting = false;
    }

    /// Get the selected text
    fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection?;
        if start == end {
            return None;
        }
        let (min, max) = if start < end { (start, end) } else { (end, start) };
        let text = self.text.as_ref();
        if max <= text.len() {
            Some(text[min..max].to_string())
        } else {
            None
        }
    }

    fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }
}

impl Focusable for SelectableTextState {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SelectableTextState {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = self.text.clone();
        let selection = self.selection;

        div()
            .id("selectable-text")
            .key_context("SelectableText")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::copy))
            .child(SelectableTextElement::new(
                "selectable-text-element",
                text,
                selection,
                cx.entity().clone(),
            ))
    }
}

/// Internal element for rendering selectable text
struct SelectableTextElement {
    id: ElementId,
    text: SharedString,
    styled_text: StyledText,
    selection: Option<(usize, usize)>,
    state_entity: Entity<SelectableTextState>,
}

impl SelectableTextElement {
    fn new(
        id: impl Into<ElementId>,
        text: SharedString,
        selection: Option<(usize, usize)>,
        state_entity: Entity<SelectableTextState>,
    ) -> Self {
        Self {
            id: id.into(),
            styled_text: StyledText::new(text.clone()),
            text,
            selection,
            state_entity,
        }
    }

    /// Paint selection highlight
    fn paint_selection(&self, text_layout: &TextLayout, window: &mut Window, cx: &mut App) {
        let Some((start, end)) = self.selection else {
            return;
        };
        if start == end {
            return;
        }

        let (min, max) = if start < end { (start, end) } else { (end, start) };
        let Some(start_pos) = text_layout.position_for_index(min) else {
            return;
        };
        let Some(end_pos) = text_layout.position_for_index(max) else {
            return;
        };

        let line_height = text_layout.line_height();
        window.paint_quad(quad(
            Bounds::from_corners(start_pos, point(end_pos.x, end_pos.y + line_height)),
            px(0.),
            cx.theme().selection,
            Edges::default(),
            gpui::transparent_black(),
            BorderStyle::default(),
        ));
    }
}

impl IntoElement for SelectableTextElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SelectableTextElement {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let text_style = window.text_style();
        self.styled_text =
            StyledText::new(self.text.clone()).with_runs(vec![text_style.to_run(self.text.len())]);
        let (layout_id, _) = self
            .styled_text
            .request_layout(global_id, inspector_id, window, cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.styled_text
            .prepaint(global_id, inspector_id, bounds, &mut (), window, cx);
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let text_layout = self.styled_text.layout().clone();
        let state_entity = self.state_entity.clone();

        // Paint selection highlight
        self.paint_selection(&text_layout, window, cx);

        // Paint text
        self.styled_text
            .paint(global_id, None, bounds, &mut (), &mut (), window, cx);

        // Set cursor style
        window.set_cursor_style(CursorStyle::IBeam, hitbox);

        let has_selection = self.selection.map(|(s, e)| s != e).unwrap_or(false);

        // Mouse down - start selection and focus
        window.on_mouse_event({
            let state_entity = state_entity.clone();
            let text_layout = text_layout.clone();
            let hitbox = hitbox.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if !hitbox.is_hovered(window) || !phase.bubble() {
                    return;
                }
                if let Ok(index) = text_layout.index_for_position(event.position) {
                    state_entity.update(cx, |state, cx| {
                        state.selection = Some((index, index));
                        state.is_selecting = true;
                        state.focus_handle.focus(window);
                        cx.notify();
                    });
                }
            }
        });

        // Mouse move - update selection (always register, check is_selecting inside)
        window.on_mouse_event({
            let state_entity = state_entity.clone();
            let text_layout = text_layout.clone();
            move |event: &MouseMoveEvent, phase, _, cx| {
                if !phase.bubble() {
                    return;
                }
                // Use unwrap_or_else to handle both Ok (inside text) and Err (outside bounds)
                let index = text_layout
                    .index_for_position(event.position)
                    .unwrap_or_else(|idx| idx);
                state_entity.update(cx, |state, cx| {
                    // Only update if currently selecting
                    if state.is_selecting && let Some((start, _)) = state.selection {
                        state.selection = Some((start, index));
                        cx.notify();
                    }
                });
            }
        });

        // Mouse up - end selection (always register)
        window.on_mouse_event({
            let state_entity = state_entity.clone();
            move |_: &MouseUpEvent, phase, _, cx| {
                if !phase.bubble() {
                    return;
                }
                state_entity.update(cx, |state, cx| {
                    if state.is_selecting {
                        state.is_selecting = false;
                        cx.notify();
                    }
                });
            }
        });

        // Click outside - clear selection
        if has_selection {
            window.on_mouse_event({
                let state_entity = state_entity.clone();
                let hitbox = hitbox.clone();
                move |_: &MouseDownEvent, _, window, cx| {
                    if !hitbox.is_hovered(window) {
                        state_entity.update(cx, |state, cx| {
                            state.selection = None;
                            state.is_selecting = false;
                            cx.notify();
                        });
                    }
                }
            });
        }
    }
}
