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

use crate::connection::get_connection_manager;
use crate::{
    assets::CustomIconName,
    connection::RedisServer,
    helpers::{is_development, is_windows, validate_common_string, validate_host, validate_long_string},
    states::{
        FontSize, FontSizeAction, LocaleAction, Route, ServerEvent, SettingsAction, ThemeAction, ZedisGlobalStore,
        ZedisServerState, i18n_common, i18n_servers, i18n_sidebar,
    },
};
use gpui::{
    App, Context, Corner, Entity, MouseButton, Pixels, SharedString, Subscription, Window, div, prelude::*, px,
    uniform_list,
};
use gpui_component::{
    ActiveTheme, Icon, IconName, ThemeMode, WindowExt,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    form::{field, v_form},
    input::{Input, InputState, NumberInput},
    label::Label,
    list::ListItem,
    menu::{ContextMenuExt, DropdownMenu, PopupMenuItem},
    scroll::ScrollableElement,
    tooltip::Tooltip,
    v_flex,
};
use std::{cell::Cell, rc::Rc};
use tracing::info;

// Constants for UI layout
const ICON_PADDING: Pixels = px(8.0);
const ICON_MARGIN: Pixels = px(4.0);
const LABEL_PADDING: Pixels = px(2.0);
const STAR_BUTTON_HEIGHT: f32 = 48.0;
const SETTINGS_BUTTON_HEIGHT: f32 = 44.0;
const SERVER_LIST_ITEM_BORDER_WIDTH: f32 = 3.0;
const SETTINGS_ICON_SIZE: f32 = 18.0;

/// Internal state for sidebar component
///
/// Caches server list to avoid repeated queries and tracks current selection.
#[derive(Default)]
struct SidebarState {
    /// List of (server_id, server_name) tuples for display
    /// First entry is always (empty, empty) representing the home page
    server_names: Vec<(SharedString, SharedString)>,

    /// Currently selected server ID (empty string means home page)
    server_id: SharedString,

    /// Server ID that was right-clicked (for context menu)
    right_clicked_server_id: Option<SharedString>,
}

/// Sidebar navigation component
///
/// Features:
/// - Star button (link to GitHub)
/// - Server list for quick navigation between servers and home
/// - Settings menu with theme and language options
///
/// The sidebar provides quick access to:
/// - Home page (server management)
/// - Connected Redis servers
/// - Application settings (theme, language)
pub struct ZedisSidebar {
    /// Internal state with cached server list
    state: SidebarState,

    /// Reference to server state for Redis operations
    server_state: Entity<ZedisServerState>,

    /// Event subscriptions for reactive updates
    _subscriptions: Vec<Subscription>,
}

impl ZedisSidebar {
    /// Create a new sidebar component with event subscriptions
    ///
    /// Sets up listeners for:
    /// - Server selection changes (updates current selection)
    /// - Server list updates (refreshes displayed servers)
    pub fn new(server_state: Entity<ZedisServerState>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut subscriptions = vec![];

        // Subscribe to server events for reactive updates
        subscriptions.push(cx.subscribe(&server_state, |this, _server_state, event, cx| {
            match event {
                ServerEvent::ServerSelected(server_id, _) => {
                    // Update current selection highlight
                    this.state.server_id = server_id.clone();
                    // Also refresh server list since opened_servers may have changed
                    this.update_server_names(cx);
                }
                ServerEvent::ServerListUpdated => {
                    // Refresh server list when servers are added/removed/updated
                    this.update_server_names(cx);
                }
                _ => {
                    return;
                }
            }
            cx.notify();
        }));

        // Get current server ID for initial selection
        let state = server_state.read(cx).clone();
        let server_id = state.server_id().to_string().into();

        let mut this = Self {
            server_state,
            state: SidebarState {
                server_id,
                ..Default::default()
            },
            _subscriptions: subscriptions,
        };

        info!("Creating new sidebar view");

        // Load initial server list
        this.update_server_names(cx);
        this
    }

