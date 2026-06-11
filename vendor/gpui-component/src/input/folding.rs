use std::collections::BTreeSet;

use ropey::Rope;

use crate::input::RopeExt as _;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct FoldRange {
    pub(super) start_row: usize,
    pub(super) end_row: usize,
    pub(super) start_offset: usize,
    pub(super) end_offset: usize,
}

#[derive(Clone, Debug, Default)]
pub(super) struct JsonFoldState {
    enabled: bool,
    ranges_dirty: bool,
    ranges: Vec<FoldRange>,
    folded_start_rows: BTreeSet<usize>,
    /// Folded ranges sorted by (start_row, end_row), rebuilt whenever the
    /// folded set changes. Kept small (only user-folded ranges) so per-row
    /// queries never scan the full `ranges` list.
    folded_cache: Vec<FoldRange>,
    /// Merged, sorted, non-overlapping hidden row intervals (inclusive).
    /// Lets `is_row_hidden` answer in O(log folds) instead of O(ranges).
    hidden_intervals: Vec<(usize, usize)>,
}

impl JsonFoldState {
    pub(super) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }

        self.enabled = enabled;
        self.folded_start_rows.clear();
        self.ranges_dirty = true;
        self.rebuild_folded_cache();
    }

    pub(super) fn clear(&mut self) -> bool {
        let had_folds = !self.folded_start_rows.is_empty();
        self.folded_start_rows.clear();
        if had_folds {
            self.rebuild_folded_cache();
        }
        had_folds
    }

    pub(super) fn mark_dirty(&mut self) {
        self.ranges_dirty = true;
    }

    /// Whether any rows are currently folded.
    ///
    /// When false, no fold data is needed for rendering, so callers can skip
    /// `ensure_ranges` (a full-text scan) entirely.
    pub(super) fn has_folds(&self) -> bool {
        !self.folded_start_rows.is_empty()
    }

    pub(super) fn ensure_ranges(&mut self, text: &Rope) {
        if !self.enabled || !self.ranges_dirty {
            return;
        }

        self.ranges = collect_json_fold_ranges(text);
        self.folded_start_rows
            .retain(|row| self.ranges.iter().any(|range| range.start_row == *row));
        self.ranges_dirty = false;
        self.rebuild_folded_cache();
    }

    /// Whether `row` falls inside any folded (hidden) interval.
    ///
    /// Binary-searches the merged `hidden_intervals` in O(log folds); called
    /// per-row per-frame, so it must stay cheap. An empty interval list makes
    /// `partition_point` return 0 and `checked_sub(1)` short-circuit to `false`.
    pub(super) fn is_row_hidden(&self, row: usize) -> bool {
        let ix = self.hidden_intervals.partition_point(|(start, _)| *start <= row);
        ix.checked_sub(1)
            .and_then(|i| self.hidden_intervals.get(i))
            .is_some_and(|(start, end)| *start <= row && row <= *end)
    }

    pub(super) fn hidden_range_for_row(&self, row: usize) -> Option<&FoldRange> {
        // `find` already returns `None` for a non-hidden row, so no separate
        // hidden-check guard is needed. `folded_cache` is sorted by start_row,
        // so the first match is the outermost folded range covering `row`.
        self.folded_cache
            .iter()
            .find(|range| row > range.start_row && row <= range.end_row)
    }

    pub(super) fn unfold_ranges_intersecting_rows(&mut self, start_row: usize, end_row: usize) -> usize {
        let rows_to_unfold = self
            .folded_cache
            .iter()
            .filter(|range| start_row <= range.end_row && end_row > range.start_row)
            .map(|range| range.start_row)
            .collect::<Vec<_>>();
        let unfolded_count = rows_to_unfold.len();

        for row in rows_to_unfold {
            self.folded_start_rows.remove(&row);
        }

        if unfolded_count > 0 {
            self.rebuild_folded_cache();
        }

        unfolded_count
    }

    /// Count visible wrapped lines as `total - hidden`, walking only the
    /// hidden intervals instead of every row in the text.
    pub(super) fn visible_wrapped_line_count(
        &self,
        total_wrapped_lines: usize,
        line_heights: impl Fn(usize) -> usize,
    ) -> usize {
        if self.hidden_intervals.is_empty() {
            return total_wrapped_lines;
        }

        let hidden: usize = self
            .hidden_intervals
            .iter()
            .flat_map(|(start, end)| *start..=*end)
            .map(line_heights)
            .sum();
        total_wrapped_lines.saturating_sub(hidden)
    }

    pub(super) fn toggle_at_offset(&mut self, text: &Rope, offset: usize) -> Option<FoldRange> {
        if !self.enabled {
            return None;
        }

        self.ensure_ranges(text);
        let offset = text.clip_offset(offset.min(text.len()), sum_tree::Bias::Left);
        let mut candidate_offsets = Vec::with_capacity(2);
        candidate_offsets.push(offset);
        if offset > 0 {
            candidate_offsets.push(text.clip_offset(offset.saturating_sub(1), sum_tree::Bias::Left));
        }

        for candidate in candidate_offsets {
            let Some(ch) = text.char_at(candidate) else {
                continue;
            };

            if ch != '{' && ch != '[' {
                continue;
            }

            let Some(range) = self
                .ranges
                .iter()
                .find(|range| range.start_offset == candidate)
                .cloned()
            else {
                continue;
            };

            if !self.folded_start_rows.insert(range.start_row) {
                self.folded_start_rows.remove(&range.start_row);
            }
            self.rebuild_folded_cache();
            return Some(range);
        }

        None
    }

    pub(super) fn fold_marker_for_row(&self, text: &Rope, row: usize) -> Option<String> {
        let ix = self.folded_cache.partition_point(|range| range.start_row < row);
        let range = self.folded_cache.get(ix).filter(|range| range.start_row == row)?;
        let line_end = text.line_end_offset(range.end_row);
        let suffix = text.slice(range.end_offset..line_end).to_string();
        Some(format!(" ... {}", suffix.trim_start()))
    }

    /// Rebuild `folded_cache` and `hidden_intervals` from the folded set.
    ///
    /// Called whenever `folded_start_rows` or `ranges` change. Cost is
    /// O(ranges) once per fold state change, which keeps the per-frame,
    /// per-row queries cheap.
    fn rebuild_folded_cache(&mut self) {
        self.folded_cache.clear();
        self.hidden_intervals.clear();

        if self.folded_start_rows.is_empty() {
            return;
        }

        self.folded_cache.extend(
            self.ranges
                .iter()
                .filter(|range| self.folded_start_rows.contains(&range.start_row))
                .cloned(),
        );

        // `ranges` is sorted by (start_row, end_row), so hidden intervals
        // (start_row + 1 ..= end_row) arrive sorted by start; merge them.
        for range in &self.folded_cache {
            if range.end_row <= range.start_row {
                continue;
            }
            let (start, end) = (range.start_row + 1, range.end_row);
            match self.hidden_intervals.last_mut() {
                Some((_, last_end)) if start <= *last_end + 1 => {
                    *last_end = (*last_end).max(end);
                }
                _ => self.hidden_intervals.push((start, end)),
            }
        }
    }
}

