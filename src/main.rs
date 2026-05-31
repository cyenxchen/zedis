#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use crate::connection::get_servers;
use crate::constants::SIDEBAR_WIDTH;
use crate::helpers::{MemuAction, is_app_store_build, is_development, new_hot_keys};
use crate::states::update::{ZedisUpdateState, ZedisUpdateStore, check_for_updates, start_auto_update_scheduler};
use crate::states::{
    FontSize, FontSizeAction, LocaleAction, NotificationCategory, Route, ServerEvent, SettingsAction, ThemeAction,
    ZedisAppState, ZedisGlobalStore, ZedisServerState, save_app_state, update_app_state_and_save,
};
use crate::views::{ZedisContent, ZedisSidebar, ZedisTitleBar, open_about_window};
use gpui::{
    App, Application, Bounds, Entity, Menu, MenuItem, Pixels, Task, TitlebarOptions, Window, WindowAppearance,
    WindowBounds, WindowOptions, div, prelude::*, px, size,
};
use gpui_component::{ActiveTheme, Root, Theme, ThemeMode, WindowExt, h_flex, notification::Notification, v_flex};
use single_instance::SingleInstance;
use std::fs::OpenOptions;
#[cfg(target_os = "windows")]
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::{env, str::FromStr};
use tracing::{Level, error, info, warn};
use tracing_subscriber::FmtSubscriber;
#[cfg(target_os = "windows")]
use windows::{
    Win32::{
        Foundation::{BOOL, CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, WAIT_OBJECT_0},
        System::Threading::{
            AttachThreadInput, CreateEventW, GetCurrentProcessId, GetCurrentThreadId, INFINITE, SetEvent,
            WaitForSingleObject,
        },
        UI::WindowsAndMessaging::{
            ASFW_ANY, AllowSetForegroundWindow, BringWindowToTop, EnumWindows, GetClassNameW, GetForegroundWindow,
            GetWindowTextW, GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOPMOST, IsIconic, IsWindowVisible,
            SW_RESTORE, SW_SHOW, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SetForegroundWindow, SetWindowPos, ShowWindow,
        },
    },
    core::HSTRING,
};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

rust_i18n::i18n!("locales", fallback = "en");

const PKG_NAME: &str = env!("CARGO_PKG_NAME");
const APP_WINDOW_TITLE: &str = "Zedis";
const SINGLE_INSTANCE_ID: &str = "com.bigtree.zedis";
#[cfg(target_os = "windows")]
const WINDOWS_WINDOW_CLASS_NAME: &str = "Zed::Window";
#[cfg(target_os = "windows")]
const WINDOWS_WINDOW_TITLE: &str = APP_WINDOW_TITLE;
#[cfg(target_os = "windows")]
const SINGLE_INSTANCE_ACTIVATION_EVENT: &str = "Local\\com.bigtree.zedis.activate";

mod assets;
mod components;
mod connection;
mod constants;
mod error;
mod helpers;
mod states;
mod views;

pub struct Zedis {
    pending_notification: Option<Notification>,
    last_bounds: Bounds<Pixels>,
    save_task: Option<Task<()>>,
    // views
    sidebar: Entity<ZedisSidebar>,
    content: Entity<ZedisContent>,
    title_bar: Option<Entity<ZedisTitleBar>>,
    theme_update_task: Option<Task<()>>,
    _activation_listener_task: Option<Task<()>>,
}

