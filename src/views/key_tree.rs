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
    components::{FormDialog, FormField, SkeletonLoading, open_add_form_dialog},
    connection::QueryMode,
    helpers::{EditorAction, KeyTreeAction, validate_long_string, validate_ttl},
    states::{KeyType, ServerEvent, ZedisGlobalStore, ZedisServerState, i18n_common, i18n_key_tree},
};
use ahash::{AHashMap, AHashSet};
use gpui::{
    App, AppContext, Corner, Entity, FocusHandle, Focusable, Hsla, MouseButton, ScrollStrategy, SharedString,
    Subscription, WeakEntity, Window, div, prelude::*, px,
};
use gpui_component::IndexPath;
use gpui_component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::{
    ActiveTheme, Disableable, Icon, IconName, StyledExt, WindowExt,
    button::{Button, ButtonVariants, DropdownButton},
    dialog::DialogButtonProps,
    h_flex,
    input::{Input, InputEvent, InputState, SelectAll},
    label::Label,
    scroll::ScrollableElement,
    v_flex,
};
use rust_i18n::t;
use std::rc::Rc;
use tracing::{debug, info};

// Constants for tree layout and behavior
const TREE_INDENT_BASE: f32 = 16.0; // Base indentation per level in pixels
const TREE_INDENT_OFFSET: f32 = 8.0; // Additional offset for all items
const EXPANDED_ITEMS_INITIAL_CAPACITY: usize = 10;
const AUTO_EXPAND_THRESHOLD: usize = 100; // Auto-expand tree if fewer than this many keys
const KEY_TYPE_FADE_ALPHA: f32 = 0.8; // Background transparency for key type badges
const KEY_TYPE_BORDER_FADE_ALPHA: f32 = 0.5; // Border transparency for key type badges
const STRIPE_BACKGROUND_ALPHA_DARK: f32 = 0.1; // Odd row background alpha for dark theme
const STRIPE_BACKGROUND_ALPHA_LIGHT: f32 = 0.03; // Odd row background alpha for light theme

#[derive(Default)]
struct KeyTreeState {
    /// Primary keyword used for Redis SCAN.
    keyword: SharedString,
    server_id: SharedString,
    /// Unique ID for the current key tree (changes when keys are reloaded)
    key_tree_id: SharedString,
    /// Whether the tree is empty (no keys found)
    is_empty: bool,
    /// Current query mode (All/Prefix/Exact)
    query_mode: QueryMode,
    /// Error message to display if key loading fails
    error: Option<SharedString>,
    /// Set of expanded folder paths (persisted during tree rebuilds)
    expanded_items: AHashSet<SharedString>,
    /// Index path to scroll to when the tree is updated
    scroll_to_index: Option<IndexPath>,
    /// Anchor key for Shift+click range selection.
    ///
    /// The visible tree can be rebuilt while SCAN keeps loading keys, so a stored
    /// row index may point at a different key by the next click.
    anchor_key: Option<SharedString>,
}

#[derive(Default, Debug, Clone)]
struct KeyTreeItem {
    id: SharedString,
    label: SharedString,
    depth: usize,
    key_type: KeyType,
    expanded: bool,
    children_count: usize,
    is_folder: bool,
}

struct LocalFilterInput {
    input: Entity<InputState>,
    _subscription: Subscription,
}

fn new_key_tree_items(
    mut keys: Vec<(SharedString, KeyType)>,
    filter_terms: Vec<SharedString>,
    expand_all: bool,
    expanded_items: AHashSet<SharedString>,
    separator: &str,
    max_key_tree_depth: usize,
) -> Vec<KeyTreeItem> {
    keys.sort_unstable_by_key(|(k, _)| k.clone());
    let expanded_items_set = expanded_items.iter().map(|s| s.as_str()).collect::<AHashSet<&str>>();
    let mut items: AHashMap<SharedString, KeyTreeItem> = AHashMap::with_capacity(100);

    for (key, key_type) in keys {
        if !key_matches_filter_terms(key.as_str(), &filter_terms) {
            continue;
        }
        // no colon in the key, it's a simple key
        if !key.contains(separator) {
            items.insert(
                key.clone(),
                KeyTreeItem {
                    id: key.clone(),
                    label: key.clone(),
                    key_type,
                    ..Default::default()
                },
            );
            continue;
        }

        let mut dir = String::with_capacity(50);
        let mut key_tree_item: Option<KeyTreeItem> = None;
        // max levels of depth
        for (index, k) in key.splitn(max_key_tree_depth, separator).enumerate() {
            // if key_tre_item is not None, it means we are in a folder
            // because it's not the last part of the key
            if let Some(key_tree_item) = key_tree_item.take() {
                let entry = items.entry(key_tree_item.id.clone()).or_insert_with(|| key_tree_item);
                entry.is_folder = true;
                entry.children_count += 1;
            }

            let expanded = expand_all || index == 0 || expanded_items_set.contains(dir.as_str());
            if !expanded {
                break;
            }
            let name: SharedString = k.to_string().into();
            if index != 0 {
                dir.push_str(separator);
            };
            dir.push_str(k);

            let item_id: SharedString = dir.clone().into();
            key_tree_item = Some(KeyTreeItem {
                id: item_id.clone(),
                label: name.clone(),
                key_type,
                depth: index,
                expanded,
                ..Default::default()
            });
        }
        if let Some(key_tree_item) = key_tree_item.take() {
            items.insert(key_tree_item.id.clone(), key_tree_item);
        }
    }

    let mut children_map: AHashMap<String, Vec<KeyTreeItem>> = AHashMap::new();

    let mut result = Vec::with_capacity(items.len());

    for item in items.into_values() {
        let size = item.id.len() - item.label.len();
        let parent_id = if size == 0 { "" } else { &item.id[..(size - 1)] };
        children_map.entry(parent_id.to_string()).or_default().push(item);
    }

    fn build_sorted_list(parent_id: &str, map: &mut AHashMap<String, Vec<KeyTreeItem>>, result: &mut Vec<KeyTreeItem>) {
        if let Some(mut children) = map.remove(parent_id) {
            children.sort_unstable_by(|a, b| b.is_folder.cmp(&a.is_folder).then_with(|| a.label.cmp(&b.label)));

            for child in children {
                let child_id = child.id.to_string();
                result.push(child);
                build_sorted_list(&child_id, map, result);
            }
        }
    }

    build_sorted_list("", &mut children_map, &mut result);

    result
}

