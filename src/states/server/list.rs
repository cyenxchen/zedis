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

use super::{
    KeyType, RedisValueData, ServerTask, ZedisServerState,
    value::{RedisListValue, RedisValue, RedisValueStatus},
};
use crate::{
    connection::{RedisAsyncConn, get_connection_manager},
    error::Error,
    helpers::codec::{CompressionFormat, MAX_DECOMPRESS_BYTES, decompress, detect},
    states::ServerEvent,
};
use bytes::Bytes;
use gpui::{SharedString, prelude::*};
use redis::{cmd, pipe};
use std::sync::Arc;
use uuid::Uuid;

type Result<T, E = Error> = std::result::Result<T, E>;

/// Convert bytes to display string, handling compressed data.
///
/// Detects if the bytes are compressed and decompresses them before converting to string.
/// This ensures that compressed data is displayed correctly in the UI.
fn bytes_to_display_string(bytes: &[u8]) -> String {
    let detection = detect(bytes);

    // Try to decompress if compression is detected
    let data = if detection.compression != CompressionFormat::None {
        decompress(bytes, detection.compression, MAX_DECOMPRESS_BYTES).unwrap_or_else(|_| bytes.to_vec())
    } else {
        bytes.to_vec()
    };

    String::from_utf8_lossy(&data).to_string()
}

/// Fetch a range of elements from a Redis List.
///
/// Returns a vector of strings. Binary data is lossily converted to UTF-8.
async fn get_redis_list_value(conn: &mut RedisAsyncConn, key: &str, start: usize, stop: usize) -> Result<Vec<String>> {
    // Fetch raw bytes to handle binary data safely
    let value: Vec<Vec<u8>> = cmd("LRANGE").arg(key).arg(start).arg(stop).query_async(conn).await?;
    if value.is_empty() {
        return Ok(vec![]);
    }
    let value: Vec<String> = value.iter().map(|v| bytes_to_display_string(v)).collect();
    Ok(value)
}

/// Initial load for a List key.
/// Fetches the total length (LLEN) and the first 100 items.
pub(crate) async fn first_load_list_value(conn: &mut RedisAsyncConn, key: &str) -> Result<RedisValue> {
    let size: usize = cmd("LLEN").arg(key).query_async(conn).await?;
    let values = get_redis_list_value(conn, key, 0, 99).await?;
    Ok(RedisValue {
        key_type: KeyType::List,
        data: Some(RedisValueData::List(Arc::new(RedisListValue {
            size,
            values: values.into_iter().map(|v| v.into()).collect(),
            ..Default::default()
        }))),
        expire_at: None,
        ..Default::default()
    })
}