impl Zedis {
    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        server_state: Entity<ZedisServerState>,
        #[cfg(target_os = "windows")] activation_event: Option<ActivationEvent>,
    ) -> Self {
        let sidebar = cx.new(|cx| ZedisSidebar::new(server_state.clone(), window, cx));
        let content = cx.new(|cx| ZedisContent::new(server_state.clone(), window, cx));
        #[cfg(target_os = "windows")]
        let _activation_listener_task = activation_event.map(|event| start_activation_listener(event, cx));
        #[cfg(not(target_os = "windows"))]
        let _activation_listener_task = None;
        cx.subscribe(&server_state, |this, _server_state, event, cx| {
            match event {
                ServerEvent::Notification(e) => {
                    let message = e.message.clone();
                    let mut notification = match e.category {
                        NotificationCategory::Info => Notification::info(message),
                        NotificationCategory::Success => Notification::success(message),
                        NotificationCategory::Warning => Notification::warning(message),
                        _ => Notification::error(message),
                    };
                    if let Some(title) = e.title.as_ref() {
                        notification = notification.title(title);
                    }
                    this.pending_notification = Some(notification);
                }
                ServerEvent::ErrorOccurred(error) => {
                    this.pending_notification = Some(Notification::error(error.message.clone()));
                }
                _ => {
                    return;
                }
            }
            cx.notify();
        })
        .detach();
        cx.observe_window_appearance(window, |this, _window, cx| {
            if cx.global::<ZedisGlobalStore>().read(cx).theme().is_none() {
                this.theme_update_task = Some(cx.spawn(async move |_this, cx| {
                    let _ = cx.update(|cx| {
                        Theme::change(cx.window_appearance(), None, cx);
                        cx.refresh_windows();
                    });
                }));
            }
        })
        .detach();
        let title_bar = Some(cx.new(|cx| ZedisTitleBar::new(window, cx)));

        Self {
            sidebar,
            save_task: None,
            content,
            pending_notification: None,
            title_bar,
            theme_update_task: None,
            _activation_listener_task,
            last_bounds: Bounds::default(),
        }
    }
    fn persist_window_state(&mut self, new_bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        self.last_bounds = new_bounds;
        let store = cx.global::<ZedisGlobalStore>().clone();
        let mut value = store.value(cx);
        value.set_bounds(new_bounds);
        let task = cx.spawn(async move |_, cx| {
            // wait 500ms
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;

            let result = store.update(cx, move |state, cx| {
                state.set_bounds(new_bounds);
                cx.notify();
            });
            if let Err(e) = result {
                error!(error = %e, "update window bounds fail",);
                return;
            };

            cx.background_spawn(async move {
                if let Err(e) = save_app_state(&value) {
                    error!(error = %e, "save window bounds fail",);
                } else {
                    info!(bounds = ?new_bounds, "save window bounds success");
                }
            })
            .await;
        });
        self.save_task = Some(task);
    }
    fn render_titlebar(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let Some(title_bar) = self.title_bar.as_ref() else {
            return h_flex().into_any_element();
        };
        title_bar.clone().into_any_element()
    }
}

impl Render for Zedis {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dialog_layer = Root::render_dialog_layer(window, cx);
        let notification_layer = Root::render_notification_layer(window, cx);
        let current_bounds = window.bounds();
        if current_bounds != self.last_bounds {
            self.persist_window_state(current_bounds, cx);
        }
        if let Some(notification) = self.pending_notification.take() {
            window.push_notification(notification, cx);
        }
        if let Some(font_size) = cx.global::<ZedisGlobalStore>().read(cx).font_size().to_pixels() {
            window.set_rem_size(font_size);
        }

