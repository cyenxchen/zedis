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

//! Codec module for compression/decompression and format encoding/decoding.
//!
//! This module provides:
//! - Compression format detection and decompression (Gzip, Zstd, Snappy, LZ4)
//! - Content format detection (JSON, MessagePack, Text, Binary)
//! - Format conversion utilities for the edit dialog
//! - Hex encoding/decoding for binary data editing

use crate::error::Error;
use flate2::Compression as GzipCompression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use gpui::SharedString;
use lz4_flex::block::{compress_prepend_size, decompress_size_prepended};
use ruzstd::decoding::StreamingDecoder;
use serde::de::Deserialize;
use serde_json::Value as JsonValue;
use snap::read::FrameDecoder as SnappyDecoder;
use snap::write::FrameEncoder as SnappyEncoder;
use std::io::{Cursor, Read, Write};

type Result<T, E = Error> = std::result::Result<T, E>;

/// Maximum decompressed size to prevent compression bombs (64 MB)
pub const MAX_DECOMPRESS_BYTES: usize = 64 * 1024 * 1024;

/// Compression format (container layer)
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum CompressionFormat {
    #[default]
    None,
    Gzip,
    Zstd,
    Snappy,
    Lz4,
}

impl CompressionFormat {
    /// Get display name for the compression format
    pub fn as_str(&self) -> &'static str {
        match self {
            CompressionFormat::None => "None",
            CompressionFormat::Gzip => "Gzip",
            CompressionFormat::Zstd => "Zstd",
            CompressionFormat::Snappy => "Snappy",
            CompressionFormat::Lz4 => "LZ4",
        }
    }

    /// Get all compression formats for UI selection
    pub fn all() -> &'static [CompressionFormat] {
        &[
            CompressionFormat::None,
            CompressionFormat::Gzip,
            CompressionFormat::Zstd,
            CompressionFormat::Snappy,
            CompressionFormat::Lz4,
        ]
    }
}

impl From<&str> for CompressionFormat {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "gzip" => CompressionFormat::Gzip,
            "zstd" => CompressionFormat::Zstd,
            "snappy" => CompressionFormat::Snappy,
            "lz4" => CompressionFormat::Lz4,
            _ => CompressionFormat::None,
        }
    }
}

/// Content format (data layer) - what the data actually is
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[allow(dead_code)]
pub enum ContentFormat {
    #[default]
    Binary,
    Text,
    Json,
    MessagePack,
    Protobuf,
}

#[allow(dead_code)]
impl ContentFormat {
    /// Get display name for the content format
    pub fn as_str(&self) -> &'static str {
        match self {
            ContentFormat::Binary => "Binary",
            ContentFormat::Text => "Text",
            ContentFormat::Json => "JSON",
            ContentFormat::MessagePack => "MessagePack",
            ContentFormat::Protobuf => "Protobuf",
        }
    }
}

/// Edit format (UI layer) - how the user edits the data
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum EditFormat {
    #[default]
    Text,
    Json,
    Hex,
    MessagePack,
    ProtobufJson,
}

impl EditFormat {
    /// Get display name for the edit format
    pub fn as_str(&self) -> &'static str {
        match self {
            EditFormat::Text => "Text",
            EditFormat::Json => "JSON",
            EditFormat::Hex => "Hex",
            EditFormat::MessagePack => "MessagePack",
            EditFormat::ProtobufJson => "Protobuf",
        }
    }

    /// Get all edit formats for UI selection
    pub fn all() -> &'static [EditFormat] {
        &[
            EditFormat::Text,
            EditFormat::Json,
            EditFormat::Hex,
            EditFormat::MessagePack,
        ]
    }

    /// Get the syntax highlighting language for the format
    pub fn language(&self) -> &'static str {
        match self {
            EditFormat::Json | EditFormat::MessagePack | EditFormat::ProtobufJson => "json",
            _ => "text",
        }
    }
}

impl From<&str> for EditFormat {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "json" => EditFormat::Json,
            "hex" => EditFormat::Hex,
            "messagepack" | "msgpack" => EditFormat::MessagePack,
            "protobuf" | "protobufjson" => EditFormat::ProtobufJson,
            _ => EditFormat::Text,
        }
    }
}

/// Detection result for content analysis
#[derive(Debug, Clone)]
pub struct Detection {
    pub compression: CompressionFormat,
    pub content: ContentFormat,
    pub mime: Option<SharedString>,
    pub is_utf8: bool,
}

