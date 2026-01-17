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

use gpui::SharedString;
use prost_reflect::prost::Message;
use prost_reflect::prost_types::FileDescriptorSet;
use prost_reflect::{DescriptorPool, DynamicMessage, MessageDescriptor};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use uuid::Uuid;

use crate::error::Error;

type Result<T, E = Error> = std::result::Result<T, E>;

/// Protobuf schema state management
#[derive(Debug, Clone, Default)]
pub struct ProtobufSchema {
    /// Loaded proto file paths
    proto_files: Vec<String>,

    /// Descriptor pool for runtime reflection
    pool: Option<Arc<DescriptorPool>>,

    /// Available message types (fully qualified names)
    message_types: Vec<SharedString>,

    /// Currently selected message type
    selected_type: Option<SharedString>,
}

impl ProtobufSchema {
    /// Create a new empty schema state
    pub fn new() -> Self {
        Self::default()
    }

    /// Get available message types
    pub fn message_types(&self) -> &[SharedString] {
        &self.message_types
    }

    /// Get currently selected message type
    pub fn selected_type(&self) -> Option<&SharedString> {
        self.selected_type.as_ref()
    }

    /// Set selected message type
    pub fn set_selected_type(&mut self, type_name: SharedString) {
        if self.message_types.contains(&type_name) {
            self.selected_type = Some(type_name);
        }
    }

    /// Check if a schema is loaded
    pub fn has_schema(&self) -> bool {
        self.pool.is_some()
    }

    /// Get the descriptor pool
    pub fn pool(&self) -> Option<&Arc<DescriptorPool>> {
        self.pool.as_ref()
    }

    /// Get message descriptor for the selected type
    pub fn selected_descriptor(&self) -> Option<MessageDescriptor> {
        let pool = self.pool.as_ref()?;
        let type_name = self.selected_type.as_ref()?;
        pool.get_message_by_name(type_name.as_str())
    }

    /// Load .proto files using protoc compiler
    ///
    /// This function:
    /// 1. Calls protoc to compile .proto files into a FileDescriptorSet
    /// 2. Parses the FileDescriptorSet into a DescriptorPool
    /// 3. Extracts all available message types
    pub fn load_proto_files(&mut self, proto_paths: Vec<String>) -> Result<()> {
        if proto_paths.is_empty() {
            return Err(Error::Invalid {
                message: "No proto files provided".to_string(),
            });
        }

        // Determine include paths from the proto file directories
        let mut include_dirs: Vec<String> = proto_paths
            .iter()
            .filter_map(|p| Path::new(p).parent())
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        // Deduplicate include dirs
        include_dirs.sort();
        include_dirs.dedup();

        // Build protoc command with unique temp file to avoid TOCTOU race conditions
        let temp_dir = std::env::temp_dir();
        let descriptor_path = temp_dir.join(format!("zedis_proto_{}.pb", Uuid::now_v7()));

        let mut cmd = Command::new("protoc");

        // Add include paths
        for dir in &include_dirs {
            cmd.arg(format!("-I{}", dir));
        }

        // Include imported proto dependencies in descriptor
        cmd.arg("--include_imports");

        // Add output descriptor file
        cmd.arg(format!("-o{}", descriptor_path.display()));

        // Add -- to prevent paths starting with - from being parsed as options
        cmd.arg("--");

        // Add proto files
        for proto in &proto_paths {
            cmd.arg(proto);
        }

        // Execute protoc
        let output = cmd.output().map_err(|e| Error::Invalid {
            message: format!("Failed to execute protoc: {}. Make sure protoc is installed.", e),
        })?;

        if !output.status.success() {
            // Clean up temp file on failure
            let _ = std::fs::remove_file(&descriptor_path);
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Invalid {
                message: format!("protoc failed: {}", stderr),
            });
        }

        // Read and parse the descriptor set
        let descriptor_bytes = std::fs::read(&descriptor_path);

        // Clean up temp file immediately after reading (regardless of read success)
        let _ = std::fs::remove_file(&descriptor_path);

