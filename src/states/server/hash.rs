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

//! Redis HASH data type operations module.
//!
//! This module provides functionality for managing Redis HASH operations including:
//! - Loading HASH field-value pairs with pagination support via HSCAN
//! - Adding/updating fields in a HASH (HSET)
//! - Removing fields from a HASH (HDEL)
//! - Filtering HASH fields with pattern matching
//! - Efficient incremental loading for large HASHes

use super::{
    KeyType, RedisValueData, ServerTask, ZedisServerState,
    value::{RedisHashValue, RedisValue, RedisValueStatus},
};
use crate::{
    connection::{RedisAsyncConn, get_connection_manager},
    error::Error,
    helpers::codec::{CompressionFormat, MAX_DECOMPRESS_BYTES, decompress, detect},
    states::{NotificationAction, ServerEvent, i18n_hash_editor},
};
use bytes::Bytes;
use gpui::{SharedString, prelude::*};
use redis::cmd;
use std::sync::Arc;
use tracing::{debug, error, info};

type Result<T, E = Error> = std::result::Result<T, E>;

/// Type alias for HSCAN result: (cursor, vec of (field, value) pairs as bytes)
type HashScanValue = (u64, Vec<(Vec<u8>, Vec<u8>)>);

fn unique_hash_fields(fields: Vec<SharedString>) -> Vec<SharedString> {
    let mut unique = Vec::with_capacity(fields.len());
    for field in fields {
        if !unique.contains(&field) {
            unique.push(field);
        }
    }
    unique
}

fn is_current_hash_pagination(hash: &RedisHashValue, keyword: Option<&SharedString>, cursor: u64) -> bool {
    hash.keyword.as_ref() == keyword && hash.cursor == cursor
}

/// Convert raw bytes into the table preview string while preserving compressed data previews.
fn bytes_to_display_string(bytes: &[u8]) -> String {
    let detection = detect(bytes);
    let data = if detection.compression != CompressionFormat::None {
        decompress(bytes, detection.compression, MAX_DECOMPRESS_BYTES).unwrap_or_else(|_| bytes.to_vec())
    } else {
        bytes.to_vec()
    };

    String::from_utf8_lossy(&data).to_string()
}

/// Retrieves HASH field-value pairs using Redis HSCAN command for cursor-based pagination.
///
/// # Arguments
/// * `conn` - Redis async connection
/// * `key` - The HASH key to scan
/// * `keyword` - Optional filter keyword for field names (will be wrapped with wildcards)
/// * `cursor` - Current cursor position (0 to start, returned cursor to continue)
/// * `count` - Hint for number of field-value pairs to return per iteration
///
/// # Returns
/// A tuple of (next_cursor, field-value pairs) where next_cursor is 0 when scan is complete
async fn get_redis_hash_value(
    conn: &mut RedisAsyncConn,
    key: &str,
    keyword: Option<SharedString>,
    cursor: u64,
    count: usize,
) -> Result<(u64, Vec<(SharedString, SharedString)>)> {
    // Build pattern: wrap keyword with wildcards or match all fields
    let pattern = keyword
        .as_ref()
        .map(|kw| format!("*{}*", kw))
        .unwrap_or_else(|| "*".to_string());

    // Execute HSCAN with MATCH and COUNT options
    let (next_cursor, raw_values): HashScanValue = cmd("HSCAN")
        .arg(key)
        .arg(cursor)
        .arg("MATCH")
        .arg(pattern)
        .arg("COUNT")
        .arg(count)
        .query_async(conn)
        .await?;

    // Early return if no values found
    if raw_values.is_empty() {
        return Ok((next_cursor, vec![]));
    }

    // Convert bytes to UTF-8 strings (lossy conversion for non-UTF8 data)
    let values = raw_values
        .iter()
        .map(|(field, value)| {
            (
                String::from_utf8_lossy(field).to_string().into(),
                String::from_utf8_lossy(value).to_string().into(),
            )
        })
        .collect();

    Ok((next_cursor, values))
}