/// Detect compression and content format from raw bytes
pub fn detect(bytes: &[u8]) -> Detection {
    if bytes.is_empty() {
        return Detection {
            compression: CompressionFormat::None,
            content: ContentFormat::Text,
            mime: None,
            is_utf8: true,
        };
    }

    // Check compression format first
    let compression = detect_compression(bytes);
    let is_utf8 = std::str::from_utf8(bytes).is_ok();

    // If compressed, try to decompress and detect content
    if compression != CompressionFormat::None
        && let Ok(decompressed) = decompress(bytes, compression, MAX_DECOMPRESS_BYTES)
    {
        let content = detect_content(&decompressed);
        return Detection {
            compression,
            content,
            mime: compression_mime(compression),
            is_utf8: std::str::from_utf8(&decompressed).is_ok(),
        };
    }

    // Detect content format directly
    let content = detect_content(bytes);
    let mime = content_mime(content);

    Detection {
        compression,
        content,
        mime,
        is_utf8,
    }
}

/// Read the declared decompressed size from LZ4 block format header.
/// LZ4 block format prepends a 4-byte little-endian size prefix.
/// Returns None if bytes are too short.
fn lz4_declared_size(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 4 {
        return None;
    }
    let size = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    Some(size)
}

/// Detect compression format from bytes
fn detect_compression(bytes: &[u8]) -> CompressionFormat {
    if bytes.len() < 2 {
        return CompressionFormat::None;
    }

    // Gzip magic: 1f 8b
    if bytes.starts_with(&[0x1f, 0x8b]) {
        return CompressionFormat::Gzip;
    }

    // Zstd magic: 28 b5 2f fd
    if bytes.len() >= 4 && bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return CompressionFormat::Zstd;
    }

    // Snappy framed format: ff 06 00 00 73 4e 61 50 70 59
    if bytes.len() >= 10 && bytes.starts_with(&[0xff, 0x06, 0x00, 0x00, 0x73, 0x4e, 0x61, 0x50, 0x70, 0x59]) {
        return CompressionFormat::Snappy;
    }

    // LZ4 block format: try to decompress to detect
    // LZ4 doesn't have a magic number, so we try decompression
    // First check the declared size to prevent compression bomb attacks
    if let Some(declared_size) = lz4_declared_size(bytes)
        && declared_size <= MAX_DECOMPRESS_BYTES
        && decompress_size_prepended(bytes).is_ok()
    {
        return CompressionFormat::Lz4;
    }

    CompressionFormat::None
}

/// Detect content format from uncompressed bytes
fn detect_content(bytes: &[u8]) -> ContentFormat {
    if bytes.is_empty() {
        return ContentFormat::Text;
    }

    // Try UTF-8 text detection
    if let Ok(text) = std::str::from_utf8(bytes) {
        let trimmed = text.trim();

        // Check for JSON
        if ((trimmed.starts_with('{') && trimmed.ends_with('}'))
            || (trimmed.starts_with('[') && trimmed.ends_with(']')))
            && serde_json::from_str::<JsonValue>(text).is_ok()
        {
            return ContentFormat::Json;
        }

        return ContentFormat::Text;
    }

    // Check for MessagePack
    if is_valid_messagepack(bytes) {
        return ContentFormat::MessagePack;
    }

    ContentFormat::Binary
}

/// Check if bytes are valid MessagePack format
fn is_valid_messagepack(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }

    let first_byte = bytes[0];

    // Check for container types (map or array)
    let is_container =
        // FixMap (0x80 - 0x8F)
        (0x80..=0x8f).contains(&first_byte) ||
        // FixArray (0x90 - 0x9F)
        (0x90..=0x9f).contains(&first_byte) ||
        // Array16 (0xdc), Array32 (0xdd)
        first_byte == 0xdc || first_byte == 0xdd ||
        // Map16 (0xde), Map32 (0xdf)
        first_byte == 0xde || first_byte == 0xdf;

    if !is_container {
        return false;
    }

    // Try to deserialize to verify
    let mut deserializer = rmp_serde::decode::Deserializer::new(Cursor::new(bytes));
    match serde::de::IgnoredAny::deserialize(&mut deserializer) {
        Ok(_) => deserializer.get_ref().position() == bytes.len() as u64,
        Err(_) => false,
    }
}

fn compression_mime(format: CompressionFormat) -> Option<SharedString> {
    match format {
        CompressionFormat::Gzip => Some("application/gzip".into()),
        CompressionFormat::Zstd => Some("application/zstd".into()),
        CompressionFormat::Snappy => Some("application/snappy".into()),
        CompressionFormat::Lz4 => Some("application/lz4".into()),
        CompressionFormat::None => None,
    }
}

