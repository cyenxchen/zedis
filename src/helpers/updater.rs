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
use std::path::Path;
use std::process::Command;
use tracing::{debug, info};

type Result<T, E = Error> = std::result::Result<T, E>;

/// Install a downloaded update. Delegates to platform-specific logic.
pub fn install_update(downloaded_path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        install_macos(downloaded_path)
    }
    #[cfg(target_os = "windows")]
    {
        install_windows(downloaded_path)
    }
    #[cfg(target_os = "linux")]
    {
        install_linux(downloaded_path)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Err(Error::Update {
            message: "Unsupported platform for auto-update".to_string(),
        })
    }
}

/// macOS: Mount DMG, rsync .app bundle, detach DMG.
#[cfg(target_os = "macos")]
fn install_macos(dmg_path: &Path) -> Result<()> {
    let app_bundle = super::get_app_bundle_path().ok_or_else(|| Error::Update {
        message: "Cannot determine app bundle path".to_string(),
    })?;

    info!(bundle = ?app_bundle, "Installing update to app bundle");

    let mount_point = std::env::temp_dir().join("zedis-update-mount");
    // Clean up any leftover mount
    if mount_point.exists() {
        let _ = Command::new("hdiutil")
            .args(["detach", "-force"])
            .arg(&mount_point)
            .output();
    }

    // Mount DMG
    let output = Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-mountpoint"])
        .arg(&mount_point)
        .arg(dmg_path)
        .output()?;
    if !output.status.success() {
        return Err(Error::Update {
            message: format!("Failed to mount DMG: {}", String::from_utf8_lossy(&output.stderr)),
        });
    }
    debug!("DMG mounted at {:?}", mount_point);

    // rsync the .app bundle
    let source = mount_point.join("Zedis.app");
    if !source.exists() {
        let _ = Command::new("hdiutil")
            .args(["detach", "-force"])
            .arg(&mount_point)
            .output();
        return Err(Error::Update {
            message: "Zedis.app not found in DMG".to_string(),
        });
    }

    let output = Command::new("rsync")
        .args(["-a", "--delete"])
        .arg(format!("{}/", source.display()))
        .arg(format!("{}/", app_bundle.display()))
        .output()?;
    if !output.status.success() {
        let _ = Command::new("hdiutil")
            .args(["detach", "-force"])
            .arg(&mount_point)
            .output();
        return Err(Error::Update {
            message: format!("Failed to copy app bundle: {}", String::from_utf8_lossy(&output.stderr)),
        });
    }
    info!("App bundle updated via rsync");

    // Detach DMG
    let _ = Command::new("hdiutil")
        .args(["detach", "-force"])
        .arg(&mount_point)
        .output();

    // Clean up DMG file
    let _ = std::fs::remove_file(dmg_path);

    Ok(())
}

/// Windows: Unzip, rename current exe to .old, move new exe in place.
#[cfg(target_os = "windows")]
fn install_windows(zip_path: &Path) -> Result<()> {
    let extract_dir = std::env::temp_dir().join("zedis-update-extract");
    let _ = std::fs::remove_dir_all(&extract_dir);
    std::fs::create_dir_all(&extract_dir)?;

    // Use PowerShell to extract
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
                zip_path.display(),
                extract_dir.display()
            ),
        ])
        .output()?;
    if !output.status.success() {
        return Err(Error::Update {
            message: format!("Failed to extract zip: {}", String::from_utf8_lossy(&output.stderr)),
        });
    }

    let exe = std::env::current_exe()?;
    let new_exe = extract_dir.join("zedis.exe");
    if !new_exe.exists() {
        return Err(Error::Update {
            message: "zedis.exe not found in archive".to_string(),
        });
    }

    // Rename current exe to .old
    let old_exe = exe.with_extension("exe.old");
    let _ = std::fs::remove_file(&old_exe);
    std::fs::rename(&exe, &old_exe)?;

    // Move new exe in place
    std::fs::copy(&new_exe, &exe)?;

    // Clean up
    let _ = std::fs::remove_dir_all(&extract_dir);
    let _ = std::fs::remove_file(zip_path);

    Ok(())
}

/// Linux: Extract tar.gz and replace current binary.
#[cfg(target_os = "linux")]
fn install_linux(tar_path: &Path) -> Result<()> {
    let extract_dir = std::env::temp_dir().join("zedis-update-extract");
    let _ = std::fs::remove_dir_all(&extract_dir);
    std::fs::create_dir_all(&extract_dir)?;

    let output = Command::new("tar")
        .args(["xzf"])
        .arg(tar_path)
        .arg("-C")
        .arg(&extract_dir)
        .output()?;
    if !output.status.success() {
        return Err(Error::Update {
            message: format!("Failed to extract tar.gz: {}", String::from_utf8_lossy(&output.stderr)),
        });
    }

    let exe = std::env::current_exe()?;
    let new_exe = extract_dir.join("zedis");
    if !new_exe.exists() {
        return Err(Error::Update {
            message: "zedis binary not found in archive".to_string(),
        });
    }

    // Replace binary
    std::fs::copy(&new_exe, &exe)?;

    // Clean up
    let _ = std::fs::remove_dir_all(&extract_dir);
    let _ = std::fs::remove_file(tar_path);

    Ok(())
}