/// Performs initial load of a Redis HASH value.
///
/// Fetches the total number of fields (HLEN) and loads the first batch of field-value
/// pairs (up to 100). This is called when a HASH key is first opened in the editor.
///
/// # Arguments
/// * `conn` - Redis async connection
/// * `key` - The HASH key to load
///
/// # Returns
/// A `RedisValue` containing HASH metadata and initial field-value pairs
pub(crate) async fn first_load_hash_value(conn: &mut RedisAsyncConn, key: &str) -> Result<RedisValue> {
    // Get total number of fields in the HASH
    let size: usize = cmd("HLEN").arg(key).query_async(conn).await?;

    // Load first batch of field-value pairs (up to 100)
    let (cursor, values) = get_redis_hash_value(conn, key, None, 0, 100).await?;

    // If cursor is 0, all values have been loaded in one iteration
    let done = cursor == 0;

    Ok(RedisValue {
        key_type: KeyType::Hash,
        data: Some(RedisValueData::Hash(Arc::new(RedisHashValue {
            cursor,
            size,
            values,
            done,
            ..Default::default()
        }))),
        ..Default::default()
    })
}
impl ZedisServerState {
    /// Adds or updates a field-value pair in the Redis HASH.
    ///
    /// Uses HSET command which creates a new field or updates an existing one.
    /// Updates the UI state and shows appropriate notifications based on whether
    /// it was a new field (count=1) or an update (count=0).
    ///
    /// # Arguments
    /// * `new_field` - The field name to add/update
    /// * `new_value` - The value to set for the field
    /// * `cx` - GPUI context for spawning async tasks and UI updates
    pub fn add_hash_value(&mut self, new_field: SharedString, new_value: SharedString, cx: &mut Context<Self>) {
        self.add_or_update_hash_value(new_field, new_value, cx);
    }
    /// Updates a field-value pair in the Redis HASH.
    ///
    /// Uses HSET command to update the value of the specified field.
    ///
    /// # Arguments
    /// * `new_field` - The field name to update
    /// * `new_value` - The value to set for the field
    /// * `cx` - GPUI context for spawning async tasks and UI updates
    pub fn update_hash_value(&mut self, new_field: SharedString, new_value: SharedString, cx: &mut Context<Self>) {
        self.add_or_update_hash_value(new_field, new_value, cx);
    }

    /// Fetch raw bytes for a HASH field and emit an event when the edit dialog can open.
    pub fn fetch_hash_value_for_edit(&mut self, field: SharedString, cx: &mut Context<Self>) {
        let Some((key, _)) = self.try_get_mut_key_value() else {
            return;
        };

        let server_id = self.server_id.clone();
        let db = self.db;
        let key_for_task = key.clone();
        let key_for_event = key.clone();
        let field_clone = field.clone();
        info!(
            key = %key,
            field = %field,
            "Fetching Redis hash field bytes for dialog editing"
        );

        self.spawn(
            ServerTask::UpdateHashValue,
            move || async move {
                let mut conn = get_connection_manager().get_connection(&server_id, db).await?;
                let bytes: Vec<u8> = cmd("HGET")
                    .arg(key_for_task.as_str())
                    .arg(field.as_str())
                    .query_async(&mut conn)
                    .await?;
                Ok(bytes)
            },
            move |this, result, cx| {
                if this.key.as_ref() != Some(&key_for_event) {
                    let current_key = this.key.clone().unwrap_or_default();
                    debug!(
                        expected_key = key_for_event.as_str(),
                        current_key = current_key.as_str(),
                        field = field_clone.as_str(),
                        "Skip stale Redis hash edit dialog because selected key changed"
                    );
                    cx.notify();
                    return;
                }

                match result {
                    Ok(bytes) => {
                        info!(
                            key = %key_for_event,
                            field = %field_clone,
                            bytes_len = bytes.len(),
                            "Redis hash field bytes are ready for dialog editing"
                        );
                        cx.emit(ServerEvent::HashEditDialogReady(key_for_event, field_clone, bytes));
                    }
                    Err(err) => {
                        error!(
                            key = %key_for_event,
                            field = %field_clone,
                            error = %err,
                            "Failed to fetch Redis hash field bytes for dialog editing"
                        );
                        cx.emit(ServerEvent::ErrorOccurred(crate::states::ErrorMessage {
                            category: "fetch_hash_value_for_edit".into(),
                            message: err.to_string().into(),
                            created_at: crate::helpers::unix_ts(),
                        }));
                    }
                }
                cx.notify();
            },
            cx,
        );
    }

