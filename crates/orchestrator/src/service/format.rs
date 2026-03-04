use foldhash::HashMap;
use foldhash::HashMapExt;

use bumpalo::Bump;
use mago_database::ReadDatabase;
use mago_database::file::File;
use mago_database::file::FileId;
use mago_formatter::Formatter;
use mago_formatter::settings::FormatSettings;
use mago_php_version::PHPVersion;
use mago_syntax::error::ParseError;
use mago_syntax::settings::ParserSettings;
use std::borrow::Cow;

use crate::error::OrchestratorError;
use crate::merge::{LineRange, merge_formatted_lines};
use crate::service::pipeline::StatelessParallelPipeline;
use crate::service::pipeline::StatelessReducer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileFormatStatus {
    Unchanged,
    Changed(String),
    FailedToParse(ParseError),
}

#[derive(Debug)]
pub struct FormatResult {
    pub changed_files: HashMap<FileId, FileFormatStatus>,
}

#[derive(Debug)]
pub struct FormatService {
    database: ReadDatabase,
    php_version: PHPVersion,
    settings: FormatSettings,
    parser_settings: ParserSettings,
    use_progress_bars: bool,
}

impl FormatService {
    #[must_use]
    pub fn new(
        database: ReadDatabase,
        php_version: PHPVersion,
        settings: FormatSettings,
        parser_settings: ParserSettings,
        use_progress_bars: bool,
    ) -> Self {
        Self { database, php_version, settings, parser_settings, use_progress_bars }
    }

    pub fn format_file(self, file: &File) -> Result<FileFormatStatus, OrchestratorError> {
        let arena = Bump::new();

        self.format_file_in(file, &arena)
    }

    pub fn format_file_in(self, file: &File, arena: &Bump) -> Result<FileFormatStatus, OrchestratorError> {
        let formatter =
            Formatter::new(arena, self.php_version, self.settings).with_parser_settings(self.parser_settings);

        match formatter.format_file(file) {
            Ok(formatted_content) => {
                if file.contents == formatted_content {
                    Ok(FileFormatStatus::Unchanged)
                } else {
                    Ok(FileFormatStatus::Changed(formatted_content.to_string()))
                }
            }
            Err(parse_error) => Ok(FileFormatStatus::FailedToParse(parse_error)),
        }
    }

    /// Formats only specific line ranges within a file.
    ///
    /// This method extracts the specified line ranges, formats each chunk in isolation,
    /// and merges the formatted chunks back into the original file content.
    /// Lines outside the specified ranges remain unchanged.
    ///
    /// # Arguments
    ///
    /// * `file` - The file to format
    /// * `arena` - A bump allocator for temporary allocations during formatting
    /// * `ranges` - The line ranges to format (1-based, inclusive)
    ///
    /// # Returns
    ///
    /// - `Ok(FileFormatStatus::Unchanged)` if no changes were made
    /// - `Ok(FileFormatStatus::Changed(String))` with the merged content if formatting was applied
    /// - `Ok(FileFormatStatus::FailedToParse(ParseError))` if the file couldn't be parsed
    /// - `Err(OrchestratorError)` if formatting failed for other reasons
    ///
    /// # Behavior
    ///
    /// - Empty ranges: Falls back to full file formatting via `format_file_in`
    /// - Parse errors in chunks: Skips the problematic chunk and continues with others
    /// - Merge errors: Returns `Unchanged` to avoid data loss
    pub fn format_line_ranges(
        self,
        file: &File,
        arena: &Bump,
        ranges: &[LineRange],
    ) -> Result<FileFormatStatus, OrchestratorError> {
        // Handle empty ranges - format the entire file
        if ranges.is_empty() {
            return self.format_file_in(file, arena);
        }

        // Split file into lines for range extraction
        let original_lines: Vec<&str> = file.contents.lines().collect();
        let mut formatted_chunks: Vec<String> = Vec::with_capacity(ranges.len());
        let mut successful_ranges: Vec<LineRange> = Vec::with_capacity(ranges.len());

        // Create formatter for chunk formatting
        let formatter =
            Formatter::new(arena, self.php_version, self.settings).with_parser_settings(self.parser_settings);

        // Process each range
        for range in ranges {
            // Convert to 0-based indices
            let start_idx = range.start.saturating_sub(1);
            let end_idx = range.end.saturating_sub(1);

            // Bounds check
            if start_idx >= original_lines.len() || end_idx >= original_lines.len() || start_idx > end_idx {
                // Skip invalid range
                continue;
            }

            // Extract lines for this range
            let range_lines: Vec<&str> = original_lines[start_idx..=end_idx].iter().copied().collect();
            let chunk_content = range_lines.join("\n");

            // Create an ephemeral file for this chunk
            let chunk_file = File::ephemeral(
                Cow::Owned(format!("chunk_range_{}_{}", range.start, range.end)),
                Cow::Owned(chunk_content),
            );

            // Format the chunk
            match formatter.format_file(&chunk_file) {
                Ok(formatted_content) => {
                    formatted_chunks.push(formatted_content.to_string());
                    successful_ranges.push(*range);
                }
                Err(_) => {
                    // On parse error, skip this chunk and continue with others
                    // This handles syntax errors gracefully
                    continue;
                }
            }
        }

        // If no chunks were successfully formatted, return unchanged
        if formatted_chunks.is_empty() {
            return Ok(FileFormatStatus::Unchanged);
        }

        // Merge formatted chunks back into original content
        match merge_formatted_lines(&file.contents, &successful_ranges, formatted_chunks) {
            Ok(merged_content) => {
                if merged_content == file.contents {
                    Ok(FileFormatStatus::Unchanged)
                } else {
                    Ok(FileFormatStatus::Changed(merged_content))
                }
            }
            Err(_) => {
                // On merge error, return unchanged to avoid data loss
                Ok(FileFormatStatus::Unchanged)
            }
        }
    }