fn key_matches_filter_terms(key: &str, filter_terms: &[SharedString]) -> bool {
    filter_terms
        .iter()
        .all(|term| term.is_empty() || key.contains(term.as_str()))
}

fn visible_key_range(
    items: &[KeyTreeItem],
    anchor_key: &SharedString,
    end_key: &SharedString,
) -> Option<(usize, usize, Vec<SharedString>)> {
    let anchor_row = items
        .iter()
        .position(|item| !item.is_folder && item.id.as_str() == anchor_key.as_str())?;
    let end_row = items
        .iter()
        .position(|item| !item.is_folder && item.id.as_str() == end_key.as_str())?;
    let start = anchor_row.min(end_row);
    let end = anchor_row.max(end_row);
    let keys = items[start..=end]
        .iter()
        .filter(|item| !item.is_folder)
        .map(|item| item.id.clone())
        .collect::<Vec<_>>();

    Some((start, end, keys))
}

fn confirm_delete_selected_keys(
    keys: Vec<SharedString>,
    server_state: Entity<ZedisServerState>,
    window: &mut Window,
    cx: &mut App,
) {
    if keys.is_empty() {
        return;
    }
    let selected_count = keys.len();
    info!(
        selected_count,
        "Showing confirmation before deleting selected Redis keys"
    );
    window.open_dialog(cx, move |dialog, _, cx| {
        let locale = cx.global::<ZedisGlobalStore>().read(cx).locale();
        let message = t!(
            "key_tree.delete_selected_prompt",
            count = selected_count,
            locale = locale
        )
        .to_string();
        let confirm_label = i18n_common(cx, "confirm");
        let cancel_label = i18n_common(cx, "cancel");
        let server_state = server_state.clone();
        let keys = keys.clone();

        dialog
            .confirm()
            .button_props(
                DialogButtonProps::default()
                    .ok_text(confirm_label)
                    .cancel_text(cancel_label),
            )
            .child(v_flex().w_full().max_h(px(200.0)).overflow_y_scrollbar().child(message))
            .on_ok(move |_, window, cx| {
                let keys = keys.clone();
                info!(selected_count = keys.len(), "Confirmed selected Redis keys deletion");
                server_state.update(cx, move |state, cx| {
                    state.delete_keys(keys, cx);
                });
                window.close_dialog(cx);
                true
            })
    });
}

struct KeyTreeDelegate {
    items: Vec<KeyTreeItem>,
    selected_index: Option<IndexPath>,
    view: WeakEntity<ZedisKeyTree>,
}

impl KeyTreeDelegate {
    /// Renders the colored badge for key types (String, Hash, etc.)
    fn render_key_type_badge(&self, key_type: &KeyType) -> impl IntoElement {
        if key_type == &KeyType::Unknown {
            return div().into_any_element();
        }

        let color = key_type.color();
        let mut bg = color;
        bg.fade_out(KEY_TYPE_FADE_ALPHA);
        let mut border = color;
        border.fade_out(KEY_TYPE_BORDER_FADE_ALPHA);

        Label::new(key_type.as_str())
            .text_xs()
            .bg(bg)
            .text_color(color)
            .border_1()
            .px_1()
            .rounded_sm()
            .border_color(border)
            .into_any_element()
    }
}