    /// Update a HASH field with raw bytes from the value edit dialog.
    pub fn update_hash_value_bytes(
        &mut self,
        expected_key: SharedString,
        field: SharedString,
        new_bytes: Bytes,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.key.as_ref() != Some(&expected_key) {
            let current_key = self.key.clone().unwrap_or_default();
            debug!(
                expected_key = expected_key.as_str(),
                current_key = current_key.as_str(),
                field = field.as_str(),
                "Reject Redis hash field save because selected key changed"
            );
            return false;
        }

        let Some((key, value)) = self.try_get_mut_key_value() else {
            return false;
        };

        value.status = RedisValueStatus::Updating;
        let old_value = value.hash_value().and_then(|hash| {
            hash.values
                .iter()
                .find(|(item_field, _)| item_field == &field)
                .map(|(_, item_value)| item_value.clone())
        });

        let new_string: SharedString = bytes_to_display_string(&new_bytes).into();
        if let Some(RedisValueData::Hash(hash_data)) = value.data.as_mut() {
            let hash = Arc::make_mut(hash_data);
            if let Some((_, item_value)) = hash.values.iter_mut().find(|(item_field, _)| item_field == &field) {
                *item_value = new_string.clone();
                cx.emit(ServerEvent::ValueUpdated(key.clone()));
            }
        }
        cx.notify();

        let server_id = self.server_id.clone();
        let db = self.db;
        let key_clone = key.clone();
        let field_clone = field.clone();
        let new_bytes_vec = new_bytes.to_vec();
        info!(
            key = %key,
            field = %field,
            bytes_len = new_bytes_vec.len(),
            "Saving Redis hash field bytes from dialog editor"
        );

        self.spawn(
            ServerTask::UpdateHashValue,
            move || async move {
                let mut conn = get_connection_manager().get_connection(&server_id, db).await?;
                let _: () = cmd("HSET")
                    .arg(key.as_str())
                    .arg(field.as_str())
                    .arg(new_bytes_vec)
                    .query_async(&mut conn)
                    .await?;
                Ok(())
            },
            move |this, result, cx| {
                let key_still_selected = this.key.as_ref() == Some(&key_clone);
                if key_still_selected {
                    if let Some(value) = this.value.as_mut() {
                        value.status = RedisValueStatus::Idle;
                    }
                } else {
                    let current_key = this.key.clone().unwrap_or_default();
                    debug!(
                        expected_key = key_clone.as_str(),
                        current_key = current_key.as_str(),
                        field = field_clone.as_str(),
                        "Skip Redis hash field local update because selected key changed"
                    );
                }
                if let Err(err) = result {
                    error!(
                        key = %key_clone,
                        field = %field_clone,
                        error = %err,
                        "Failed to save Redis hash field bytes"
                    );
                    if key_still_selected
                        && let Some(original) = old_value
                        && let Some(RedisValueData::Hash(hash_data)) = this.value.as_mut().and_then(|v| v.data.as_mut())
                    {
                        let hash = Arc::make_mut(hash_data);
                        if let Some((_, item_value)) = hash
                            .values
                            .iter_mut()
                            .find(|(item_field, _)| item_field == &field_clone)
                        {
                            *item_value = original;
                        }
                    }
                    cx.emit(ServerEvent::ErrorOccurred(crate::states::ErrorMessage {
                        category: "update_hash_value".into(),
                        message: err.to_string().into(),
                        created_at: crate::helpers::unix_ts(),
                    }));
                }
                if key_still_selected {
                    cx.emit(ServerEvent::ValueUpdated(key_clone));
                }
                cx.notify();
            },
            cx,
        );
        true
    }

