//! Wrapping lines to the terminal's width.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Wraps every line to `width` terminal cells.
pub fn wrap_all(lines: &[String], width: usize) -> Vec<String> {
    lines.iter().flat_map(|line| wrap(line, width)).collect()
}

/// Wraps `line` into rows of at most `width` terminal cells, breaking after
/// a space where possible. Text is measured and split in grapheme clusters
/// (what the terminal shows as one character, such as an emoji made of
/// several code points), and widths are display cells, so wide (CJK, emoji)
/// characters count double.
pub fn wrap(line: &str, width: usize) -> Vec<String> {
    wrap_ranges(line, width)
        .into_iter()
        .map(|range| line[range].to_owned())
        .collect()
}

/// [`wrap`], as the byte range of `line` that each row shows: rows are
/// pieces of the line, without the spaces dropped where it wraps.
pub fn wrap_ranges(line: &str, width: usize) -> Vec<std::ops::Range<usize>> {
    if width == 0 {
        return std::iter::once(0..line.len()).collect();
    }
    let mut rows = Vec::new();
    // The row so far is `line[start..end]`.
    let mut start = 0;
    let mut row_width = 0;
    // Byte offset in `line` just after the row's last space.
    let mut break_at = None;
    for (end, g) in line.grapheme_indices(true) {
        let w = g.width();
        if row_width + w > width && end > start {
            if g == " " {
                // The row ends at this space, which is dropped.
                rows.push(start..end);
                start = end + g.len();
                row_width = 0;
                break_at = None;
                continue;
            }
            // Move the last word to a new row, unless only indentation
            // comes before it (which would leave a blank row).
            if let Some(i) = break_at.filter(|&i| !line[start..i].trim().is_empty()) {
                rows.push(start..start + line[start..i].trim_end().len());
                start = i;
                row_width = line[start..end].width();
            }
            break_at = None;
            // Still too wide (no break point, or the word itself is too
            // long): break right here.
            if row_width + w > width && end > start {
                rows.push(start..end);
                start = end;
                row_width = 0;
            }
        }
        row_width += w;
        if g == " " {
            break_at = Some(end + g.len());
        }
    }
    rows.push(start..line.len());
    rows
}

/// Truncates `text` to at most `width` terminal cells, without splitting a
/// grapheme cluster.
pub fn truncate(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for g in text.graphemes(true) {
        let w = g.width();
        if used + w > width {
            break;
        }
        out.push_str(g);
        used += w;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_at_spaces_by_display_width() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 4), [""]);
        // Each of these characters is two cells wide.
        assert_eq!(wrap("日本語です", 4), ["日本", "語で", "す"]);
        // After moving a word, a wide character must still fit.
        assert_eq!(wrap("ab cdef界", 5), ["ab", "cdef", "界"]);
        // Indentation alone doesn't make a blank row.
        assert_eq!(wrap(" bbbb界", 5), [" bbbb", "界"]);
        // A multi-code-point emoji is one two-cell grapheme.
        assert_eq!(wrap("ab👩‍💻", 4), ["ab👩‍💻"]);
        assert_eq!(wrap("abc👩‍💻", 4), ["abc", "👩‍💻"]);
        for row in wrap("aa bb 界界界 cc 日本語 d", 5) {
            assert!(row.width() <= 5, "{row:?} is too wide");
        }
    }

    #[test]
    fn truncates_text_to_cells() {
        assert_eq!(truncate("Alice Example", 5), "Alice");
        assert_eq!(truncate("a👩‍💻b", 2), "a");
        assert_eq!(truncate("a👩‍💻b", 3), "a👩‍💻");
    }

    #[test]
    fn wrapped_rows_know_where_they_come_from() {
        // The first of two spaces at the wrap is dropped; the second is
        // shown.
        assert_eq!(wrap_ranges("aaaa  bb", 4), [0..4, 5..8]);
        assert_eq!(wrap("aaaa  bb", 4), ["aaaa", " bb"]);
        assert_eq!(wrap_ranges("one two three", 8), [0..7, 8..13]);
        assert_eq!(wrap_ranges("", 4).len(), 1);
    }
}
