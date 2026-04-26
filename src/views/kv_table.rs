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
    components::{INDEX_COLUMN_NAME, ZedisKvDelegate, ZedisKvFetcher},
    states::{ServerEvent, ZedisGlobalStore, ZedisServerState, i18n_common, i18n_kv_table},
};
use gpui::{Entity, SharedString, Subscription, TextAlign, Window, canvas, div, prelude::*, px};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, PixelsExt,
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputEvent, InputState},
    label::Label,
    table::{Table, TableState},
    v_flex,
};
use tracing::{debug, info};

/// Width of the keyword search input field in pixels
const KEYWORD_INPUT_WIDTH: f32 = 200.0;
const TABLE_WIDTH_PADDING: f32 = 10.0;
const FLEX_COLUMN_GAP: f32 = 5.0;
const MIN_FLEX_COLUMN_WIDTH: f32 = 100.0;
const ACTION_COLUMN_WIDTH: f32 = 76.0;
const INDEX_COLUMN_WIDTH: f32 = 56.0;
const WIDTH_REFRESH_EPSILON: f32 = 1.0;

/// Defines the type of table column for different purposes.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum KvTableColumnType {
    /// Standard value column displaying data
    #[default]
    Value,
    /// Row index/number column
    Index,
    /// Action buttons column (edit, delete, etc.)
    Action,
}

/// Configuration for a table column including name, width, and alignment.
#[derive(Clone, Default)]
pub struct KvTableColumn {
    /// Type of the column
    pub column_type: KvTableColumnType,
    /// Display name of the column
    pub name: SharedString,
    /// Optional fixed width in pixels
    pub width: Option<f32>,
    /// Text alignment (left, center, right)
    pub align: Option<TextAlign>,
}

impl KvTableColumn {
    /// Creates a new value column with the given name and optional width.
    pub fn new(name: &str, width: Option<f32>) -> Self {
        Self {
            name: name.to_string().into(),
            width,
            ..Default::default()
        }
    }
}
/// A generic table view for displaying Redis key-value data.
///
/// This component handles:
/// - Displaying paginated Redis data in a table format
/// - Keyword search/filtering
/// - Real-time updates via server events
/// - Loading states and pagination indicators
pub struct ZedisKvTable<T: ZedisKvFetcher> {
    /// Value columns provided by the concrete Redis value editor.
    columns: Vec<KvTableColumn>,
    /// Table state managing the delegate and data
    table_state: Entity<TableState<ZedisKvDelegate<T>>>,
    /// Input field state for keyword search/filter
    keyword_state: Entity<InputState>,
    /// Number of currently loaded items
    items_count: usize,
    /// Total number of items available
    total_count: usize,
    /// Whether all data has been loaded
    done: bool,
    /// Whether a filter operation is in progress
    loading: bool,
    /// Last content width used to calculate table column widths.
    content_width: f32,
    /// Flag indicating the selected key has changed (triggers input reset)
    key_changed: bool,
    /// Event subscriptions for server state and input changes
    _subscriptions: Vec<Subscription>,
}
impl<T: ZedisKvFetcher> ZedisKvTable<T> {
    /// Creates a new fetcher instance with the current server value.
    fn new_values(server_state: Entity<ZedisServerState>, cx: &mut Context<Self>) -> T {
        let value = server_state.read(cx).value().cloned().unwrap_or_default();
        T::new(server_state, value)
    }