impl ListDelegate for KeyTreeDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.items.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let yellow = cx.theme().colors.yellow;
        let entry = self.items.get(ix.row)?;
        let is_folder = entry.is_folder;
        let key_id = entry.id.clone();
        let view = self.view.clone();
        let selected_ix = ix;

        // Check if this item is part of a multi-selection (read live state)
        let is_multi_selected = self.view.upgrade().is_some_and(|view| {
            let state = view.read(cx).server_state.read(cx);
            state.selected_keys_count() > 1 && state.is_key_selected(&key_id)
        });
        if is_multi_selected {
            debug!(key = %key_id, "key_tree render multi-selected item");
        }

        let icon = if !is_folder {
            // Key item: Show type badge (String, List, etc.)
            self.render_key_type_badge(&entry.key_type).into_any_element()
        } else if entry.expanded {
            // Expanded folder: Show open folder icon
            Icon::new(IconName::FolderOpen).text_color(yellow).into_any_element()
        } else {
            // Collapsed folder: Show closed folder icon
            Icon::new(IconName::Folder).text_color(yellow).into_any_element()
        };

        let even_bg = cx.theme().background;

        // Zebra striping for better readability
        let odd_bg = if cx.theme().is_dark() {
            Hsla::white().alpha(STRIPE_BACKGROUND_ALPHA_DARK)
        } else {
            Hsla::black().alpha(STRIPE_BACKGROUND_ALPHA_LIGHT)
        };

        // Show child count for folders
        let count_label = if is_folder {
            Label::new(entry.children_count.to_string())
                .text_sm()
                .text_color(cx.theme().muted_foreground)
        } else {
            Label::new("")
        };

        // Use more visible style for multi-selected items
        let bg = if is_multi_selected {
            cx.theme().list_active
        } else if ix.row.is_multiple_of(2) {
            even_bg
        } else {
            odd_bg
        };

        // Build the inner content
        let inner_content = h_flex()
            .w_full()
            .gap_2()
            // Bubble phase: set right_clicked_key for non-folder keys
            // (capture phase already cleared it, so folder clicks stay cleared)
            .when(!is_folder, |this| {
                this.on_mouse_down(MouseButton::Right, {
                    let key_id = key_id.clone();
                    let view = view.clone();
                    move |_, window, cx| {
                        let _ = view.update(cx, |v, cx| {
                            v.right_clicked_key = Some(key_id.clone());

                            let should_keep_multi_selection = {
                                let state = v.server_state.read(cx);
                                state.selected_keys_count() > 1 && state.is_key_selected(&key_id)
                            };
                            if !should_keep_multi_selection {
                                v.server_state.update(cx, |state, cx| {
                                    state.set_single_selected_key(key_id.clone(), cx);
                                });
                            }

                            // Select the key if it's not already selected
                            let current_key = v.server_state.read(cx).key();
                            if current_key.as_ref() != Some(&key_id) {
                                v.server_state.update(cx, |state, cx| {
                                    state.select_key(key_id.clone(), cx);
                                });
                            }

                            // Sync List's selected index for visual highlight
                            let list_state = v.key_tree_list_state.clone();
                            list_state.update(cx, |state, cx| {
                                state.set_selected_index(Some(selected_ix), window, cx);
                                cx.notify();
                            });

                            cx.notify();
                        });
                    }
                })
            })
            .child(icon)
            .child(div().flex_1().text_ellipsis().child(entry.label.clone()))
            .child(count_label);

        Some(
            ListItem::new(ix)
                .w_full()
                .bg(bg)
                .on_click({
                    let key_id = key_id.clone();
                    let view = view.clone();
                    move |event, _, cx| {
                        let shift_pressed = event.modifiers().shift;
                        let _ = view.update(cx, |v, cx| {
                            v.handle_item_click(&key_id, is_folder, shift_pressed, cx);
                        });
                    }
                })
                .when(is_multi_selected, |this| {
                    this.border_1().border_color(cx.theme().list_active_border).rounded_sm()
                })
                .py_1()
                .px_2()
                .pl(px(TREE_INDENT_BASE) * entry.depth + px(TREE_INDENT_OFFSET))
                .child(inner_content),
        )
    }

    fn set_selected_index(&mut self, ix: Option<IndexPath>, _window: &mut Window, _cx: &mut Context<ListState<Self>>) {
        self.selected_index = ix;
    }
}

/// Spawns a save-file dialog and exports the given keys to CSV.
///
/// Called from context menu closures where `cx` is `&mut App`.
fn spawn_export_dialog(keys: Vec<SharedString>, ss: Entity<ZedisServerState>, cx: &mut App) {
    cx.spawn(async move |cx| {
        let handle = rfd::AsyncFileDialog::new()
            .add_filter("CSV", &["csv"])
            .set_file_name("redis_keys_export.csv")
            .save_file()
            .await;
        if let Some(file) = handle {
            let path = file.path().to_string_lossy().to_string();
            let _ = ss.update(cx, |state, cx| {
                state.export_keys(keys, path, cx);
            });
        }
    })
    .detach();
}

/// Key tree view component for browsing and filtering Redis keys
///
/// Displays Redis keys in a hierarchical tree structure with:
/// - Folder navigation for key namespaces (using colon separators)
/// - Key type indicators (String, List, etc.) with color-coded badges
/// - Multiple query modes (All, Prefix, Exact)
/// - Real-time filtering and search
/// - Expandable/collapsible folders
/// - Visual feedback for selected keys
pub struct ZedisKeyTree {
    state: KeyTreeState,

    /// Reference to server state for Redis operations
    server_state: Entity<ZedisServerState>,

    /// Delegate for the key tree list
    // key_tree_delegate: Entity<KeyTreeDelegate>,

    /// State for the key tree list
    key_tree_list_state: Entity<ListState<KeyTreeDelegate>>,

    /// Input field state for keyword filtering
    keyword_state: Entity<InputState>,

    /// Additional local-only filter inputs applied to already loaded keys.
    local_filters: Vec<LocalFilterInput>,

    /// Monotonic token used to drop stale async tree builds.
    tree_build_generation: u64,

    /// Whether to enter add key mode
    should_enter_add_key_mode: Option<bool>,

    /// The key that was right-clicked (for context menu)
    right_clicked_key: Option<SharedString>,

    /// Focus handle for tracking focus within this component
    focus_handle: FocusHandle,

    /// Event subscriptions for reactive updates
    _subscriptions: Vec<Subscription>,
}

