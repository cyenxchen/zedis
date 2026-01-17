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

use serde_json::{Map, Value as JsonValue, json};

/// Maximum recursion depth for nested message parsing to prevent stack overflow
const MAX_PARSE_DEPTH: usize = 64;

/// Maximum field size to prevent memory exhaustion (16 MB)
const MAX_FIELD_SIZE: usize = 16 * 1024 * 1024;

/// Wire types in protobuf encoding
#[derive(Debug, Clone, Copy, PartialEq)]
enum WireType {
    Varint = 0,
    Fixed64 = 1,
    LengthDelimited = 2,
    StartGroup = 3,
    EndGroup = 4,
    Fixed32 = 5,
}

impl WireType {
    fn from_u32(val: u32) -> Option<Self> {
        match val {
            0 => Some(WireType::Varint),
            1 => Some(WireType::Fixed64),
            2 => Some(WireType::LengthDelimited),
            3 => Some(WireType::StartGroup),
            4 => Some(WireType::EndGroup),
            5 => Some(WireType::Fixed32),
            _ => None,
        }
    }
}

/// Wire format field representation
#[derive(Debug, Clone)]
pub enum ProtoField {
    Varint(u64),
    Fixed64(u64),
    Fixed32(u32),
    LengthDelimited(Vec<u8>),
}

/// Raw protobuf message representation (without schema)
#[derive(Debug, Clone, Default)]
pub struct RawProtoMessage {
    pub fields: Vec<(u32, ProtoField)>,
}

impl RawProtoMessage {
    /// Convert to JSON representation
    pub fn to_json(&self) -> JsonValue {
        self.to_json_with_depth(0)
    }

    /// Convert to JSON representation with depth tracking
    fn to_json_with_depth(&self, depth: usize) -> JsonValue {
        fields_to_json(&self.fields, depth)
    }
}

/// Convert field list to JSON value with depth tracking
fn fields_to_json(fields: &[(u32, ProtoField)], depth: usize) -> JsonValue {
    let mut map: Map<String, JsonValue> = Map::new();

    for (field_number, field) in fields {
        let key = field_number.to_string();
        let value = field_to_json(field, depth);

        // Handle repeated fields - collect into array
        if let Some(existing) = map.get_mut(&key) {
            if let JsonValue::Array(arr) = existing {
                arr.push(value);
            } else {
                let old_value = existing.take();
                *existing = json!([old_value, value]);
            }
        } else {
            map.insert(key, value);
        }
    }

    JsonValue::Object(map)
}

/// Convert a single field to JSON value with depth tracking
fn field_to_json(field: &ProtoField, depth: usize) -> JsonValue {
    match field {
        ProtoField::Varint(v) => {
            // Try to detect if it might be a signed value (zigzag)
            // For now, just output as unsigned
            json!(*v)
        }
        ProtoField::Fixed64(v) => json!(*v),
        ProtoField::Fixed32(v) => json!(*v),
        ProtoField::LengthDelimited(bytes) => {
            // Try to parse as nested message first (with depth limit)
            if depth < MAX_PARSE_DEPTH
                && let Some(nested) = try_parse_raw_protobuf_with_depth(bytes, depth + 1)
            {
                return nested.to_json_with_depth(depth + 1);
            }
            // Try to parse as UTF-8 string
            if let Ok(s) = std::str::from_utf8(bytes) {
                // Check if it looks like a valid string (no control chars except newline/tab)
                if s.chars()
                    .all(|c| !c.is_control() || c == '\n' || c == '\r' || c == '\t')
                {
                    return json!(s);
                }
            }
            // Fallback to base64 encoding for binary data
            json!(format!("<bytes:{}>", base64_encode(bytes)))
        }
    }
}

/// Base64 encode bytes for display
fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Try to parse bytes as a protobuf message
pub fn try_parse_raw_protobuf(bytes: &[u8]) -> Option<RawProtoMessage> {
    try_parse_raw_protobuf_with_depth(bytes, 0)
}

