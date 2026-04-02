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

use crate::error::Error;
use gpui::{App, Entity, Global};
use serde::Deserialize;
use tracing::{debug, error, info};

type Result<T, E = Error> = std::result::Result<T, E>;

const GITHUB_API_URL: &str = "https://api.github.com/repos/cyenxchen/zedis/releases/latest";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

// --- GitHub API response types ---

#[derive(Debug, Clone, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    body: Option<String>,
    html_url: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

// --- Parsed release info ---

#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub version: semver::Version,
    pub tag_name: String,
    pub body: String,
    #[allow(dead_code)]
    pub html_url: String,
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    pub name: String,
    pub download_url: String,
    pub size: u64,
}

// --- Update status state machine ---

#[derive(Debug, Clone, Default)]
pub enum UpdateStatus {
    #[default]
    Idle,
    Checking,
    Available(Box<ReleaseInfo>),
    Downloading {
        downloaded: u64,
        total: u64,
    },
    Installing,
    Installed,
    UpToDate,
    Error(String),
}

// --- Auto-check scheduler outcome ---

#[derive(Debug, Clone)]
pub enum AutoCheckOutcome {
    UpToDate,
    UpdateAvailable,
    Failed,
    Skipped,
    Dismissed,
    TimerReset,
}

// --- Progress message for download channel ---

enum DownloadMsg {
    Progress { downloaded: u64 },
    Complete { written: u64 },
    Error(String),
}

// --- Global update state ---

#[derive(Default)]
pub struct ZedisUpdateState {
    pub status: UpdateStatus,
    pub(crate) outcome_tx: Option<futures::channel::mpsc::UnboundedSender<AutoCheckOutcome>>,
    pub(crate) dialog_window: Option<gpui::AnyWindowHandle>,
}

impl ZedisUpdateState {
    fn send_outcome(&self, outcome: AutoCheckOutcome) {
        if let Some(tx) = &self.outcome_tx {
            let _ = tx.unbounded_send(outcome);
        }
    }

    fn send_check_outcome(&self, manual: bool, auto_outcome: AutoCheckOutcome) {
        self.send_outcome(if manual {
            AutoCheckOutcome::TimerReset
        } else {
            auto_outcome
        });
    }
}

#[derive(Clone)]
pub struct ZedisUpdateStore {
    state: Entity<ZedisUpdateState>,
}

impl ZedisUpdateStore {
    pub fn new(state: Entity<ZedisUpdateState>) -> Self {
        Self { state }
    }
    pub fn state(&self) -> Entity<ZedisUpdateState> {
        self.state.clone()
    }
}

impl Global for ZedisUpdateStore {}

// --- Core functions ---

fn parse_release(gh: GitHubRelease) -> Result<ReleaseInfo> {
    let version_str = gh.tag_name.strip_prefix('v').unwrap_or(&gh.tag_name);
    let version = semver::Version::parse(version_str).map_err(|e| Error::Update {
        message: format!("Invalid version tag '{}': {}", gh.tag_name, e),
    })?;
    let assets = gh
        .assets
        .into_iter()
        .map(|a| ReleaseAsset {
            name: a.name,
            download_url: a.browser_download_url,
            size: a.size,
        })
        .collect();
    Ok(ReleaseInfo {
        version,
        tag_name: gh.tag_name,
        body: gh.body.unwrap_or_default(),
        html_url: gh.html_url,
        assets,
    })
}

fn platform_asset_name() -> Option<&'static str> {
    if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        Some("Zedis-aarch64.dmg")
    } else if cfg!(target_os = "windows") {
        Some("zedis-windows.exe.zip")
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
        Some("zedis-linux-x86_64.tar.gz")
    } else {
        None
    }
}

fn get_platform_asset(assets: &[ReleaseAsset]) -> Option<&ReleaseAsset> {
    let target_name = platform_asset_name()?;
    assets.iter().find(|a| a.name == target_name)
}

pub fn current_version() -> &'static str {
    CURRENT_VERSION
}

const GITHUB_COMPARE_URL: &str = "https://api.github.com/repos/cyenxchen/zedis/compare";

fn fetch_latest_release() -> Result<ReleaseInfo> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("Zedis")
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let resp: GitHubRelease = client.get(GITHUB_API_URL).send()?.error_for_status()?.json()?;
    let mut release = parse_release(resp)?;

    // If body is empty, fetch commit messages between current and new version
    if release.body.is_empty()
        && let Ok(notes) = fetch_compare_notes(&client, &release.tag_name)
    {
        release.body = notes;
    }
    Ok(release)
}

