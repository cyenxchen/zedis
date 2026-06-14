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

//! Redis Stream editor UI component.

use crate::{
    components::{FormDialog, FormField, ZedisKvFetcher, open_add_form_dialog},
    states::{
        RedisStreamEntry, RedisValue, ZedisServerState, filter_stream_entries, i18n_stream_editor,
        parse_stream_field_values, stream_entry_fields_to_display,
    },
    views::{KvTableColumn, ZedisKvTable},
};
use gpui::{App, Entity, SharedString, Window, div, prelude::*};
use gpui_component::WindowExt;
use std::rc::Rc;
use tracing::{debug, info};

/// Manages Redis Stream entries and their display state.
struct ZedisStreamValues {
    visible_entries: Vec<RedisStreamEntry>,
    visible_entry_indexes: Option<Vec<usize>>,
    value: RedisValue,
    server_state: Entity<ZedisServerState>,
}

impl ZedisStreamValues {
    fn recalc_visible_entries(&mut self) {
        let Some(stream) = self.value.stream_value() else {
            return;
        };

        let Some(keyword) = stream.keyword.as_ref().filter(|keyword| !keyword.is_empty()) else {
            self.visible_entries = stream.values.clone();
            self.visible_entry_indexes = None;
            return;
        };

        let filtered = filter_stream_entries(&stream.values, Some(keyword));
        self.visible_entry_indexes = Some(filtered.iter().map(|(index, _)| *index).collect());
        self.visible_entries = filtered.into_iter().map(|(_, entry)| entry).collect();
    }

    fn entry_id_at(&self, row_ix: usize) -> Option<SharedString> {
        let real_index = self
            .visible_entry_indexes
            .as_ref()
            .and_then(|indexes| indexes.get(row_ix).copied())
            .unwrap_or(row_ix);

        self.value.stream_value()?.get_entry_id(real_index)
    }
}

impl ZedisKvFetcher for ZedisStreamValues {
    fn get(&self, row_ix: usize, col_ix: usize) -> Option<SharedString> {
        let entry = self.visible_entries.get(row_ix)?;
        if col_ix == 0 {
            return Some(entry.0.clone());
        }
        Some(stream_entry_fields_to_display(&entry.1))
    }

    fn count(&self) -> usize {
        self.value.stream_value().map_or(0, |stream| stream.size)
    }

    fn rows_count(&self) -> usize {
        self.visible_entries.len()
    }

    fn is_done(&self) -> bool {
        self.value.stream_value().is_some_and(|stream| stream.done)
    }

    fn load_more(&self, _window: &mut Window, cx: &mut App) {
        self.server_state.update(cx, |state, cx| {
            state.load_more_stream_value(cx);
        });
    }

    fn remove(&self, index: usize, cx: &mut App) {
        let Some(entry_id) = self.entry_id_at(index) else {
            return;
        };

        self.server_state.update(cx, |state, cx| {
            state.remove_stream_value(entry_id, cx);
        });
    }

    fn can_remove_many(&self) -> bool {
        true
    }

    fn remove_many(&self, indexes: Vec<usize>, cx: &mut App) {
        let entry_ids = indexes
            .into_iter()
            .filter_map(|index| self.entry_id_at(index))
            .collect::<Vec<_>>();

        self.server_state.update(cx, |state, cx| {
            state.remove_stream_values(entry_ids, cx);
        });
    }

    fn filter(&self, keyword: SharedString, cx: &mut App) -> bool {
        self.server_state
            .update(cx, |state, cx| state.filter_stream_value(keyword, cx))
    }

    fn filter_on_input_change() -> bool {
        true
    }

    fn handle_add_value(&self, window: &mut Window, cx: &mut App) {
        let server_state = self.server_state.clone();

        let handle_submit = Rc::new(move |values: Vec<SharedString>, window: &mut Window, cx: &mut App| {
            if values.len() != 2 {
                return false;
            }

            let entry_id = values
                .first()
                .map(|value| value.trim())
                .filter(|value| !value.is_empty() && *value != "*")
                .map(|value| value.to_string().into());
            let Some(fields) = values
                .get(1)
                .and_then(|value| parse_stream_field_values(value.as_ref()))
            else {
                debug!("Reject invalid Redis stream field JSON");
                return false;
            };

            server_state.update(cx, |state, cx| {
                state.add_stream_value(entry_id, fields, cx);
            });

            window.close_dialog(cx);
            true
        });

        let fields = vec![
            FormField::new(i18n_stream_editor(cx, "entry_id"))
                .with_placeholder(i18n_stream_editor(cx, "entry_id_placeholder")),
            FormField::new(i18n_stream_editor(cx, "fields"))
                .with_placeholder(i18n_stream_editor(cx, "fields_placeholder"))
                .with_validate(|value| parse_stream_field_values(value).is_some())
                .with_focus(),
        ];

        open_add_form_dialog(
            FormDialog {
                title: i18n_stream_editor(cx, "add_entry_title"),
                fields,
                handle_submit,
            },
            window,
            cx,
        );
    }

    fn new(server_state: Entity<ZedisServerState>, value: RedisValue) -> Self {
        let mut this = Self {
            visible_entries: Vec::new(),
            visible_entry_indexes: None,
            value,
            server_state,
        };

        this.recalc_visible_entries();
        this
    }
}

pub struct ZedisStreamEditor {
    table_state: Entity<ZedisKvTable<ZedisStreamValues>>,
}

impl ZedisStreamEditor {
    pub fn new(server_state: Entity<ZedisServerState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let table_state = cx.new(|cx| {
            ZedisKvTable::<ZedisStreamValues>::new(
                vec![
                    KvTableColumn::new(i18n_stream_editor(cx, "entry_id").as_ref(), Some(180.0)),
                    KvTableColumn::new(i18n_stream_editor(cx, "fields").as_ref(), None),
                ],
                server_state,
                window,
                cx,
            )
        });

        info!("Creating new stream editor view");

        Self { table_state }
    }

    pub fn focus_keyword(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.table_state.update(cx, |state, cx| {
            state.focus_keyword(window, cx);
        });
    }
}

impl Render for ZedisStreamEditor {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.table_state.clone()).into_any_element()
    }
}
