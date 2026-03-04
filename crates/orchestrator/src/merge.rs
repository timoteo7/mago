//! Merge logic for combining formatted chunks with original file content.
//!
//! This module provides utilities for merging formatted code chunks back into
//! the original file content while preserving indentation and handling edge cases.
//!
//! The key function `merge_formatted_lines` processes ranges in reverse order
//! (bottom-up) to avoid line shift issues when replacing content.

/// Represents a range of lines in a file.
///
/// Used to track which lines in a staged file have been modified.
/// Line numbers are 1-based, matching git diff output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    /// The starting line number (1-based, inclusive).
    pub start: usize,
    /// The ending line number (1-based, inclusive).
    pub end: usize,
}

/// Errors that can occur during the merge operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeError {
    /// The specified line range is invalid (e.g., start > end, or out of bounds).
    InvalidRange { start: usize, end: usize },
    /// The number of formatted chunks doesn't match the number of ranges.
    ChunkCountMismatch { expected: usize, got: usize },
    /// A specified line number was not found in the original content.
    LineNotFound { line: usize },
}

/// Default context lines to add around changed ranges.
/// This ensures the formatter has enough context to make correct decisions
/// about spacing, indentation, and code structure.
pub const DEFAULT_CONTEXT_LINES: usize = 5;

/// Expands line ranges to include surrounding context.
///
/// This is useful when formatting only changed lines in a file, as it gives
/// the formatter enough context to make correct decisions about:
/// - Spacing around `use` statements
/// - Indentation relative to class/namespace declarations
/// - Blank lines between methods and properties
///
/// # Arguments
///
/// * `ranges` - The original line ranges to expand
/// * `total_lines` - Total number of lines in the file
/// * `context_lines` - Number of context lines to add before and after each range
///
/// # Returns
///
/// A new vector of ranges, potentially merged and expanded with context.
/// Adjacent or overlapping ranges are merged to avoid redundant formatting.
pub fn expand_ranges_with_context(
    ranges: &[LineRange],
    total_lines: usize,
    context_lines: usize,
) -> Vec<LineRange> {
    if ranges.is_empty() {
        return Vec::new();
    }

    // Expand each range with context
    let mut expanded: Vec<LineRange> = ranges
        .iter()
        .map(|range| {
            let start = range.start.saturating_sub(context_lines).max(1);
            let end = (range.end + context_lines).min(total_lines);
            LineRange { start, end }
        })
        .collect();

    // Sort by start line
    expanded.sort_by_key(|r| r.start);

    // Merge overlapping or adjacent ranges
    let mut merged: Vec<LineRange> = Vec::new();
    for range in expanded {
        if let Some(last) = merged.last_mut() {
            // If ranges overlap or are adjacent (within 5 lines), merge them
            if range.start <= last.end + 5 {
                last.end = last.end.max(range.end);
            } else {
                merged.push(range);
            }
        } else {
            merged.push(range);
        }
    }

    merged
}

impl std::fmt::Display for MergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MergeError::InvalidRange { start, end } => {
                write!(f, "Invalid line range: start ({start}) > end ({end})")
            }
            MergeError::ChunkCountMismatch { expected, got } => {
                write!(f, "Chunk count mismatch: expected {expected} chunks, got {got}")
            }
            MergeError::LineNotFound { line } => {
                write!(f, "Line {line} not found in original content")
            }
        }
    }
}

impl std::error::Error for MergeError {}

