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

//! Edit session state for value editing dialog.
//!
//! This module provides:
//! - Session state management for editing Redis values
//! - Format detection and conversion
//! - Compression handling
//! - Validation and error tracking

use crate::error::Error;
use crate::helpers::codec::{
    CompressionFormat, ContentFormat, EditFormat, MAX_DECOMPRESS_BYTES, compress, decode_to_text, decompress, detect,
    encode_from_text, suggest_edit_format, validate_format,
};
use bytes::Bytes;
use gpui::SharedString;

type Result<T, E = Error> = std::result::Result<T, E>;

/// Status of the edit session
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[allow(dead_code)]
pub enum EditStatus {
    #[default]
    Idle,
    Loading,
    Saving,
}

/// Edit session state for value editing
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct EditSession {
    // Original data
    /// The Redis key being edited
    pub key: SharedString,
    /// Original raw bytes from Redis
    pub original_bytes: Bytes,
    /// Original size in bytes
    pub original_len: usize,

    // Detection results
    /// Detected compression format
    pub compression: CompressionFormat,
    /// Detected content format
    pub content: ContentFormat,
    /// MIME type (if detected)
    pub mime: Option<SharedString>,
    /// Whether the text is truncated (preview mode)
    pub is_preview: bool,

    // Current edit state
    /// Current edit format (how user sees/edits the data)
    pub editor_format: EditFormat,
    /// Current text in the editor
    pub editor_text: SharedString,
    /// Decompressed bytes (working copy)
    pub working_bytes: Vec<u8>,
    /// Selected compression for saving
    pub save_compression: CompressionFormat,

    // State flags
    /// Whether the value has been modified
    pub dirty: bool,
    /// Whether the current text is valid for the format
    pub valid: bool,
    /// Validation error message (if any)
    pub error: Option<String>,
    /// Current status
    pub status: EditStatus,

    // Limits
    /// Maximum bytes to decompress (prevents compression bombs)
    pub max_decompress_bytes: usize,
}

impl Default for EditSession {
    fn default() -> Self {
        Self {
            key: SharedString::default(),
            original_bytes: Bytes::new(),
            original_len: 0,
            compression: CompressionFormat::None,
            content: ContentFormat::Text,
            mime: None,
            is_preview: false,
            editor_format: EditFormat::Text,
            editor_text: SharedString::default(),
            working_bytes: Vec::new(),
            save_compression: CompressionFormat::None,
            dirty: false,
            valid: true,
            error: None,
            status: EditStatus::Idle,
            max_decompress_bytes: MAX_DECOMPRESS_BYTES,
        }
    }
}

impl EditSession {
    /// Create a new edit session from raw bytes
    pub fn new(key: SharedString, raw: Bytes) -> Self {
        let original_len = raw.len();
        Self {
            key,
            original_bytes: raw,
            original_len,
            ..Default::default()
        }
    }

    /// Detect format and initialize the session
    ///
    /// This should be called after creating the session to:
    /// 1. Detect compression format
    /// 2. Decompress if needed
    /// 3. Detect content format
    /// 4. Generate initial editor text
    pub fn detect_and_init(&mut self) -> Result<()> {
        self.status = EditStatus::Loading;

        // Detect compression and content format
        let detection = detect(&self.original_bytes);
        self.compression = detection.compression;
        self.content = detection.content;
        self.mime = detection.mime;
        self.save_compression = self.compression; // Default to keeping original compression

        // Decompress if needed
        self.working_bytes = if self.compression != CompressionFormat::None {
            decompress(&self.original_bytes, self.compression, self.max_decompress_bytes)?
        } else {
            self.original_bytes.to_vec()
        };

        // Suggest the best edit format based on content
        self.editor_format = suggest_edit_format(self.content, detection.is_utf8);

        // Generate editor text (allow fallback during initialization)
        self.refresh_editor_text(true)?;

        self.status = EditStatus::Idle;
        self.dirty = false;
        self.valid = true;
        self.error = None;

        Ok(())
    }