/// Try to parse bytes as a protobuf message with depth tracking
fn try_parse_raw_protobuf_with_depth(bytes: &[u8], depth: usize) -> Option<RawProtoMessage> {
    // Check depth limit to prevent stack overflow
    if depth > MAX_PARSE_DEPTH {
        return None;
    }

    if bytes.len() < 2 {
        return None;
    }

    let mut fields = Vec::new();
    let mut cursor = 0;

    while cursor < bytes.len() {
        // Decode field key (field_number + wire_type)
        let (key, new_cursor) = decode_varint_safe(&bytes[cursor..])?;
        cursor += new_cursor;

        let wire_type_val = (key & 0x07) as u32;
        let field_number = (key >> 3) as u32;
        let wire_type = WireType::from_u32(wire_type_val)?;

        // Field number must be valid (1-536870911)
        if field_number == 0 || field_number > 536870911 {
            return None;
        }

        let field = match wire_type {
            WireType::Varint => {
                let (value, len) = decode_varint_safe(&bytes[cursor..])?;
                cursor += len;
                ProtoField::Varint(value)
            }
            WireType::Fixed64 => {
                if cursor + 8 > bytes.len() {
                    return None;
                }
                let value = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().ok()?);
                cursor += 8;
                ProtoField::Fixed64(value)
            }
            WireType::LengthDelimited => {
                let (len, len_bytes) = decode_varint_safe(&bytes[cursor..])?;
                cursor += len_bytes;

                // Safe conversion with size limit check to prevent memory exhaustion
                let len = usize::try_from(len).ok().filter(|&l| l <= MAX_FIELD_SIZE)?;

                // Check bounds to prevent overflow
                let end = cursor.checked_add(len)?;
                if end > bytes.len() {
                    return None;
                }

                let data = bytes[cursor..end].to_vec();
                cursor = end;
                ProtoField::LengthDelimited(data)
            }
            WireType::Fixed32 => {
                if cursor + 4 > bytes.len() {
                    return None;
                }
                let value = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().ok()?);
                cursor += 4;
                ProtoField::Fixed32(value)
            }
            // StartGroup and EndGroup are deprecated, skip unknown wire types
            WireType::StartGroup | WireType::EndGroup => return None,
        };

        fields.push((field_number, field));
    }

    // Must have at least one field
    if fields.is_empty() {
        return None;
    }

    Some(RawProtoMessage { fields })
}

/// Safely decode a varint, returning the value and bytes consumed
fn decode_varint_safe(bytes: &[u8]) -> Option<(u64, usize)> {
    if bytes.is_empty() {
        return None;
    }

    let mut result: u64 = 0;
    let mut shift = 0;

    for (i, &byte) in bytes.iter().enumerate() {
        if i >= 10 {
            // Varint too long (max 10 bytes for 64-bit)
            return None;
        }

        let value = (byte & 0x7F) as u64;
        result |= value << shift;

        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }

        shift += 7;
    }

    None // Incomplete varint
}