        let content = v_flex()
            .id(PKG_NAME)
            .size_full()
            .child(self.render_titlebar(window, cx))
            .child(
                h_flex()
                    .id(PKG_NAME)
                    .bg(cx.theme().background)
                    .size_full()
                    .child(
                        div()
                            .w(px(SIDEBAR_WIDTH))
                            .flex_none()
                            .h_full()
                            .child(self.sidebar.clone()),
                    )
                    .child(self.content.clone())
                    .children(dialog_layer)
                    .children(notification_layer),
            );
        content
            .on_action(cx.listener(|_this, e: &ThemeAction, _window, cx| {
                let action = *e;

                // Convert action to theme mode
                let mode = match action {
                    ThemeAction::Light => Some(ThemeMode::Light),
                    ThemeAction::Dark => Some(ThemeMode::Dark),
                    ThemeAction::System => None, // Follow OS theme
                };

                // Determine actual render mode (resolve System to Light/Dark)
                let render_mode = match mode {
                    Some(m) => m,
                    None => match cx.window_appearance() {
                        WindowAppearance::Light => ThemeMode::Light,
                        _ => ThemeMode::Dark,
                    },
                };

                // Apply theme immediately for instant visual feedback
                Theme::change(render_mode, None, cx);

                // Save preference to disk asynchronously
                update_app_state_and_save(cx, "save_theme", move |state, _cx| {
                    state.set_theme(mode);
                });
            }))
            // Locale action handler - changes language and saves to disk
            .on_action(cx.listener(|_this, e: &LocaleAction, _window, cx| {
                let locale = match e {
                    LocaleAction::Zh => "zh",
                    LocaleAction::En => "en",
                };

                // Save locale preference and refresh UI
                update_app_state_and_save(cx, "save_locale", move |state, _cx| {
                    state.set_locale(locale.to_string());
                });
            }))
            .on_action(cx.listener(move |_this, e: &FontSizeAction, _window, cx| {
                let action = *e;

                let font_size = match action {
                    FontSizeAction::Large => Some(FontSize::Large),
                    FontSizeAction::Small => Some(FontSize::Small),
                    _ => None,
                };
                // Save locale preference and refresh UI
                update_app_state_and_save(cx, "save_font_size", move |state, _cx| {
                    state.set_font_size(font_size);
                });
            }))
            .on_action(cx.listener(move |_this, e: &SettingsAction, _window, cx| {
                let action = *e;
                if action == SettingsAction::Editor {
                    cx.update_global::<ZedisGlobalStore, ()>(|store, cx| {
                        store.update(cx, |state, cx| {
                            state.go_to(Route::Settings, cx);
                        });
                    });
                }
            }))
            .on_action(cx.listener(|_this, e: &MemuAction, window, cx| match e {
                MemuAction::CheckForUpdates => {
                    check_for_updates(true, cx);
                }
                MemuAction::Minimize => {
                    info!("minimize window requested");
                    window.minimize_window();
                }
                _ => {
                    cx.propagate();
                }
            }))
    }
}

fn init_logger() {
    let mut level = Level::INFO;
    if let Ok(log_level) = env::var("RUST_LOG")
        && let Ok(value) = Level::from_str(log_level.as_str())
    {
        level = value;
    }
    let log_file = env::var("ZEDIS_DEBUG_LOG").ok();
    let timer = tracing_subscriber::fmt::time::OffsetTime::local_rfc_3339().unwrap_or_else(|_| {
        tracing_subscriber::fmt::time::OffsetTime::new(
            time::UtcOffset::from_hms(0, 0, 0).unwrap_or(time::UtcOffset::UTC),
            time::format_description::well_known::Rfc3339,
        )
    });

    if let Some(path) = log_file.clone() {
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                let path_for_writer = path.clone();
                let writer = move || {
                    file.try_clone().unwrap_or_else(|_| {
                        OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&path_for_writer)
                            .expect("open log file")
                    })
                };
                eprintln!("logging to file: {path}");
                let subscriber = FmtSubscriber::builder()
                    .with_max_level(level)
                    .with_timer(timer)
                    .with_writer(writer)
                    .with_ansi(false)
                    .finish();
                tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
                return;
            }
            Err(e) => {
                eprintln!("failed to open log file {path}: {e}");
            }
        }
    }

    let subscriber = FmtSubscriber::builder()
        .with_max_level(level)
        .with_timer(timer)
        .with_ansi(is_development())
        .finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");
}

#[cfg(target_os = "macos")]
fn single_instance_key() -> String {
    let lock_path = crate::helpers::get_or_create_config_dir()
        .map(|dir| dir.join(format!("{SINGLE_INSTANCE_ID}.lock")))
        .unwrap_or_else(|e| {
            let fallback = env::temp_dir().join(format!("{SINGLE_INSTANCE_ID}.lock"));
            tracing::warn!(error = %e, path = %fallback.display(), "failed to use config dir for single instance lock");
            fallback
        });

    lock_path.to_string_lossy().into_owned()
}

#[cfg(not(target_os = "macos"))]
fn single_instance_key() -> String {
    SINGLE_INSTANCE_ID.to_string()
}

#[cfg(target_os = "windows")]
pub(crate) struct ActivationEvent {
    handle: HANDLE,
}

#[cfg(target_os = "windows")]
unsafe impl Send for ActivationEvent {}

#[cfg(target_os = "windows")]
impl Drop for ActivationEvent {
    fn drop(&mut self) {
        if let Err(e) = unsafe { CloseHandle(self.handle) } {
            warn!(error = %e, "failed to close single instance activation event handle");
        }
    }
}