    fn add_or_update_hash_value(&mut self, new_field: SharedString, new_value: SharedString, cx: &mut Context<Self>) {
        // Early return if no key/value is selected
        let Some((key, value)) = self.try_get_mut_key_value() else {
            return;
        };

        // Update UI state to show "updating" status
        value.status = RedisValueStatus::Updating;
        cx.notify();

        let server_id = self.server_id.clone();
        let db = self.db;
        let key_clone = key.clone();
        let new_field_clone = new_field.clone();
        let new_value_clone = new_value.clone();

        self.spawn(
            ServerTask::AddSetValue,
            // Async operation: execute HSET on Redis
            move || async move {
                let mut conn = get_connection_manager().get_connection(&server_id, db).await?;

                // HSET returns 1 if new field created, 0 if existing field updated
                let count: usize = cmd("HSET")
                    .arg(key.as_str())
                    .arg(new_field.as_str())
                    .arg(new_value.as_str())
                    .query_async(&mut conn)
                    .await?;
                Ok(count)
            },
            // UI callback: handle result and update local state
            move |this, result, cx| {
                let title = i18n_hash_editor(cx, "add_value_success");
                let msg = i18n_hash_editor(cx, "add_value_success_tips");
                let update_exist_field_value_msg = i18n_hash_editor(cx, "update_exist_field_value_success_tips");

                if let Some(value) = this.value.as_mut() {
                    value.status = RedisValueStatus::Idle;

                    if let Ok(count) = result
                        && let Some(RedisValueData::Hash(hash_data)) = value.data.as_mut()
                    {
                        let hash = Arc::make_mut(hash_data);

                        // Increment size only if new field was created
                        hash.size += count;

                        // Update existing field value in local state if field already exists
                        for item in hash.values.iter_mut() {
                            if item.0 == new_field_clone {
                                item.1 = new_value_clone.clone();
                                break;
                            }
                        }

                        // Show different notifications based on operation type
                        if count == 0 {
                            // Existing field was updated
                            cx.emit(ServerEvent::Notification(NotificationAction::new_info(
                                update_exist_field_value_msg,
                            )));
                        } else {
                            // New field was created
                            cx.emit(ServerEvent::Notification(
                                NotificationAction::new_success(msg).with_title(title),
                            ));
                        }

                        cx.emit(ServerEvent::ValueAdded(key_clone));
                    }
                }
                cx.notify();
            },
            cx,
        );
    }
    /// Applies a filter to HASH fields by resetting the scan state with a keyword.
    ///
    /// Creates a new HASH value state with the filter keyword and triggers a load.
    /// This allows users to search for specific fields matching a pattern.
    ///
    /// # Arguments
    /// * `keyword` - The search keyword to filter field names (will be wrapped with wildcards)
    /// * `cx` - GPUI context for UI updates
    pub fn filter_hash_value(&mut self, keyword: SharedString, cx: &mut Context<Self>) -> bool {
        let Some((key, value)) = self.try_get_mut_key_value() else {
            return false;
        };
        let Some(hash) = value.hash_value() else {
            return false;
        };
        let filter_keyword = if keyword.is_empty() {
            None
        } else {
            Some(keyword.clone())
        };

        // Create new HASH state with filter keyword, reset cursor to start fresh scan
        let new_hash = RedisHashValue {
            keyword: filter_keyword,
            size: hash.size,
            ..Default::default()
        };
        value.data = Some(RedisValueData::Hash(Arc::new(new_hash)));
        self.remember_value_filter_keyword_for_key(key.as_str(), keyword);

        // Trigger load with the new filter
        self.load_more_hash_value(cx);
        true
    }
    /// Removes a field from the Redis HASH.
    ///
    /// Uses HDEL command to delete the specified field and updates both the
    /// Redis field count and the local UI state.
    ///
    /// # Arguments
    /// * `remove_field` - The field name to remove from the HASH
    /// * `cx` - GPUI context for spawning async tasks and UI updates
    pub fn remove_hash_value(&mut self, remove_field: SharedString, cx: &mut Context<Self>) {
        let Some((key, value)) = self.try_get_mut_key_value() else {
            return;
        };

        // Update UI state to show loading
        value.status = RedisValueStatus::Loading;
        cx.notify();

        let server_id = self.server_id.clone();
        let db = self.db;
        let remove_field_clone = remove_field.clone();
        let key_clone = key.clone();

        self.spawn(
            ServerTask::RemoveHashValue,
            // Async operation: execute HDEL on Redis
            move || async move {
                let mut conn = get_connection_manager().get_connection(&server_id, db).await?;

                // HDEL returns number of fields removed (0 if doesn't exist, 1 if removed)
                let count: usize = cmd("HDEL")
                    .arg(key.as_str())
                    .arg(remove_field.as_str())
                    .query_async(&mut conn)
                    .await?;
                Ok(count)
            },
            // UI callback: update local state to reflect removal
            move |this, result, cx| {
                if let Ok(count) = result {
                    // Only update if field was actually removed
                    if count != 0
                        && let Some(RedisValueData::Hash(hash_data)) = this.value.as_mut().and_then(|v| v.data.as_mut())
                    {
                        let hash = Arc::make_mut(hash_data);

                        // Remove from local field-value list
                        hash.values.retain(|(field, _)| field != &remove_field_clone);

                        // Decrease HASH size by number of removed fields
                        hash.size -= count;
                    }

                    cx.emit(ServerEvent::ValueUpdated(key_clone));

                    // Reset status to idle
                    if let Some(value) = this.value.as_mut() {
                        value.status = RedisValueStatus::Idle;
                    }
                    cx.notify();
                }
            },
            cx,
        );
    }