/// Fetch commit messages between current version and the new tag via GitHub compare API.
fn fetch_compare_notes(client: &reqwest::blocking::Client, new_tag: &str) -> Result<String> {
    let current_tag = format!("v{}", CURRENT_VERSION);
    let url = format!("{}/{}...{}", GITHUB_COMPARE_URL, current_tag, new_tag);
    let resp: serde_json::Value = client.get(&url).send()?.error_for_status()?.json()?;
    let commits = resp["commits"].as_array().ok_or_else(|| Error::Update {
        message: "No commits in compare response".to_string(),
    })?;
    let notes: Vec<String> = commits
        .iter()
        .filter_map(|c| {
            let msg = c["commit"]["message"].as_str()?;
            let first_line = msg.lines().next()?;
            // Skip merge commits
            if first_line.starts_with("Merge ") {
                return None;
            }
            Some(format!("- {}", first_line))
        })
        .collect();
    Ok(notes.join("\n"))
}

fn download_file(
    url: &str,
    path: &std::path::Path,
    tx: futures::channel::mpsc::UnboundedSender<DownloadMsg>,
) -> Result<u64> {
    use std::io::{Read, Write};
    let client = reqwest::blocking::Client::builder()
        .user_agent("Zedis")
        .timeout(std::time::Duration::from_secs(600))
        .build()?;
    let mut resp = client.get(url).send()?.error_for_status()?;
    let mut file = std::fs::File::create(path)?;
    let mut downloaded = 0u64;
    let mut buf = vec![0u8; 65536];
    let mut last_report = std::time::Instant::now();
    loop {
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        downloaded += n as u64;
        // Report progress at most every 50ms
        if last_report.elapsed() >= std::time::Duration::from_millis(50) {
            let _ = tx.unbounded_send(DownloadMsg::Progress { downloaded });
            last_report = std::time::Instant::now();
        }
    }
    Ok(downloaded)
}

fn is_fake_update() -> bool {
    std::env::var("ZEDIS_FAKE_UPDATE").is_ok_and(|v| v == "1" || v == "true")
}

fn fake_release() -> ReleaseInfo {
    let current = semver::Version::parse(CURRENT_VERSION).unwrap_or(semver::Version::new(0, 0, 0));
    let fake_version = semver::Version::new(current.major, current.minor, current.patch + 1);
    ReleaseInfo {
        version: fake_version.clone(),
        tag_name: format!("v{}", fake_version),
        body: "- [Demo] This is a fake update for UI testing\n- No actual download or install will happen".to_string(),
        html_url: String::new(),
        assets: vec![ReleaseAsset {
            name: platform_asset_name().unwrap_or("zedis-unknown").to_string(),
            download_url: String::new(),
            size: 50 * 1024 * 1024, // 50 MB fake size
        }],
    }
}