    /// Prepares table columns by adding action and index columns, then calculating widths.
    ///
    /// # Logic:
    /// 1. Adds fixed action and index columns at the start
    /// 2. Keeps value columns in the horizontally scrollable area
    /// 3. Calculates remaining space for columns without fixed widths
    /// 4. Distributes remaining width evenly among flexible columns
    fn new_columns(mut columns: Vec<KvTableColumn>, content_width: f32, cx: &mut Context<Self>) -> Vec<KvTableColumn> {
        // Fixed columns must stay as the leftmost prefix for gpui-component's table renderer.
        columns.insert(
            0,
            KvTableColumn {
                column_type: KvTableColumnType::Action,
                name: i18n_common(cx, "action"),
                width: Some(ACTION_COLUMN_WIDTH),
                align: Some(TextAlign::Center),
            },
        );

        columns.insert(
            1,
            KvTableColumn {
                column_type: KvTableColumnType::Index,
                name: INDEX_COLUMN_NAME.to_string().into(),
                width: Some(INDEX_COLUMN_WIDTH),
                align: Some(TextAlign::Center),
            },
        );

        // Calculate remaining width and count columns without fixed width
        let mut remaining_width = content_width - TABLE_WIDTH_PADDING;
        let mut flexible_columns = 0;

        for column in &columns {
            if let Some(width) = column.width {
                remaining_width -= width;
            } else {
                flexible_columns += 1;
            }
        }

        // Distribute remaining width among flexible columns
        let flexible_width = if flexible_columns > 0 {
            Some(((remaining_width / flexible_columns as f32) - FLEX_COLUMN_GAP).max(MIN_FLEX_COLUMN_WIDTH))
        } else {
            None
        };

        for column in &mut columns {
            if column.width.is_none() {
                column.width = flexible_width;
            }
        }

        debug!(
            columns = columns.len(),
            content_width, "Configured key value table columns"
        );

        columns
    }

    fn initial_content_width(window: &Window, cx: &mut Context<Self>) -> f32 {
        cx.global::<ZedisGlobalStore>()
            .read(cx)
            .content_width()
            .map(|width| width.as_f32())
            .unwrap_or_else(|| window.viewport_size().width.as_f32())
    }

    fn refresh_columns_for_width(&mut self, content_width: f32, cx: &mut Context<Self>) {
        if content_width <= 0.0 || (self.content_width - content_width).abs() < WIDTH_REFRESH_EPSILON {
            return;
        }

        let columns = Self::new_columns(self.columns.clone(), content_width, cx);
        let changed = self.table_state.update(cx, |state, cx| {
            let changed = state.delegate_mut().update_column_layout(columns);
            if changed {
                state.refresh(cx);
            }
            changed
        });
        debug!(
            old_content_width = self.content_width,
            content_width, changed, "Checked key value table columns after table width changed"
        );
        self.content_width = content_width;
    }