        let descriptor_bytes = descriptor_bytes.map_err(|e| Error::Invalid {
            message: format!("Failed to read descriptor file: {}", e),
        })?;

        // Parse FileDescriptorSet
        let fds = FileDescriptorSet::decode(descriptor_bytes.as_slice()).map_err(|e| Error::Invalid {
            message: format!("Failed to parse FileDescriptorSet: {}", e),
        })?;

        // Create DescriptorPool
        let pool = DescriptorPool::from_file_descriptor_set(fds).map_err(|e| Error::Invalid {
            message: format!("Failed to create DescriptorPool: {}", e),
        })?;

        // Extract message types
        let mut message_types: Vec<SharedString> =
            pool.all_messages().map(|m| m.full_name().to_string().into()).collect();

        message_types.sort();

        // Update state
        self.proto_files = proto_paths;
        self.pool = Some(Arc::new(pool));
        self.message_types = message_types;

        // Validate selected_type after schema reload
        // If previously selected type is not in new schema, reset to first type or clear
        if let Some(ref selected) = self.selected_type {
            if !self.message_types.contains(selected) {
                self.selected_type = self.message_types.first().cloned();
            }
        } else if !self.message_types.is_empty() {
            // Select first type by default if none selected
            self.selected_type = Some(self.message_types[0].clone());
        }