/// Check for updates. `manual` = true when triggered by user menu action.
pub fn check_for_updates(manual: bool, cx: &App) {
    let store = cx.global::<ZedisUpdateStore>().clone();
    let state_entity = store.state();

    // Guard: skip if already checking or downloading
    {
        let state = state_entity.read(cx);
        if matches!(
            state.status,
            UpdateStatus::Checking | UpdateStatus::Downloading { .. } | UpdateStatus::Installing
        ) {
            state.send_check_outcome(manual, AutoCheckOutcome::Skipped);
            return;
        }
    };

    // For auto-checks, skip if checked recently (within 1 hour)
    if !manual {
        let last_check = cx
            .global::<super::ZedisGlobalStore>()
            .read(cx)
            .last_update_check()
            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok());
        if let Some(last) = last_check {
            let elapsed = chrono::Utc::now().signed_duration_since(last);
            if elapsed.num_hours() < 1 {
                debug!("Skipping auto-check (last check was {}m ago)", elapsed.num_minutes());
                state_entity.read(cx).send_outcome(AutoCheckOutcome::Skipped);
                return;
            }
        }
    }

    // Set checking status
    cx.spawn(async move |cx| {
        let _ = state_entity.update(cx, |state, cx| {
            state.status = UpdateStatus::Checking;
            cx.notify();
        });

        // Fake update mode: skip network, use fake release
        if is_fake_update() {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(1))
                .await;
            let release = fake_release();
            info!("[FAKE] Simulating update available: {}", release.version);
            let _ = state_entity.update(cx, |state, cx| {
                state.status = UpdateStatus::Available(Box::new(release));
                state.send_check_outcome(manual, AutoCheckOutcome::UpdateAvailable);
                cx.notify();
            });
            cx.update(crate::views::open_update_dialog).ok();
            return;
        }

        // Fetch release on background thread (use channel to avoid blocking the executor)
        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(fetch_latest_release());
        });
        let result = rx.await.unwrap_or_else(|_| {
            Err(Error::Update {
                message: "Failed to check for updates".to_string(),
            })
        });

        match result {
            Ok(release) => {
                let current = semver::Version::parse(CURRENT_VERSION).unwrap_or(semver::Version::new(0, 0, 0));
                if release.version > current {
                    // Check if user skipped this version (only for auto-check)
                    if !manual {
                        let skipped = cx
                            .update(|cx| {
                                cx.global::<super::ZedisGlobalStore>()
                                    .read(cx)
                                    .skipped_version()
                                    .map(|s| s.to_string())
                            })
                            .ok()
                            .flatten();
                        if skipped.as_deref() == Some(release.tag_name.as_str()) {
                            debug!(version = %release.tag_name, "Skipping update (user skipped)");
                            let _ = state_entity.update(cx, |state, cx| {
                                state.status = UpdateStatus::Idle;
                                state.send_outcome(AutoCheckOutcome::Skipped);
                                cx.notify();
                            });
                            return;
                        }
                    }
                    info!(
                        current = %current,
                        new = %release.version,
                        "Update available"
                    );
                    let _ = state_entity.update(cx, |state, cx| {
                        state.status = UpdateStatus::Available(Box::new(release));
                        state.send_check_outcome(manual, AutoCheckOutcome::UpdateAvailable);
                        cx.notify();
                    });
                    cx.update(crate::views::open_update_dialog).ok();
                } else {
                    info!("Already up to date ({})", current);
                    let _ = state_entity.update(cx, |state, cx| {
                        state.status = UpdateStatus::UpToDate;
                        state.send_check_outcome(manual, AutoCheckOutcome::UpToDate);
                        cx.notify();
                    });
                    if manual {
                        cx.update(crate::views::open_update_dialog).ok();
                    }
                }
                // Save last check time only on success
                cx.update(|cx| {
                    super::update_app_state_and_save(cx, "save_last_update_check", |state, _cx| {
                        state.set_last_update_check(chrono::Utc::now().to_rfc3339());
                    });
                })
                .ok();
            }
            Err(e) => {
                error!(error = %e, "Failed to check for updates");
                let msg = e.to_string();
                let _ = state_entity.update(cx, |state, cx| {
                    state.status = UpdateStatus::Error(msg);
                    state.send_check_outcome(manual, AutoCheckOutcome::Failed);
                    cx.notify();
                });
                if manual {
                    cx.update(crate::views::open_update_dialog).ok();
                }
            }
        }
    })
    .detach();
}

