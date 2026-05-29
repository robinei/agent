//! Shared utility functions for file-manipulation tools.
//!
//! Extracted from `edit.rs` so that `write.rs` and `restore_edit.rs` can
//! reuse the same matching / rendering logic.

use std::collections::HashSet;

use unicode_normalization::UnicodeNormalization;

/// Fuzzy-normalize text for matching:
/// - NFKC normalize
/// - Strip trailing whitespace per line
/// - Normalize smart quotes to ASCII
/// - Normalize dashes to ASCII hyphen
/// - Normalize special spaces to regular space
pub(super) fn fuzzy_normalize(s: &str) -> String {
    let s: String = s.nfkc().collect();
    let mut result = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\u{2018}' | '\u{2019}' | '\u{201b}' => result.push('\''),
            '\u{201c}' | '\u{201d}' | '\u{201e}' => result.push('"'),
            '\u{2013}' | '\u{2014}' | '\u{2212}' => result.push('-'),
            '\u{00a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{3000}' => {
                result.push(' ');
            }
            c => result.push(c),
        }
    }
    let lines: Vec<&str> = result.lines().collect();
    let stripped: Vec<String> = lines.iter().map(|l| l.trim_end().to_string()).collect();
    let joined = stripped.join("\n");
    joined.trim_end().to_string()
}

/// Apply a single edit. Returns new content on success.
pub(super) fn apply_edit(
    original: &str,
    old_text: &str,
    new_text: &str,
    index: usize,
) -> Result<String, String> {
    let _ = index;
    if let Some(pos) = original.find(old_text) {
        let mut result = String::with_capacity(original.len() + new_text.len());
        result.push_str(&original[..pos]);
        result.push_str(new_text);
        result.push_str(&original[pos + old_text.len()..]);
        return Ok(result);
    }

    let norm_original = fuzzy_normalize(original);
    let norm_old = fuzzy_normalize(old_text);

    if let Some(pos) = norm_original.find(&norm_old) {
        let orig_pos = map_normalized_pos(original, &norm_original, pos)
            .ok_or_else(|| format!("Edit #{}: could not map fuzzy match position", index + 1))?;

        let orig_end = map_normalized_pos(original, &norm_original, pos + norm_old.len())
            .ok_or_else(|| {
                format!(
                    "Edit #{}: could not map fuzzy match end position",
                    index + 1
                )
            })?;

        let mut result = String::with_capacity(original.len() + new_text.len());
        result.push_str(&original[..orig_pos]);
        result.push_str(new_text);
        result.push_str(&original[orig_end..]);
        return Ok(result);
    }

    Err(format!(
        "Edit #{}: oldText not found (exact + fuzzy). oldText (first 100): {:?}",
        index + 1,
        &old_text[..old_text.len().min(100)]
    ))
}

/// Count fuzzy matches of `text` in `content`.
pub(super) fn count_fuzzy_matches(content: &str, text: &str) -> usize {
    let norm_content = fuzzy_normalize(content);
    let norm_text = fuzzy_normalize(text);
    norm_content.match_indices(&norm_text).count()
}

// ── Context window rendering ──

const CONTEXT_LINES: usize = 3;

/// Map a byte position in `content` to a 1-indexed line number.
fn byte_to_line(content: &str, byte_pos: usize) -> usize {
    content[..byte_pos].matches('\n').count() + 1
}

/// Given a byte range `[start, end)` in `content`, return the
/// (start_line, end_line) of the affected region (1-indexed, inclusive).
fn get_line_range(content: &str, byte_start: usize, byte_end: usize) -> (usize, usize) {
    let start_line = byte_to_line(content, byte_start);
    let content_line = byte_to_line(content, byte_end);
    let end_line = if byte_end > 0 && &content[byte_end - 1..byte_end] == "\n" {
        content_line - 1
    } else {
        content_line
    };
    (start_line, end_line.max(start_line))
}