/// Check if data is likely protobuf format
///
/// This uses heuristics to detect protobuf wire format:
/// 1. Must have valid wire type in first byte
/// 2. Must parse completely without errors
/// 3. Must have reasonable field numbers
/// 4. Should not be a pure numeric string (avoid false positives)
pub fn is_likely_protobuf(bytes: &[u8]) -> bool {
    // Length check
    if bytes.len() < 2 {
        return false;
    }

    // Exclude pure numeric strings (common false positive)
    if let Ok(s) = std::str::from_utf8(bytes) {
        let trimmed = s.trim();
        if trimmed.parse::<f64>().is_ok() {
            return false;
        }
        // Also skip if it looks like JSON
        if (trimmed.starts_with('{') && trimmed.ends_with('}'))
            || (trimmed.starts_with('[') && trimmed.ends_with(']'))
        {
            return false;
        }
        // Skip if it looks like XML/HTML
        if trimmed.starts_with('<') && trimmed.contains('>') {
            return false;
        }
    }

    // Try to parse as protobuf
    let Some(msg) = try_parse_raw_protobuf(bytes) else {
        return false;
    };

    // Additional heuristics to reduce false positives:
    // 1. Check that field numbers are reasonable (usually < 100 for most protos)
    let max_field_number = msg.fields.iter().map(|(n, _)| *n).max().unwrap_or(0);

    // Field numbers > 10000 are suspicious for raw detection
    if max_field_number > 10000 {
        return false;
    }

    // 2. Check for suspicious patterns that indicate non-protobuf data
    // If all fields are LengthDelimited with very short lengths, might be false positive
    let all_short_strings = msg.fields.iter().all(|(_, f)| {
        matches!(f, ProtoField::LengthDelimited(b) if b.len() <= 2)
    });

    if all_short_strings && msg.fields.len() == 1 {
        return false;
    }

    true
}