fn content_mime(format: ContentFormat) -> Option<SharedString> {
    match format {
        ContentFormat::Json => Some("application/json".into()),
        ContentFormat::MessagePack => Some("application/msgpack".into()),
        ContentFormat::Protobuf => Some("application/x-protobuf".into()),
        ContentFormat::Text => Some("text/plain".into()),
        ContentFormat::Binary => Some("application/octet-stream".into()),
    }
}

// ============================================
// Compression / Decompression
// ============================================

/// Decompress bytes using the specified compression format
pub fn decompress(bytes: &[u8], format: CompressionFormat, max_bytes: usize) -> Result<Vec<u8>> {
    match format {
        CompressionFormat::None => Ok(bytes.to_vec()),
        CompressionFormat::Gzip => decompress_gzip(bytes, max_bytes),
        CompressionFormat::Zstd => decompress_zstd(bytes, max_bytes),
        CompressionFormat::Snappy => decompress_snappy(bytes, max_bytes),
        CompressionFormat::Lz4 => decompress_lz4(bytes, max_bytes),
    }
}

/// Compress bytes using the specified compression format
pub fn compress(bytes: &[u8], format: CompressionFormat) -> Result<Vec<u8>> {
    match format {
        CompressionFormat::None => Ok(bytes.to_vec()),
        CompressionFormat::Gzip => compress_gzip(bytes),
        CompressionFormat::Zstd => compress_zstd(bytes),
        CompressionFormat::Snappy => compress_snappy(bytes),
        CompressionFormat::Lz4 => compress_lz4(bytes),
    }
}

fn decompress_gzip(bytes: &[u8], max_bytes: usize) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(bytes);
    let mut result = Vec::with_capacity(bytes.len().min(max_bytes));

    // Read in chunks to avoid memory exhaustion
    let mut buffer = [0u8; 8192];
    loop {
        let n = decoder.read(&mut buffer).map_err(|e| Error::Invalid {
            message: format!("Gzip decompression failed: {}", e),
        })?;

        if n == 0 {
            break;
        }

        if result.len() + n > max_bytes {
            return Err(Error::Invalid {
                message: format!("Decompressed size exceeds limit of {} bytes", max_bytes),
            });
        }

        result.extend_from_slice(&buffer[..n]);
    }

    Ok(result)
}

fn compress_gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), GzipCompression::default());
    encoder.write_all(bytes).map_err(|e| Error::Invalid {
        message: format!("Gzip compression failed: {}", e),
    })?;
    encoder.finish().map_err(|e| Error::Invalid {
        message: format!("Gzip compression failed: {}", e),
    })
}

fn decompress_zstd(bytes: &[u8], max_bytes: usize) -> Result<Vec<u8>> {
    let mut decoder = StreamingDecoder::new(bytes).map_err(|e| Error::Invalid {
        message: format!("Zstd decompression failed: {}", e),
    })?;

    let mut result = Vec::with_capacity(bytes.len().min(max_bytes));
    let mut buffer = [0u8; 8192];

    loop {
        let n = decoder.read(&mut buffer).map_err(|e| Error::Invalid {
            message: format!("Zstd decompression failed: {}", e),
        })?;

        if n == 0 {
            break;
        }

        if result.len() + n > max_bytes {
            return Err(Error::Invalid {
                message: format!("Decompressed size exceeds limit of {} bytes", max_bytes),
            });
        }

        result.extend_from_slice(&buffer[..n]);
    }

    Ok(result)
}

fn compress_zstd(bytes: &[u8]) -> Result<Vec<u8>> {
    // Use zstd crate for compression (ruzstd is decode-only)
    zstd::encode_all(Cursor::new(bytes), 3).map_err(|e| Error::Invalid {
        message: format!("Zstd compression failed: {}", e),
    })
}

fn decompress_snappy(bytes: &[u8], max_bytes: usize) -> Result<Vec<u8>> {
    let mut decoder = SnappyDecoder::new(bytes);
    let mut result = Vec::with_capacity(bytes.len().min(max_bytes));
    let mut buffer = [0u8; 8192];

    loop {
        let n = decoder.read(&mut buffer).map_err(|e| Error::Invalid {
            message: format!("Snappy decompression failed: {}", e),
        })?;

        if n == 0 {
            break;
        }

        if result.len() + n > max_bytes {
            return Err(Error::Invalid {
                message: format!("Decompressed size exceeds limit of {} bytes", max_bytes),
            });
        }

        result.extend_from_slice(&buffer[..n]);
    }

    Ok(result)
}