/// Start downloading the update. Called from the update dialog.
pub fn download_update(cx: &App) {
    let store = cx.global::<ZedisUpdateStore>().clone();
    let state_entity = store.state();

    // Extract release info from current state
    let release = {
        let state = state_entity.read(cx);
        match &state.status {
            UpdateStatus::Available(release) => (**release).clone(),
            _ => return,
        }
    };

    // Fake update mode: simulate download progress and install
    if is_fake_update() {
        let total_size = release.assets.first().map(|a| a.size).unwrap_or(50 * 1024 * 1024);
        cx.spawn(async move |cx| {
            info!("[FAKE] Simulating download...");
            let _ = state_entity.update(cx, |state, cx| {
                state.status = UpdateStatus::Downloading {
                    downloaded: 0,
                    total: total_size,
                };
                cx.notify();
            });

            // Simulate download progress over ~2 seconds
            let steps = 20u64;
            let chunk = total_size / steps;
            for i in 1..=steps {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(100))
                    .await;
                let downloaded = (chunk * i).min(total_size);
                let _ = state_entity.update(cx, |state, cx| {
                    state.status = UpdateStatus::Downloading {
                        downloaded,
                        total: total_size,
                    };
                    cx.notify();
                });
            }

            // Simulate installing
            info!("[FAKE] Simulating install...");
            let _ = state_entity.update(cx, |state, cx| {
                state.status = UpdateStatus::Installing;
                cx.notify();
            });
            cx.background_executor()
                .timer(std::time::Duration::from_secs(2))
                .await;

            // Done
            info!("[FAKE] Simulating install complete");
            let _ = state_entity.update(cx, |state, cx| {
                state.status = UpdateStatus::Installed;
                cx.notify();
            });
        })
        .detach();
        return;
    }

    let Some(asset) = get_platform_asset(&release.assets).cloned() else {
        cx.spawn(async move |cx| {
            let _ = state_entity.update(cx, |state, cx| {
                state.status = UpdateStatus::Error("No compatible download found for this platform".to_string());
                cx.notify();
            });
        })
        .detach();
        return;
    };

    let url = asset.download_url.clone();
    let file_name = asset.name.clone();
    let total_size = asset.size;

    cx.spawn(async move |cx| {
        // Set downloading status
        let _ = state_entity.update(cx, |state, cx| {
            state.status = UpdateStatus::Downloading {
                downloaded: 0,
                total: total_size,
            };
            cx.notify();
        });

        // Prepare download directory
        let download_dir = std::env::temp_dir().join("zedis-update");
        if let Err(e) = std::fs::create_dir_all(&download_dir) {
            let msg = format!("Failed to create download directory: {}", e);
            let _ = state_entity.update(cx, |state, cx| {
                state.status = UpdateStatus::Error(msg);
                cx.notify();
            });
            return;
        }
        let download_path = download_dir.join(&file_name);

        // Set up progress channel
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let download_path_clone = download_path.clone();

        // Start download on background thread
        std::thread::spawn(move || {
            let result = download_file(&url, &download_path_clone, tx.clone());
            match result {
                Ok(written) => {
                    let _ = tx.unbounded_send(DownloadMsg::Complete { written });
                }
                Err(e) => {
                    let _ = tx.unbounded_send(DownloadMsg::Error(e.to_string()));
                }
            }
        });

        // Monitor progress
        use futures::StreamExt;
        let mut last_percent: u32 = 0;
        while let Some(msg) = rx.next().await {
            match msg {
                DownloadMsg::Progress { downloaded } => {
                    let percent = if total_size > 0 {
                        (downloaded as f64 / total_size as f64 * 100.0) as u32
                    } else {
                        0
                    };
                    // Only notify UI when integer percentage changes
                    if percent != last_percent {
                        last_percent = percent;
                        let _ = state_entity.update(cx, |state, cx| {
                            state.status = UpdateStatus::Downloading {
                                downloaded,
                                total: total_size,
                            };
                            cx.notify();
                        });
                    }
                }
                DownloadMsg::Complete { written } => {
                    info!(path = ?download_path, "Download complete");
                    if total_size > 0 && written != total_size {
                        let msg = format!(
                            "Download size mismatch: expected {} bytes, got {} bytes",
                            total_size, written
                        );
                        error!("{}", msg);
                        let _ = std::fs::remove_file(&download_path);
                        let _ = state_entity.update(cx, |state, cx| {
                            state.status = UpdateStatus::Error(msg);
                            cx.notify();
                        });
                        break;
                    }
                    // Auto-start install
                    let _ = state_entity.update(cx, |state, cx| {
                        state.status = UpdateStatus::Installing;
                        cx.notify();
                    });

                    let install_path = download_path.clone();
                    let (install_tx, install_rx) = futures::channel::oneshot::channel();
                    std::thread::spawn(move || {
                        let _ = install_tx.send(crate::helpers::install_update(&install_path));
                    });
                    let install_result = install_rx.await.unwrap_or_else(|_| {
                        Err(Error::Update {
                            message: "Install thread panicked".to_string(),
                        })
                    });

                    match install_result {
                        Ok(()) => {
                            info!("Update installed successfully");
                            let _ = state_entity.update(cx, |state, cx| {
                                state.status = UpdateStatus::Installed;
                                cx.notify();
                            });
                        }
                        Err(e) => {
                            let msg = format!("Installation failed: {}", e);
                            error!("{}", msg);
                            let _ = state_entity.update(cx, |state, cx| {
                                state.status = UpdateStatus::Error(msg);
                                cx.notify();
                            });
                        }
                    }
                    break;
                }
                DownloadMsg::Error(e) => {
                    error!(error = %e, "Download failed");
                    // Clean up partial download
                    let _ = std::fs::remove_file(&download_path);
                    let _ = state_entity.update(cx, |state, cx| {
                        state.status = UpdateStatus::Error(e);
                        cx.notify();
                    });
                    break;
                }
            }
        }
    })
    .detach();
}