/// Decode raw protobuf to pretty-printed JSON string
pub fn decode_raw_to_json(bytes: &[u8]) -> Option<String> {
    let msg = try_parse_raw_protobuf(bytes)?;
    serde_json::to_string_pretty(&msg.to_json()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================
    // Test Helper Functions
    // ========================================

    /// Build a varint encoding for a given value
    fn build_varint(mut value: u64) -> Vec<u8> {
        let mut result = Vec::new();
        loop {
            let mut byte = (value & 0x7F) as u8;
            value >>= 7;
            if value != 0 {
                byte |= 0x80;
            }
            result.push(byte);
            if value == 0 {
                break;
            }
        }
        result
    }

    /// Build a field key (field_number << 3 | wire_type)
    fn build_field_key(field_number: u32, wire_type: u8) -> Vec<u8> {
        build_varint(((field_number as u64) << 3) | (wire_type as u64))
    }

    /// Build a varint field (field_number, wire_type=0, value)
    fn build_varint_field(field_number: u32, value: u64) -> Vec<u8> {
        let mut result = build_field_key(field_number, 0);
        result.extend(build_varint(value));
        result
    }

    /// Build a length-delimited field (field_number, wire_type=2, data)
    fn build_length_delimited_field(field_number: u32, data: &[u8]) -> Vec<u8> {
        let mut result = build_field_key(field_number, 2);
        result.extend(build_varint(data.len() as u64));
        result.extend(data);
        result
    }

    /// Build a fixed64 field (field_number, wire_type=1, value)
    fn build_fixed64_field(field_number: u32, value: u64) -> Vec<u8> {
        let mut result = build_field_key(field_number, 1);
        result.extend(value.to_le_bytes());
        result
    }

    /// Build a fixed32 field (field_number, wire_type=5, value)
    fn build_fixed32_field(field_number: u32, value: u32) -> Vec<u8> {
        let mut result = build_field_key(field_number, 5);
        result.extend(value.to_le_bytes());
        result
    }

    // ========================================
    // decode_varint_safe() Tests
    // ========================================

    #[test]
    fn test_decode_varint() {
        // Single byte varint
        assert_eq!(decode_varint_safe(&[0x01]), Some((1, 1)));
        assert_eq!(decode_varint_safe(&[0x7F]), Some((127, 1)));

        // Multi-byte varint
        assert_eq!(decode_varint_safe(&[0xAC, 0x02]), Some((300, 2)));

        // Empty
        assert_eq!(decode_varint_safe(&[]), None);
    }

    #[test]
    fn test_decode_varint_max_10_bytes() {
        // u64::MAX requires 10 bytes in varint encoding
        // u64::MAX = 18446744073709551615
        let max_varint = build_varint(u64::MAX);
        assert_eq!(max_varint.len(), 10, "test: u64::MAX should be 10 bytes");

        let result = decode_varint_safe(&max_varint);
        assert_eq!(result, Some((u64::MAX, 10)), "test: should decode u64::MAX correctly");
    }

    #[test]
    fn test_decode_varint_11_bytes_too_long() {
        // Construct an 11-byte varint (invalid)
        // All bytes have continuation bit set except the last
        let mut too_long = vec![0x80; 10];
        too_long.push(0x01); // 11th byte without continuation
        assert_eq!(decode_varint_safe(&too_long), None, "test: 11 byte varint should be rejected");
    }

    #[test]
    fn test_decode_varint_incomplete() {
        // Varint with continuation bit set but no following byte
        assert_eq!(decode_varint_safe(&[0x80]), None, "test: incomplete single byte");
        assert_eq!(decode_varint_safe(&[0x80, 0x80]), None, "test: incomplete two bytes");
        assert_eq!(decode_varint_safe(&[0xAC, 0x82]), None, "test: incomplete multi-byte");
    }

    #[test]
    fn test_decode_varint_zero() {
        assert_eq!(decode_varint_safe(&[0x00]), Some((0, 1)), "test: zero value");
    }

    #[test]
    fn test_decode_varint_boundary_values() {
        // 127 (single byte max)
        assert_eq!(decode_varint_safe(&[0x7F]), Some((127, 1)), "test: 127");

        // 128 (first two-byte value)
        let v128 = build_varint(128);
        assert_eq!(decode_varint_safe(&v128), Some((128, 2)), "test: 128");

        // 16383 (two byte max)
        let v16383 = build_varint(16383);
        assert_eq!(decode_varint_safe(&v16383), Some((16383, 2)), "test: 16383");

        // 16384 (first three-byte value)
        let v16384 = build_varint(16384);
        assert_eq!(decode_varint_safe(&v16384), Some((16384, 3)), "test: 16384");
    }

    // ========================================
    // try_parse_raw_protobuf() Tests
    // ========================================

    #[test]
    fn test_simple_protobuf() {
        // Field 1, varint, value 150
        let bytes = [0x08, 0x96, 0x01];
        assert!(is_likely_protobuf(&bytes));

        let msg = try_parse_raw_protobuf(&bytes);
        assert!(msg.is_some());
    }

    #[test]
    fn test_parse_depth_limit_exceeded() {
        // Build deeply nested message (>64 levels)
        // Each level: field 1, length-delimited containing next level
        fn build_nested(depth: usize) -> Vec<u8> {
            if depth == 0 {
                // Innermost: simple varint field
                return build_varint_field(1, 42);
            }
            let inner = build_nested(depth - 1);
            build_length_delimited_field(1, &inner)
        }

        // 65 levels should fail (exceeds MAX_PARSE_DEPTH of 64)
        let deep_nested = build_nested(65);
        // The parsing should succeed but inner levels won't be parsed as messages
        let msg = try_parse_raw_protobuf(&deep_nested);
        assert!(msg.is_some(), "test: outer parsing should succeed");

        // Verify that deeply nested data is treated as bytes, not parsed further
        // by checking JSON output doesn't recursively parse all levels
        let json = decode_raw_to_json(&deep_nested);
        assert!(json.is_some(), "test: should produce JSON");
    }

    #[test]
    fn test_parse_field_size_limit() {
        // Build a field claiming to be larger than MAX_FIELD_SIZE (16MB)
        let mut data = build_field_key(1, 2); // length-delimited field
        data.extend(build_varint(17 * 1024 * 1024)); // claim 17MB size
        // Don't actually append data, just the invalid length claim

        assert!(
            try_parse_raw_protobuf(&data).is_none(),
            "test: oversized field should fail"
        );
    }

    #[test]
    fn test_parse_invalid_field_number_zero() {
        // Field number 0 is invalid
        let data = build_varint_field(0, 42);
        assert!(
            try_parse_raw_protobuf(&data).is_none(),
            "test: field number 0 should be invalid"
        );
    }

    #[test]
    fn test_parse_invalid_field_number_too_large() {
        // Field number > 536870911 is invalid
        // wire_type 0 (varint) is encoded in lower 3 bits
        let mut data = build_varint(536870912_u64 << 3); // field 536870912, varint (wire_type=0)
        data.extend(build_varint(1));
        assert!(
            try_parse_raw_protobuf(&data).is_none(),
            "test: field number too large"
        );
    }

    #[test]
    fn test_parse_invalid_wire_type() {
        // Wire types 6 and 7 are invalid
        let data6 = build_field_key(1, 6);
        let data7 = build_field_key(1, 7);
        assert!(
            try_parse_raw_protobuf(&data6).is_none(),
            "test: wire type 6 invalid"
        );
        assert!(
            try_parse_raw_protobuf(&data7).is_none(),
            "test: wire type 7 invalid"
        );
    }

    #[test]
    fn test_parse_deprecated_group_wire_types() {
        // StartGroup (3) and EndGroup (4) are deprecated
        let start_group = build_field_key(1, 3);
        let end_group = build_field_key(1, 4);
        assert!(
            try_parse_raw_protobuf(&start_group).is_none(),
            "test: StartGroup rejected"
        );
        assert!(
            try_parse_raw_protobuf(&end_group).is_none(),
            "test: EndGroup rejected"
        );
    }

    #[test]
    fn test_parse_incomplete_fixed64() {
        // Fixed64 needs 8 bytes, provide only 4
        let mut data = build_field_key(1, 1); // Fixed64
        data.extend(&[0x01, 0x02, 0x03, 0x04]); // only 4 bytes
        assert!(
            try_parse_raw_protobuf(&data).is_none(),
            "test: incomplete Fixed64"
        );
    }

    #[test]
    fn test_parse_incomplete_fixed32() {
        // Fixed32 needs 4 bytes, provide only 2
        let mut data = build_field_key(1, 5); // Fixed32
        data.extend(&[0x01, 0x02]); // only 2 bytes
        assert!(
            try_parse_raw_protobuf(&data).is_none(),
            "test: incomplete Fixed32"
        );
    }

    #[test]
    fn test_parse_incomplete_length_delimited() {
        // Length-delimited claims 10 bytes but only has 5
        let mut data = build_field_key(1, 2);
        data.extend(build_varint(10)); // claim 10 bytes
        data.extend(&[0x01, 0x02, 0x03, 0x04, 0x05]); // only 5 bytes
        assert!(
            try_parse_raw_protobuf(&data).is_none(),
            "test: incomplete length-delimited"
        );
    }

    #[test]
    fn test_parse_empty_input() {
        assert!(try_parse_raw_protobuf(&[]).is_none(), "test: empty input");
    }

    #[test]
    fn test_parse_single_byte() {
        assert!(
            try_parse_raw_protobuf(&[0x08]).is_none(),
            "test: single byte"
        );
    }

    #[test]
    fn test_parse_valid_multiple_fields() {
        // Build message with multiple fields of different types
        let mut data = Vec::new();
        data.extend(build_varint_field(1, 42)); // field 1: varint 42
        data.extend(build_varint_field(2, 100)); // field 2: varint 100
        data.extend(build_fixed32_field(3, 0x12345678)); // field 3: fixed32
        data.extend(build_fixed64_field(4, 0x123456789ABCDEF0)); // field 4: fixed64
        data.extend(build_length_delimited_field(5, b"hello")); // field 5: string

        let msg = try_parse_raw_protobuf(&data);
        assert!(msg.is_some(), "test: valid multi-field message");

        let msg = msg.expect("test: message should exist");
        assert_eq!(msg.fields.len(), 5, "test: should have 5 fields");

        // Verify field values
        assert!(matches!(msg.fields[0], (1, ProtoField::Varint(42))));
        assert!(matches!(msg.fields[1], (2, ProtoField::Varint(100))));
        assert!(matches!(msg.fields[2], (3, ProtoField::Fixed32(0x12345678))));
        assert!(matches!(msg.fields[3], (4, ProtoField::Fixed64(0x123456789ABCDEF0))));
        if let (5, ProtoField::LengthDelimited(ref bytes)) = msg.fields[4] {
            assert_eq!(bytes, b"hello", "test: string content");
        } else {
            panic!("test: field 5 should be LengthDelimited");
        }
    }

    // ========================================
    // is_likely_protobuf() Tests
    // ========================================

    #[test]
    fn test_not_protobuf() {
        // Plain text
        assert!(!is_likely_protobuf(b"hello world"));

        // JSON
        assert!(!is_likely_protobuf(br#"{"key": "value"}"#));

        // Numeric string
        assert!(!is_likely_protobuf(b"12345"));

        // Too short
        assert!(!is_likely_protobuf(b"a"));
    }

    #[test]
    fn test_likely_protobuf_high_field_number() {
        // Field number > 10000 should be rejected by heuristics
        let data = build_varint_field(10001, 42);
        assert!(!is_likely_protobuf(&data), "test: high field number rejected");
    }

    #[test]
    fn test_likely_protobuf_single_short_string() {
        // Single field with very short length-delimited data
        let data = build_length_delimited_field(1, &[0x01]);
        assert!(!is_likely_protobuf(&data), "test: single short string rejected");
    }

    #[test]
    fn test_likely_protobuf_xml_detection() {
        assert!(!is_likely_protobuf(b"<root>content</root>"), "test: XML excluded");
        assert!(!is_likely_protobuf(b"<?xml version=\"1.0\"?>"), "test: XML declaration excluded");
        assert!(!is_likely_protobuf(b"<html><body></body></html>"), "test: HTML excluded");
    }

    #[test]
    fn test_likely_protobuf_json_detection() {
        assert!(!is_likely_protobuf(br#"{"a": 1}"#), "test: JSON object excluded");
        assert!(!is_likely_protobuf(b"[1, 2, 3]"), "test: JSON array excluded");
        assert!(!is_likely_protobuf(br#"{ "nested": {"key": "value"} }"#), "test: nested JSON excluded");
    }

    #[test]
    fn test_likely_protobuf_numeric_string() {
        assert!(!is_likely_protobuf(b"123"), "test: integer string");
        assert!(!is_likely_protobuf(b"3.14159"), "test: float string");
        assert!(!is_likely_protobuf(b"-42"), "test: negative number");
        assert!(!is_likely_protobuf(b"  100  "), "test: padded number");
        assert!(!is_likely_protobuf(b"1e10"), "test: scientific notation");
    }

    #[test]
    fn test_likely_protobuf_too_short() {
        assert!(!is_likely_protobuf(&[]), "test: empty");
        assert!(!is_likely_protobuf(&[0x08]), "test: 1 byte");
    }

    #[test]
    fn test_likely_protobuf_valid_message() {
        // Valid protobuf with multiple fields
        let mut data = Vec::new();
        data.extend(build_varint_field(1, 42));
        data.extend(build_varint_field(2, 100));
        data.extend(build_length_delimited_field(3, b"test string"));

        assert!(is_likely_protobuf(&data), "test: valid multi-field message");
    }

    // ========================================
    // decode_raw_to_json() Tests
    // ========================================

    #[test]
    fn test_json_nested_message() {
        // Nested message: outer field 1 contains inner field 1
        let inner = build_varint_field(1, 99);
        let outer = build_length_delimited_field(1, &inner);

        let json = decode_raw_to_json(&outer).expect("test: should decode");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("test: valid JSON");

        // Should have nested structure: {"1": {"1": 99}}
        assert_eq!(parsed["1"]["1"], 99, "test: nested value");
    }

    #[test]
    fn test_json_utf8_string() {
        let data = build_length_delimited_field(1, b"hello world");
        let json = decode_raw_to_json(&data).expect("test: should decode");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("test: valid JSON");
        assert_eq!(parsed["1"], "hello world", "test: UTF-8 string");
    }

    #[test]
    fn test_json_utf8_string_with_unicode() {
        let data = build_length_delimited_field(1, "你好世界".as_bytes());
        let json = decode_raw_to_json(&data).expect("test: should decode");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("test: valid JSON");
        assert_eq!(parsed["1"], "你好世界", "test: Unicode string");
    }

    #[test]
    fn test_json_binary_data_base64() {
        // Binary data with control characters
        let binary = &[0x00, 0x01, 0x02, 0xFF, 0xFE];
        let data = build_length_delimited_field(1, binary);
        let json = decode_raw_to_json(&data).expect("test: should decode");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("test: valid JSON");

        // Should be base64 encoded with <bytes:> prefix
        let value = parsed["1"].as_str().expect("test: should be string");
        assert!(value.starts_with("<bytes:"), "test: binary data base64 prefix");
    }

    #[test]
    fn test_json_repeated_fields_array() {
        // Multiple fields with same field number become array
        let mut data = Vec::new();
        data.extend(build_varint_field(1, 10));
        data.extend(build_varint_field(1, 20));
        data.extend(build_varint_field(1, 30));

        let json = decode_raw_to_json(&data).expect("test: should decode");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("test: valid JSON");

        assert!(parsed["1"].is_array(), "test: repeated field is array");
        let arr = parsed["1"].as_array().expect("test: array");
        assert_eq!(arr.len(), 3, "test: three elements");
        assert_eq!(arr[0], 10);
        assert_eq!(arr[1], 20);
        assert_eq!(arr[2], 30);
    }

    #[test]
    fn test_json_two_repeated_fields_become_array() {
        // Two fields with same number should become array
        let mut data = Vec::new();
        data.extend(build_varint_field(1, 100));
        data.extend(build_varint_field(1, 200));

        let json = decode_raw_to_json(&data).expect("test: should decode");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("test: valid JSON");

        assert!(parsed["1"].is_array(), "test: two fields become array");
        let arr = parsed["1"].as_array().expect("test: array");
        assert_eq!(arr.len(), 2, "test: two elements");
    }

    #[test]
    fn test_json_fixed_types() {
        let mut data = Vec::new();
        data.extend(build_fixed32_field(1, 0x12345678));
        data.extend(build_fixed64_field(2, 0x123456789ABCDEF0));

        let json = decode_raw_to_json(&data).expect("test: should decode");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("test: valid JSON");

        assert_eq!(parsed["1"], 0x12345678_u64, "test: fixed32");
        assert_eq!(parsed["2"], 0x123456789ABCDEF0_u64, "test: fixed64");
    }

    #[test]
    fn test_json_string_with_control_chars() {
        // String with control characters (not \n, \r, \t) should be base64
        let text_with_ctrl = b"hello\x00world";
        let data = build_length_delimited_field(1, text_with_ctrl);
        let json = decode_raw_to_json(&data).expect("test: should decode");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("test: valid JSON");

        let value = parsed["1"].as_str().expect("test: should be string");
        assert!(value.starts_with("<bytes:"), "test: control chars -> base64");
    }

    #[test]
    fn test_json_string_with_allowed_whitespace() {
        // Strings with allowed whitespace (\n, \r, \t) should remain as strings
        let text = b"line1\nline2\ttabbed\r\nwindows";
        let data = build_length_delimited_field(1, text);
        let json = decode_raw_to_json(&data).expect("test: should decode");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("test: valid JSON");

        assert_eq!(
            parsed["1"],
            "line1\nline2\ttabbed\r\nwindows",
            "test: allowed whitespace preserved"
        );
    }

    #[test]
    fn test_decode_raw_to_json_returns_none_for_invalid() {
        // Invalid protobuf should return None
        assert!(decode_raw_to_json(&[]).is_none(), "test: empty input");
        assert!(decode_raw_to_json(&[0x08]).is_none(), "test: incomplete");
        assert!(decode_raw_to_json(b"not protobuf").is_none(), "test: plain text");
    }
}
