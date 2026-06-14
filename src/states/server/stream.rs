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

//! Redis Stream data type operations.
//!
//! The loading logic mirrors upstream Zedis: use `XLEN` for total size and
//! `XRANGE`/`XREVRANGE` with exclusive cursors for paged browsing.

use super::{
    KeyType, RedisValueData, ServerEvent, ServerTask, ZedisServerState,
    value::{RedisStreamEntry, RedisStreamValue, RedisValue, RedisValueStatus},
};
use crate::{
    connection::{RedisAsyncConn, get_connection_manager},
    error::Error,
};
use gpui::{SharedString, prelude::*};
use redis::cmd;
use std::sync::Arc;
use tracing::{debug, error, info};

type Result<T, E = Error> = std::result::Result<T, E>;
type RawStreamData = Vec<(String, Vec<String>)>;

const STREAM_PAGE_SIZE: usize = 100;
const STREAM_FILTER_PAGE_SIZE: usize = 1000;

fn stream_text_from_json(value: &serde_json::Value) -> SharedString {
    match value {
        serde_json::Value::String(value) => value.clone().into(),
        _ => value.to_string().into(),
    }
}

fn entry_fields_to_json(fields: &[(SharedString, SharedString)]) -> SharedString {
    let pairs = fields
        .iter()
        .map(|(field, value)| {
            serde_json::Value::Array(vec![
                serde_json::Value::String(field.to_string()),
                serde_json::Value::String(value.to_string()),
            ])
        })
        .collect::<Vec<_>>();
    serde_json::Value::Array(pairs).to_string().into()
}

fn parse_stream_object_pair(
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Option<(SharedString, SharedString)> {
    if let (Some(field), Some(value)) = (fields.get("field"), fields.get("value")) {
        let field = stream_text_from_json(field);
        if field.trim().is_empty() {
            return None;
        }
        return Some((field, stream_text_from_json(value)));
    }

    if fields.len() != 1 {
        return None;
    }

    let (field, value) = fields.iter().next()?;
    if field.trim().is_empty() {
        return None;
    }
    Some((field.clone().into(), stream_text_from_json(value)))
}

fn parse_stream_array_pair(fields: &[serde_json::Value]) -> Option<(SharedString, SharedString)> {
    if fields.len() != 2 {
        return None;
    }
    let field = stream_text_from_json(&fields[0]);
    if field.trim().is_empty() {
        return None;
    }
    Some((field, stream_text_from_json(&fields[1])))
}

fn parse_stream_field_pair(value: &serde_json::Value) -> Option<(SharedString, SharedString)> {
    match value {
        serde_json::Value::Array(fields) => parse_stream_array_pair(fields),
        serde_json::Value::Object(fields) => parse_stream_object_pair(fields),
        _ => None,
    }
}

pub(crate) fn parse_stream_field_values(value: &str) -> Option<Vec<(SharedString, SharedString)>> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    let parsed: serde_json::Value = serde_json::from_str(value).ok()?;
    let values = match &parsed {
        serde_json::Value::Object(fields) => fields
            .iter()
            .map(|(field, value)| {
                if field.trim().is_empty() {
                    None
                } else {
                    Some((field.clone().into(), stream_text_from_json(value)))
                }
            })
            .collect::<Option<Vec<_>>>()?,
        serde_json::Value::Array(fields)
            if fields.len() % 2 == 0 && fields.iter().all(|value| !value.is_array() && !value.is_object()) =>
        {
            fields
                .chunks_exact(2)
                .map(parse_stream_array_pair)
                .collect::<Option<Vec<_>>>()?
        }
        serde_json::Value::Array(fields) => fields.iter().map(parse_stream_field_pair).collect::<Option<Vec<_>>>()?,
        _ => return None,
    };

    if values.is_empty() { None } else { Some(values) }
}

fn insert_added_stream_entry(stream: &mut RedisStreamValue, entry: RedisStreamEntry) {
    stream.size += 1;
    if stream.reverse {
        stream.values.insert(0, entry);
    } else if stream.done {
        stream.values.push(entry);
    }
}