    pub fn remove_hash_values(&mut self, remove_fields: Vec<SharedString>, cx: &mut Context<Self>) {
        let remove_fields = unique_hash_fields(remove_fields);
        if remove_fields.is_empty() {
            debug!("Skip batch hash removal because no fields were selected");
            return;
        }
        if remove_fields.len() == 1 {
            self.remove_hash_value(remove_fields[0].clone(), cx);
            return;
        }

        let Some((key, value)) = self.try_get_mut_key_value() else {
            return;
        };

        value.status = RedisValueStatus::Loading;
        cx.notify();

        let server_id = self.server_id.clone();
        let db = self.db;
        let key_clone = key.clone();
        let fields_for_task = remove_fields.clone();
        info!(
            key = %key,
            count = remove_fields.len(),
            "Removing multiple Redis hash fields"
        );

        self.spawn(
            ServerTask::RemoveHashValues,
            move || async move {
                let mut conn = get_connection_manager().get_connection(&server_id, db).await?;
                let mut binding = cmd("HDEL");
                binding.arg(key.as_str());
                for field in &fields_for_task {
                    binding.arg(field.as_str());
                }
                let count: usize = binding.query_async(&mut conn).await?;
                Ok(count)
            },
            move |this, result, cx| {
                if let Some(value) = this.value.as_mut() {
                    if let Ok(count) = result {
                        if count != 0
                            && let Some(RedisValueData::Hash(hash_data)) = value.data.as_mut()
                        {
                            let hash = Arc::make_mut(hash_data);
                            hash.values.retain(|(field, _)| !remove_fields.contains(field));
                            hash.size = hash.size.saturating_sub(count);
                            info!(
                                key = %key_clone,
                                removed = count,
                                "Removed multiple Redis hash fields"
                            );
                        }
                        cx.emit(ServerEvent::ValueUpdated(key_clone));
                    }
                    value.status = RedisValueStatus::Idle;
                }
                cx.notify();
            },
            cx,
        );
    }