#[derive(Clone, Copy)]
struct JsonBracket {
    ch: char,
    row: usize,
    offset: usize,
}

fn collect_json_fold_ranges(text: &Rope) -> Vec<FoldRange> {
    let mut ranges = Vec::new();
    let mut stack: Vec<JsonBracket> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    let mut row = 0;
    let mut offset = 0;

    for ch in text.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
        } else {
            match ch {
                '"' => in_string = true,
                '{' | '[' => stack.push(JsonBracket { ch, row, offset }),
                '}' | ']' => {
                    if let Some(open) = pop_matching_bracket(&mut stack, ch)
                        && open.row < row
                    {
                        ranges.push(FoldRange {
                            start_row: open.row,
                            end_row: row,
                            start_offset: open.offset,
                            end_offset: offset,
                        });
                    }
                }
                _ => {}
            }
        }

        offset += ch.len_utf8();
        if ch == '\n' {
            row += 1;
        }
    }

    ranges.sort_by_key(|range| (range.start_row, range.end_row));
    ranges
}

fn pop_matching_bracket(stack: &mut Vec<JsonBracket>, close: char) -> Option<JsonBracket> {
    let expected = match close {
        '}' => '{',
        ']' => '[',
        _ => return None,
    };

    while let Some(open) = stack.pop() {
        if open.ch == expected {
            return Some(open);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_multiline_json_object_and_array_ranges() {
        let text = Rope::from("{\n  \"items\": [\n    {\n      \"id\": 1\n    }\n  ]\n}");

        let ranges = collect_json_fold_ranges(&text);

        assert_eq!(
            ranges
                .iter()
                .map(|range| (range.start_row, range.end_row))
                .collect::<Vec<_>>(),
            vec![(0, 6), (1, 5), (2, 4)]
        );
    }

    #[test]
    fn ignores_braces_inside_json_strings() {
        let text = Rope::from("{\n  \"text\": \"not a { fold }\"\n}");

        let ranges = collect_json_fold_ranges(&text);

        assert_eq!(
            ranges
                .iter()
                .map(|range| (range.start_row, range.end_row))
                .collect::<Vec<_>>(),
            vec![(0, 2)]
        );
    }

    #[test]
    fn toggles_folded_rows_and_builds_marker_from_closing_line() {
        let text = Rope::from("{\n  \"host\": {\n    \"ip\": \"1.1.1.1\"\n  },\n  \"x\": 1\n}");
        let mut state = JsonFoldState::default();
        state.set_enabled(true);

        let offset = text.line_start_offset(1) + text.slice_line(1).len().saturating_sub(1);
        let toggled = state.toggle_at_offset(&text, offset);

        assert!(toggled.is_some());
        assert!(state.has_folds());
        assert!(state.is_row_hidden(2));
        assert!(state.is_row_hidden(3));
        assert!(!state.is_row_hidden(4));
        assert_eq!(state.fold_marker_for_row(&text, 1).as_deref(), Some(" ... },"));

        let toggled = state.toggle_at_offset(&text, offset);

        assert!(toggled.is_some());
        assert!(!state.is_row_hidden(2));
        assert!(!state.has_folds());
    }

    #[test]
    fn unfolds_all_folded_ranges_that_cover_target_rows() {
        let text = Rope::from("{\n  \"host\": {\n    \"ip\": \"1.1.1.1\"\n  },\n  \"x\": 1\n}");
        let mut state = JsonFoldState::default();
        state.set_enabled(true);

        let outer_offset = 0;
        let inner_offset = text.line_start_offset(1) + text.slice_line(1).len().saturating_sub(1);
        state.toggle_at_offset(&text, outer_offset);
        state.toggle_at_offset(&text, inner_offset);

        assert!(state.is_row_hidden(2));

        let unfolded_count = state.unfold_ranges_intersecting_rows(2, 2);

        assert_eq!(unfolded_count, 2);
        assert!(!state.is_row_hidden(2));
        assert_eq!(state.fold_marker_for_row(&text, 0), None);
        assert_eq!(state.fold_marker_for_row(&text, 1), None);
    }

    #[test]
    fn hidden_range_for_row_returns_outermost_folded_range() {
        let text = Rope::from("{\n  \"host\": {\n    \"ip\": \"1.1.1.1\"\n  },\n  \"x\": 1\n}");
        let mut state = JsonFoldState::default();
        state.set_enabled(true);

        let outer_offset = 0;
        let inner_offset = text.line_start_offset(1) + text.slice_line(1).len().saturating_sub(1);
        state.toggle_at_offset(&text, outer_offset);
        state.toggle_at_offset(&text, inner_offset);

        // Row 2 is covered by both folds; the first (outermost) range wins,
        // matching the previous linear-scan behavior.
        let range = state.hidden_range_for_row(2).expect("row 2 should be hidden");
        assert_eq!((range.start_row, range.end_row), (0, 5));
        assert_eq!(state.hidden_range_for_row(0), None);
    }

    #[test]
    fn visible_wrapped_line_count_subtracts_hidden_rows() {
        let text = Rope::from("{\n  \"host\": {\n    \"ip\": \"1.1.1.1\"\n  },\n  \"x\": 1\n}");
        let mut state = JsonFoldState::default();
        state.set_enabled(true);

        // 6 rows, every row 1 wrapped line.
        assert_eq!(state.visible_wrapped_line_count(6, |_| 1), 6);

        let inner_offset = text.line_start_offset(1) + text.slice_line(1).len().saturating_sub(1);
        state.toggle_at_offset(&text, inner_offset);

        // Rows 2..=3 hidden.
        assert_eq!(state.visible_wrapped_line_count(6, |_| 1), 4);
    }

    #[test]
    fn perf_smoke_large_json() {
        // Mirror the reported scenario: a multi-MB pretty-printed JSON with
        // tens of thousands of fold ranges. Per-frame queries must stay fast.
        let mut json = String::from("{\n  \"mapping\": [\n");
        for i in 0..20_000 {
            json.push_str("    {\n      \"commPoint\": [\n        \"LD_Device11$GGIO100$ST$Beh$stVal.INS\"\n      ],\n      \"des\": [\n        \"TransSubstation/PROP/OTHER/C1-LD_Device11\"\n      ]\n    }");
            json.push_str(if i + 1 < 20_000 { ",\n" } else { "\n" });
        }
        json.push_str("  ]\n}\n");
        let text = Rope::from(json.as_str());
        let total_rows = text.lines_len();

        let mut state = JsonFoldState::default();
        state.set_enabled(true);

        let t = std::time::Instant::now();
        state.ensure_ranges(&text);
        let collect_ms = t.elapsed().as_millis();

        // Fold the outer "mapping" array (the `[` on row 1).
        let bracket_offset = json.find('[').expect("array bracket");
        let t = std::time::Instant::now();
        let toggled = state.toggle_at_offset(&text, bracket_offset);
        let toggle_ms = t.elapsed().as_millis();
        assert!(toggled.is_some());

        // Simulate one frame: visibility check for every row plus the
        // wrapped-line count, like calculate_visible_range/prepaint do.
        let t = std::time::Instant::now();
        let visible = (0..total_rows).filter(|row| !state.is_row_hidden(*row)).count();
        let count = state.visible_wrapped_line_count(total_rows, |_| 1);
        let frame_ms = t.elapsed().as_millis();

        assert_eq!(visible, count);
        println!(
            "rows={} ranges-collect={}ms toggle={}ms frame-scan={}ms visible={}",
            total_rows, collect_ms, toggle_ms, frame_ms, visible
        );
        assert!(frame_ms < 100, "per-frame fold scan too slow: {}ms", frame_ms);
    }

    #[test]
    fn clear_resets_fold_state() {
        let text = Rope::from("{\n  \"host\": {\n    \"ip\": \"1.1.1.1\"\n  },\n  \"x\": 1\n}");
        let mut state = JsonFoldState::default();
        state.set_enabled(true);

        let inner_offset = text.line_start_offset(1) + text.slice_line(1).len().saturating_sub(1);
        state.toggle_at_offset(&text, inner_offset);
        assert!(state.is_row_hidden(2));

        assert!(state.clear());
        assert!(!state.is_row_hidden(2));
        assert!(!state.has_folds());
        assert!(!state.clear());
    }
}