fn stream_entry_matches_keyword(entry: &RedisStreamEntry, keyword: &str) -> bool {
    if keyword.is_empty() {
        return true;
    }
    let keyword = keyword.to_lowercase();
    if entry.0.to_lowercase().contains(&keyword) {
        return true;
    }
    entry
        .1
        .iter()
        .any(|(field, value)| field.to_lowercase().contains(&keyword) || value.to_lowercase().contains(&keyword))
}

fn stream_keyword_match_count(entries: &[RedisStreamEntry], keyword: Option<&SharedString>) -> usize {
    let Some(keyword) = keyword.filter(|keyword| !keyword.is_empty()) else {
        return entries.len();
    };

    entries
        .iter()
        .filter(|entry| stream_entry_matches_keyword(entry, keyword.as_ref()))
        .count()
}

fn should_auto_load_filtered_stream(stream: &RedisStreamValue) -> bool {
    stream.keyword.as_ref().is_some_and(|keyword| !keyword.is_empty())
        && !stream.done
        && stream_keyword_match_count(&stream.values, stream.keyword.as_ref()) < 50
}

fn unique_stream_entry_ids(ids: Vec<SharedString>) -> Vec<SharedString> {
    let mut unique = Vec::with_capacity(ids.len());
    for id in ids {
        if !unique.contains(&id) {
            unique.push(id);
        }
    }
    unique
}

/// Converts a Redis stream entry field list into a stable table cell string.
pub(crate) fn stream_entry_fields_to_display(fields: &[(SharedString, SharedString)]) -> SharedString {
    entry_fields_to_json(fields)
}

pub(crate) fn filter_stream_entries(
    entries: &[RedisStreamEntry],
    keyword: Option<&SharedString>,
) -> Vec<(usize, RedisStreamEntry)> {
    let keyword = keyword.map(|keyword| keyword.as_str()).unwrap_or_default();
    entries
        .iter()
        .cloned()
        .enumerate()
        .filter(|(_, entry)| stream_entry_matches_keyword(entry, keyword))
        .collect()
}

/// Fetches a page of stream entries using XRANGE (ascending) or XREVRANGE (descending).
async fn get_redis_stream_value(
    conn: &mut RedisAsyncConn,
    key: &str,
    cursor: Option<String>,
    count: usize,
    reverse: bool,
) -> Result<(String, Vec<RedisStreamEntry>)> {
    let entries: RawStreamData = if reverse {
        let end = cursor.map_or_else(|| "+".to_string(), |cursor| format!("({cursor}"));
        cmd("XREVRANGE")
            .arg(key)
            .arg(&end)
            .arg("-")
            .arg("COUNT")
            .arg(count)
            .query_async(conn)
            .await?
    } else {
        let start = cursor.map_or_else(|| "-".to_string(), |cursor| format!("({cursor}"));
        cmd("XRANGE")
            .arg(key)
            .arg(&start)
            .arg("+")
            .arg("COUNT")
            .arg(count)
            .query_async(conn)
            .await?
    };

    let done = entries.len() < count;
    let values: Vec<RedisStreamEntry> = entries
        .into_iter()
        .map(|(id, flat_fields)| {
            let mut field_values = Vec::with_capacity(flat_fields.len() / 2);
            let mut iter = flat_fields.into_iter();
            while let Some(field) = iter.next() {
                if let Some(value) = iter.next() {
                    field_values.push((field.into(), value.into()));
                }
            }
            (id.into(), field_values)
        })
        .collect::<Vec<_>>();

    let cursor = if done {
        String::new()
    } else {
        values.last().map(|(id, _)| id.to_string()).unwrap_or_default()
    };

    Ok((cursor, values))
}

pub(crate) async fn first_load_stream_value(conn: &mut RedisAsyncConn, key: &str, reverse: bool) -> Result<RedisValue> {
    let size: usize = cmd("XLEN").arg(key).query_async(conn).await?;
    let (cursor, values) = get_redis_stream_value(conn, key, None, STREAM_PAGE_SIZE, reverse).await?;
    let done = cursor.is_empty();

    Ok(RedisValue {
        key_type: KeyType::Stream,
        data: Some(RedisValueData::Stream(Arc::new(RedisStreamValue {
            keyword: None,
            cursor,
            size,
            done,
            values,
            reverse,
        }))),
        ..Default::default()
    })
}