    /// Loads the next batch of HASH field-value pairs using cursor-based pagination.
    ///
    /// Uses HSCAN to incrementally load field-value pairs without blocking on large HASHes.
    /// When filtering is active, uses larger batch sizes (1000) for better performance.
    ///
    /// # Arguments
    /// * `cx` - GPUI context for spawning async tasks and UI updates
    pub fn load_more_hash_value(&mut self, cx: &mut Context<Self>) {
        let Some((key, value)) = self.try_get_mut_key_value() else {
            return;
        };

        // Update UI to show loading state
        value.status = RedisValueStatus::Loading;
        cx.notify();

        // Extract current cursor and filter keyword from HASH state
        let (cursor, keyword) = match value.hash_value() {
            Some(hash) => (hash.cursor, hash.keyword.clone()),
            None => return,
        };
        let request_cursor = cursor;
        let request_keyword = keyword.clone();

        let server_id = self.server_id.clone();
        let db = self.db;
        cx.emit(ServerEvent::ValuePaginationStarted(key.clone()));

        let key_clone = key.clone();

        self.spawn(
            ServerTask::LoadMoreValue,
            // Async operation: fetch next batch using HSCAN
            move || async move {
                let mut conn = get_connection_manager().get_connection(&server_id, db).await?;

                // Use larger batch size when filtering to reduce round trips
                let count = if request_keyword.is_some() { 1000 } else { 100 };

                get_redis_hash_value(&mut conn, &key, request_keyword, request_cursor, count).await
            },
            // UI callback: merge results into local state
            move |this, result, cx| {
                if this.key.as_ref() != Some(&key_clone) {
                    let current_key = this.key.clone().unwrap_or_default();
                    tracing::debug!(
                        expected_key = key_clone.as_str(),
                        current_key = current_key.as_str(),
                        "Skip stale hash value pagination result"
                    );
                    return;
                }

                let mut should_load_more = false;
                if let Ok((new_cursor, new_values)) = result
                    && let Some(RedisValueData::Hash(hash_data)) = this.value.as_mut().and_then(|v| v.data.as_mut())
                {
                    let hash = Arc::make_mut(hash_data);
                    if !is_current_hash_pagination(hash, keyword.as_ref(), cursor) {
                        tracing::debug!(
                            key = key_clone.as_str(),
                            request_cursor = cursor,
                            current_cursor = hash.cursor,
                            request_keyword_len = keyword.as_ref().map(|keyword| keyword.as_str().len()).unwrap_or(0),
                            current_keyword_len =
                                hash.keyword.as_ref().map(|keyword| keyword.as_str().len()).unwrap_or(0),
                            "Skip stale hash value pagination result because filter state changed"
                        );
                        return;
                    }
                    hash.cursor = new_cursor;

                    // Mark as done when cursor returns to 0 (scan complete)
                    if new_cursor == 0 {
                        hash.done = true;
                    }

                    // Append new field-value pairs to existing list
                    if !new_values.is_empty() {
                        hash.values.extend(new_values);
                    }
                    if !hash.done && hash.values.len() < 50 {
                        should_load_more = true;
                    }
                }

                cx.emit(ServerEvent::ValuePaginationFinished(key_clone));

                // Reset status to idle
                if let Some(value) = this.value.as_mut() {
                    value.status = RedisValueStatus::Idle;
                }
                cx.notify();
                if should_load_more {
                    this.load_more_hash_value(cx);
                }
            },
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{RedisHashValue, is_current_hash_pagination, unique_hash_fields};
    use gpui::SharedString;

    #[test]
    fn keeps_hash_fields_unique_for_batch_removal() {
        let fields = vec![
            SharedString::from("a"),
            SharedString::from("b"),
            SharedString::from("a"),
        ];

        assert_eq!(
            unique_hash_fields(fields),
            vec![SharedString::from("a"), SharedString::from("b")]
        );
    }

    #[test]
    fn accepts_hash_pagination_for_matching_filter_state() {
        let keyword = SharedString::from("field");
        let hash = RedisHashValue {
            keyword: Some(keyword.clone()),
            cursor: 42,
            ..Default::default()
        };

        assert!(is_current_hash_pagination(&hash, Some(&keyword), 42));
    }

    #[test]
    fn rejects_hash_pagination_when_filter_state_changed() {
        let old_keyword = SharedString::from("old");
        let hash = RedisHashValue {
            keyword: Some(SharedString::from("new")),
            cursor: 0,
            ..Default::default()
        };

        assert!(!is_current_hash_pagination(&hash, Some(&old_keyword), 42));
    }
}
