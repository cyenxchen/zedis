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
    connection::{KeyBackupProgress, get_connection_manager},
    states::{NotificationAction, ServerEvent, ServerTask, ZedisServerState},
};
use futures::{StreamExt, channel::mpsc::UnboundedReceiver};
use gpui::prelude::*;
use humansize::{DECIMAL, format_size};
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use tracing::{info, warn};

fn key_backup_directory(path: &Path) -> Option<&Path> {
    path.parent().filter(|dir| !dir.as_os_str().is_empty())
}

fn reveal_backup_directory(file_path: &str) {
    let path = Path::new(file_path);
    let dir = key_backup_directory(path)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let result = if cfg!(target_os = "macos") {
        Command::new("open").arg(&dir).spawn()
    } else if cfg!(target_os = "windows") {
        Command::new("explorer.exe").arg(&dir).spawn()
    } else {
        Command::new("xdg-open").arg(&dir).spawn()
    };

    match result {
        Ok(_) => info!(
            path = %path.display(),
            dir = %dir.display(),
            "opened key backup directory"
        ),
        Err(err) => warn!(
            path = %path.display(),
            dir = %dir.display(),
            error = %err,
            "failed to open key backup directory"
        ),
    }
}

impl ZedisServerState {
    fn watch_key_backup_progress(&self, mut rx: UnboundedReceiver<KeyBackupProgress>, cx: &mut Context<Self>) {
        cx.spawn(async move |handle, cx| {
            while let Some(progress) = rx.next().await {
                let _ = handle.update(cx, |_, cx| {
                    cx.emit(ServerEvent::KeyBackupProgress(progress));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Exports a key-level backup for the current connection.
    pub fn export_key_backup(&mut self, file_path: String, cx: &mut Context<Self>) {
        let server_id = self.server_id.clone();
        let db = self.db;
        let preset_credentials = self.preset_credentials.clone();
        let (progress_tx, progress_rx) = futures::channel::mpsc::unbounded();
        self.watch_key_backup_progress(progress_rx, cx);

        self.spawn(
            ServerTask::ExportKeyBackup,
            move || async move {
                info!(server_id = %server_id, db, path = %file_path, "export key backup task started");
                get_connection_manager()
                    .export_key_backup(&server_id, db, preset_credentials, &file_path, Some(progress_tx))
                    .await
            },
            move |_this, result, cx| {
                if let Ok(summary) = result {
                    let size = format_size(summary.bytes, DECIMAL);
                    let message = format!(
                        "Key backup exported: {} ({} keys, {})",
                        summary.file_path, summary.key_count, size
                    );
                    cx.emit(ServerEvent::KeyBackupExported(summary.key_count, summary.bytes));
                    cx.emit(ServerEvent::Notification(NotificationAction::new_success(
                        message.into(),
                    )));
                    reveal_backup_directory(&summary.file_path);
                }
                cx.notify();
            },
            cx,
        );
    }

    /// Restores a key-level backup to the current connection.
    pub fn restore_key_backup(&mut self, file_path: String, cx: &mut Context<Self>) {
        let server_id = self.server_id.clone();
        let db = self.db;
        let preset_credentials = self.preset_credentials.clone();
        let (progress_tx, progress_rx) = futures::channel::mpsc::unbounded();
        self.watch_key_backup_progress(progress_rx, cx);

        self.spawn(
            ServerTask::RestoreKeyBackup,
            move || async move {
                info!(server_id = %server_id, db, path = %file_path, "restore key backup task started");
                get_connection_manager()
                    .restore_key_backup(&server_id, db, preset_credentials, &file_path, Some(progress_tx))
                    .await
            },
            move |this, result, cx| {
                if let Ok(summary) = result {
                    let message = if summary.failed_count == 0 {
                        format!(
                            "Key backup restored: {} ({} keys)",
                            summary.file_path, summary.restored_count
                        )
                    } else {
                        format!(
                            "Key backup restored with failures: {} ({} restored, {} failed)",
                            summary.file_path, summary.restored_count, summary.failed_count
                        )
                    };
                    cx.emit(ServerEvent::KeyBackupRestored(
                        summary.restored_count,
                        summary.failed_count,
                    ));
                    if summary.failed_count == 0 {
                        cx.emit(ServerEvent::Notification(NotificationAction::new_success(
                            message.into(),
                        )));
                    } else {
                        warn!(
                            path = %summary.file_path,
                            restored_count = summary.restored_count,
                            failed_count = summary.failed_count,
                            "key backup restore completed with failures"
                        );
                        cx.emit(ServerEvent::Notification(NotificationAction::new_warning(
                            message.into(),
                        )));
                    }
                    let keyword = this.keyword.clone();
                    this.scan(keyword, cx);
                }
                cx.notify();
            },
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_backup_directory_uses_parent_directory() {
        let dir = PathBuf::from("Downloads").join("Zedis Backups");
        let path = dir.join("redis.zedis-backup.jsonl");

        assert_eq!(key_backup_directory(&path), Some(dir.as_path()));
    }

    #[test]
    fn key_backup_directory_ignores_empty_relative_parent() {
        assert_eq!(key_backup_directory(Path::new("redis.zedis-backup.jsonl")), None);
    }
}