/// Find changed line numbers by locating `new_text` in the final content.
///
/// Returns a set of 1-indexed line numbers that contain the changed text.
/// Uses a simple non-overlapping search so duplicate new_text values
/// don't collide.
pub(super) fn find_changed_lines(final_content: &str, new_texts: &[String]) -> HashSet<usize> {
    let mut claimed: Vec<std::ops::Range<usize>> = Vec::new();
    let mut changed: HashSet<usize> = HashSet::new();

    for new_text in new_texts {
        let mut search_from = 0usize;
        loop {
            let remaining = &final_content[search_from..];
            if let Some(rel) = remaining.find(new_text.as_str()) {
                let abs = search_from + rel;
                let range = abs..abs + new_text.len();
                let overlaps = claimed
                    .iter()
                    .any(|r| r.start < range.end && range.start < r.end);
                if !overlaps {
                    claimed.push(range.clone());
                    let (s, e) = get_line_range(final_content, range.start, range.end);
                    for line in s..=e {
                        changed.insert(line);
                    }
                    break;
                }
                search_from = abs + 1;
            } else {
                break;
            }
        }
    }
    changed
}

/// Render a line-numbered context window around changed lines.
pub(super) fn build_context_window(
    file_path: &str,
    content: &str,
    changed_lines: &HashSet<usize>,
    edit_count: usize,
    edit_id: u64,
) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    if changed_lines.is_empty() {
        return format!(
            "edit_id: {}\n{} edit(s) applied to {}\n",
            edit_id, edit_count, file_path
        );
    }

    let mut windows: Vec<(usize, usize)> = changed_lines
        .iter()
        .map(|&ln| {
            let s2 = if ln > CONTEXT_LINES {
                ln - CONTEXT_LINES
            } else {
                1
            };
            let e2 = (ln + CONTEXT_LINES).min(total_lines);
            (s2, e2)
        })
        .collect();

    windows.sort_by_key(|&(s, _)| s);
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in windows {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 + 1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        merged.push((s, e));
    }

    let mut result = format!(
        "edit_id: {}\n{} edit(s) applied to {}\n",
        edit_id, edit_count, file_path
    );
    let first = merged.first().map(|&(s, _)| s > 1).unwrap_or(false);
    let last = merged
        .last()
        .map(|&(_, e)| e < total_lines)
        .unwrap_or(false);
    if first {
        result.push_str("...\n");
    }
    for (i, &(start, end)) in merged.iter().enumerate() {
        if i > 0 {
            result.push_str("...\n");
        }
        for line_num in start..=end {
            let line_content = lines.get(line_num - 1).copied().unwrap_or("");
            let sep = if changed_lines.contains(&line_num) {
                "~| "
            } else {
                " | "
            };
            result.push_str(&format!("{:>6}{}{}\n", line_num, sep, line_content));
        }
    }
    if last {
        result.push_str("...\n");
    }
    result
}