    /// Refresh the editor text from working bytes
    ///
    /// # Arguments
    /// * `allow_fallback` - If true, allows automatic fallback to Hex format on decode failure.
    ///   Use true for initialization, false for user-initiated format switches.
    fn refresh_editor_text(&mut self, allow_fallback: bool) -> Result<()> {
        match decode_to_text(&self.working_bytes, self.editor_format) {
            Ok(text) => {
                self.editor_text = text.into();
                self.valid = true;
                self.error = None;
                Ok(())
            }
            Err(e) => {
                if allow_fallback {
                    // During initialization, fall back to hex
                    self.editor_format = EditFormat::Hex;
                    let text = decode_to_text(&self.working_bytes, EditFormat::Hex)?;
                    self.editor_text = text.into();
                    self.valid = true;
                    self.error = Some(format!("Switched to Hex: {}", e));
                    Ok(())
                } else {
                    // User-initiated format switch: don't fallback, return error
                    self.valid = false;
                    self.error = Some(e.to_string());
                    Err(e)
                }
            }
        }
    }

    /// Change the editor format
    ///
    /// This will convert the current text to the new format.
    /// Returns error if conversion fails (format is rolled back on failure).
    pub fn set_editor_format(&mut self, fmt: EditFormat) -> Result<()> {
        if fmt == self.editor_format {
            return Ok(());
        }

        let old_format = self.editor_format;

        // Special handling: JSON ↔ MessagePack conversion
        // Both formats use JSON text as editor_text, so we can convert at the value level
        if (old_format == EditFormat::Json && fmt == EditFormat::MessagePack)
            || (old_format == EditFormat::MessagePack && fmt == EditFormat::Json)
        {
            // Validate that current text is valid JSON
            let value: serde_json::Value = serde_json::from_str(&self.editor_text).map_err(|e| Error::Invalid {
                message: format!("Invalid JSON: {}", e),
            })?;

            // Re-format for consistency
            self.editor_text = serde_json::to_string_pretty(&value)
                .map_err(|e| Error::Invalid { message: e.to_string() })?
                .into();

            // Update working_bytes to match the new format
            self.working_bytes = encode_from_text(&self.editor_text, fmt)?;
            self.editor_format = fmt;
            self.dirty = true;
            self.valid = true;
            self.error = None;
            return Ok(());
        }

        // Special handling: switching TO MessagePack from other formats (except Json, handled above)
        // This prevents data loss when switching through intermediate formats like Hex
        // Strategy: Always try to parse bytes as JSON first, then convert to MessagePack.
        // This is because in most cases, users want to "convert my data to MessagePack",
        // not "interpret these bytes as MessagePack".
        if fmt == EditFormat::MessagePack {
            // First sync working_bytes from current editor content
            let bytes = encode_from_text(&self.editor_text, self.editor_format)?;

            // Try to parse bytes as JSON (either binary JSON or UTF-8 string JSON)
            // and convert to MessagePack. Do NOT try to detect if it's already MessagePack
            // because MessagePack can parse almost any byte sequence (e.g., '{' = 0x7b = 123).
            let parse_result = serde_json::from_slice::<serde_json::Value>(&bytes).or_else(
                |_| match std::str::from_utf8(&bytes) {
                    Ok(text) => serde_json::from_str(text),
                    Err(e) => Err(serde_json::Error::io(std::io::Error::other(e))),
                },
            );

            let value = parse_result.map_err(|e| Error::Invalid {
                message: format!("Cannot convert to MessagePack: {}", e),
            })?;

            self.working_bytes = rmp_serde::to_vec(&value).map_err(|e| Error::Invalid {
                message: e.to_string(),
            })?;

            self.editor_format = fmt;
            self.refresh_editor_text(false)?;
            self.dirty = true;
            return Ok(());
        }

        // Special handling: switching FROM MessagePack to other formats (except Json, handled above)
        // working_bytes is MessagePack, convert to JSON bytes first as universal intermediate format
        if old_format == EditFormat::MessagePack {
            // working_bytes is MessagePack, convert to JSON bytes first
            let value: serde_json::Value =
                rmp_serde::from_slice(&self.working_bytes).map_err(|e| Error::Invalid {
                    message: e.to_string(),
                })?;

            self.working_bytes =
                serde_json::to_vec(&value).map_err(|e| Error::Invalid { message: e.to_string() })?;

            self.editor_format = fmt;
            self.refresh_editor_text(false)?;
            self.dirty = true;
            return Ok(());
        }

        // Other format switches use byte-level conversion
        let bytes = encode_from_text(&self.editor_text, self.editor_format)?;

        // Save old state for rollback (including working_bytes!)
        let old_working_bytes = std::mem::replace(&mut self.working_bytes, bytes);
        let old_valid = self.valid;
        let old_error = self.error.clone();
        let old_text = self.editor_text.clone();
        self.editor_format = fmt;

        // Try to convert bytes to new format (no fallback allowed)
        if let Err(e) = self.refresh_editor_text(false) {
            // Rollback ALL state on failure
            self.working_bytes = old_working_bytes;
            self.editor_format = old_format;
            self.editor_text = old_text;
            self.valid = old_valid;
            self.error = old_error;
            return Err(e);
        }

        self.dirty = true;
        Ok(())
    }