    pub fn run(self) -> Result<FormatResult, OrchestratorError> {
        let context = FormatContext {
            php_version: self.php_version,
            settings: self.settings,
            parser_settings: self.parser_settings,
        };

        let pipeline = StatelessParallelPipeline::new(
            "✨ Formatting",
            self.database,
            context,
            Box::new(FormatReducer),
            self.use_progress_bars,
        );

        pipeline.run(|context, arena, file| {
            let formatter = Formatter::new(arena, context.php_version, context.settings)
                .with_parser_settings(context.parser_settings);
            let status = match formatter.format_file(&file) {
                Ok(formatted_content) => {
                    if file.contents == formatted_content {
                        FileFormatStatus::Unchanged
                    } else {
                        FileFormatStatus::Changed(formatted_content.to_string())
                    }
                }
                Err(parse_error) => FileFormatStatus::FailedToParse(parse_error),
            };

            let mut changed_files = HashMap::with_capacity(1);
            changed_files.insert(file.id, status);

            Ok(FormatResult { changed_files })
        })
    }

    /// Runs the formatter on a specific subset of files by ID.
    ///
    /// This method formats only the files with the given IDs, rather than all files
    /// in the database. This is useful for formatting only staged files in git
    /// pre-commit hooks.
    ///
    /// # Arguments
    ///
    /// * `file_ids` - Iterator of file IDs to format
    ///
    /// # Returns
    ///
    /// A [`FormatResult`] containing the formatting status for each processed file.
    pub fn run_on_files<Iter>(self, file_ids: Iter) -> Result<FormatResult, OrchestratorError>
    where
        Iter: IntoIterator<Item = FileId>,
    {
        let context = FormatContext {
            php_version: self.php_version,
            settings: self.settings,
            parser_settings: self.parser_settings,
        };

        let pipeline = StatelessParallelPipeline::new(
            "✨ Formatting",
            self.database,
            context,
            Box::new(FormatReducer),
            self.use_progress_bars,
        );

        pipeline.run_on_files(file_ids, |context, arena, file| {
            let formatter = Formatter::new(arena, context.php_version, context.settings)
                .with_parser_settings(context.parser_settings);
            let status = match formatter.format_file(&file) {
                Ok(formatted_content) => {
                    if file.contents == formatted_content {
                        FileFormatStatus::Unchanged
                    } else {
                        FileFormatStatus::Changed(formatted_content.to_string())
                    }
                }
                Err(parse_error) => FileFormatStatus::FailedToParse(parse_error),
            };

            let mut changed_files = HashMap::with_capacity(1);
            changed_files.insert(file.id, status);

            Ok(FormatResult { changed_files })
        })
    }
}

impl Default for FormatResult {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatResult {
    #[must_use]
    pub fn new() -> Self {
        Self { changed_files: HashMap::new() }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changed_files.is_empty()
    }

    #[must_use]
    pub fn is_successful(&self) -> bool {
        self.changed_files.values().all(|status| !matches!(status, FileFormatStatus::FailedToParse(_)))
    }

    #[must_use]
    pub fn is_failed(&self) -> bool {
        self.changed_files.values().any(|status| matches!(status, FileFormatStatus::FailedToParse(_)))
    }

    #[must_use]
    pub fn is_changed(&self) -> bool {
        self.changed_files.values().any(|status| matches!(status, FileFormatStatus::Changed(_)))
    }

    pub fn parse_errors(&self) -> impl Iterator<Item = (&FileId, &ParseError)> {
        self.changed_files.iter().filter_map(|(file_id, status)| {
            if let FileFormatStatus::FailedToParse(error) = status { Some((file_id, error)) } else { None }
        })
    }

    pub fn changed_files(&self) -> impl Iterator<Item = (&FileId, &String)> {
        self.changed_files.iter().filter_map(|(file_id, status)| {
            if let FileFormatStatus::Changed(content) = status { Some((file_id, content)) } else { None }
        })
    }

    #[must_use]
    pub fn changed_files_count(&self) -> usize {
        self.changed_files.values().filter(|status| matches!(status, FileFormatStatus::Changed(_))).count()
    }
}

/// Shared, read-only context provided to each parallel formatting task.
#[derive(Clone, Copy)]
struct FormatContext {
    /// The target PHP version for formatting rules.
    php_version: PHPVersion,
    /// The configured settings for the formatter.
    settings: FormatSettings,
    /// The parser settings.
    parser_settings: ParserSettings,
}

#[derive(Debug, Clone)]
struct FormatReducer;

impl StatelessReducer<FormatResult, FormatResult> for FormatReducer {
    fn reduce(&self, results: Vec<FormatResult>) -> Result<FormatResult, OrchestratorError> {
        let mut changed_files = HashMap::with_capacity(results.len());

        for result in results {
            changed_files.extend(result.changed_files);
        }

        Ok(FormatResult { changed_files })
    }
}