impl ZedisKeyTree {
    /// Create a new key tree view with event subscriptions
    ///
    /// Sets up reactive updates when server state changes and
    /// initializes UI components (tree, search input).
    pub fn new(server_state: Entity<ZedisServerState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = Vec::new();

        // Subscribe to server state changes to rebuild tree when keys change
        subscriptions.push(cx.observe(&server_state, |this, _model, cx| {
            this.update_key_tree(false, cx);
        }));
        subscriptions.push(
            cx.subscribe(&server_state, |this, _server_state, event, cx| match event {
                ServerEvent::KeyCollapseAll => {
                    this.state.expanded_items.clear();
                    this.update_key_tree(true, cx);
                }
                ServerEvent::ServerSelected(_, _) => {
                    this.reset(cx);
                }
                ServerEvent::EditonActionTriggered(action) if action == &EditorAction::Create => {
                    this.should_enter_add_key_mode = Some(true);
                    cx.notify();
                }
                ServerEvent::KeySelectionChanged => {
                    let selected_count = this.server_state.read(cx).selected_keys_count();
                    debug!(selected_count, "key_tree selection changed");
                    this.key_tree_list_state.update(cx, |_state, cx| {
                        cx.notify();
                    });
                    cx.notify();
                }
                _ => {}
            }),
        );

        // Initialize keyword search input with placeholder
        let keyword_state = cx.new(|cx| {
            InputState::new(window, cx)
                .clean_on_escape()
                .placeholder(i18n_common(cx, "filter_placeholder"))
        });
        // initial focus
        keyword_state.update(cx, |state, cx| {
            state.focus(window, cx);
        });

        let server_state_value = server_state.read(cx);
        let server_id = server_state_value.server_id().to_string();
        let query_mode = server_state_value.query_mode();

        // Subscribe to search input events (Enter key triggers filter)
        subscriptions.push(cx.subscribe_in(&keyword_state, window, |view, _, event, _, cx| {
            if let InputEvent::PressEnter { .. } = &event {
                view.handle_filter(cx);
            }
        }));

        info!(server_id, "Creating new key tree view");

        let view_weak = cx.entity().downgrade();
        let delegate = KeyTreeDelegate {
            items: Vec::new(),
            selected_index: None,
            view: view_weak,
        };
        let key_tree_list_state = cx.new(|cx| ListState::new(delegate, window, cx));
        subscriptions.push(cx.subscribe(&key_tree_list_state, |view, _, event, cx| match event {
            ListEvent::Select(ix) => {
                view.select_item_by_index(ix, false, true, cx);
            }
            ListEvent::Confirm(ix) => {
                view.select_item_by_index(ix, true, false, cx);
            }
            _ => {}
        }));

        let focus_handle = cx.focus_handle();

        let mut this = Self {
            state: KeyTreeState {
                query_mode,
                server_id: server_id.into(),
                expanded_items: AHashSet::with_capacity(EXPANDED_ITEMS_INITIAL_CAPACITY),
                ..Default::default()
            },
            key_tree_list_state,
            keyword_state,
            local_filters: Vec::new(),
            tree_build_generation: 0,
            server_state,
            should_enter_add_key_mode: None,
            right_clicked_key: None,
            focus_handle,
            _subscriptions: subscriptions,
        };

        // Initial tree build
        this.update_key_tree(true, cx);

        this
    }

    fn reset(&mut self, cx: &mut Context<Self>) {
        self.state = KeyTreeState::default();
        self.local_filters.clear();
        self.right_clicked_key = None;
        debug!("Reset key tree local filter chain");
        cx.notify();
    }

    /// Focuses the keyword search input field.
    pub fn focus_keyword(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.keyword_state.update(cx, |state, cx| {
            state.focus(window, cx);
        });
    }
    fn reset_expand(&mut self, _cx: &mut Context<Self>) {
        self.state.expanded_items.clear();
        self.state.scroll_to_index = Some(IndexPath::new(0));
    }

    fn active_filter_terms(&self, cx: &App) -> Vec<SharedString> {
        let mut terms = Vec::with_capacity(self.local_filters.len() + 1);
        if !self.state.keyword.is_empty() {
            terms.push(self.state.keyword.clone());
        }
        for filter in &self.local_filters {
            let value = filter.input.read(cx).value();
            let trimmed = value.as_str().trim();
            if !trimmed.is_empty() {
                terms.push(trimmed.to_string().into());
            }
        }
        terms
    }

    fn handle_local_filter_change(&mut self, cx: &mut Context<Self>) {
        let filter_terms = self.active_filter_terms(cx);
        debug!(
            filter_count = filter_terms.len(),
            local_filter_count = self.local_filters.len(),
            "Key tree local filter changed"
        );
        self.update_key_tree(true, cx);
    }