#[cfg(target_os = "windows")]
fn open_activation_event() -> Option<ActivationEvent> {
    let event_name = HSTRING::from(SINGLE_INSTANCE_ACTIVATION_EVENT);
    let event = unsafe { CreateEventW(None, false, false, &event_name) };
    match event {
        Ok(handle) => {
            info!(
                event = SINGLE_INSTANCE_ACTIVATION_EVENT,
                "opened single instance activation event"
            );
            Some(ActivationEvent { handle })
        }
        Err(e) => {
            error!(error = %e, event = SINGLE_INSTANCE_ACTIVATION_EVENT, "failed to open single instance activation event");
            None
        }
    }
}

#[cfg(target_os = "windows")]
fn open_existing_activation_event() -> Option<ActivationEvent> {
    let event_name = HSTRING::from(SINGLE_INSTANCE_ACTIVATION_EVENT);
    let event = unsafe { CreateEventW(None, false, false, &event_name) };
    match event {
        Ok(handle) => {
            let event = ActivationEvent { handle };
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                Some(event)
            } else {
                None
            }
        }
        Err(e) => {
            error!(error = %e, event = SINGLE_INSTANCE_ACTIVATION_EVENT, "failed to open existing single instance activation event");
            None
        }
    }
}

#[cfg(target_os = "windows")]
fn notify_existing_instance() {
    if restore_existing_window_from_new_process() {
        return;
    }

    let mut event = None;
    for _ in 0..10 {
        event = open_existing_activation_event();
        if event.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let Some(event) = event else {
        warn!(
            event = SINGLE_INSTANCE_ACTIVATION_EVENT,
            "existing zedis activation event was not ready"
        );
        return;
    };

    if let Err(e) = unsafe { AllowSetForegroundWindow(ASFW_ANY) } {
        warn!(error = %e, "failed to allow existing zedis instance to set foreground window");
    } else {
        info!("allowed existing zedis instance to set foreground window");
    }

    if let Err(e) = unsafe { SetEvent(event.handle) } {
        error!(error = %e, event = SINGLE_INSTANCE_ACTIVATION_EVENT, "failed to notify existing zedis instance");
        return;
    }
    info!(
        event = SINGLE_INSTANCE_ACTIVATION_EVENT,
        "notified existing zedis instance to activate"
    );
}

#[cfg(not(target_os = "windows"))]
fn notify_existing_instance() {}

#[cfg(target_os = "windows")]
struct ExistingWindowSearch {
    current_pid: u32,
    hwnd: HWND,
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn enum_existing_zedis_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return BOOL(1);
    }

    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let search = unsafe { &mut *(lparam.0 as *mut ExistingWindowSearch) };
    if pid == 0 || pid == search.current_pid {
        return BOOL(1);
    }

    let mut class_name = [0u16; 64];
    let class_len = unsafe { GetClassNameW(hwnd, &mut class_name) };
    if class_len <= 0 || String::from_utf16_lossy(&class_name[..class_len as usize]) != WINDOWS_WINDOW_CLASS_NAME {
        return BOOL(1);
    }

    let mut title = [0u16; 64];
    let title_len = unsafe { GetWindowTextW(hwnd, &mut title) };
    if title_len <= 0 || String::from_utf16_lossy(&title[..title_len as usize]) != WINDOWS_WINDOW_TITLE {
        return BOOL(1);
    }

    search.hwnd = hwnd;
    BOOL(0)
}

#[cfg(target_os = "windows")]
fn find_existing_zedis_window() -> Option<HWND> {
    let mut search = ExistingWindowSearch {
        current_pid: unsafe { GetCurrentProcessId() },
        hwnd: HWND::default(),
    };

    let result = unsafe {
        EnumWindows(
            Some(enum_existing_zedis_window),
            LPARAM(&mut search as *mut ExistingWindowSearch as isize),
        )
    };
    if let Err(e) = result {
        error!(error = %e, "failed to enumerate windows for existing zedis instance");
    }

    if search.hwnd.0 == 0 { None } else { Some(search.hwnd) }
}

