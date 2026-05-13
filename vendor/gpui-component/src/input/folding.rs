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
}

impl JsonFoldState {
    pub(super) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }

        self.enabled = enabled;
        self.folded_start_rows.clear();
        self.ranges_dirty = true;
    }

    pub(super) fn clear(&mut self) -> bool {
        let had_folds = !self.folded_start_rows.is_empty();
        self.folded_start_rows.clear();
        had_folds
    }

    pub(super) fn mark_dirty(&mut self) {
        self.ranges_dirty = true;
    }

    pub(super) fn ensure_ranges(&mut self, text: &Rope) {
        if !self.enabled || !self.ranges_dirty {
            return;
        }

        self.ranges = collect_json_fold_ranges(text);
        self.folded_start_rows
            .retain(|row| self.ranges.iter().any(|range| range.start_row == *row));
        self.ranges_dirty = false;
    }

    pub(super) fn is_row_hidden(&self, row: usize) -> bool {
        self.folded_ranges()
            .any(|range| row > range.start_row && row <= range.end_row)
    }

    pub(super) fn hidden_range_for_row(&self, row: usize) -> Option<&FoldRange> {
        self.folded_ranges()
            .find(|range| row > range.start_row && row <= range.end_row)
    }

    pub(super) fn unfold_ranges_intersecting_rows(&mut self, start_row: usize, end_row: usize) -> usize {
        let rows_to_unfold = self
            .folded_ranges()
            .filter(|range| start_row <= range.end_row && end_row > range.start_row)
            .map(|range| range.start_row)
            .collect::<Vec<_>>();
        let unfolded_count = rows_to_unfold.len();

        for row in rows_to_unfold {
            self.folded_start_rows.remove(&row);
        }

        unfolded_count
    }

    pub(super) fn visible_wrapped_line_count(
        &self,
        total_lines: usize,
        line_heights: impl Fn(usize) -> usize,
    ) -> usize {
        (0..total_lines)
            .filter(|row| !self.is_row_hidden(*row))
            .map(line_heights)
            .sum()
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
            return Some(range);
        }

        None
    }

    pub(super) fn fold_marker_for_row(&self, text: &Rope, row: usize) -> Option<String> {
        let range = self.folded_ranges().find(|range| range.start_row == row)?;
        let line_end = text.line_end_offset(range.end_row);
        let suffix = text.slice(range.end_offset..line_end).to_string();
        Some(format!(" ... {}", suffix.trim_start()))
    }

    fn folded_ranges(&self) -> impl Iterator<Item = &FoldRange> {
        self.ranges
            .iter()
            .filter(|range| self.folded_start_rows.contains(&range.start_row))
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
        assert!(state.is_row_hidden(2));
        assert!(state.is_row_hidden(3));
        assert!(!state.is_row_hidden(4));
        assert_eq!(state.fold_marker_for_row(&text, 1).as_deref(), Some(" ... },"));

        let toggled = state.toggle_at_offset(&text, offset);

        assert!(toggled.is_some());
        assert!(!state.is_row_hidden(2));
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
}
