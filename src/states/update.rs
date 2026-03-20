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

// --- Progress message for download channel ---

enum DownloadMsg {
    Progress { downloaded: u64 },
    Complete,
    Error(String),
}

// --- Global update state ---

#[derive(Default)]
pub struct ZedisUpdateState {
    pub status: UpdateStatus,
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

fn get_platform_asset(assets: &[ReleaseAsset]) -> Option<&ReleaseAsset> {
    let target_name = if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") {
        "Zedis-aarch64.dmg"
    } else if cfg!(target_os = "windows") {
        "zedis-windows.exe.zip"
    } else if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
        "zedis-linux-x86_64.tar.gz"
    } else {
        return None;
    };
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
    let resp: GitHubRelease = client.get(GITHUB_API_URL).send()?.json()?;
    let mut release = parse_release(resp)?;

    // If body is empty, fetch commit messages between current and new version
    if release.body.is_empty() {
        if let Ok(notes) = fetch_compare_notes(&client, &release.tag_name) {
            release.body = notes;
        }
    }
    Ok(release)
}

/// Fetch commit messages between current version and the new tag via GitHub compare API.
fn fetch_compare_notes(client: &reqwest::blocking::Client, new_tag: &str) -> Result<String> {
    let current_tag = format!("v{}", CURRENT_VERSION);
    let url = format!("{}/{}...{}", GITHUB_COMPARE_URL, current_tag, new_tag);
    let resp: serde_json::Value = client.get(&url).send()?.json()?;
    let commits = resp["commits"]
        .as_array()
        .ok_or_else(|| Error::Update {
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
) -> Result<()> {
    use std::io::{Read, Write};
    let client = reqwest::blocking::Client::builder()
        .user_agent("Zedis")
        .timeout(std::time::Duration::from_secs(600))
        .build()?;
    let mut resp = client.get(url).send()?;
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
    Ok(())
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
            return;
        }
    }

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

        // Fetch release on background thread
        let result = std::thread::spawn(fetch_latest_release)
            .join()
            .map_err(|_| Error::Update {
                message: "Failed to check for updates".to_string(),
            });

        let result = match result {
            Ok(Ok(release)) => Ok(release),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(e),
        };

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
                        cx.notify();
                    });
                    // Open update dialog
                    cx.update(|cx| {
                        crate::views::open_update_dialog(cx);
                    })
                    .ok();
                } else {
                    info!("Already up to date ({})", current);
                    let _ = state_entity.update(cx, |state, cx| {
                        state.status = UpdateStatus::UpToDate;
                        cx.notify();
                    });
                    if manual {
                        cx.update(|cx| {
                            crate::views::open_update_dialog(cx);
                        })
                        .ok();
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "Failed to check for updates");
                let msg = e.to_string();
                let _ = state_entity.update(cx, |state, cx| {
                    state.status = UpdateStatus::Error(msg);
                    cx.notify();
                });
                if manual {
                    cx.update(|cx| {
                        crate::views::open_update_dialog(cx);
                    })
                    .ok();
                }
            }
        }

        // Save last check time
        cx.update(|cx| {
            super::update_app_state_and_save(cx, "save_last_update_check", |state, _cx| {
                state.set_last_update_check(chrono::Utc::now().to_rfc3339());
            });
        })
        .ok();
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
                Ok(()) => {
                    let _ = tx.unbounded_send(DownloadMsg::Complete);
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
                DownloadMsg::Complete => {
                    info!(path = ?download_path, "Download complete");
                    // Auto-start install
                    let _ = state_entity.update(cx, |state, cx| {
                        state.status = UpdateStatus::Installing;
                        cx.notify();
                    });

                    let install_path = download_path.clone();
                    let install_result = std::thread::spawn(move || crate::helpers::install_update(&install_path))
                        .join()
                        .map_err(|_| Error::Update {
                            message: "Install thread panicked".to_string(),
                        });

                    match install_result {
                        Ok(Ok(())) => {
                            info!("Update installed successfully");
                            let _ = state_entity.update(cx, |state, cx| {
                                state.status = UpdateStatus::Installed;
                                cx.notify();
                            });
                        }
                        Ok(Err(e)) | Err(e) => {
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