    /// Update cached server list from server state
    ///
    /// Rebuilds the server_names list with:
    /// - First entry: (empty, empty) for home page
    /// - Remaining entries: (server_id, server_name) for each opened server
    fn update_server_names(&mut self, cx: &mut Context<Self>) {
        // Start with home page entry
        let mut server_names = vec![(SharedString::default(), SharedString::default())];

        let server_state = self.server_state.read(cx);
        let opened_servers = server_state.opened_servers();
        if let Some(servers) = server_state.servers() {
            server_names.extend(
                servers
                    .iter()
                    .filter(|server| opened_servers.contains(&SharedString::from(server.id.clone())))
                    .map(|server| (server.id.clone().into(), server.name.clone().into())),
            );
        }
        self.state.server_names = server_names;
    }

    /// Open edit server dialog for the specified server
    ///
    /// Shows a form pre-filled with current server configuration.
    /// Detects config changes and reconnects if needed.
    fn open_edit_server_dialog(&self, server_id: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        // 1. Get current server configuration
        let server = self.server_state.read(cx).server(&server_id).cloned();
        let Some(server) = server else {
            return;
        };

        // 2. Save original config hash for change detection
        let original_hash = server.get_hash();

        // 3. Create form input states
        let name_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(i18n_common(cx, "name_placeholder"))
                .validate(|s, _cx| validate_common_string(s))
        });
        let host_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(i18n_common(cx, "host_placeholder"))
                .validate(|s, _cx| validate_host(s))
        });
        let port_state = cx.new(|cx| InputState::new(window, cx).placeholder(i18n_common(cx, "port_placeholder")));
        let username_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(i18n_common(cx, "username_placeholder"))
                .validate(|s, _cx| validate_common_string(s))
        });
        let password_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(i18n_common(cx, "password_placeholder"))
                .validate(|s, _cx| validate_common_string(s))
                .masked(true)
        });

        let (cert_min_rows, cert_max_rows) = (2, 100);
        let client_cert_state = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(cert_min_rows, cert_max_rows)
                .placeholder(i18n_common(cx, "client_cert_placeholder"))
        });
        let client_key_state = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(cert_min_rows, cert_max_rows)
                .placeholder(i18n_common(cx, "client_key_placeholder"))
        });
        let root_cert_state = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(cert_min_rows, cert_max_rows)
                .placeholder(i18n_common(cx, "root_cert_placeholder"))
        });
        let master_name_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(i18n_servers(cx, "master_name_placeholder"))
                .validate(|s, _cx| validate_common_string(s))
        });
        let ssh_addr_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(i18n_servers(cx, "ssh_addr_placeholder"))
                .validate(|s, _cx| validate_common_string(s))
        });
        let ssh_username_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(i18n_servers(cx, "ssh_username_placeholder"))
                .validate(|s, _cx| validate_common_string(s))
        });
        let ssh_password_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(i18n_servers(cx, "ssh_password_placeholder"))
                .validate(|s, _cx| validate_common_string(s))
                .masked(true)
        });
        let ssh_key_state = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(cert_min_rows, cert_max_rows)
                .placeholder(i18n_servers(cx, "ssh_key_placeholder"))
        });
        let description_state = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(i18n_common(cx, "description_placeholder"))
                .validate(|s, _cx| validate_long_string(s))
        });

        // 4. Fill existing data into form
        name_state.update(cx, |state, cx| {
            state.set_value(server.name.clone(), window, cx);
        });
        host_state.update(cx, |state, cx| {
            state.set_value(server.host.clone(), window, cx);
        });
        if server.port != 0 {
            port_state.update(cx, |state, cx| {
                state.set_value(server.port.to_string(), window, cx);
            });
        }
        username_state.update(cx, |state, cx| {
            state.set_value(server.username.clone().unwrap_or_default(), window, cx);
        });
        password_state.update(cx, |state, cx| {
            state.set_value(server.password.clone().unwrap_or_default(), window, cx);
        });
        client_cert_state.update(cx, |state, cx| {
            state.set_value(server.client_cert.clone().unwrap_or_default(), window, cx);
        });
        client_key_state.update(cx, |state, cx| {
            state.set_value(server.client_key.clone().unwrap_or_default(), window, cx);
        });
        root_cert_state.update(cx, |state, cx| {
            state.set_value(server.root_cert.clone().unwrap_or_default(), window, cx);
        });
        master_name_state.update(cx, |state, cx| {
            state.set_value(server.master_name.clone().unwrap_or_default(), window, cx);
        });
        ssh_addr_state.update(cx, |state, cx| {
            state.set_value(server.ssh_addr.clone().unwrap_or_default(), window, cx);
        });
        ssh_username_state.update(cx, |state, cx| {
            state.set_value(server.ssh_username.clone().unwrap_or_default(), window, cx);
        });
        ssh_password_state.update(cx, |state, cx| {
            state.set_value(server.ssh_password.clone().unwrap_or_default(), window, cx);
        });
        ssh_key_state.update(cx, |state, cx| {
            state.set_value(server.ssh_key.clone().unwrap_or_default(), window, cx);
        });
        description_state.update(cx, |state, cx| {
            state.set_value(server.description.clone().unwrap_or_default(), window, cx);
        });

        // 5. Create TLS and SSH toggle states
        let server_enable_tls = Rc::new(Cell::new(server.tls.unwrap_or(false)));
        let server_insecure_tls = Rc::new(Cell::new(server.insecure.unwrap_or(false)));
        let server_ssh_tunnel = Rc::new(Cell::new(server.ssh_tunnel.unwrap_or(false)));

        // Clone states for submit handler
        let server_state = self.server_state.clone();
        let server_id_clone = server_id.to_string();
        let name_state_clone = name_state.clone();
        let host_state_clone = host_state.clone();
        let port_state_clone = port_state.clone();
        let username_state_clone = username_state.clone();
        let password_state_clone = password_state.clone();
        let client_cert_state_clone = client_cert_state.clone();
        let client_key_state_clone = client_key_state.clone();
        let root_cert_state_clone = root_cert_state.clone();
        let master_name_state_clone = master_name_state.clone();
        let ssh_addr_state_clone = ssh_addr_state.clone();
        let ssh_username_state_clone = ssh_username_state.clone();
        let ssh_password_state_clone = ssh_password_state.clone();
        let ssh_key_state_clone = ssh_key_state.clone();
        let description_state_clone = description_state.clone();
        let server_enable_tls_for_submit = server_enable_tls.clone();
        let server_insecure_tls_for_submit = server_insecure_tls.clone();
        let server_ssh_tunnel_for_submit = server_ssh_tunnel.clone();

        // 6. Create submit handler with change detection and reconnect logic
        let handle_submit = Rc::new(move |window: &mut Window, cx: &mut App| {
            let name = name_state_clone.read(cx).value();
            let host = host_state_clone.read(cx).value();
            let port = port_state_clone.read(cx).value().parse::<u16>().unwrap_or(6379);

            if name.is_empty() || host.is_empty() {
                return false;
            }

            let password_val = password_state_clone.read(cx).value();
            let password = if password_val.is_empty() {
                None
            } else {
                Some(password_val)
            };
            let username_val = username_state_clone.read(cx).value();
            let username = if username_val.is_empty() {
                None
            } else {
                Some(username_val)
            };

            let enable_tls = server_enable_tls_for_submit.get();
            let (client_cert, client_key, root_cert) = if enable_tls {
                let client_cert_val = client_cert_state_clone.read(cx).value();
                let client_cert = if client_cert_val.is_empty() {
                    None
                } else {
                    Some(client_cert_val)
                };
                let client_key_val = client_key_state_clone.read(cx).value();
                let client_key = if client_key_val.is_empty() {
                    None
                } else {
                    Some(client_key_val)
                };
                let root_cert_val = root_cert_state_clone.read(cx).value();
                let root_cert = if root_cert_val.is_empty() {
                    None
                } else {
                    Some(root_cert_val)
                };
                (client_cert, client_key, root_cert)
            } else {
                (None, None, None)
            };

            let insecure_tls = if server_insecure_tls_for_submit.get() {
                Some(true)
            } else {
                None
            };

            let master_name_val = master_name_state_clone.read(cx).value();
            let master_name = if master_name_val.is_empty() {
                None
            } else {
                Some(master_name_val)
            };

            let desc_val = description_state_clone.read(cx).value();
            let description = if desc_val.is_empty() { None } else { Some(desc_val) };

            let ssh_tunnel = server_ssh_tunnel_for_submit.get();
            let ssh_addr_val = ssh_addr_state_clone.read(cx).value();
            let ssh_addr = if ssh_addr_val.is_empty() {
                None
            } else {
                Some(ssh_addr_val)
            };
            let ssh_username_val = ssh_username_state_clone.read(cx).value();
            let ssh_username = if ssh_username_val.is_empty() {
                None
            } else {
                Some(ssh_username_val)
            };
            let ssh_password_val = ssh_password_state_clone.read(cx).value();
            let ssh_password = if ssh_password_val.is_empty() {
                None
            } else {
                Some(ssh_password_val)
            };
            let ssh_key_val = ssh_key_state_clone.read(cx).value();
            let ssh_key = if ssh_key_val.is_empty() {
                None
            } else {
                Some(ssh_key_val)
            };

            // Get current server for preserving non-editable fields
            let current_server = server_state
                .read(cx)
                .server(&server_id_clone)
                .cloned()
                .unwrap_or_default();

            // Build new server config
            let new_server = RedisServer {
                id: server_id_clone.clone(),
                name: name.to_string(),
                host: host.to_string(),
                port,
                username: username.map(|u| u.to_string()),
                password: password.map(|p| p.to_string()),
                master_name: master_name.map(|m| m.to_string()),
                description: description.map(|d| d.to_string()),
                tls: if enable_tls { Some(enable_tls) } else { None },
                insecure: insecure_tls,
                client_cert: client_cert.map(|c| c.to_string()),
                client_key: client_key.map(|k| k.to_string()),
                root_cert: root_cert.map(|r| r.to_string()),
                ssh_tunnel: if ssh_tunnel { Some(ssh_tunnel) } else { None },
                ssh_addr: ssh_addr.map(|a| a.to_string()),
                ssh_username: ssh_username.map(|u| u.to_string()),
                ssh_password: ssh_password.map(|p| p.to_string()),
                ssh_key: ssh_key.map(|k| k.to_string()),
                ..current_server
            };

            // Check if config has changed
            let new_hash = new_server.get_hash();
            let config_changed = new_hash != original_hash;

            // Update server configuration
            server_state.update(cx, |state, cx| {
                state.update_or_insrt_server(new_server, cx);
            });

            // If config changed, reconnect
            if config_changed {
                info!("Server config changed, reconnecting: {}", server_id_clone);

                // Clear old connection cache
                get_connection_manager().remove_client(&server_id_clone);

                // Get preset credentials
                let preset_credentials = cx.global::<ZedisGlobalStore>().read(cx).preset_credentials();

                // Force reconnect with new config
                server_state.update(cx, |state, cx| {
                    state.reconnect(preset_credentials, cx);
                });
            }

            window.close_dialog(cx);
            true
        });

        // 7. Open dialog
        let focus_handle_done = Cell::new(false);
        window.open_dialog(cx, move |dialog, window, cx| {
            let title = i18n_servers(cx, "update_server_title");

            // Prepare field labels
            let name_label = i18n_common(cx, "name");
            let host_label = i18n_common(cx, "host");
            let port_label = i18n_common(cx, "port");
            let username_label = i18n_common(cx, "username");
            let password_label = i18n_common(cx, "password");
            let tls_label = i18n_common(cx, "tls");
            let tls_check_label = i18n_common(cx, "tls_check_label");
            let insecure_tls_label = i18n_common(cx, "insecure_tls");
            let insecure_tls_check_label = i18n_common(cx, "insecure_tls_check_label");
            let client_cert_label = i18n_common(cx, "client_cert");
            let client_key_label = i18n_common(cx, "client_key");
            let root_cert_label = i18n_common(cx, "root_cert");
            let description_label = i18n_common(cx, "description");
            let master_name_label = i18n_servers(cx, "master_name");
            let ssh_addr_label = i18n_servers(cx, "ssh_addr");
            let ssh_username_label = i18n_servers(cx, "ssh_username");
            let ssh_password_label = i18n_servers(cx, "ssh_password");
            let ssh_key_label = i18n_servers(cx, "ssh_key");
            let ssh_tunnel_label = i18n_servers(cx, "ssh_tunnel");
            let ssh_tunnel_check_label = i18n_servers(cx, "ssh_tunnel_check_label");

            dialog
                .title(title)
                .overlay(true)
                .child({
                    if !focus_handle_done.get() {
                        name_state.clone().update(cx, |this, cx| {
                            this.focus(window, cx);
                        });
                        focus_handle_done.set(true);
                    }
                    let mut form = v_form()
                        .child(field().label(name_label).child(Input::new(&name_state)))
                        .child(field().label(host_label).child(Input::new(&host_state)))
                        .child(field().label(port_label).child(NumberInput::new(&port_state)))
                        .child(field().label(username_label).child(Input::new(&username_state)))
                        .child(
                            field()
                                .label(password_label)
                                .child(Input::new(&password_state).mask_toggle()),
                        )
                        .child(field().label(tls_label).child({
                            let server_enable_tls = server_enable_tls.clone();
                            Checkbox::new("edit-redis-server-tls")
                                .label(tls_check_label)
                                .checked(server_enable_tls.get())
                                .on_click(move |checked, _, cx| {
                                    server_enable_tls.set(*checked);
                                    cx.stop_propagation();
                                })
                        }));

                    if server_enable_tls.get() {
                        form = form
                            .child(field().label(insecure_tls_label).child({
                                let server_insecure_tls = server_insecure_tls.clone();
                                Checkbox::new("edit-redis-server-insecure-tls")
                                    .label(insecure_tls_check_label)
                                    .checked(server_insecure_tls.get())
                                    .on_click(move |checked, _, cx| {
                                        server_insecure_tls.set(*checked);
                                        cx.stop_propagation();
                                    })
                            }))
                            .child(field().label(client_cert_label).child(Input::new(&client_cert_state)))
                            .child(field().label(client_key_label).child(Input::new(&client_key_state)))
                            .child(field().label(root_cert_label).child(Input::new(&root_cert_state)));
                    }

                    form = form.child(field().label(ssh_tunnel_label).child({
                        let server_ssh_tunnel = server_ssh_tunnel.clone();
                        Checkbox::new("edit-redis-server-ssh-tunnel")
                            .label(ssh_tunnel_check_label)
                            .checked(server_ssh_tunnel.get())
                            .on_click(move |checked, _, cx| {
                                server_ssh_tunnel.set(*checked);
                                cx.stop_propagation();
                            })
                    }));

                    if server_ssh_tunnel.get() {
                        form = form
                            .child(field().label(ssh_addr_label).child(Input::new(&ssh_addr_state)))
                            .child(field().label(ssh_username_label).child(Input::new(&ssh_username_state)))
                            .child(field().label(ssh_password_label).child(Input::new(&ssh_password_state)))
                            .child(field().label(ssh_key_label).child(Input::new(&ssh_key_state)));
                    }

                    form = form
                        .child(field().label(master_name_label).child(Input::new(&master_name_state)))
                        .child(field().label(description_label).child(Input::new(&description_state)));

                    div()
                        .id("edit-servers-scrollable-container")
                        .max_h(px(600.0))
                        .child(form)
                        .overflow_y_scrollbar()
                })
                .on_ok({
                    let handle = handle_submit.clone();
                    move |_, window, cx| handle(window, cx)
                })
                .footer({
                    let handle = handle_submit.clone();
                    move |_, _, _, cx| {
                        let submit_label = i18n_common(cx, "submit");
                        let cancel_label = i18n_common(cx, "cancel");

                        let mut buttons = vec![
                            Button::new("cancel").label(cancel_label).on_click(|_, window, cx| {
                                window.close_dialog(cx);
                            }),
                            Button::new("ok").primary().label(submit_label).on_click({
                                let handle = handle.clone();
                                move |_, window, cx| {
                                    handle.clone()(window, cx);
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

    /// Render the scrollable server list
    ///
    /// Shows:
    /// - Home page item (always first)
    /// - All opened server items
    ///
    /// Current selection is highlighted with background color and border.
    /// Clicking an item navigates to that server or home page.
    /// Right-clicking shows a context menu to close the server.
    fn render_server_list(&self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let view_for_capture = view.clone();
        let view_for_menu = view.clone();
        let servers = self.state.server_names.clone();
        let current_server_id_clone = self.state.server_id.clone();
        let is_match_route = matches!(
            cx.global::<ZedisGlobalStore>().read(cx).route(),
            Route::Home | Route::Editor
        );

        let home_label = i18n_sidebar(cx, "home");
        let close_label = i18n_sidebar(cx, "close");
        let edit_label = i18n_sidebar(cx, "edit");
        let list_active_color = cx.theme().list_active;
        let list_active_border_color = cx.theme().list_active_border;

        let right_clicked_server_id = self.state.right_clicked_server_id.clone();

        uniform_list("sidebar-redis-servers", servers.len(), move |range, _window, _cx| {
            range
                .map(|index| {
                    let (server_id, server_name) = servers.get(index).cloned().unwrap_or_default();

                    let is_home = server_id.is_empty();
                    let is_current = is_match_route && server_id == current_server_id_clone;

                    // Display "Home" for empty server_name, otherwise use server name
                    let name = if server_name.is_empty() {
                        home_label.clone()
                    } else {
                        server_name.clone()
                    };

                    let view = view.clone();
                    let view_for_capture = view_for_capture.clone();
                    let view_for_menu = view_for_menu.clone();
                    let tooltip_name = name.clone();
                    let close_label = close_label.clone();
                    let right_click_server_id = server_id.clone();
                    let right_clicked_server_id = right_clicked_server_id.clone();

                    div()
                        .id(("sidebar-server-tooltip", index))
                        .tooltip(move |window, cx| Tooltip::new(tooltip_name.clone()).build(window, cx))
                        .capture_any_mouse_down(move |event, _, cx| {
                            if event.button == MouseButton::Right {
                                view_for_capture.update(cx, |this, cx| {
                                    this.state.right_clicked_server_id = None;
                                    cx.notify();
                                });
                            }
                        })
                        .context_menu({
                            let close_label = close_label.clone();
                            let edit_label = edit_label.clone();
                            let right_clicked_server_id = right_clicked_server_id.clone();
                            move |menu, _window, _cx| {
                                // Only show context menu for non-home items
                                if let Some(server_id) = right_clicked_server_id.clone() {
                                    if server_id.is_empty() {
                                        return menu;
                                    }
                                    let view = view_for_menu.clone();
                                    let view_for_edit = view_for_menu.clone();
                                    let server_id_for_close = server_id.clone();
                                    let server_id_for_edit = server_id.clone();

                                    // Edit menu item (before close)
                                    menu.item(PopupMenuItem::new(edit_label.clone()).on_click(move |_, window, cx| {
                                        let server_id = server_id_for_edit.clone();
                                        view_for_edit.update(cx, |this, cx| {
                                            this.open_edit_server_dialog(server_id, window, cx);
                                        });
                                    }))
                                    // Close menu item
                                    .item(
                                        PopupMenuItem::new(close_label.clone()).on_click(move |_, _window, cx| {
                                            let server_id = server_id_for_close.clone();
                                            view.update(cx, |this, cx| {
                                                // Close connection in connection manager
                                                get_connection_manager().remove_client(&server_id);

                                                // Update server state
                                                this.server_state.update(cx, |state, cx| {
                                                    state.close_server(&server_id, cx);
                                                });

                                                // Navigate to home if we're closing the current server
                                                cx.update_global::<ZedisGlobalStore, ()>(|store, cx| {
                                                    store.update(cx, |state, cx| {
                                                        state.go_to(Route::Home, cx);
                                                    });
                                                });
                                            });
                                        }),
                                    )
                                } else {
                                    menu
                                }
                            }
                        })
                        .child(
                            ListItem::new(("sidebar-redis-server", index))
                                .w_full()
                                .when(is_current, |this| this.bg(list_active_color))
                                .py_4()
                                .border_r(px(SERVER_LIST_ITEM_BORDER_WIDTH))
                                .when(is_current, |this| this.border_color(list_active_border_color))
                                .child(
                                    v_flex()
                                        .items_center()
                                        .on_mouse_down(MouseButton::Right, {
                                            let view = view.clone();
                                            let server_id = right_click_server_id.clone();
                                            move |_, _, cx| {
                                                view.update(cx, |this, cx| {
                                                    this.state.right_clicked_server_id = Some(server_id.clone());
                                                    cx.notify();
                                                });
                                            }
                                        })
                                        .child(Icon::new(IconName::LayoutDashboard))
                                        .child(Label::new(name).text_ellipsis().text_xs()),
                                )
                                .on_click(move |_, _window, cx| {
                                    // Don't do anything if already selected
                                    if is_current {
                                        return;
                                    }

                                    // Determine target route based on home/server
                                    let route = if is_home { Route::Home } else { Route::Editor };

                                    view.update(cx, |this, cx| {
                                        let preset_credentials =
                                            cx.global::<ZedisGlobalStore>().read(cx).preset_credentials();

                                        // Update global route
                                        cx.update_global::<ZedisGlobalStore, ()>(|store, cx| {
                                            store.update(cx, |state, cx| {
                                                state.go_to(route, cx);
                                            });
                                        });

                                        this.server_state.update(cx, |state, cx| {
                                            state.select(server_id.clone(), 0, preset_credentials, cx);
                                        });
                                    });
                                }),
                        )
                })
                .collect()
        })
        .size_full()
    }

    /// Render settings button with dropdown menu
    ///
    /// The dropdown contains two submenus:
    /// 1. Theme selection (Light/Dark/System)
    /// 2. Language selection (English/Chinese)
    ///
    /// Changes are saved to disk and applied immediately across all windows.
    fn render_settings_button(&self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = cx.global::<ZedisGlobalStore>().read(cx);

        // Determine currently selected theme mode
        let current_action = match store.theme() {
            Some(ThemeMode::Light) => ThemeAction::Light,
            Some(ThemeMode::Dark) => ThemeAction::Dark,
            _ => ThemeAction::System,
        };

        // Determine currently selected locale
        let locale = store.locale();
        let current_locale = match locale {
            "zh" => LocaleAction::Zh,
            _ => LocaleAction::En,
        };
        let current_font_size = store.font_size();

        let btn = Button::new("zedis-sidebar-setting-btn")
            .ghost()
            .w_full()
            .h(px(SETTINGS_BUTTON_HEIGHT))
            .tooltip(i18n_sidebar(cx, "settings"))
            .child(Icon::new(IconName::Settings).size(px(SETTINGS_ICON_SIZE)))
            .dropdown_menu_with_anchor(Corner::BottomRight, move |menu, window, cx| {
                let theme_text = i18n_sidebar(cx, "theme");
                let lang_text = i18n_sidebar(cx, "lang");
                let font_size_text = i18n_sidebar(cx, "font_size");

                // Theme submenu with light/dark/system options
                menu.submenu_with_icon(
                    Some(Icon::new(IconName::Sun).px(ICON_PADDING).mr(ICON_MARGIN)),
                    theme_text,
                    window,
                    cx,
                    move |submenu, _window, _cx| {
                        submenu
                            .menu_element_with_check(
                                current_action == ThemeAction::Light,
                                Box::new(ThemeAction::Light),
                                |_window, cx| Label::new(i18n_sidebar(cx, "light")).text_xs().p(LABEL_PADDING),
                            )
                            .menu_element_with_check(
                                current_action == ThemeAction::Dark,
                                Box::new(ThemeAction::Dark),
                                |_window, cx| Label::new(i18n_sidebar(cx, "dark")).text_xs().p(LABEL_PADDING),
                            )
                            .menu_element_with_check(
                                current_action == ThemeAction::System,
                                Box::new(ThemeAction::System),
                                |_window, cx| Label::new(i18n_sidebar(cx, "system")).text_xs().p(LABEL_PADDING),
                            )
                    },
                )
                // Language submenu with Chinese/English options
                .submenu_with_icon(
                    Some(Icon::new(CustomIconName::Languages).px(ICON_PADDING).mr(ICON_MARGIN)),
                    lang_text,
                    window,
                    cx,
                    move |submenu, _window, _cx| {
                        submenu
                            .menu_element_with_check(
                                current_locale == LocaleAction::Zh,
                                Box::new(LocaleAction::Zh),
                                |_window, _cx| Label::new("中文").text_xs().p(LABEL_PADDING),
                            )
                            .menu_element_with_check(
                                current_locale == LocaleAction::En,
                                Box::new(LocaleAction::En),
                                |_window, _cx| Label::new("English").text_xs().p(LABEL_PADDING),
                            )
                    },
                )
                .submenu_with_icon(
                    Some(Icon::new(CustomIconName::ALargeSmall).px(ICON_PADDING).mr(ICON_MARGIN)),
                    font_size_text,
                    window,
                    cx,
                    move |submenu, _window, _cx| {
                        submenu
                            .menu_element_with_check(
                                current_font_size == FontSize::Large,
                                Box::new(FontSizeAction::Large),
                                move |_window, cx| {
                                    let text = i18n_sidebar(cx, "font_size_large");
                                    Label::new(text).text_xs().p(LABEL_PADDING)
                                },
                            )
                            .menu_element_with_check(
                                current_font_size == FontSize::Medium,
                                Box::new(FontSizeAction::Medium),
                                move |_window, cx| {
                                    let text = i18n_sidebar(cx, "font_size_medium");
                                    Label::new(text).text_xs().p(LABEL_PADDING)
                                },
                            )
                            .menu_element_with_check(
                                current_font_size == FontSize::Small,
                                Box::new(FontSizeAction::Small),
                                move |_window, cx| {
                                    let text = i18n_sidebar(cx, "font_size_small");
                                    Label::new(text).text_xs().p(LABEL_PADDING)
                                },
                            )
                    },
                )
                .menu_element_with_icon(
                    Icon::new(IconName::Settings2),
                    Box::new(SettingsAction::Editor),
                    move |_window, cx| Label::new(i18n_sidebar(cx, "other_settings")).p(LABEL_PADDING),
                )
            });
        div().border_t_1().border_color(cx.theme().border).child(btn)
    }

    /// Render GitHub star button (link to repository)
    fn render_star(&self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().border_b_1().border_color(cx.theme().border).child(
            Button::new("github")
                .ghost()
                .h(px(STAR_BUTTON_HEIGHT))
                .w_full()
                .tooltip(i18n_sidebar(cx, "star"))
                .child(
                    v_flex()
                        .items_center()
                        .justify_center()
                        .child(Icon::new(IconName::GitHub))
                        .child(Label::new("ZEDIS").text_xs()),
                )
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.open_url("https://github.com/vicanso/zedis");
                })),
        )
    }
}

impl Render for ZedisSidebar {
    /// Main render method - displays vertical sidebar with navigation and settings
    ///
    /// Layout structure (top to bottom):
    /// 1. GitHub star button
    /// 2. Server list (scrollable, takes remaining space)
    /// 3. Settings button (theme & language)
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        tracing::debug!("Rendering sidebar view");
        let show_settings_button = is_development();

        v_flex()
            .size_full()
            .id("sidebar-container")
            .justify_start()
            .border_r_1()
            .border_color(cx.theme().border)
            .when(show_settings_button, |this| this.child(self.render_star(window, cx)))
            .child(
                // Server list takes up remaining vertical space
                div().flex_1().size_full().child(self.render_server_list(window, cx)),
            )
            .when(show_settings_button, |this| {
                this.child(self.render_settings_button(window, cx))
            })
    }
}