#[cfg(target_os = "windows")]
fn restore_existing_window_from_new_process() -> bool {
    let Some(hwnd) = find_existing_zedis_window() else {
        warn!("no existing zedis window found for direct activation");
        return false;
    };

    let current_thread_id = unsafe { GetCurrentThreadId() };
    let target_thread_id = unsafe { GetWindowThreadProcessId(hwnd, None) };
    let foreground_hwnd = unsafe { GetForegroundWindow() };
    let foreground_thread_id = if foreground_hwnd.0 == 0 {
        0
    } else {
        unsafe { GetWindowThreadProcessId(foreground_hwnd, None) }
    };

    let attached_target = target_thread_id != 0
        && target_thread_id != current_thread_id
        && unsafe { AttachThreadInput(current_thread_id, target_thread_id, true).as_bool() };
    let attached_foreground = foreground_thread_id != 0
        && foreground_thread_id != current_thread_id
        && foreground_thread_id != target_thread_id
        && unsafe { AttachThreadInput(current_thread_id, foreground_thread_id, true).as_bool() };

    let was_minimized = unsafe { IsIconic(hwnd).as_bool() };
    if was_minimized {
        let _ = unsafe { ShowWindow(hwnd, SW_RESTORE) };
    } else {
        let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
    }

    if let Err(e) = unsafe { BringWindowToTop(hwnd) } {
        warn!(error = %e, "failed to bring existing zedis window to top");
    }
    if let Err(e) = unsafe { SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW) } {
        warn!(error = %e, "failed to temporarily set existing zedis window topmost");
    }
    if let Err(e) = unsafe {
        SetWindowPos(
            hwnd,
            HWND_NOTOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        )
    } {
        warn!(error = %e, "failed to restore existing zedis window z-order");
    }

    let foreground_set = unsafe { SetForegroundWindow(hwnd).as_bool() };

    if attached_foreground {
        let _ = unsafe { AttachThreadInput(current_thread_id, foreground_thread_id, false) };
    }
    if attached_target {
        let _ = unsafe { AttachThreadInput(current_thread_id, target_thread_id, false) };
    }

    info!(
        foreground_set,
        was_minimized, attached_target, attached_foreground, "directly activated existing zedis window"
    );
    foreground_set
}

struct SingleInstanceGuard {
    _instance: SingleInstance,
    #[cfg(target_os = "windows")]
    activation_event: Option<ActivationEvent>,
}

fn acquire_single_instance() -> Option<SingleInstanceGuard> {
    let key = single_instance_key();
    match SingleInstance::new(&key) {
        Ok(instance) if instance.is_single() => {
            info!(key = %key, "acquired single instance lock");
            Some(SingleInstanceGuard {
                _instance: instance,
                #[cfg(target_os = "windows")]
                activation_event: open_activation_event(),
            })
        }
        Ok(_) => {
            info!(key = %key, "another zedis instance is already running");
            notify_existing_instance();
            None
        }
        Err(e) => {
            error!(error = %e, key = %key, "failed to acquire single instance lock");
            None
        }
    }
}

fn activate_existing_windows(cx: &mut App) {
    let windows = cx.windows();
    if windows.is_empty() {
        warn!("single instance activation requested but no zedis windows are open");
        return;
    }

    info!(window_count = windows.len(), "activating existing zedis windows");
    for window in windows {
        if let Err(e) = window.update(cx, |_, window, _| window.activate_window()) {
            warn!(error = %e, "failed to activate zedis window");
        }
    }
    cx.activate(true);
}

#[cfg(target_os = "windows")]
fn start_activation_listener(event: ActivationEvent, cx: &mut Context<Zedis>) -> Task<()> {
    let activation_requested = Arc::new(AtomicBool::new(false));
    let listener_flag = activation_requested.clone();

    let spawn_result = std::thread::Builder::new()
        .name("zedis-single-instance-activation-listener".to_string())
        .spawn(move || {
            loop {
                let wait_result = unsafe { WaitForSingleObject(event.handle, INFINITE) };
                if wait_result == WAIT_OBJECT_0 {
                    listener_flag.store(true, Ordering::Release);
                } else {
                    error!(wait_result = ?wait_result, "single instance activation listener stopped");
                    break;
                }
            }
        });

    if let Err(e) = spawn_result {
        error!(error = %e, "failed to start single instance activation listener");
    }

    cx.spawn(async move |_, cx| {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(150))
                .await;
            if activation_requested.swap(false, Ordering::AcqRel) {
                info!("single instance activation request received");
                if let Err(e) = cx.update(activate_existing_windows) {
                    error!(error = %e, "failed to process single instance activation request");
                }
            }
        }
    })
}