    fn add_local_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let filter_index = self.local_filters.len();
        let filter_state = cx.new(|cx| {
            InputState::new(window, cx)
                .clean_on_escape()
                .placeholder(i18n_key_tree(cx, "secondary_filter_placeholder"))
        });
        let subscription = cx.subscribe_in(&filter_state, window, |view, _, event, _, cx| match event {
            InputEvent::Change | InputEvent::PressEnter { .. } => {
                view.handle_local_filter_change(cx);
            }
            _ => {}
        });
        self.local_filters.push(LocalFilterInput {
            input: filter_state.clone(),
            _subscription: subscription,
        });
        filter_state.update(cx, |state, cx| {
            state.focus(window, cx);
        });
        debug!(
            filter_index,
            filter_count = self.local_filters.len(),
            "Added key tree local filter"
        );
        cx.notify();
    }

    fn remove_local_filter(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.local_filters.len() {
            return;
        }
        self.local_filters.remove(index);
        debug!(
            filter_index = index,
            filter_count = self.local_filters.len(),
            "Removed key tree local filter"
        );
        self.update_key_tree(true, cx);
        cx.notify();
    }

    /// Update the key tree structure when server state changes
    ///
    /// Rebuilds the tree only if the tree ID has changed (indicating new keys loaded).
    /// Preserves expanded folder state across rebuilds. Auto-expands all folders
    /// if the total key count is below the threshold.
    fn update_key_tree(&mut self, force_update: bool, cx: &mut Context<Self>) {
        let server_state = self.server_state.read(cx);
        let key_tree_id = server_state.key_tree_id();

        self.state.query_mode = server_state.query_mode();

        // Skip rebuild if tree ID hasn't changed (same keys)
        if !force_update && self.state.key_tree_id == key_tree_id {
            return;
        }
        self.state.key_tree_id = key_tree_id.to_string().into();

        // Auto-expand all folders if key count is small
        let expand_all = server_state.scan_count() < AUTO_EXPAND_THRESHOLD;
        let keys_snapshot: Vec<(SharedString, KeyType)> =
            server_state.keys().iter().map(|(k, v)| (k.clone(), *v)).collect();
        let expanded_items = self.state.expanded_items.clone();

        let view_handle = cx.entity().downgrade();
        let filter_terms = self.active_filter_terms(cx);
        self.tree_build_generation = self.tree_build_generation.wrapping_add(1);
        let build_generation = self.tree_build_generation;

        self.key_tree_list_state.update(cx, move |_state, cx| {
            let app_state = cx.global::<ZedisGlobalStore>().value(cx);
            let separator = app_state.key_separator().to_string();
            let max_key_tree_depth = app_state.max_key_tree_depth();
            cx.spawn(async move |handle, cx| {
                let task = cx.background_spawn(async move {
                    let start = std::time::Instant::now();
                    let items = new_key_tree_items(
                        keys_snapshot,
                        filter_terms,
                        expand_all,
                        expanded_items,
                        &separator,
                        max_key_tree_depth,
                    );
                    tracing::debug!("Key tree build time: {:?}", start.elapsed());
                    items
                });

                let result = task.await;
                let result_is_empty = result.is_empty();
                let should_apply = view_handle
                    .update(cx, |view: &mut ZedisKeyTree, cx| {
                        if view.tree_build_generation != build_generation {
                            debug!(
                                build_generation,
                                current_generation = view.tree_build_generation,
                                "Skip stale key tree build result"
                            );
                            return false;
                        }
                        if result_is_empty {
                            view.reset_expand(cx);
                        }
                        true
                    })
                    .unwrap_or(false);
                if !should_apply {
                    return;
                }
                let _ = handle.update(cx, |this, cx| {
                    this.delegate_mut().items = result;
                    cx.notify();
                });
            })
            .detach();
        });
    }

    /// Handle filter/search action when user submits keyword
    ///
    /// Delegates to server state to perform the actual filtering based on
    /// current query mode. Ignores if a scan is already in progress.
    fn handle_filter(&mut self, cx: &mut Context<Self>) {
        // Don't trigger filter while already scanning
        if self.server_state.read(cx).scaning() {
            return;
        }

        let keyword = self.keyword_state.read(cx).value();
        self.state.keyword = keyword.clone();
        debug!(
            keyword_len = keyword.as_str().len(),
            local_filter_count = self.local_filters.len(),
            "Submitting key tree filter"
        );
        self.server_state.update(cx, move |handle, cx| {
            handle.handle_filter(keyword, cx);
        });
    }

    fn select_all_visible_keys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input_focus = self.keyword_state.read(cx).focus_handle(cx);
        if input_focus.is_focused(window) {
            input_focus.dispatch_action(&SelectAll, window, cx);
            return;
        }
        for filter in &self.local_filters {
            let filter_focus = filter.input.read(cx).focus_handle(cx);
            if filter_focus.is_focused(window) {
                filter_focus.dispatch_action(&SelectAll, window, cx);
                return;
            }
        }
        let keys: Vec<SharedString> = self
            .key_tree_list_state
            .read(cx)
            .delegate()
            .items
            .iter()
            .filter(|item| !item.is_folder)
            .map(|item| item.id.clone())
            .collect();
        if !keys.is_empty() {
            self.server_state.update(cx, |state, cx| {
                state.select_key_range(keys, cx);
            });
        }
    }

    fn delete_selected_keys_from_keyboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.keyword_state.read(cx).focus_handle(cx).is_focused(window) {
            return;
        }
        if self
            .local_filters
            .iter()
            .any(|filter| filter.input.read(cx).focus_handle(cx).is_focused(window))
        {
            return;
        }
        let keys: Vec<SharedString> = self.server_state.read(cx).selected_keys().iter().cloned().collect();
        if keys.is_empty() {
            return;
        }
        info!(
            selected_count = keys.len(),
            "Deleting selected Redis keys from key tree shortcut"
        );
        if keys.len() > 1 {
            confirm_delete_selected_keys(keys, self.server_state.clone(), window, cx);
        } else {
            self.server_state.update(cx, |state, cx| {
                state.delete_keys(keys, cx);
            });
        }
    }

    /// Handle explicit refresh action and clear per-key value searches.
    fn handle_refresh(&mut self, cx: &mut Context<Self>) {
        // Don't trigger refresh while already scanning
        if self.server_state.read(cx).scaning() {
            return;
        }

        let keyword = self.keyword_state.read(cx).value();
        self.state.keyword = keyword.clone();
        debug!(
            keyword_len = keyword.as_str().len(),
            local_filter_count = self.local_filters.len(),
            "Refreshing key tree filter chain"
        );
        self.server_state.update(cx, move |handle, cx| {
            handle.refresh_keys(keyword, cx);
        });
    }

    fn handle_add_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let category_list = ["String", "List", "Set", "Zset", "Hash"];
        let fields = vec![
            FormField::new(i18n_key_tree(cx, "category"))
                .with_options(category_list.iter().map(|s| s.to_string().into()).collect()),
            FormField::new(i18n_common(cx, "key"))
                .with_placeholder(i18n_common(cx, "key_placeholder"))
                .with_focus()
                .with_validate(validate_long_string),
            FormField::new(i18n_common(cx, "ttl"))
                .with_placeholder(i18n_common(cx, "ttl_placeholder"))
                .with_validate(validate_ttl),
        ];
        let server_state = self.server_state.clone();
        let handle_submit = Rc::new(move |values: Vec<SharedString>, window: &mut Window, cx: &mut App| {
            if values.len() != 3 {
                return false;
            }
            let index = values[0].parse::<usize>().unwrap_or(0);
            let category = category_list.get(index).cloned().unwrap_or_default();

            server_state.update(cx, |this, cx| {
                this.add_key(category.to_string().into(), values[1].clone(), values[2].clone(), cx);
            });
            window.close_dialog(cx);
            true
        });

        open_add_form_dialog(
            FormDialog {
                title: i18n_key_tree(cx, "add_key_title"),
                fields,
                handle_submit,
            },
            window,
            cx,
        );
        let entity_id = cx.entity_id();
        cx.defer(move |cx| {
            cx.notify(entity_id);
        });
    }

    fn handle_import_keys(&self, cx: &mut Context<Self>) {
        let server_state = self.server_state.clone();

        cx.spawn(async move |_this, cx| {
            let handle = rfd::AsyncFileDialog::new()
                .add_filter("CSV", &["csv"])
                .set_title("Import Redis keys")
                .pick_file()
                .await;

            if let Some(file) = handle {
                let path = file.path().to_string_lossy().to_string();
                let _ = server_state.update(cx, |state, cx| {
                    state.import_keys(path, cx);
                });
            }
        })
        .detach();
    }

    fn get_tree_status_view(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let server_state = self.server_state.read(cx);
        // if scanning, return None
        if server_state.scaning() {
            if self.key_tree_list_state.read(cx).delegate().items.is_empty() {
                return Some(div().m_5().child(SkeletonLoading::new()).into_any_element());
            }
            return None;
        }
        if !self.state.is_empty && self.state.error.is_none() {
            return None;
        }

        let mut text = SharedString::default();

        if self.state.query_mode == QueryMode::Exact {
            if let Some(value) = server_state.value()
                && value.is_expired()
            {
                text = i18n_key_tree(cx, "key_not_exists");
            }
        } else {
            text = self
                .state
                .error
                .clone()
                .unwrap_or_else(|| i18n_key_tree(cx, "no_keys_found"))
        }
        if text.is_empty() {
            return Some(h_flex().into_any_element());
        }
        Some(
            div()
                .h_flex()
                .w_full()
                .items_center()
                .justify_center()
                .gap_2()
                .pt_5()
                .px_2()
                .child(Icon::new(IconName::Info).text_sm())
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .child(Label::new(text).text_sm().whitespace_normal()),
                )
                .into_any_element(),
        )
    }

    fn select_item_by_index(&mut self, ix: &IndexPath, toggle: bool, sync_selected_keys: bool, cx: &mut Context<Self>) {
        let Some((id, is_folder)) = self.key_tree_list_state.update(cx, |state, _cx| {
            let item = state.delegate().items.get(ix.row)?;
            let id = item.id.clone();
            let is_folder = item.is_folder;
            Some((id, is_folder))
        }) else {
            return;
        };
        self.select_item(id, is_folder, toggle, sync_selected_keys, cx);
    }

    fn select_item(
        &mut self,
        item_id: SharedString,
        is_folder: bool,
        toggle: bool,
        sync_selected_keys: bool,
        cx: &mut Context<Self>,
    ) {
        if is_folder {
            if sync_selected_keys {
                self.state.anchor_key = None;
                debug!(folder = %item_id, "Clear key tree key selection from folder list event");
                self.server_state.update(cx, |state, cx| {
                    state.clear_selected_keys(cx);
                });
            }
            if self.state.expanded_items.contains(&item_id) {
                if !toggle {
                    return;
                }
                // User clicked an expanded folder -> collapse it
                self.state.expanded_items.remove(&item_id);
            } else {
                // User clicked a collapsed folder -> expand it and load data
                self.state.expanded_items.insert(item_id.clone());
                self.server_state.update(cx, |state, cx| {
                    state.scan_prefix(format!("{}:", item_id.as_str()).into(), cx);
                });
            }
            self.update_key_tree(true, cx);
        } else {
            let is_selected = self.server_state.read(cx).key().as_ref() == Some(&item_id);
            // Select Key
            if !is_selected {
                self.server_state.update(cx, |state, cx| {
                    state.select_key(item_id.clone(), cx);
                });
            }
            if sync_selected_keys {
                self.state.anchor_key = Some(item_id.clone());
                debug!(key = %item_id, "Sync key tree single selection from list event");
                self.server_state.update(cx, |state, cx| {
                    state.set_single_selected_key(item_id.clone(), cx);
                });
            }
        }
    }

    /// Handle item click with optional Shift key for range selection
    fn handle_item_click(
        &mut self,
        key_id: &SharedString,
        is_folder: bool,
        shift_pressed: bool,
        cx: &mut Context<Self>,
    ) {
        if is_folder {
            self.state.anchor_key = None;
            // Clear multi-selection when clicking folders
            self.server_state.update(cx, |state, cx| {
                state.clear_selected_keys(cx);
            });
            return;
        }

        if shift_pressed && self.state.anchor_key.is_some() {
            // Shift+click: perform range selection
            self.handle_range_selection(key_id, cx);
        } else {
            // Normal click: update anchor and set single selection
            self.state.anchor_key = Some(key_id.clone());
            debug!(key = %key_id, "Set key tree single selection from normal click");
            self.server_state.update(cx, |state, cx| {
                state.set_single_selected_key(key_id.clone(), cx);
            });
        }
        cx.notify();
    }

    /// Handle range selection when Shift+click is detected
    fn handle_range_selection(&mut self, end_key: &SharedString, cx: &mut Context<Self>) {
        let Some(anchor_key) = self.state.anchor_key.clone() else {
            return;
        };

        let Some((start, end, keys_in_range)) = visible_key_range(
            &self.key_tree_list_state.read(cx).delegate().items,
            &anchor_key,
            end_key,
        ) else {
            debug!(
                anchor_key = %anchor_key,
                end_key = %end_key,
                "Reset key tree range selection because the anchor or target key is no longer visible"
            );
            self.state.anchor_key = Some(end_key.clone());
            self.server_state.update(cx, |state, cx| {
                state.set_single_selected_key(end_key.clone(), cx);
            });
            return;
        };

        if keys_in_range.is_empty() {
            return;
        }

        debug!(
            start_row = start,
            end_row = end,
            anchor_key = %anchor_key,
            end_key = %end_key,
            selected_count = keys_in_range.len(),
            "key_tree range selection"
        );

        // Update server state with selected keys
        self.server_state.update(cx, |state, cx| {
            state.select_key_range(keys_in_range, cx);
        });

        cx.notify();
    }

    /// Render the tree view or empty state message
    ///
    /// Displays:
    /// - Tree structure with keys and folders (normal state)
    /// - "Key not exists" message (Exact mode with expired key)
    /// - Error or "no keys found" message (empty state)
    fn render_tree(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(status_view) = self.get_tree_status_view(cx) {
            return status_view.into_any_element();
        }

        let view = cx.entity().downgrade();
        let view_for_capture = cx.entity().downgrade();
        let server_state = self.server_state.clone();

        div()
            .p_1()
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().sidebar_foreground)
            .h_full()
            .child(List::new(&self.key_tree_list_state))
            // Capture phase: clear right_clicked_key on right-click (before child elements)
            // Child elements will set it back if clicking on a key
            .capture_any_mouse_down(move |event, _, cx| {
                if event.button == MouseButton::Right {
                    let _ = view_for_capture.update(cx, |v, cx| {
                        v.right_clicked_key = None;
                        cx.notify();
                    });
                }
            })
            .context_menu({
                move |menu, _window, cx| {
                    // Read the latest right_clicked_key from view
                    let right_clicked_key = view.upgrade().and_then(|v| v.read(cx).right_clicked_key.clone());

                    // Check if multiple keys are selected
                    let selected_count = view
                        .upgrade()
                        .map(|v| v.read(cx).server_state.read(cx).selected_keys_count())
                        .unwrap_or(0);

                    if selected_count > 1 {
                        // Multi-selection: show export and batch delete options
                        let ss_export = server_state.clone();
                        let ss_delete = server_state.clone();
                        menu.item(
                            PopupMenuItem::new(format!(
                                "{} ({})",
                                i18n_key_tree(cx, "export_selected"),
                                selected_count
                            ))
                            .on_click(move |_, _window, cx| {
                                let keys: Vec<SharedString> =
                                    ss_export.read(cx).selected_keys().iter().cloned().collect();
                                spawn_export_dialog(keys, ss_export.clone(), cx);
                            }),
                        )
                        .item(
                            PopupMenuItem::new(format!(
                                "{} ({})",
                                i18n_key_tree(cx, "delete_selected"),
                                selected_count
                            ))
                            .on_click(move |_, window, cx| {
                                let keys: Vec<SharedString> =
                                    ss_delete.read(cx).selected_keys().iter().cloned().collect();
                                confirm_delete_selected_keys(keys, ss_delete.clone(), window, cx);
                            }),
                        )
                    } else if let Some(key) = right_clicked_key {
                        // Single selection: show export, duplicate, and delete
                        let key_dup = key.clone();
                        let key_del = key.clone();
                        let ss_export = server_state.clone();
                        let ss_dup = server_state.clone();
                        let ss_del = server_state.clone();

                        menu.item(PopupMenuItem::new(i18n_key_tree(cx, "export_key")).on_click({
                            let key = key.clone();
                            move |_, _window, cx| {
                                spawn_export_dialog(vec![key.clone()], ss_export.clone(), cx);
                            }
                        }))
                        .item(
                            PopupMenuItem::new(i18n_key_tree(cx, "duplicate_key")).on_click(move |_, _window, cx| {
                                ss_dup.update(cx, |state, cx| {
                                    state.duplicate_key(key_dup.clone(), cx);
                                });
                            }),
                        )
                        .separator()
                        .item(
                            PopupMenuItem::new(i18n_key_tree(cx, "delete_key")).on_click(move |_, _window, cx| {
                                ss_del.update(cx, |state, cx| {
                                    state.delete_key(key_del.clone(), cx);
                                });
                            }),
                        )
                    } else {
                        menu
                    }
                }
            })
            .into_any_element()
    }
    /// Render the search/filter input bar with query mode selector
    ///
    /// Features:
    /// - Query mode dropdown (All/Prefix/Exact) with visual indicators
    /// - Search input field with placeholder
    /// - Search button (with loading state during scan)
    /// - Clearable input (X button appears when text entered)
    fn render_keyword_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let server_state = self.server_state.read(cx);
        let scaning = server_state.scaning();
        let server_id = server_state.server_id();
        let server_state_keyword = server_state.keyword().clone();
        // Sync input field when server changes OR when keyword changes (e.g., after async restore)
        if server_id != self.state.server_id.as_str() || server_state_keyword != self.state.keyword {
            let server_changed = server_id != self.state.server_id.as_str();
            self.state.server_id = server_id.to_string().into();
            // Sync input field with server's cached keyword
            self.state.keyword = server_state_keyword.clone();
            if server_changed {
                self.local_filters.clear();
                debug!(server_id, "Cleared key tree local filters after server change");
            }
            self.keyword_state.update(cx, |state, cx| {
                state.set_value(server_state_keyword, window, cx);
            });
        }
        let query_mode = self.state.query_mode;

        // Select icon based on query mode
        let icon = match query_mode {
            QueryMode::All => Icon::new(IconName::Asterisk), // * for all keys
            QueryMode::Prefix => Icon::new(CustomIconName::ChevronUp), // ~ for prefix
            QueryMode::Exact => Icon::new(CustomIconName::Equal), // = for exact match
        };
        let query_mode_dropdown = DropdownButton::new("dropdown")
            .button(Button::new("key-tree-query-mode-btn").ghost().px_2().icon(icon))
            .dropdown_menu_with_anchor(Corner::TopLeft, move |menu, _, _| {
                // Build menu with checkmarks for current mode
                menu.menu_element_with_check(query_mode == QueryMode::All, Box::new(QueryMode::All), |_, cx| {
                    Label::new(i18n_key_tree(cx, "query_mode_all")).ml_2().text_xs()
                })
                .menu_element_with_check(query_mode == QueryMode::Prefix, Box::new(QueryMode::Prefix), |_, cx| {
                    Label::new(i18n_key_tree(cx, "query_mode_prefix")).ml_2().text_xs()
                })
                .menu_element_with_check(
                    query_mode == QueryMode::Exact,
                    Box::new(QueryMode::Exact),
                    |_, cx| Label::new(i18n_key_tree(cx, "query_mode_exact")).ml_2().text_xs(),
                )
            });
        // Search button (shows loading spinner during scan)
        let search_btn = Button::new("key-tree-search-btn")
            .ghost()
            .tooltip(i18n_key_tree(cx, "search_tooltip"))
            .loading(scaning)
            .disabled(scaning)
            .icon(IconName::Search)
            .on_click(cx.listener(|this, _, _, cx| {
                this.handle_filter(cx);
            }));
        // keyword input
        let keyword_input = Input::new(&self.keyword_state)
            .w_full()
            .flex_1()
            .px_0()
            .mr_2()
            .prefix(query_mode_dropdown)
            .suffix(search_btn)
            .cleanable(true);
        let local_filter_inputs = self
            .local_filters
            .iter()
            .enumerate()
            .map(|(index, filter)| {
                let remove_btn = Button::new(("key-tree-local-filter-remove", index))
                    .ghost()
                    .tooltip(i18n_key_tree(cx, "remove_filter_tooltip"))
                    .icon(IconName::Close)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.remove_local_filter(index, cx);
                    }));

                Input::new(&filter.input)
                    .w(px(128.0))
                    .flex_shrink_0()
                    .suffix(remove_btn)
                    .cleanable(true)
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let add_filter_btn = Button::new("key-tree-add-filter-btn")
            .outline()
            .tooltip(i18n_key_tree(cx, "add_filter_tooltip"))
            .icon(IconName::Plus)
            .on_click(cx.listener(|this, _, window, cx| {
                this.add_local_filter(window, cx);
            }));
        // Refresh button
        let refresh_btn = Button::new("key-tree-refresh-btn")
            .outline()
            .tooltip(i18n_key_tree(cx, "refresh_keys_tooltip"))
            .loading(scaning)
            .disabled(scaning)
            .icon(CustomIconName::RotateCw)
            .on_click(cx.listener(|this, _, _, cx| {
                this.handle_refresh(cx);
            }));
        h_flex()
            .p_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(keyword_input)
            .children(local_filter_inputs)
            .child(add_filter_btn)
            .child(refresh_btn)
            .child(
                Button::new("key-tree-add-btn")
                    .outline()
                    .icon(CustomIconName::FilePlusCorner)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.handle_add_key(window, cx);
                    })),
            )
            .child(
                Button::new("key-tree-import-btn")
                    .outline()
                    .tooltip(i18n_key_tree(cx, "import_keys_tooltip"))
                    .icon(CustomIconName::FileInput)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.handle_import_keys(cx);
                    })),
            )
    }
}