/// Merge formatted chunks into the original content at specified line ranges.
///
/// This function replaces portions of the original content with formatted chunks
/// at the specified line ranges. Ranges are processed in reverse order (bottom-up)
/// to avoid line shift issues.
///
/// # Arguments
///
/// * `original` - The original file content
/// * `ranges` - The line ranges to replace (1-based, inclusive)
/// * `formatted_chunks` - The formatted content to insert at each range
///
/// # Returns
///
/// The merged content with all ranges replaced, or an error if:
/// - The number of chunks doesn't match the number of ranges
/// - A range is invalid (start > end)
/// - A range refers to a line that doesn't exist
pub fn merge_formatted_lines(
    original: &str,
    ranges: &[LineRange],
    formatted_chunks: Vec<String>,
) -> Result<String, MergeError> {
    // Handle empty input
    if ranges.is_empty() {
        return Ok(original.to_string());
    }

    // Validate chunk count matches range count
    if formatted_chunks.len() != ranges.len() {
        return Err(MergeError::ChunkCountMismatch { expected: ranges.len(), got: formatted_chunks.len() });
    }

    // Validate all ranges
    for range in ranges {
        if range.start > range.end {
            return Err(MergeError::InvalidRange { start: range.start, end: range.end });
        }
    }

    // Split original into lines (preserve line endings for reconstruction)
    let lines: Vec<&str> = original.lines().collect();
    let total_lines = lines.len();

    // Validate all ranges are within bounds
    for range in ranges {
        if range.end > total_lines {
            return Err(MergeError::LineNotFound { line: range.end });
        }
    }

    // Create mutable copy of lines
    let mut result_lines: Vec<String> = lines.iter().map(|s| s.to_string()).collect();

    // Process ranges in reverse order (bottom-up) to avoid line shift issues
    let mut indexed_chunks: Vec<(usize, &LineRange, &String)> =
        ranges.iter().zip(formatted_chunks.iter()).enumerate().map(|(i, (range, chunk))| (i, range, chunk)).collect();

    // Sort by start line descending (process bottom ranges first)
    indexed_chunks.sort_by(|a, b| b.1.start.cmp(&a.1.start));

    for (_idx, range, chunk) in indexed_chunks {
        // Convert to 0-based index for array access
        let start_idx = range.start - 1;
        let end_idx = range.end - 1;

        // Split chunk into lines (chunk already has correct indentation from formatter)
        let chunk_lines: Vec<String> = chunk.lines().map(|s| s.to_string()).collect();

        // Replace the range with the new lines
        result_lines.splice(start_idx..=end_idx, chunk_lines);
    }

    // Reconstruct the content
    // Preserve trailing newline if original had one
    let has_trailing_newline = original.ends_with('\n');
    let mut result = result_lines.join("\n");
    if has_trailing_newline {
        result.push('\n');
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_single_line_change() {
        let original = "line1\nline2\nline3\n";
        let ranges = vec![LineRange { start: 2, end: 2 }];
        let chunks = vec!["formatted_line2\n".to_string()];

        let result = merge_formatted_lines(original, &ranges, chunks).unwrap();
        assert_eq!(result, "line1\nformatted_line2\nline3\n");
    }

    #[test]
    fn test_merge_with_indentation_preservation() {
        let original = "class Test {\n    public function foo() {\n        return true;\n    }\n}\n";
        let ranges = vec![LineRange { start: 2, end: 4 }];
        // Chunk already has correct indentation from formatter
        let chunks = vec!["    public function foo() {\n        return true;\n    }".to_string()];

        let result = merge_formatted_lines(original, &ranges, chunks).unwrap();
        // The formatted chunk should be inserted as-is (formatter already set correct indent)
        assert!(result.contains("    public function foo()"));
        assert!(result.contains("        return true;"));
    }

    #[test]
    fn test_merge_multiple_ranges_reverse_order() {
        let original = "line1\nline2\nline3\nline4\nline5\n";
        let ranges = vec![LineRange { start: 2, end: 2 }, LineRange { start: 4, end: 4 }];
        let chunks = vec!["new_line2\n".to_string(), "new_line4\n".to_string()];

        let result = merge_formatted_lines(original, &ranges, chunks).unwrap();
        assert_eq!(result, "line1\nnew_line2\nline3\nnew_line4\nline5\n");
    }

    #[test]
    fn test_merge_empty_input() {
        let original = "unchanged content\n";
        let ranges: Vec<LineRange> = vec![];
        let chunks: Vec<String> = vec![];

        let result = merge_formatted_lines(original, &ranges, chunks).unwrap();
        assert_eq!(result, original);
    }

    #[test]
    fn test_merge_chunk_count_mismatch() {
        let original = "line1\nline2\n";
        let ranges = vec![LineRange { start: 1, end: 1 }];
        let chunks: Vec<String> = vec![];

        let result = merge_formatted_lines(original, &ranges, chunks);
        assert!(matches!(result, Err(MergeError::ChunkCountMismatch { expected: 1, got: 0 })));
    }

    #[test]
    fn test_merge_invalid_range() {
        let original = "line1\nline2\n";
        let ranges = vec![LineRange { start: 5, end: 3 }]; // start > end
        let chunks = vec!["chunk\n".to_string()];

        let result = merge_formatted_lines(original, &ranges, chunks);
        assert!(matches!(result, Err(MergeError::InvalidRange { start: 5, end: 3 })));
    }

    #[test]
    fn test_merge_line_not_found() {
        let original = "line1\nline2\n";
        let ranges = vec![LineRange { start: 1, end: 10 }]; // line 10 doesn't exist
        let chunks = vec!["chunk\n".to_string()];

        let result = merge_formatted_lines(original, &ranges, chunks);
        assert!(matches!(result, Err(MergeError::LineNotFound { line: 10 })));
    }

    #[test]
    fn test_merge_multiline_range() {
        let original = "line1\nline2\nline3\nline4\nline5\n";
        let ranges = vec![LineRange { start: 2, end: 4 }];
        let chunks = vec!["new2\nnew3\nnew4\n".to_string()];

        let result = merge_formatted_lines(original, &ranges, chunks).unwrap();
        assert_eq!(result, "line1\nnew2\nnew3\nnew4\nline5\n");
    }

    #[test]
    fn test_merge_preserves_trailing_newline() {
        let with_newline = "line1\nline2\n";
        let without_newline = "line1\nline2";

        let ranges = vec![LineRange { start: 1, end: 1 }];
        let chunks = vec!["new1\n".to_string()];

        let result = merge_formatted_lines(with_newline, &ranges, chunks.clone()).unwrap();
        assert!(result.ends_with('\n'));

        let result = merge_formatted_lines(without_newline, &ranges, chunks).unwrap();
        assert!(!result.ends_with('\n'));
    }
}