fn main() {
    init_logger();
    let Some(single_instance) = acquire_single_instance() else {
        return;
    };
    let SingleInstanceGuard {
        _instance: _single_instance,
        #[cfg(target_os = "windows")]
        activation_event,
    } = single_instance;
    let app = Application::new().with_assets(assets::Assets);
    app.on_reopen(activate_existing_windows);
    let app_state = ZedisAppState::try_new().unwrap_or_else(|e| {
        error!(error = %e, "Failed to load app state, using default state");
        ZedisAppState::new()
    });
    let mut server_state = ZedisServerState::new();
    match get_servers() {
        Ok(servers) => {
            server_state.set_servers(servers);
        }
        Err(e) => {
            error!(error = %e, "get servers fail",);
        }
    }
    info!(is_app_store_build = is_app_store_build(), "detect app build");

    app.run(move |cx| {
        // This must be called before using any GPUI Component features.
        gpui_component::init(cx);
        crate::components::init_selectable_text(cx);

        cx.activate(true);
        let window_bounds = if let Some(bounds) = app_state.bounds() {
            info!(bounds = ?bounds, "get window bounds from setting");
            *bounds
        } else {
            let mut window_size = size(px(1200.), px(750.));
            if let Some(display) = cx.primary_display() {
                let display_size = display.bounds().size;
                window_size.width = window_size.width.min(display_size.width * 0.85);
                window_size.height = window_size.height.min(display_size.height * 0.85);
            }
            Bounds::centered(None, window_size, cx)
        };
        let app_state = cx.new(|_| app_state);
        let app_store = ZedisGlobalStore::new(app_state);
        if let Some(theme) = app_store.read(cx).theme() {
            Theme::change(theme, None, cx);
        }
        cx.set_global(app_store);
        let update_state = cx.new(|_| ZedisUpdateState::default());
        cx.set_global(ZedisUpdateStore::new(update_state));
        cx.bind_keys(new_hot_keys());
        cx.on_action(|e: &MemuAction, cx: &mut App| match e {
            MemuAction::Quit => {
                cx.quit();
            }
            MemuAction::About => {
                open_about_window(cx);
            }
            MemuAction::CheckForUpdates => {
                check_for_updates(true, cx);
            }
            MemuAction::Minimize => {
                info!("minimize action ignored because no target window is active");
            }
        });
        cx.set_menus(vec![Menu {
            name: "Zedis".into(),
            items: vec![
                MenuItem::action("About Zedis", MemuAction::About),
                MenuItem::action("Quit", MemuAction::Quit),
            ],
        }]);

        let server_state = cx.new(|_| server_state.clone());
        cx.spawn(async move |cx| {
            #[cfg(target_os = "windows")]
            let mut activation_event = activation_event;
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(window_bounds)),
                    #[cfg(not(target_os = "linux"))]
                    titlebar: Some(TitlebarOptions {
                        title: Some(APP_WINDOW_TITLE.into()),
                        appears_transparent: true,
                        traffic_light_position: Some(gpui::point(px(9.0), px(9.0))),
                    }),
                    show: true,
                    window_min_size: Some(size(px(600.), px(400.))),
                    ..Default::default()
                },
                |window, cx| {
                    #[cfg(target_os = "macos")]
                    window.on_window_should_close(cx, move |_window, cx| {
                        cx.hide();
                        false
                    });
                    let zedis_view = cx.new(|cx| {
                        #[cfg(target_os = "windows")]
                        {
                            Zedis::new(window, cx, server_state, activation_event.take())
                        }
                        #[cfg(not(target_os = "windows"))]
                        {
                            Zedis::new(window, cx, server_state)
                        }
                    });
                    cx.new(|cx| Root::new(zedis_view, window, cx))
                },
            )?;

            Ok::<_, anyhow::Error>(())
        })
        .detach();

        // Auto-check for updates (skip dev and App Store builds)
        if !is_development() && !is_app_store_build() {
            start_auto_update_scheduler(cx);
        }
    });
}