impl Render for ZedisKeyTree {
    /// Main render method - displays search bar and tree structure
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(scroll_to_index) = self.state.scroll_to_index.take() {
            self.key_tree_list_state.update(cx, |state, cx| {
                state.scroll_to_item(scroll_to_index, ScrollStrategy::Top, window, cx);
            });
        }
        if let Some(true) = self.should_enter_add_key_mode.take() {
            self.handle_add_key(window, cx);
        }
        v_flex()
            .h_full()
            .w_full()
            .key_context("KeyTree")
            .track_focus(&self.focus_handle)
            .child(self.render_keyword_input(window, cx))
            .child(self.render_tree(cx))
            .on_action(cx.listener(|this, action: &KeyTreeAction, window, cx| match action {
                KeyTreeAction::SelectAll => this.select_all_visible_keys(window, cx),
                KeyTreeAction::DeleteSelected => this.delete_selected_keys_from_keyboard(window, cx),
            }))
            .on_action(cx.listener(|this, e: &QueryMode, _window, cx| {
                let new_mode = *e;

                // Step 1: Update server state with new query mode
                this.server_state.update(cx, |state, cx| {
                    state.set_query_mode(new_mode, cx);
                });

                // Step 2: Update local UI state
                this.state.query_mode = new_mode;
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ss(value: &str) -> SharedString {
        value.to_string().into()
    }

    #[test]
    fn filters_key_tree_items_by_all_terms() {
        let keys = vec![
            (ss("CMC_{DCC0001}_sg.device"), KeyType::String),
            (ss("CMC_{DCC0001}_sg.bus"), KeyType::String),
            (ss("CMC_{DCC0002}_sg.device"), KeyType::String),
        ];

        let items = new_key_tree_items(
            keys,
            vec![ss("DCC0001"), ss("sg.device")],
            true,
            AHashSet::new(),
            ":",
            10,
        );

        let ids = items.iter().map(|item| item.id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, vec!["CMC_{DCC0001}_sg.device"]);
    }

    #[test]
    fn ignores_empty_filter_terms() {
        assert!(key_matches_filter_terms(
            "CMC_{DCC0001}_sg.device",
            &[SharedString::default(), ss("DCC0001")]
        ));
    }

    #[test]
    fn visible_key_range_uses_key_ids_and_skips_folders() {
        let items = vec![
            KeyTreeItem {
                id: ss("group"),
                label: ss("group"),
                is_folder: true,
                ..Default::default()
            },
            KeyTreeItem {
                id: ss("group:a"),
                label: ss("a"),
                ..Default::default()
            },
            KeyTreeItem {
                id: ss("group:b"),
                label: ss("b"),
                ..Default::default()
            },
            KeyTreeItem {
                id: ss("other"),
                label: ss("other"),
                is_folder: true,
                ..Default::default()
            },
            KeyTreeItem {
                id: ss("other:c"),
                label: ss("c"),
                ..Default::default()
            },
        ];

        let (_, _, keys) =
            visible_key_range(&items, &ss("group:a"), &ss("other:c")).expect("test: visible keys should be found");

        assert_eq!(keys, vec![ss("group:a"), ss("group:b"), ss("other:c")]);
    }

    #[test]
    fn visible_key_range_returns_none_when_anchor_is_no_longer_visible() {
        let items = vec![KeyTreeItem {
            id: ss("current"),
            label: ss("current"),
            ..Default::default()
        }];

        assert!(visible_key_range(&items, &ss("stale"), &ss("current")).is_none());
    }
}