impl ZedisServerState {
    pub fn filter_stream_value(&mut self, keyword: SharedString, cx: &mut Context<Self>) -> bool {
        let (key, should_load_more) = {
            let Some((key, value)) = self.try_get_mut_key_value() else {
                return false;
            };
            let Some(stream_value) = value.stream_value() else {
                return false;
            };

            let filter_keyword = if keyword.is_empty() {
                None
            } else {
                Some(keyword.clone())
            };
            let should_load_more = filter_keyword.is_some()
                && !stream_value.done
                && !value.is_loading()
                && stream_keyword_match_count(&stream_value.values, filter_keyword.as_ref()) < 50;
            let new_stream_value = RedisStreamValue {
                keyword: filter_keyword,
                cursor: stream_value.cursor.clone(),
                size: stream_value.size,
                done: stream_value.done,
                values: stream_value.values.clone(),
                reverse: stream_value.reverse,
            };
            value.data = Some(RedisValueData::Stream(Arc::new(new_stream_value)));
            (key, should_load_more)
        };
        self.remember_value_filter_keyword_for_key(key.as_str(), keyword);
        if should_load_more {
            cx.emit(ServerEvent::ValueUpdated(key.clone()));
            info!(
                key = %key,
                "Start loading remaining Redis stream entries for keyword search"
            );
            self.load_more_stream_value(cx);
        } else {
            cx.emit(ServerEvent::ValueUpdated(key));
        }
        true
    }

    pub fn load_more_stream_value(&mut self, cx: &mut Context<Self>) {
        let Some((key, value)) = self.try_get_mut_key_value() else {
            return;
        };
        let (cursor, reverse, keyword) = match value.stream_value() {
            Some(stream) if !stream.done => (stream.cursor.clone(), stream.reverse, stream.keyword.clone()),
            _ => return,
        };

        value.status = RedisValueStatus::Loading;
        cx.notify();

        let server_id = self.server_id.clone();
        let db = self.db;
        let guard_key = key.clone();
        let page_size = if keyword.is_some() {
            STREAM_FILTER_PAGE_SIZE
        } else {
            STREAM_PAGE_SIZE
        };
        cx.emit(ServerEvent::ValuePaginationStarted(key.clone()));

        self.spawn(
            ServerTask::LoadMoreValue,
            move || async move {
                let mut conn = get_connection_manager().get_connection(&server_id, db).await?;
                get_redis_stream_value(&mut conn, key.as_str(), Some(cursor), page_size, reverse).await
            },
            move |this, result, cx| {
                if this.key.as_ref() != Some(&guard_key) {
                    debug!(
                        expected_key = guard_key.as_str(),
                        current_key = this.key.clone().unwrap_or_default().as_str(),
                        "Skip stale Redis stream pagination result"
                    );
                    return;
                }

                let mut should_load_more = false;
                match result {
                    Ok((new_cursor, new_values)) => {
                        if let Some(RedisValueData::Stream(stream_data)) =
                            this.value.as_mut().and_then(|value| value.data.as_mut())
                        {
                            let stream = Arc::make_mut(stream_data);
                            if new_cursor.is_empty() {
                                stream.done = true;
                            }
                            stream.cursor = new_cursor;
                            if !new_values.is_empty() {
                                stream.values.extend(new_values);
                            }
                            should_load_more = should_auto_load_filtered_stream(stream);
                            debug!(
                                key = guard_key.as_str(),
                                loaded = stream.values.len(),
                                total = stream.size,
                                keyword_len = stream
                                    .keyword
                                    .as_ref()
                                    .map(|keyword| keyword.as_str().len())
                                    .unwrap_or(0),
                                match_count = stream_keyword_match_count(&stream.values, stream.keyword.as_ref()),
                                should_load_more,
                                "Loaded Redis stream entry page"
                            );
                        }
                    }
                    Err(err) => {
                        error!(key = %guard_key, error = %err, "Failed to load more Redis stream values");
                        cx.emit(ServerEvent::ErrorOccurred(crate::states::ErrorMessage {
                            category: "load_more_stream_value".into(),
                            message: err.to_string().into(),
                            created_at: crate::helpers::unix_ts(),
                        }));
                    }
                }

                if let Some(value) = this.value.as_mut() {
                    value.status = RedisValueStatus::Idle;
                }
                cx.emit(ServerEvent::ValuePaginationFinished(guard_key));
                cx.notify();

                if should_load_more {
                    this.load_more_stream_value(cx);
                }
            },
            cx,
        );
    }