impl ZedisServerState {
    pub fn filter_list_value(&mut self, keyword: SharedString, cx: &mut Context<Self>) {
        let Some((_, value)) = self.try_get_mut_key_value() else {
            return;
        };
        let Some(list_value) = value.list_value() else {
            return;
        };
        let new_list_value = RedisListValue {
            keyword: Some(keyword.clone()),
            size: list_value.size,
            values: list_value.values.clone(),
        };
        value.data = Some(RedisValueData::List(Arc::new(new_list_value)));
        cx.emit(ServerEvent::ValueUpdated(self.key.clone().unwrap_or_default()));
    }
    pub fn remove_list_value(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some((key, value)) = self.try_get_mut_key_value() else {
            return;
        };
        value.status = RedisValueStatus::Updating;
        cx.notify();
        let server_id = self.server_id.clone();
        let db = self.db;
        let key_clone = key.clone();
        self.spawn(
            ServerTask::RemoveListValue,
            move || async move {
                let unique_marker = Uuid::new_v4().to_string();
                let mut conn = get_connection_manager().get_connection(&server_id, db).await?;
                let _: () = pipe()
                    .atomic()
                    .cmd("LSET")
                    .arg(key.as_str())
                    .arg(index)
                    .arg(&unique_marker)
                    .cmd("LREM")
                    .arg(key.as_str())
                    .arg(1)
                    .arg(&unique_marker)
                    .query_async(&mut conn)
                    .await?;

                Ok(())
            },
            move |this, result, cx| {
                if let Some(value) = this.value.as_mut() {
                    if result.is_ok()
                        && let Some(RedisValueData::List(list_data)) = value.data.as_mut()
                    {
                        let list = Arc::make_mut(list_data);
                        list.size -= 1;
                        // Only remove from local cache if index is within bounds
                        // (index may be out of bounds due to pagination/filtering)
                        if index < list.values.len() {
                            list.values.remove(index);
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
    pub fn push_list_value(&mut self, new_value: SharedString, mode: SharedString, cx: &mut Context<Self>) {
        let Some((key, value)) = self.try_get_mut_key_value() else {
            return;
        };
        let is_lpush = mode == "1";
        let mut pushed_value = false;
        value.status = RedisValueStatus::Updating;
        if let Some(RedisValueData::List(list_data)) = value.data.as_mut() {
            // Use Arc::make_mut to get mutable access (Cow behavior)
            let list = Arc::make_mut(list_data);
            if is_lpush {
                list.values.insert(0, new_value.clone());
                pushed_value = true;
            } else if list.values.len() == list.size {
                list.values.push(new_value.clone());
                pushed_value = true;
            }
            list.size += 1;
        }

        cx.notify();
        let server_id = self.server_id.clone();
        let db = self.db;
        let key_clone = key.clone();
        self.spawn(
            ServerTask::PushListValue,
            move || async move {
                let mut conn = get_connection_manager().get_connection(&server_id, db).await?;
                let cmd_name = if is_lpush { "LPUSH" } else { "RPUSH" };

                let _: () = cmd(cmd_name)
                    .arg(key.as_str())
                    .arg(new_value.as_str())
                    .query_async(&mut conn)
                    .await?;
                Ok(())
            },
            move |this, result, cx| {
                if let Some(value) = this.value.as_mut() {
                    value.status = RedisValueStatus::Idle;
                    if result.is_err()
                        && let Some(RedisValueData::List(list_data)) = this.value.as_mut().and_then(|v| v.data.as_mut())
                    {
                        // Use Arc::make_mut to get mutable access (Cow behavior)
                        let list = Arc::make_mut(list_data);
                        // Always rollback size since we always increment it optimistically
                        list.size -= 1;
                        // Only rollback values if we actually pushed
                        if pushed_value {
                            if is_lpush {
                                list.values.remove(0);
                            } else {
                                list.values.pop();
                            }
                        }
                    }
                }
                cx.emit(ServerEvent::ValueUpdated(key_clone));
                cx.notify();
            },
            cx,
        );
    }
    /// Update a specific item in a Redis List.
    ///
    /// Performs an optimistic lock check: verifies if the current value at `index`
    /// matches `original_value` before updating.
    pub fn update_list_value(
        &mut self,
        index: usize,
        original_value: SharedString,
        new_value: SharedString,
        cx: &mut Context<Self>,
    ) {
        let Some((key, value)) = self.try_get_mut_key_value() else {
            return;
        };
        value.status = RedisValueStatus::Updating;
        if let Some(RedisValueData::List(list_data)) = value.data.as_mut() {
            // Use Arc::make_mut to get mutable access (Cow behavior)
            let list = Arc::make_mut(list_data);
            if index < list.values.len() {
                list.values[index] = new_value.clone();
                cx.emit(ServerEvent::ValueUpdated(key.clone()));
            }
        }
        cx.notify();
        // Optimization: We don't clone the entire value here.
        // We only need basic info for the background task.
        let server_id = self.server_id.clone();
        let db = self.db;

        // Prepare data for the async block (move ownership)
        let key_clone = key.clone();
        let original_value_clone = original_value.clone();
        let new_value_clone = new_value.clone();

        self.spawn(
            ServerTask::UpdateListValue,
            move || async move {
                let mut conn = get_connection_manager().get_connection(&server_id, db).await?;

                // 1. Optimistic Lock Check: Get current value as bytes
                // Use bytes to handle compressed/binary data correctly
                let current_bytes: Vec<u8> = cmd("LINDEX")
                    .arg(key.as_str())
                    .arg(index)
                    .query_async(&mut conn)
                    .await?;

                // Convert to display string for comparison (handles decompression)
                let current_value = bytes_to_display_string(&current_bytes);

                if current_value != original_value_clone.as_ref() {
                    return Err(Error::Invalid {
                        message: format!(
                            "Value changed (expected: '{}', actual: '{}'), update aborted.",
                            original_value_clone, current_value
                        ),
                    });
                }

                // 2. Perform Update
                let _: () = cmd("LSET")
                    .arg(key.as_str())
                    .arg(index)
                    .arg(new_value_clone.as_str())
                    .query_async(&mut conn)
                    .await?;

                // Return the new value so UI thread can update local state
                Ok(())
            },
            move |this, result, cx| {
                if let Some(value) = this.value.as_mut() {
                    value.status = RedisValueStatus::Idle;
                    if result.is_err()
                        && let Some(RedisValueData::List(list_data)) = this.value.as_mut().and_then(|v| v.data.as_mut())
                    {
                        // Use Arc::make_mut to get mutable access (Cow behavior)
                        let list = Arc::make_mut(list_data);
                        if index < list.values.len() {
                            list.values[index] = original_value;
                        }
                    }
                }
                cx.emit(ServerEvent::ValueUpdated(key_clone));

                cx.notify();
            },
            cx,
        );
    }
    /// Load the next page of items for the current List.
    pub fn load_more_list_value(&mut self, cx: &mut Context<Self>) {
        let Some((key, value)) = self.try_get_mut_key_value() else {
            return;
        };

        // Check if we have valid list data BEFORE setting Loading status
        // to avoid leaving status in Loading if we return early
        let current_len = match value.list_value() {
            Some(list) => list.values.len(),
            None => return,
        };

        value.status = RedisValueStatus::Loading;
        cx.notify();

        let server_id = self.server_id.clone();
        let db = self.db;
        // Calculate pagination
        let start = current_len;
        let stop = start + 99; // Load 100 items
        cx.emit(ServerEvent::ValuePaginationStarted(key.clone()));
        let key_clone = key.clone();
        self.spawn(
            ServerTask::LoadMoreValue,
            move || async move {
                let mut conn = get_connection_manager().get_connection(&server_id, db).await?;
                // Fetch only the new items
                let new_values = get_redis_list_value(&mut conn, &key, start, stop).await?;
                Ok(new_values)
            },
            move |this, result, cx| {
                if let Ok(new_values) = result
                    && !new_values.is_empty()
                {
                    // Update Local State (UI Thread)
                    // Append new items to the existing list
                    if let Some(RedisValueData::List(list_data)) = this.value.as_mut().and_then(|v| v.data.as_mut()) {
                        let list = Arc::make_mut(list_data);
                        list.values.extend(new_values.into_iter().map(|v| v.into()));
                    }
                }
                cx.emit(ServerEvent::ValuePaginationFinished(key_clone));
                if let Some(value) = this.value.as_mut() {
                    value.status = RedisValueStatus::Idle;
                }
                cx.notify();
            },
            cx,
        );
    }

    /// Fetch raw bytes for a list item and emit event when ready.
    ///
    /// Fetches the raw bytes from Redis using LINDEX, then emits `ListEditDialogReady` event.
    /// The UI layer (ZedisEditor) should listen for this event and open the edit dialog.
    pub fn fetch_list_value_for_edit(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some((key, _)) = self.try_get_mut_key_value() else {
            return;
        };
        let server_id = self.server_id.clone();
        let db = self.db;
        let key_clone = key.clone();

        self.spawn(
            ServerTask::UpdateListValue,
            move || async move {
                let mut conn = get_connection_manager().get_connection(&server_id, db).await?;
                let bytes: Vec<u8> = cmd("LINDEX")
                    .arg(key_clone.as_str())
                    .arg(index)
                    .query_async(&mut conn)
                    .await?;
                Ok(bytes)
            },
            move |_this, result, cx| {
                if let Ok(bytes) = result {
                    cx.emit(ServerEvent::ListEditDialogReady(index, bytes));
                }
                cx.notify();
            },
            cx,
        );
    }

    /// Update a list item at the given index with raw bytes.
    ///
    /// Uses LSET command to update the value directly with bytes.
    pub fn update_list_value_bytes(&mut self, index: usize, new_bytes: Bytes, cx: &mut Context<Self>) {
        let Some((key, value)) = self.try_get_mut_key_value() else {
            return;
        };
        value.status = RedisValueStatus::Updating;

        // Save old value for rollback on failure
        let old_value: Option<SharedString> = value.list_value().and_then(|list| list.values.get(index).cloned());

        // Update local state with string representation (decompress if needed for display)
        let new_string: SharedString = bytes_to_display_string(&new_bytes).into();
        if let Some(RedisValueData::List(list_data)) = value.data.as_mut() {
            let list = Arc::make_mut(list_data);
            if index < list.values.len() {
                list.values[index] = new_string.clone();
                cx.emit(ServerEvent::ValueUpdated(key.clone()));
            }
        }
        cx.notify();

        let server_id = self.server_id.clone();
        let db = self.db;
        let key_clone = key.clone();
        let new_bytes_vec = new_bytes.to_vec();

        self.spawn(
            ServerTask::UpdateListValue,
            move || async move {
                let mut conn = get_connection_manager().get_connection(&server_id, db).await?;

                let _: () = cmd("LSET")
                    .arg(key.as_str())
                    .arg(index)
                    .arg(new_bytes_vec)
                    .query_async(&mut conn)
                    .await?;

                Ok(())
            },
            move |this, result, cx| {
                if let Some(value) = this.value.as_mut() {
                    value.status = RedisValueStatus::Idle;
                }
                if let Err(e) = &result {
                    // Rollback local state on failure
                    if let Some(original) = old_value
                        && let Some(RedisValueData::List(list_data)) = this.value.as_mut().and_then(|v| v.data.as_mut())
                    {
                        let list = Arc::make_mut(list_data);
                        if index < list.values.len() {
                            list.values[index] = original;
                        }
                    }
                    cx.emit(ServerEvent::ErrorOccurred(crate::states::ErrorMessage {
                        category: "update_list_value".into(),
                        message: e.to_string().into(),
                        created_at: crate::helpers::unix_ts(),
                    }));
                }
                cx.emit(ServerEvent::ValueUpdated(key_clone));
                cx.notify();
            },
            cx,
        );
    }
}