    /// Update the editor text
    ///
    /// Validates the text and updates dirty/valid flags.
    pub fn set_editor_text(&mut self, text: SharedString) {
        if text == self.editor_text {
            return;
        }

        self.editor_text = text;
        self.dirty = true;

        // Validate the text
        match validate_format(&self.editor_text, self.editor_format) {
            Ok(()) => {
                self.valid = true;
                self.error = None;
            }
            Err(e) => {
                self.valid = false;
                self.error = Some(e.to_string());
            }
        }
    }

    /// Set the compression format for saving
    pub fn set_save_compression(&mut self, compression: CompressionFormat) {
        if compression != self.save_compression {
            self.save_compression = compression;
            self.dirty = true;
        }
    }

    /// Validate current editor text
    #[allow(dead_code)]
    pub fn validate(&mut self) -> bool {
        match validate_format(&self.editor_text, self.editor_format) {
            Ok(()) => {
                self.valid = true;
                self.error = None;
                true
            }
            Err(e) => {
                self.valid = false;
                self.error = Some(e.to_string());
                false
            }
        }
    }

    /// Check if the session can be saved
    #[allow(dead_code)]
    pub fn can_save(&self) -> bool {
        self.valid && self.dirty && !self.is_preview && self.status == EditStatus::Idle
    }

    /// Build the final bytes for saving
    ///
    /// This will:
    /// 1. Convert editor text back to bytes
    /// 2. Apply compression if selected
    pub fn build_save_bytes(&mut self) -> Result<Vec<u8>> {
        if !self.valid {
            return Err(Error::Invalid {
                message: self.error.clone().unwrap_or_else(|| "Invalid data".to_string()),
            });
        }

        // Convert text to bytes
        let raw_bytes = encode_from_text(&self.editor_text, self.editor_format)?;

        // Apply compression
        let final_bytes = compress(&raw_bytes, self.save_compression)?;

        Ok(final_bytes)
    }

    /// Get available edit formats based on content
    #[allow(dead_code)]
    pub fn available_edit_formats(&self) -> Vec<EditFormat> {
        let mut formats = vec![EditFormat::Text, EditFormat::Hex];

        // Add JSON if content is JSON or can be parsed as JSON
        if self.content == ContentFormat::Json || serde_json::from_str::<serde_json::Value>(&self.editor_text).is_ok() {
            formats.insert(1, EditFormat::Json);
        }

        // Add MessagePack if content is MessagePack
        if self.content == ContentFormat::MessagePack {
            formats.push(EditFormat::MessagePack);
        }

        formats
    }