/// Map a position in the normalized string back to a byte position in the original.
fn map_normalized_pos(original: &str, _normalized: &str, norm_pos: usize) -> Option<usize> {
    let mut orig_byte_pos = 0usize;
    let mut norm_char_pos = 0usize;
    for ch in original.chars() {
        if norm_char_pos >= norm_pos {
            return Some(orig_byte_pos);
        }
        let nfkc_count: usize = ch.nfkc().count();
        let next_norm = norm_char_pos + nfkc_count;
        if next_norm > norm_pos {
            return Some(orig_byte_pos + ch.len_utf8());
        }
        norm_char_pos = next_norm;
        orig_byte_pos += ch.len_utf8();
    }
    if norm_char_pos >= norm_pos {
        Some(original.len())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_normalize(input: &str, expected: &str) {
        assert_eq!(fuzzy_normalize(input), expected);
    }

    #[test]
    fn test_fuzzy_smart_quotes() {
        check_normalize("\u{201c}hello\u{201d}", "\"hello\"");
    }

    #[test]
    fn test_fuzzy_dashes() {
        check_normalize("a\u{2014}b", "a-b");
    }

    #[test]
    fn test_fuzzy_trailing_whitespace() {
        check_normalize("hello  \nworld  ", "hello\nworld");
    }

    #[test]
    fn test_fuzzy_nfkc() {
        check_normalize("\u{2160}", "I"); // ROMAN NUMERAL ONE → I
    }

    #[test]
    fn test_apply_edit_exact() {
        let result = apply_edit("hello world", "hello", "goodbye", 0).unwrap();
        assert_eq!(result, "goodbye world");
    }

    #[test]
    fn test_apply_edit_fuzzy() {
        let result = apply_edit("hello\u{2014}world", "hello-world", "hi-world", 0).unwrap();
        assert_eq!(result, "hi-world");
    }

    #[test]
    fn test_apply_edit_not_found() {
        let result = apply_edit("hello", "xyz", "abc", 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_count_fuzzy_matches() {
        let content = "a\n\u{201c}B\u{201d}\n\"B\"\n";
        assert_eq!(count_fuzzy_matches(content, "\"B\""), 2);
    }

    #[test]
    fn test_count_no_match() {
        assert_eq!(count_fuzzy_matches("hello world", "xyz"), 0);
    }

    #[test]
    fn test_context_window_single_edit() {
        let changed: HashSet<usize> = [3].into();
        let content = "line1\nline2\nLINE3\nline4\nline5\n";
        let result = build_context_window("test.rs", content, &changed, 1, 1);
        assert!(result.contains("     1 | line1\n"));
        assert!(result.contains("     3~| LINE3\n"));
        assert!(result.contains("     5 | line5\n"));
        assert!(!result.contains("..."));
    }

    #[test]
    fn test_context_window_two_disjoint() {
        let changed: HashSet<usize> = [2, 14].into();
        let content = "a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nN\no\n";
        let result = build_context_window("test.rs", content, &changed, 2, 1);
        assert!(result.contains("     1 | a\n"));
        assert!(result.contains("     5 | e\n"));
        assert!(result.contains("...\n"));
        assert!(result.contains("    11 | k\n"));
        assert!(result.contains("    15 | o\n"));
    }

    #[test]
    fn test_context_window_nearby_merge() {
        let changed: HashSet<usize> = [2, 4].into();
        let content = "a\nB\nc\nD\ne\nf\ng\n";
        let result = build_context_window("test.rs", content, &changed, 2, 1);
        assert!(result.contains("     1 | a\n"));
        assert!(result.contains("     7 | g\n"));
        assert!(!result.contains("..."));
    }

    #[test]
    fn test_context_window_clamp_start() {
        let changed: HashSet<usize> = [1].into();
        let content = "A\nb\nc\nd\ne\n";
        let result = build_context_window("test.rs", content, &changed, 1, 1);
        assert!(result.contains("     1~| A\n"));
        assert!(result.contains("     4 | d\n"));
        assert!(result.contains("...\n"));
        assert!(!result.contains("     5 |"));
    }

    #[test]
    fn test_context_window_clamp_end() {
        let changed: HashSet<usize> = [5].into();
        let content = "a\nb\nc\nd\nE\n";
        let result = build_context_window("test.rs", content, &changed, 1, 1);
        assert!(result.contains("...\n"));
        assert!(result.contains("     2 | b\n"));
        assert!(result.contains("     5~| E\n"));
        assert!(!result.contains("     1 |"));
    }

    #[test]
    fn test_find_changed_lines_basic() {
        let content = "line1\nline2\nline3\n";
        let new_texts = vec!["line3".to_string()];
        let changed = find_changed_lines(content, &new_texts);
        assert_eq!(changed.len(), 1);
        assert!(changed.contains(&3));
    }

    #[test]
    fn test_find_changed_lines_multi() {
        let content = "a\nB\nc\nD\ne\nf\ng\n";
        let new_texts = vec!["B".to_string(), "D".to_string()];
        let changed = find_changed_lines(content, &new_texts);
        assert!(changed.contains(&2));
        assert!(changed.contains(&4));
    }
}