/// Reset update status to Idle (e.g., when user closes the dialog).
pub fn reset_status(cx: &App) {
    let store = cx.global::<ZedisUpdateStore>().clone();
    let state_entity = store.state();
    cx.spawn(async move |cx| {
        let _ = state_entity.update(cx, |state, cx| {
            if matches!(
                state.status,
                UpdateStatus::Downloading { .. } | UpdateStatus::Installing
            ) {
                return;
            }
            state.status = UpdateStatus::Idle;
            state.dialog_window = None;
            state.send_outcome(AutoCheckOutcome::Dismissed);
            cx.notify();
        });
    })
    .detach();
}

/// Skip the current available version.
pub fn skip_version(cx: &App) {
    let store = cx.global::<ZedisUpdateStore>().clone();
    let tag = {
        let state = store.state().read(cx);
        match &state.status {
            UpdateStatus::Available(release) => release.tag_name.clone(),
            _ => return,
        }
    };
    super::update_app_state_and_save(cx, "skip_version", move |app_state, _cx| {
        app_state.set_skipped_version(Some(tag.clone()));
    });
}

// --- Auto-check scheduler ---

const AUTO_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(12 * 60 * 60);
const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Start the auto-update check scheduler. Checks immediately on start,
/// then every 12 hours. Retries every 5 minutes on failure. Resets timer on
/// manual check or dialog dismiss.
pub fn start_auto_update_scheduler(cx: &App) {
    let store = cx.global::<ZedisUpdateStore>().clone();
    let state_entity = store.state();
    let (tx, mut rx) = futures::channel::mpsc::unbounded();

    cx.spawn(async move |cx| {
        use futures::{FutureExt, StreamExt};

        // Install the sender into state
        let _ = state_entity.update(cx, |state, _cx| {
            state.outcome_tx = Some(tx);
        });

        loop {
            // Drain stale signals from manual checks
            while rx.try_next().is_ok_and(|v| v.is_some()) {}

            // Trigger auto-check
            cx.update(|cx| check_for_updates(false, cx)).ok();

            // Wait for outcome
            let Some(outcome) = rx.next().await else {
                break;
            };

            let mut wait_duration = match outcome {
                AutoCheckOutcome::UpToDate | AutoCheckOutcome::Skipped => AUTO_CHECK_INTERVAL,
                AutoCheckOutcome::Failed => RETRY_INTERVAL,
                AutoCheckOutcome::UpdateAvailable => {
                    // Wait for user to dismiss dialog or manual check to reset
                    loop {
                        let Some(next) = rx.next().await else {
                            return;
                        };
                        if matches!(next, AutoCheckOutcome::Dismissed | AutoCheckOutcome::TimerReset) {
                            break;
                        }
                    }
                    AUTO_CHECK_INTERVAL
                }
                AutoCheckOutcome::Dismissed | AutoCheckOutcome::TimerReset => AUTO_CHECK_INTERVAL,
            };

            // Wait for delay, but also listen for timer reset signals
            loop {
                futures::select! {
                    signal = rx.next() => {
                        match signal {
                            Some(AutoCheckOutcome::TimerReset | AutoCheckOutcome::Dismissed) => {
                                wait_duration = AUTO_CHECK_INTERVAL;
                                continue; // Restart timer
                            }
                            Some(_) => continue,
                            None => return,
                        }
                    }
                    _ = cx.background_executor().timer(wait_duration).fuse() => {
                        break; // Timer expired, proceed to next check
                    }
                }
            }
        }
    })
    .detach();
}

/// Restart the application after update.
pub fn restart_app(cx: &mut App) {
    #[cfg(target_os = "macos")]
    {
        if let Some(app_bundle) = crate::helpers::get_app_bundle_path() {
            let _ = std::process::Command::new("open").arg("-n").arg(app_bundle).spawn();
        }
        cx.quit();
    }
    #[cfg(not(target_os = "macos"))]
    {
        // On Windows/Linux, just quit. User will restart manually.
        cx.quit();
    }
}