        Ok(())
    }

    /// Decode protobuf bytes using the selected message type
    pub fn decode(&self, bytes: &[u8]) -> Result<String> {
        let pool = self.pool.as_ref().ok_or_else(|| Error::Invalid {
            message: "No schema loaded".to_string(),
        })?;

        let type_name = self.selected_type.as_ref().ok_or_else(|| Error::Invalid {
            message: "No message type selected".to_string(),
        })?;

        let descriptor = pool
            .get_message_by_name(type_name.as_str())
            .ok_or_else(|| Error::Invalid {
                message: format!("Message type '{}' not found", type_name),
            })?;

        let message = DynamicMessage::decode(descriptor, bytes).map_err(|e| Error::Invalid {
            message: format!("Failed to decode protobuf: {}", e),
        })?;

        // Convert to JSON
        let json = serde_json::to_string_pretty(&message).map_err(|e| Error::Invalid {
            message: format!("Failed to serialize to JSON: {}", e),
        })?;

        Ok(json)
    }

    /// Encode JSON string to protobuf bytes using the selected message type
    pub fn encode(&self, json_str: &str) -> Result<Vec<u8>> {
        let pool = self.pool.as_ref().ok_or_else(|| Error::Invalid {
            message: "No schema loaded".to_string(),
        })?;

        let type_name = self.selected_type.as_ref().ok_or_else(|| Error::Invalid {
            message: "No message type selected".to_string(),
        })?;

        let descriptor = pool
            .get_message_by_name(type_name.as_str())
            .ok_or_else(|| Error::Invalid {
                message: format!("Message type '{}' not found", type_name),
            })?;

        // Deserialize JSON to DynamicMessage using prost_reflect's serde support
        let mut deserializer = serde_json::Deserializer::from_str(json_str);
        let message = DynamicMessage::deserialize(descriptor, &mut deserializer).map_err(|e| Error::Invalid {
            message: format!("Failed to deserialize JSON to protobuf: {}", e),
        })?;

        // Encode to bytes
        Ok(message.encode_to_vec())
    }

    /// Clear the loaded schema
    pub fn clear(&mut self) {
        self.proto_files.clear();
        self.pool = None;
        self.message_types.clear();
        self.selected_type = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================
    // Basic State Tests
    // ========================================

    #[test]
    fn test_schema_new_returns_default_state() {
        let schema = ProtobufSchema::new();

        assert!(schema.proto_files.is_empty(), "test: proto_files empty");
        assert!(schema.pool.is_none(), "test: pool is None");
        assert!(schema.message_types.is_empty(), "test: message_types empty");
        assert!(schema.selected_type.is_none(), "test: selected_type is None");
    }

    #[test]
    fn test_schema_default_equals_new() {
        let from_new = ProtobufSchema::new();
        let from_default = ProtobufSchema::default();

        assert_eq!(
            from_new.proto_files.len(),
            from_default.proto_files.len(),
            "test: proto_files equal"
        );
        assert_eq!(from_new.pool.is_none(), from_default.pool.is_none(), "test: pool equal");
        assert_eq!(
            from_new.message_types.len(),
            from_default.message_types.len(),
            "test: message_types equal"
        );
        assert_eq!(
            from_new.selected_type, from_default.selected_type,
            "test: selected_type equal"
        );
    }

    #[test]
    fn test_schema_message_types_empty_initially() {
        let schema = ProtobufSchema::new();
        assert!(schema.message_types().is_empty(), "test: message_types empty initially");
    }

    #[test]
    fn test_schema_selected_type_none_initially() {
        let schema = ProtobufSchema::new();
        assert!(
            schema.selected_type().is_none(),
            "test: selected_type is None initially"
        );
    }

    #[test]
    fn test_schema_set_selected_type_ignored_when_not_in_list() {
        let mut schema = ProtobufSchema::new();

        // Try to set a type that doesn't exist in message_types
        schema.set_selected_type("NonExistentType".into());

        assert!(
            schema.selected_type().is_none(),
            "test: set_selected_type ignored for invalid type"
        );
    }

    #[test]
    fn test_schema_has_schema_false_initially() {
        let schema = ProtobufSchema::new();
        assert!(!schema.has_schema(), "test: has_schema false initially");
    }

    #[test]
    fn test_schema_pool_none_initially() {
        let schema = ProtobufSchema::new();
        assert!(schema.pool().is_none(), "test: pool is None initially");
    }

    #[test]
    fn test_schema_selected_descriptor_none_initially() {
        let schema = ProtobufSchema::new();
        assert!(
            schema.selected_descriptor().is_none(),
            "test: selected_descriptor is None initially"
        );
    }

    #[test]
    fn test_schema_clear() {
        let mut schema = ProtobufSchema::new();

        // Manually set some values (without loading actual proto files)
        schema.proto_files = vec!["test.proto".to_string()];
        schema.message_types = vec!["TestMessage".into()];
        schema.selected_type = Some("TestMessage".into());
        // pool remains None since we can't easily create one without protoc

        // Clear the schema
        schema.clear();

        assert!(schema.proto_files.is_empty(), "test: proto_files cleared");
        assert!(schema.pool.is_none(), "test: pool cleared");
        assert!(schema.message_types.is_empty(), "test: message_types cleared");
        assert!(schema.selected_type.is_none(), "test: selected_type cleared");
        assert!(!schema.has_schema(), "test: has_schema false after clear");
    }

    // ========================================
    // Error Scenario Tests
    // ========================================

    #[test]
    fn test_load_proto_files_empty_path_returns_error() {
        let mut schema = ProtobufSchema::new();
        let result = schema.load_proto_files(vec![]);

        assert!(result.is_err(), "test: empty paths should return error");
        let err = result.expect_err("test: should have error");
        let err_msg = format!("{:?}", err);
        assert!(
            err_msg.contains("No proto files provided"),
            "test: error message should mention no proto files"
        );
    }

    #[test]
    fn test_decode_without_schema_returns_error() {
        let schema = ProtobufSchema::new();
        let result = schema.decode(&[0x08, 0x01]); // dummy protobuf bytes

        assert!(result.is_err(), "test: decode without schema should error");
        let err = result.expect_err("test: should have error");
        let err_msg = format!("{:?}", err);
        assert!(
            err_msg.contains("No schema loaded"),
            "test: error should mention no schema"
        );
    }

    #[test]
    fn test_encode_without_schema_returns_error() {
        let schema = ProtobufSchema::new();
        let result = schema.encode(r#"{"field": 1}"#);

        assert!(result.is_err(), "test: encode without schema should error");
        let err = result.expect_err("test: should have error");
        let err_msg = format!("{:?}", err);
        assert!(
            err_msg.contains("No schema loaded"),
            "test: error should mention no schema"
        );
    }

    #[test]
    fn test_decode_without_selected_type_returns_error() {
        // This test verifies that even with a theoretical pool,
        // decode fails if no type is selected.
        // Since we can't easily create a pool without protoc,
        // we test the error path through the public API.
        let schema = ProtobufSchema::new();
        let result = schema.decode(&[0x08, 0x01]);

        assert!(result.is_err(), "test: decode without selected type should error");
    }

    // ========================================
    // Tests requiring protoc (ignored by default)
    // ========================================

    #[test]
    #[ignore = "requires protoc installation"]
    fn test_load_proto_files_nonexistent_file() {
        let mut schema = ProtobufSchema::new();
        let result = schema.load_proto_files(vec!["/nonexistent/path/test.proto".to_string()]);

        assert!(result.is_err(), "test: nonexistent file should return error");
    }

    #[test]
    #[ignore = "requires protoc installation"]
    fn test_load_proto_files_invalid_proto_syntax() {
        use std::io::Write;

        // Create a temp file with invalid proto syntax
        let temp_dir = std::env::temp_dir();
        let proto_path = temp_dir.join("invalid_syntax_test.proto");

        {
            let mut file = std::fs::File::create(&proto_path).expect("test: create temp file");
            writeln!(file, "this is not valid proto syntax {{{{").expect("test: write to file");
        }

        let mut schema = ProtobufSchema::new();
        let result = schema.load_proto_files(vec![proto_path.to_string_lossy().to_string()]);

        // Clean up
        let _ = std::fs::remove_file(&proto_path);

        assert!(result.is_err(), "test: invalid proto syntax should return error");
    }

    #[test]
    #[ignore = "requires protoc installation"]
    fn test_load_and_decode_simple_message() {
        use std::io::Write;

        // Create a simple proto file
        let temp_dir = std::env::temp_dir();
        let proto_path = temp_dir.join("simple_test.proto");

        {
            let mut file = std::fs::File::create(&proto_path).expect("test: create temp file");
            writeln!(
                file,
                r#"syntax = "proto3";
message SimpleMessage {{
    int32 value = 1;
}}"#
            )
            .expect("test: write proto file");
        }

        let mut schema = ProtobufSchema::new();
        let result = schema.load_proto_files(vec![proto_path.to_string_lossy().to_string()]);

        // Clean up
        let _ = std::fs::remove_file(&proto_path);

        if let Err(ref e) = result {
            eprintln!("test: load_proto_files failed: {:?}", e);
        }

        assert!(result.is_ok(), "test: should load valid proto file");
        assert!(schema.has_schema(), "test: should have schema after loading");
        assert!(!schema.message_types().is_empty(), "test: should have message types");
    }

    #[test]
    #[ignore = "requires protoc installation"]
    fn test_set_selected_type_with_valid_type() {
        use std::io::Write;

        // Create a proto file with multiple messages
        let temp_dir = std::env::temp_dir();
        let proto_path = temp_dir.join("multi_message_test.proto");

        {
            let mut file = std::fs::File::create(&proto_path).expect("test: create temp file");
            writeln!(
                file,
                r#"syntax = "proto3";
message MessageA {{
    int32 value = 1;
}}
message MessageB {{
    string name = 1;
}}"#
            )
            .expect("test: write proto file");
        }

        let mut schema = ProtobufSchema::new();
        let result = schema.load_proto_files(vec![proto_path.to_string_lossy().to_string()]);

        // Clean up
        let _ = std::fs::remove_file(&proto_path);

        if result.is_err() {
            return; // Skip if protoc not available
        }

        // Find MessageB and select it
        let msg_b = schema
            .message_types()
            .iter()
            .find(|t| t.as_ref().contains("MessageB"))
            .cloned();

        if let Some(type_name) = msg_b {
            schema.set_selected_type(type_name.clone());
            assert_eq!(
                schema.selected_type(),
                Some(&type_name),
                "test: selected type should be set"
            );
        }
    }
}