    pub fn add_stream_value(
        &mut self,
        entry_id: Option<SharedString>,
        values: Vec<(SharedString, SharedString)>,
        cx: &mut Context<Self>,
    ) {
        if values.is_empty() {
            debug!("Skip Redis stream XADD because no field-value pairs were provided");
            return;
        }

        let Some((key, value)) = self.try_get_mut_key_value() else {
            return;
        };
        value.status = RedisValueStatus::Updating;
        cx.notify();

        let server_id = self.server_id.clone();
        let db = self.db;
        let key_clone = key.clone();
        let id = entry_id.unwrap_or_else(|| "*".into());
        let values_for_task = values.clone();

        info!(
            key = %key,
            id = id.as_str(),
            fields = values.len(),
            "Adding Redis stream entry"
        );

        self.spawn(
            ServerTask::AddStreamEntry,
            move || async move {
                let mut conn = get_connection_manager().get_connection(&server_id, db).await?;
                let mut binding = cmd("XADD");
                let mut command = binding.arg(key.as_str()).arg(id.as_str());
                for (field, value) in values_for_task {
                    command = command.arg(field.as_str()).arg(value.as_str());
                }
                let id: String = command.query_async(&mut conn).await?;
                Ok(id)
            },
            move |this, result, cx| {
                if let Some(value) = this.value.as_mut() {
                    value.status = RedisValueStatus::Idle;
                }

                match result {
                    Ok(id) => {
                        if let Some(RedisValueData::Stream(stream_data)) =
                            this.value.as_mut().and_then(|value| value.data.as_mut())
                        {
                            insert_added_stream_entry(Arc::make_mut(stream_data), (id.into(), values));
                        }
                        cx.emit(ServerEvent::ValueAdded(key_clone));
                    }
                    Err(err) => {
                        error!(key = %key_clone, error = %err, "Failed to add Redis stream entry");
                        cx.emit(ServerEvent::ErrorOccurred(crate::states::ErrorMessage {
                            category: "add_stream_value".into(),
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

    pub fn remove_stream_value(&mut self, entry_id: SharedString, cx: &mut Context<Self>) {
        self.remove_stream_values(vec![entry_id], cx);
    }

    pub fn remove_stream_values(&mut self, entry_ids: Vec<SharedString>, cx: &mut Context<Self>) {
        let entry_ids = unique_stream_entry_ids(entry_ids);
        if entry_ids.is_empty() {
            debug!("Skip Redis stream removal because no entry IDs were selected");
            return;
        }

        let Some((key, value)) = self.try_get_mut_key_value() else {
            return;
        };
        value.status = RedisValueStatus::Updating;
        cx.notify();

        let server_id = self.server_id.clone();
        let db = self.db;
        let key_clone = key.clone();
        let entry_ids_for_task = entry_ids.clone();
        let task = if entry_ids.len() == 1 {
            ServerTask::RemoveStreamEntry
        } else {
            ServerTask::RemoveStreamEntries
        };

        info!(
            key = %key,
            count = entry_ids.len(),
            "Removing Redis stream entries"
        );

        self.spawn(
            task,
            move || async move {
                let mut conn = get_connection_manager().get_connection(&server_id, db).await?;
                let mut binding = cmd("XDEL");
                binding.arg(key.as_str());
                for id in &entry_ids_for_task {
                    binding.arg(id.as_str());
                }
                let removed: usize = binding.query_async(&mut conn).await?;
                Ok(removed)
            },
            move |this, result, cx| {
                if let Some(value) = this.value.as_mut() {
                    value.status = RedisValueStatus::Idle;
                }

                match result {
                    Ok(removed) => {
                        if removed > 0
                            && let Some(RedisValueData::Stream(stream_data)) =
                                this.value.as_mut().and_then(|value| value.data.as_mut())
                        {
                            let stream = Arc::make_mut(stream_data);
                            stream.values.retain(|(id, _)| !entry_ids.contains(id));
                            stream.size = stream.size.saturating_sub(removed);
                        }
                        cx.emit(ServerEvent::ValueUpdated(key_clone));
                    }
                    Err(err) => {
                        error!(key = %key_clone, error = %err, "Failed to remove Redis stream entries");
                        cx.emit(ServerEvent::ErrorOccurred(crate::states::ErrorMessage {
                            category: "remove_stream_value".into(),
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
}

#[cfg(test)]
mod tests {
    use super::{
        filter_stream_entries, insert_added_stream_entry, parse_stream_field_values, should_auto_load_filtered_stream,
        stream_entry_fields_to_display, stream_entry_matches_keyword, stream_keyword_match_count,
        unique_stream_entry_ids,
    };
    use gpui::SharedString;

    use crate::states::RedisStreamValue;

    #[test]
    fn renders_stream_fields_as_json_pairs() {
        let fields = vec![
            (SharedString::from("field"), SharedString::from("value")),
            (SharedString::from("num"), SharedString::from("1")),
        ];

        assert_eq!(
            stream_entry_fields_to_display(&fields),
            SharedString::from(r#"[["field","value"],["num","1"]]"#)
        );
    }

    #[test]
    fn matches_stream_entry_by_id_field_or_value() {
        let entry = (
            SharedString::from("1718000000000-0"),
            vec![
                (SharedString::from("device"), SharedString::from("DCC0001")),
                (SharedString::from("status"), SharedString::from("online")),
            ],
        );

        assert!(stream_entry_matches_keyword(&entry, "171800"));
        assert!(stream_entry_matches_keyword(&entry, "DEVICE"));
        assert!(stream_entry_matches_keyword(&entry, "dcc0001"));
        assert!(!stream_entry_matches_keyword(&entry, "missing"));
    }

    #[test]
    fn counts_stream_keyword_matches_case_insensitively() {
        let entries = vec![
            (
                SharedString::from("1-0"),
                vec![(SharedString::from("device"), SharedString::from("DCC0001"))],
            ),
            (
                SharedString::from("2-0"),
                vec![(SharedString::from("device"), SharedString::from("DCC0002"))],
            ),
        ];
        let keyword = SharedString::from("dcc0001");

        assert_eq!(stream_keyword_match_count(&entries, Some(&keyword)), 1);
        assert_eq!(stream_keyword_match_count(&entries, None), 2);
    }

    #[test]
    fn auto_loads_filtered_stream_until_enough_matches_or_done() {
        let filtered_stream = RedisStreamValue {
            keyword: Some(SharedString::from("needle")),
            cursor: "1-0".to_string(),
            size: 100,
            done: false,
            reverse: true,
            values: vec![(
                SharedString::from("1-0"),
                vec![(SharedString::from("field"), SharedString::from("haystack"))],
            )],
        };
        assert!(should_auto_load_filtered_stream(&filtered_stream));

        let done_stream = RedisStreamValue {
            done: true,
            ..filtered_stream.clone()
        };
        assert!(!should_auto_load_filtered_stream(&done_stream));

        let unfiltered_stream = RedisStreamValue {
            keyword: None,
            ..filtered_stream
        };
        assert!(!should_auto_load_filtered_stream(&unfiltered_stream));
    }

    #[test]
    fn filters_stream_entries_with_original_indexes() {
        let entries = vec![
            (
                SharedString::from("1-0"),
                vec![(SharedString::from("kind"), SharedString::from("alpha"))],
            ),
            (
                SharedString::from("2-0"),
                vec![(SharedString::from("kind"), SharedString::from("beta"))],
            ),
        ];
        let keyword = SharedString::from("beta");

        let filtered = filter_stream_entries(&entries, Some(&keyword));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].0, 1);
        assert_eq!(filtered[0].1.0, SharedString::from("2-0"));
    }

    #[test]
    fn keeps_stream_entry_ids_unique_for_batch_remove() {
        let ids = vec![
            SharedString::from("1-0"),
            SharedString::from("2-0"),
            SharedString::from("1-0"),
        ];

        assert_eq!(
            unique_stream_entry_ids(ids),
            vec![SharedString::from("1-0"), SharedString::from("2-0")]
        );
    }

    #[test]
    fn preserves_duplicate_stream_fields_in_display() {
        let fields = vec![
            (SharedString::from("field"), SharedString::from("first")),
            (SharedString::from("field"), SharedString::from("second")),
        ];

        assert_eq!(
            stream_entry_fields_to_display(&fields),
            SharedString::from(r#"[["field","first"],["field","second"]]"#)
        );
    }

    #[test]
    fn parses_stream_fields_from_json_object_array_and_flat_array() {
        assert_eq!(
            parse_stream_field_values(r#"{"field":"value","num":1}"#),
            Some(vec![
                (SharedString::from("field"), SharedString::from("value")),
                (SharedString::from("num"), SharedString::from("1")),
            ])
        );
        assert_eq!(
            parse_stream_field_values(r#"[["field","value"],["field","second"]]"#),
            Some(vec![
                (SharedString::from("field"), SharedString::from("value")),
                (SharedString::from("field"), SharedString::from("second")),
            ])
        );
        assert_eq!(
            parse_stream_field_values(r#"["field","value","num",1]"#),
            Some(vec![
                (SharedString::from("field"), SharedString::from("value")),
                (SharedString::from("num"), SharedString::from("1")),
            ])
        );
        assert_eq!(
            parse_stream_field_values(r#"[{"field":"device","value":"DCC0001"}]"#),
            Some(vec![(SharedString::from("device"), SharedString::from("DCC0001"))])
        );
        assert_eq!(parse_stream_field_values(r#"[]"#), None);
        assert_eq!(parse_stream_field_values(r#"[["field"]]"#), None);
    }

    #[test]
    fn inserts_added_stream_entry_by_current_order() {
        let mut reverse_stream = RedisStreamValue {
            cursor: String::new(),
            size: 1,
            done: false,
            reverse: true,
            keyword: None,
            values: vec![(
                SharedString::from("2-0"),
                vec![(SharedString::from("field"), SharedString::from("old"))],
            )],
        };
        insert_added_stream_entry(
            &mut reverse_stream,
            (
                SharedString::from("3-0"),
                vec![(SharedString::from("field"), SharedString::from("new"))],
            ),
        );
        assert_eq!(reverse_stream.size, 2);
        assert_eq!(reverse_stream.get_entry_id(0), Some(SharedString::from("3-0")));

        let mut forward_stream = RedisStreamValue {
            cursor: String::new(),
            size: 1,
            done: false,
            reverse: false,
            keyword: None,
            values: vec![(
                SharedString::from("1-0"),
                vec![(SharedString::from("field"), SharedString::from("old"))],
            )],
        };
        insert_added_stream_entry(
            &mut forward_stream,
            (
                SharedString::from("3-0"),
                vec![(SharedString::from("field"), SharedString::from("new"))],
            ),
        );
        assert_eq!(forward_stream.size, 2);
        assert_eq!(forward_stream.values.len(), 1);

        let mut loaded_forward_stream = RedisStreamValue {
            cursor: String::new(),
            size: 1,
            done: true,
            reverse: false,
            keyword: None,
            values: vec![(
                SharedString::from("1-0"),
                vec![(SharedString::from("field"), SharedString::from("old"))],
            )],
        };
        insert_added_stream_entry(
            &mut loaded_forward_stream,
            (
                SharedString::from("3-0"),
                vec![(SharedString::from("field"), SharedString::from("new"))],
            ),
        );
        assert_eq!(loaded_forward_stream.values.len(), 2);
        assert_eq!(loaded_forward_stream.get_entry_id(1), Some(SharedString::from("3-0")));
    }
}