    /// Get available compression formats
    #[allow(dead_code)]
    pub fn available_compression_formats(&self) -> &'static [CompressionFormat] {
        CompressionFormat::all()
    }

    /// Check if format switch would lose data
    #[allow(dead_code)]
    pub fn would_lose_data(&self, target_format: EditFormat) -> bool {
        // Switching from binary formats to text might lose data
        matches!(
            (self.editor_format, target_format),
            (EditFormat::Hex, EditFormat::Text) | (EditFormat::MessagePack, EditFormat::Text)
        )
    }

    /// Get display info for current state
    #[allow(dead_code)]
    pub fn status_text(&self) -> &'static str {
        match self.status {
            EditStatus::Idle => "",
            EditStatus::Loading => "Loading...",
            EditStatus::Saving => "Saving...",
        }
    }

    /// Get size info string
    #[allow(dead_code)]
    pub fn size_info(&self) -> String {
        let working_size = self.working_bytes.len();
        if self.compression != CompressionFormat::None {
            format!(
                "{} bytes (compressed: {} bytes, {})",
                working_size,
                self.original_len,
                self.compression.as_str()
            )
        } else {
            format!("{} bytes", self.original_len)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_session() {
        let session = EditSession::new("test:key".into(), Bytes::from("hello world"));
        assert_eq!(session.key.as_ref(), "test:key");
        assert_eq!(session.original_len, 11);
        assert!(!session.dirty);
    }

    #[test]
    fn test_detect_and_init_text() {
        // Note: Use "plain text" instead of "hello world" to avoid LZ4 magic number detection
        let mut session = EditSession::new("test:key".into(), Bytes::from("plain text here"));
        session.detect_and_init().expect("init failed");

        assert_eq!(session.compression, CompressionFormat::None);
        assert_eq!(session.content, ContentFormat::Text);
        assert_eq!(session.editor_format, EditFormat::Text);
        assert_eq!(session.editor_text.as_ref(), "plain text here");
        assert!(session.valid);
        assert!(!session.dirty);
    }

    #[test]
    fn test_detect_and_init_json() {
        let json = r#"{"name": "test", "value": 123}"#;
        let mut session = EditSession::new("test:key".into(), Bytes::from(json));
        session.detect_and_init().expect("init failed");

        assert_eq!(session.compression, CompressionFormat::None);
        assert_eq!(session.content, ContentFormat::Json);
        assert_eq!(session.editor_format, EditFormat::Json);
        assert!(session.valid);
    }

    #[test]
    fn test_set_editor_text_validation() {
        let mut session = EditSession::new("test:key".into(), Bytes::from("{}"));
        session.detect_and_init().expect("init failed");
        session.editor_format = EditFormat::Json;

        // Valid JSON
        session.set_editor_text(r#"{"key": "value"}"#.into());
        assert!(session.valid);
        assert!(session.dirty);

        // Invalid JSON
        session.set_editor_text("{invalid}".into());
        assert!(!session.valid);
        assert!(session.error.is_some());
    }

    #[test]
    fn test_format_switch() {
        let mut session = EditSession::new("test:key".into(), Bytes::from("hello"));
        session.detect_and_init().expect("init failed");

        // Switch to hex
        session
            .set_editor_format(EditFormat::Hex)
            .expect("format switch failed");
        assert_eq!(session.editor_format, EditFormat::Hex);
        assert_eq!(session.editor_text.as_ref(), "68 65 6c 6c 6f");
        assert!(session.dirty);
    }

    #[test]
    fn test_format_switch_rollback_on_failure() {
        // Test that format switch rolls back on failure without auto-fallback to Hex
        let mut session = EditSession::new("test:key".into(), Bytes::from("hello"));
        session.detect_and_init().expect("init failed");

        // Should start as Text format
        assert_eq!(session.editor_format, EditFormat::Text);
        assert!(session.valid);
        assert!(session.error.is_none());

        // Try to switch to JSON (should fail because "hello" is not valid JSON)
        let result = session.set_editor_format(EditFormat::Json);
        assert!(result.is_err());

        // Format should be rolled back to Text, NOT auto-fallback to Hex
        assert_eq!(session.editor_format, EditFormat::Text);
        assert!(session.valid);
        assert!(session.error.is_none());
        assert_eq!(session.editor_text.as_ref(), "hello");
    }

    #[test]
    fn test_format_switch_working_bytes_rollback() {
        // Test that working_bytes is properly rolled back when format switch fails
        // Use Text → JSON switch which should fail for non-JSON text
        // Note: Use "plain text" instead of "hello world" to avoid LZ4 magic number detection

        let mut session = EditSession::new("test:key".into(), Bytes::from("plain text here"));
        session.detect_and_init().expect("init failed");

        // Should start as Text format
        assert_eq!(session.editor_format, EditFormat::Text);
        assert_eq!(session.editor_text.as_ref(), "plain text here");

        // Save original state for comparison
        let original_working_bytes = session.working_bytes.clone();
        let original_editor_text = session.editor_text.clone();

        // Try to switch to JSON (should fail because "plain text here" is not valid JSON)
        let result = session.set_editor_format(EditFormat::Json);
        assert!(result.is_err());

        // Format should be rolled back to Text
        assert_eq!(session.editor_format, EditFormat::Text);
        // working_bytes should also be rolled back
        assert_eq!(session.working_bytes, original_working_bytes);
        assert_eq!(session.editor_text, original_editor_text);

        // Now try switching to Hex - this should still work
        session
            .set_editor_format(EditFormat::Hex)
            .expect("hex switch should succeed");
        assert_eq!(session.editor_format, EditFormat::Hex);

        // And back to Text should also work
        session
            .set_editor_format(EditFormat::Text)
            .expect("text switch should succeed");
        assert_eq!(session.editor_format, EditFormat::Text);
        assert_eq!(session.editor_text.as_ref(), "plain text here");
    }

    #[test]
    fn test_format_switch_json_msgpack_interconversion() {
        // Test that JSON ↔ MessagePack can switch freely
        // Both formats use JSON text as editor_text, so value-level conversion should work

        // Create MessagePack binary data
        let original = serde_json::json!({"name": "test", "value": 123});
        let msgpack = rmp_serde::to_vec(&original).expect("msgpack encode failed");

        let mut session = EditSession::new("test:key".into(), Bytes::from(msgpack));
        session.detect_and_init().expect("init failed");

        // Should detect as MessagePack format, editor shows JSON text
        assert_eq!(session.editor_format, EditFormat::MessagePack);
        assert!(session.editor_text.contains("\"name\""));
        assert!(session.editor_text.contains("\"value\""));

        // Switch to JSON should succeed (value-level conversion)
        session
            .set_editor_format(EditFormat::Json)
            .expect("msgpack to json switch should succeed");
        assert_eq!(session.editor_format, EditFormat::Json);
        assert!(session.editor_text.contains("\"name\""));
        assert!(session.valid);
        assert!(session.error.is_none());

        // Switch back to MessagePack should also succeed
        session
            .set_editor_format(EditFormat::MessagePack)
            .expect("json to msgpack switch should succeed");
        assert_eq!(session.editor_format, EditFormat::MessagePack);
        assert!(session.editor_text.contains("\"name\""));
        assert!(session.valid);
        assert!(session.error.is_none());

        // Multiple round-trips should work
        for _ in 0..3 {
            session
                .set_editor_format(EditFormat::Json)
                .expect("json switch should succeed");
            assert_eq!(session.editor_format, EditFormat::Json);

            session
                .set_editor_format(EditFormat::MessagePack)
                .expect("msgpack switch should succeed");
            assert_eq!(session.editor_format, EditFormat::MessagePack);
        }

        // Verify content is preserved
        assert!(session.editor_text.contains("\"name\""));
        assert!(session.editor_text.contains("\"value\""));
    }

    #[test]
    fn test_can_save() {
        let mut session = EditSession::new("test:key".into(), Bytes::from("hello"));
        session.detect_and_init().expect("init failed");

        // Initially not dirty
        assert!(!session.can_save());

        // After edit
        session.set_editor_text("world".into());
        assert!(session.can_save());

        // Invalid text
        session.editor_format = EditFormat::Json;
        session.set_editor_text("invalid json".into());
        assert!(!session.can_save());
    }

    #[test]
    fn test_build_save_bytes() {
        let mut session = EditSession::new("test:key".into(), Bytes::from("hello"));
        session.detect_and_init().expect("init failed");
        session.set_editor_text("world".into());

        let bytes = session.build_save_bytes().expect("build failed");
        assert_eq!(bytes, b"world");
    }

    #[test]
    fn test_compression_roundtrip() {
        use crate::helpers::codec::compress;

        // Create gzip compressed data
        let original = b"hello world";
        let compressed = compress(original, CompressionFormat::Gzip).expect("compress failed");

        let mut session = EditSession::new("test:key".into(), Bytes::from(compressed));
        session.detect_and_init().expect("init failed");

        assert_eq!(session.compression, CompressionFormat::Gzip);
        assert_eq!(session.editor_text.as_ref(), "hello world");

        // Edit and save with same compression
        session.set_editor_text("hello universe".into());
        let saved = session.build_save_bytes().expect("save failed");

        // Decompress to verify
        let decompressed =
            decompress(&saved, CompressionFormat::Gzip, MAX_DECOMPRESS_BYTES).expect("decompress failed");
        assert_eq!(decompressed, b"hello universe");
    }

    #[test]
    fn test_format_switch_json_hex_msgpack_no_data_loss() {
        // Test that JSON → Hex → MessagePack → JSON preserves data
        // This is the exact scenario reported in the bug

        let json = r#"{"key": "value", "number": 42}"#;
        let mut session = EditSession::new("test:key".into(), Bytes::from(json));
        session.detect_and_init().expect("init failed");

        // Should detect as JSON
        assert_eq!(session.editor_format, EditFormat::Json);
        assert!(session.editor_text.contains("\"key\""));
        assert!(session.editor_text.contains("\"value\""));

        // Switch to Hex - should show hex representation of JSON bytes
        session
            .set_editor_format(EditFormat::Hex)
            .expect("json to hex switch should succeed");
        assert_eq!(session.editor_format, EditFormat::Hex);
        // JSON starts with '{' which is 0x7b
        assert!(session.editor_text.starts_with("7b"));

        // Switch to MessagePack - should convert JSON data to MessagePack, NOT interpret hex as msgpack
        session
            .set_editor_format(EditFormat::MessagePack)
            .expect("hex to msgpack switch should succeed");
        assert_eq!(session.editor_format, EditFormat::MessagePack);
        // MessagePack editor shows JSON text, data should be preserved
        assert!(session.editor_text.contains("\"key\""));
        assert!(session.editor_text.contains("\"value\""));
        assert!(session.editor_text.contains("42"));

        // Switch back to JSON - data should still be intact
        session
            .set_editor_format(EditFormat::Json)
            .expect("msgpack to json switch should succeed");
        assert_eq!(session.editor_format, EditFormat::Json);
        assert!(session.editor_text.contains("\"key\""));
        assert!(session.editor_text.contains("\"value\""));
        assert!(session.editor_text.contains("42"));

        // Switch back to Hex - should show same hex as before
        session
            .set_editor_format(EditFormat::Hex)
            .expect("json to hex switch should succeed");
        assert_eq!(session.editor_format, EditFormat::Hex);
        assert!(session.editor_text.starts_with("7b"));
    }

    #[test]
    fn test_format_switch_msgpack_hex_json_no_data_loss() {
        // Test that MessagePack → Hex → JSON → MessagePack preserves data

        let original = serde_json::json!({"name": "test", "items": [1, 2, 3]});
        let msgpack = rmp_serde::to_vec(&original).expect("msgpack encode failed");

        let mut session = EditSession::new("test:key".into(), Bytes::from(msgpack));
        session.detect_and_init().expect("init failed");

        // Should detect as MessagePack
        assert_eq!(session.editor_format, EditFormat::MessagePack);
        assert!(session.editor_text.contains("\"name\""));
        assert!(session.editor_text.contains("\"items\""));

        // Switch to Hex - should show hex of MessagePack bytes
        session
            .set_editor_format(EditFormat::Hex)
            .expect("msgpack to hex switch should succeed");
        assert_eq!(session.editor_format, EditFormat::Hex);

        // Switch to JSON - should show the data as JSON
        session
            .set_editor_format(EditFormat::Json)
            .expect("hex to json switch should succeed");
        assert_eq!(session.editor_format, EditFormat::Json);
        assert!(session.editor_text.contains("\"name\""));
        assert!(session.editor_text.contains("\"items\""));

        // Switch back to MessagePack - data should be preserved
        session
            .set_editor_format(EditFormat::MessagePack)
            .expect("json to msgpack switch should succeed");
        assert_eq!(session.editor_format, EditFormat::MessagePack);
        assert!(session.editor_text.contains("\"name\""));
        assert!(session.editor_text.contains("\"items\""));
    }

    #[test]
    fn test_format_switch_text_hex_msgpack() {
        // Test switching from Text (non-JSON) through Hex to MessagePack
        // This should fail gracefully since plain text can't be converted to MessagePack

        let mut session = EditSession::new("test:key".into(), Bytes::from("plain text here"));
        session.detect_and_init().expect("init failed");

        assert_eq!(session.editor_format, EditFormat::Text);

        // Switch to Hex
        session
            .set_editor_format(EditFormat::Hex)
            .expect("text to hex switch should succeed");
        assert_eq!(session.editor_format, EditFormat::Hex);

        // Switch to MessagePack should fail (plain text is not valid JSON or MessagePack)
        let result = session.set_editor_format(EditFormat::MessagePack);
        assert!(result.is_err());

        // Format should remain as Hex
        assert_eq!(session.editor_format, EditFormat::Hex);
    }
}