    /// Creates a new table view with the given columns and server state.
    ///
    /// Sets up:
    /// - Event subscriptions for server state changes
    /// - Keyword search input field
    /// - Table state with data delegate
    pub fn new(
        columns: Vec<KvTableColumn>,
        server_state: Entity<ZedisServerState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = Vec::new();

        // Subscribe to server events to update table data
        subscriptions.push(cx.subscribe(&server_state, |this, server_state, event, cx| {
            match event {
                // Update fetcher when data changes
                ServerEvent::ValuePaginationFinished(_)
                | ServerEvent::ValueLoaded(_)
                | ServerEvent::ValueAdded(_)
                | ServerEvent::ValueUpdated(_) => {
                    let fetcher = Self::new_values(server_state.clone(), cx);
                    this.loading = false;
                    this.done = fetcher.is_done();
                    this.items_count = fetcher.rows_count();
                    this.total_count = fetcher.count();
                    this.table_state.update(cx, |state, _| {
                        state.delegate_mut().set_fetcher(fetcher);
                    });
                }
                // Clear search when key selection changes
                ServerEvent::KeySelected(_) => {
                    this.key_changed = true;
                }
                _ => {}
            }
        }));

        // Initialize keyword search input field
        let keyword_state = cx.new(|cx| {
            InputState::new(window, cx)
                .clean_on_escape()
                .placeholder(i18n_common(cx, "keyword_placeholder"))
        });

        // Subscribe to input events to trigger search on Enter
        subscriptions.push(cx.subscribe(&keyword_state, |this, _, event, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.handle_filter(cx);
            }
        }));

        // Initialize table data and state
        let fetcher = Self::new_values(server_state, cx);
        let done = fetcher.is_done();
        let items_count = fetcher.rows_count();
        let total_count = fetcher.count();
        let content_width = Self::initial_content_width(window, cx);
        let delegate = ZedisKvDelegate::new(
            Self::new_columns(columns.clone(), content_width, cx),
            fetcher,
            window,
            cx,
        );
        let table_state = cx.new(|cx| TableState::new(delegate, window, cx));

        info!("Creating new key value table view");

        Self {
            columns,
            table_state,
            keyword_state,
            items_count,
            total_count,
            done,
            loading: false,
            content_width,
            key_changed: false,
            _subscriptions: subscriptions,
        }
    }

    /// Triggers a filter operation using the current keyword from the input field.
    fn handle_filter(&mut self, cx: &mut Context<Self>) {
        let keyword = self.keyword_state.read(cx).value();
        self.loading = true;
        self.table_state.update(cx, |state, cx| {
            state.delegate().fetcher().filter(keyword, cx);
        });
    }

    /// Focuses the keyword search input field.
    pub fn focus_keyword(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.keyword_state.update(cx, |state, cx| {
            state.focus(window, cx);
        });
    }
}
impl<T: ZedisKvFetcher> Render for ZedisKvTable<T> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text_color = cx.theme().muted_foreground;

        // Clear search input when key changes
        if self.key_changed {
            self.keyword_state.update(cx, |input, cx| {
                input.set_value(SharedString::default(), window, cx);
            });
            self.key_changed = false;
        }

        // Handler for adding new values
        let handle_add_value = cx.listener(|this, _, window, cx| {
            this.table_state.update(cx, |state, cx| {
                state.delegate().fetcher().handle_add_value(window, cx);
            });
        });

        // Search button with loading state
        let search_btn = Button::new("kv-table-search-btn")
            .ghost()
            .icon(IconName::Search)
            .tooltip(i18n_kv_table(cx, "search_tooltip"))
            .loading(self.loading)
            .disabled(self.loading)
            .on_click(cx.listener(|this, _, _, cx| {
                this.handle_filter(cx);
            }));

        // Completion indicator icon
        let status_icon = if self.done {
            Icon::new(CustomIconName::CircleCheckBig) // All data loaded
        } else {
            Icon::new(CustomIconName::CircleDotDashed) // More data available
        };
        let view = cx.entity().clone();

        v_flex()
            .h_full()
            .w_full()
            // Main table area
            .child(
                div()
                    .size_full()
                    .flex_1()
                    .relative()
                    .child(
                        Table::new(&self.table_state)
                            .stripe(true) // Alternating row colors for better readability
                            .bordered(true) // Table borders
                            .scrollbar_visible(true, true), // Show both scrollbars
                    )
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                view.update(cx, |this, cx| {
                                    this.refresh_columns_for_width(bounds.size.width.as_f32(), cx);
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    ),
            )
            // Footer toolbar with search and status
            .child(
                h_flex()
                    .w_full()
                    .p_2()
                    // Left side: Add button and search input
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("add-value-btn")
                                    .icon(CustomIconName::FilePlusCorner)
                                    .tooltip(i18n_kv_table(cx, "add_value_tooltip"))
                                    .on_click(handle_add_value),
                            )
                            .child(
                                Input::new(&self.keyword_state)
                                    .w(px(KEYWORD_INPUT_WIDTH))
                                    .suffix(search_btn)
                                    .cleanable(true),
                            )
                            .flex_1(),
                    )
                    // Right side: Status icon and count
                    .child(status_icon.text_color(text_color).mr_2())
                    .child(
                        Label::new(format!("{} / {}", self.items_count, self.total_count))
                            .text_sm()
                            .text_color(text_color),
                    ),
            )
            .into_any_element()
    }
}