fn compress_snappy(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = SnappyEncoder::new(Vec::new());
    encoder.write_all(bytes).map_err(|e| Error::Invalid {
        message: format!("Snappy compression failed: {}", e),
    })?;
    encoder.into_inner().map_err(|e| Error::Invalid {
        message: format!("Snappy compression failed: {}", e),
    })
}

fn decompress_lz4(bytes: &[u8], max_bytes: usize) -> Result<Vec<u8>> {
    let result = decompress_size_prepended(bytes).map_err(|e| Error::Invalid {
        message: format!("LZ4 decompression failed: {}", e),
    })?;

    if result.len() > max_bytes {
        return Err(Error::Invalid {
            message: format!("Decompressed size exceeds limit of {} bytes", max_bytes),
        });
    }

    Ok(result)
}

fn compress_lz4(bytes: &[u8]) -> Result<Vec<u8>> {
    // Use block mode with prepended size (same as decompression expects)
    Ok(compress_prepend_size(bytes))
}

// ============================================
// Content Encoding / Decoding
// ============================================

/// Decode bytes to text representation based on edit format
pub fn decode_to_text(bytes: &[u8], format: EditFormat) -> Result<String> {
    match format {
        EditFormat::Text => String::from_utf8(bytes.to_vec()).map_err(|e| Error::Invalid {
            message: format!("Invalid UTF-8: {}", e),
        }),
        EditFormat::Json => {
            let text = String::from_utf8(bytes.to_vec()).map_err(|e| Error::Invalid {
                message: format!("Invalid UTF-8: {}", e),
            })?;
            // Pretty print JSON
            let value: JsonValue = serde_json::from_str(&text).map_err(|e| Error::Invalid {
                message: format!("Invalid JSON: {}", e),
            })?;
            serde_json::to_string_pretty(&value).map_err(|e| Error::Invalid {
                message: format!("JSON serialization failed: {}", e),
            })
        }
        EditFormat::Hex => Ok(bytes_to_hex(bytes)),
        EditFormat::MessagePack => {
            let value: JsonValue = rmp_serde::from_slice(bytes).map_err(|e| Error::Invalid {
                message: format!("Invalid MessagePack: {}", e),
            })?;
            serde_json::to_string_pretty(&value).map_err(|e| Error::Invalid {
                message: format!("JSON serialization failed: {}", e),
            })
        }
        EditFormat::ProtobufJson => {
            // Protobuf decoding requires schema, return error
            Err(Error::Invalid {
                message: "Protobuf decoding requires schema".to_string(),
            })
        }
    }
}

/// Encode text back to bytes based on edit format
pub fn encode_from_text(text: &str, format: EditFormat) -> Result<Vec<u8>> {
    match format {
        EditFormat::Text => Ok(text.as_bytes().to_vec()),
        EditFormat::Json => {
            // Validate JSON and compact it
            let value: JsonValue = serde_json::from_str(text).map_err(|e| Error::Invalid {
                message: format!("Invalid JSON: {}", e),
            })?;
            serde_json::to_vec(&value).map_err(|e| Error::Invalid {
                message: format!("JSON serialization failed: {}", e),
            })
        }
        EditFormat::Hex => hex_to_bytes(text),
        EditFormat::MessagePack => {
            // Parse JSON and convert to MessagePack
            let value: JsonValue = serde_json::from_str(text).map_err(|e| Error::Invalid {
                message: format!("Invalid JSON: {}", e),
            })?;
            rmp_serde::to_vec(&value).map_err(|e| Error::Invalid {
                message: format!("MessagePack serialization failed: {}", e),
            })
        }
        EditFormat::ProtobufJson => {
            // Protobuf encoding requires schema, return error
            Err(Error::Invalid {
                message: "Protobuf encoding requires schema".to_string(),
            })
        }
    }
}

// ============================================
// Hex Utilities
// ============================================

/// Convert bytes to hex string representation
/// Format: "00 01 02 03 ..." (space-separated pairs)
pub fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ")
}

/// Convert hex string back to bytes
/// Accepts: "00 01 02 03" or "00010203" formats
pub fn hex_to_bytes(hex: &str) -> Result<Vec<u8>> {
    // Remove all whitespace and convert to lowercase
    let cleaned: String = hex.chars().filter(|c| !c.is_whitespace()).collect();

    if !cleaned.len().is_multiple_of(2) {
        return Err(Error::Invalid {
            message: "Hex string must have even number of characters".to_string(),
        });
    }

    let mut result = Vec::with_capacity(cleaned.len() / 2);

    for i in (0..cleaned.len()).step_by(2) {
        let byte_str = &cleaned[i..i + 2];
        let byte = u8::from_str_radix(byte_str, 16).map_err(|e| Error::Invalid {
            message: format!("Invalid hex character at position {}: {}", i, e),
        })?;
        result.push(byte);
    }

    Ok(result)
}

/// Validate that text is valid for the given edit format
pub fn validate_format(text: &str, format: EditFormat) -> Result<()> {
    match format {
        EditFormat::Text => Ok(()),
        EditFormat::Json | EditFormat::MessagePack | EditFormat::ProtobufJson => {
            serde_json::from_str::<JsonValue>(text).map_err(|e| Error::Invalid {
                message: format!("Invalid JSON: {}", e),
            })?;
            Ok(())
        }
        EditFormat::Hex => {
            hex_to_bytes(text)?;
            Ok(())
        }
    }
}

/// Determine the best edit format for given content
pub fn suggest_edit_format(content: ContentFormat, is_utf8: bool) -> EditFormat {
    match content {
        ContentFormat::Json => EditFormat::Json,
        ContentFormat::MessagePack => EditFormat::MessagePack,
        ContentFormat::Protobuf => EditFormat::ProtobufJson,
        ContentFormat::Text => EditFormat::Text,
        ContentFormat::Binary => {
            if is_utf8 {
                EditFormat::Text
            } else {
                EditFormat::Hex
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_roundtrip() {
        let original = vec![0x00, 0x01, 0xfe, 0xff];
        let hex = bytes_to_hex(&original);
        assert_eq!(hex, "00 01 fe ff");
        let decoded = hex_to_bytes(&hex).expect("hex decode failed");
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_hex_no_spaces() {
        let hex = "0001feff";
        let decoded = hex_to_bytes(hex).expect("hex decode failed");
        assert_eq!(decoded, vec![0x00, 0x01, 0xfe, 0xff]);
    }

    #[test]
    fn test_detect_json() {
        let json = br#"{"key": "value"}"#;
        let detection = detect(json);
        assert_eq!(detection.compression, CompressionFormat::None);
        assert_eq!(detection.content, ContentFormat::Json);
        assert!(detection.is_utf8);
    }

    #[test]
    fn test_detect_text() {
        // Note: Use "plain text" instead of "hello world" to avoid LZ4 magic number detection
        let text = b"plain text here";
        let detection = detect(text);
        assert_eq!(detection.compression, CompressionFormat::None);
        assert_eq!(detection.content, ContentFormat::Text);
        assert!(detection.is_utf8);
    }

    #[test]
    fn test_gzip_roundtrip() {
        let original = b"hello world, this is a test for compression";
        let compressed = compress_gzip(original).expect("gzip compress failed");
        let decompressed = decompress_gzip(&compressed, MAX_DECOMPRESS_BYTES).expect("gzip decompress failed");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_lz4_roundtrip() {
        let original = b"hello world, this is a test for compression";
        let compressed = compress_lz4(original).expect("lz4 compress failed");
        let decompressed = decompress_lz4(&compressed, MAX_DECOMPRESS_BYTES).expect("lz4 decompress failed");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_snappy_roundtrip() {
        let original = b"hello world, this is a test for compression";
        let compressed = compress_snappy(original).expect("snappy compress failed");
        let decompressed = decompress_snappy(&compressed, MAX_DECOMPRESS_BYTES).expect("snappy decompress failed");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_zstd_roundtrip() {
        let original = b"hello world, this is a test for compression";
        let compressed = compress_zstd(original).expect("zstd compress failed");
        let decompressed = decompress_zstd(&compressed, MAX_DECOMPRESS_BYTES).expect("zstd decompress failed");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_json_format_roundtrip() {
        let json = r#"{"name":"test","value":123}"#;
        let bytes = json.as_bytes();
        let text = decode_to_text(bytes, EditFormat::Json).expect("decode failed");
        assert!(text.contains("\"name\""));
        let encoded = encode_from_text(&text, EditFormat::Json).expect("encode failed");
        let decoded_value: JsonValue = serde_json::from_slice(&encoded).expect("parse failed");
        assert_eq!(decoded_value["name"], "test");
    }

    #[test]
    fn test_messagepack_format_roundtrip() {
        // Create some messagepack data
        let original = serde_json::json!({"name": "test", "value": 123});
        let msgpack = rmp_serde::to_vec(&original).expect("msgpack encode failed");

        let text = decode_to_text(&msgpack, EditFormat::MessagePack).expect("decode failed");
        assert!(text.contains("\"name\""));

        let encoded = encode_from_text(&text, EditFormat::MessagePack).expect("encode failed");
        let decoded: JsonValue = rmp_serde::from_slice(&encoded).expect("msgpack decode failed");
        assert_eq!(decoded["name"], "test");
    }
}
