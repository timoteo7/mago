//! Linting command implementation.
//!
//! This module implements the `mago lint` command, which checks PHP code against a
//! configurable set of linting rules to identify style violations, code smells, and
//! potential quality issues.
//!
//! # Features
//!
//! The linter provides several modes of operation:
//!
//! - **Full Linting** (default): Run all enabled rules against the codebase
//! - **Semantics Only** (`--semantics`): Only validate syntax and basic semantic structure
//! - **Pedantic Mode** (`--pedantic`): Enable all available rules for maximum thoroughness
//! - **Targeted Linting** (`--only`): Run only specific rules by code
//!
//! # Rule Discovery
//!
//! The command supports introspection of available rules:
//!
//! - **List Rules** (`--list-rules`): Display all currently enabled rules
//! - **Explain Rule** (`--explain <CODE>`): Show detailed documentation for a specific rule
//!
//! # Baseline Support
//!
//! The linter integrates with baseline functionality to enable incremental adoption
//! by ignoring pre-existing issues while catching new ones. See [`BaselineReportingArgs`]
//! for baseline options.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use bumpalo::Bump;
use clap::ColorChoice;
use clap::Parser;
use colored::Colorize;

use mago_database::DatabaseReader;
use mago_linter::registry::RuleRegistry;
use mago_linter::rule::AnyRule;
use mago_linter::rule_meta::RuleEntry;
use mago_orchestrator::service::lint::LintMode;
use mago_reporting::Level;
use mago_text_edit::ApplyResult;
use mago_text_edit::Safety;
use mago_text_edit::TextEditor;

use crate::commands::args::baseline_reporting::BaselineReportingArgs;
use crate::commands::stdin_input;
use crate::config::Configuration;
use crate::error::Error;
use crate::utils::create_orchestrator;
use crate::utils::git;
use mago_orchestrator::merge::merge_formatted_lines;

/// Command for linting PHP source code.
///
/// This command runs configurable linting rules to check code style, consistency,
/// and quality. It supports multiple modes including semantic-only validation,
/// pedantic checking, and targeted rule execution.
///
/// # Examples
///
/// Lint all configured source paths:
/// ```text
/// mago lint
/// ```
///
/// Lint specific files or directories:
/// ```text
/// mago lint src/ tests/
/// ```
///
/// Run only specific rules:
/// ```text
/// mago lint --only no-empty,constant-condition
/// ```
///
/// Show documentation for a rule:
/// ```text
/// mago lint --explain no-empty
/// ```
#[derive(Parser, Debug)]
#[command(
    name = "lint",
    about = "Lints PHP source code for style, consistency, and structural errors.",
    long_about = indoc::indoc! {"
        Analyzes PHP files to find and report stylistic issues, inconsistencies, and
        potential code quality improvements based on a configurable set of rules.

        This is the primary tool for ensuring your codebase adheres to established
        coding standards and best practices.

        USAGE:

            mago lint
            mago lint src/
            mago lint --list-rules
            mago lint --explain no-empty
            mago lint --only no-empty,constant-condition

        By default, it lints all source paths defined in your `mago.toml` file. You can
        also provide specific file or directory paths to lint a subset of your project.
    "}
)]
pub struct LintCommand {
    /// Specific files or directories to lint instead of using configuration.
    ///
    /// When provided, these paths override the source configuration in mago.toml.
    /// You can specify individual files or entire directories to lint.
    #[arg()]
    pub path: Vec<PathBuf>,

    /// Skip linter rules and only perform basic syntax and semantic validation.
    ///
    /// This mode only checks that your PHP code parses correctly and has valid
    /// semantic structure, without applying any style or quality rules.
    /// Useful for quick syntax validation.
    #[arg(long, short = 's', conflicts_with_all = ["list_rules", "explain", "only"])]
    pub semantics: bool,

    /// Enable every available linter rule for maximum thoroughness.
    ///
    /// This overrides your configuration and enables all rules, including those
    /// disabled by default. The output will be extremely verbose and is not
    /// recommended for regular use. Useful for comprehensive code audits.
    #[arg(long)]
    pub pedantic: bool,

    /// Show detailed documentation for a specific linter rule.
    ///
    /// Displays the rule's description, examples of good and bad code,
    /// and available configuration options. Use the rule's code name,
    /// such as 'no-empty' or 'prefer-while-loop'.
    #[arg(
        long,
        conflicts_with_all = ["list_rules", "sort", "fixable_only", "semantics", "reporting_target", "reporting_format"]
    )]
    pub explain: Option<String>,

    /// Show all currently enabled linter rules and their descriptions.
    ///
    /// This displays a table of all rules that are active for your current
    /// configuration, along with their severity levels and categories.
    /// Combine with --json for machine-readable output.
    #[arg(
        long,
        conflicts_with_all = ["explain", "sort", "fixable_only", "semantics", "reporting_target", "reporting_format"]
    )]
    pub list_rules: bool,

    /// Output rule information in JSON format.
    ///
    /// When combined with --list-rules, outputs rule information as JSON
    /// instead of a human-readable table. Useful for generating documentation
    /// or integrating with other tools.
    #[arg(long, requires = "list_rules")]
    pub json: bool,

    /// Run only specific rules, ignoring the configuration file.
    ///
    /// Provide a comma-separated list of rule codes to run only those rules.
    /// This overrides your mago.toml configuration and is useful for targeted
    /// analysis or testing specific rules.
    #[arg(short, long, conflicts_with = "semantics", num_args = 1.., value_delimiter = ',')]
    pub only: Vec<String>,

    /// Only lint files that are staged in git.
    ///
    /// This flag is designed for git pre-commit hooks. It will find all PHP files
    /// currently staged for commit and lint only those files.
    ///
    /// Fails if not in a git repository.
    #[arg(long, conflicts_with_all = ["path", "list_rules", "explain"])]
    pub staged: bool,

    /// Only fix lint issues in staged lines within staged files.
    ///
    /// This flag is designed for git pre-commit hooks to only fix issues in
    /// lines that are staged for commit, preserving unstaged changes.
    /// Requires --fix to be enabled.
    ///
    /// Fails if not in a git repository or if any staged file has unstaged changes.
    #[arg(long, conflicts_with_all = ["staged", "path", "list_rules", "explain"])]
    pub staged_lines: bool,

    /// Do not re-stage modified files after fixing.
    ///
    /// By default, fixed files are re-staged automatically when using --staged-lines.
    /// When this flag is used, the fixed changes will remain as unstaged changes.
    #[arg(long, requires = "staged_lines")]
    pub no_stage: bool,

    /// Only fix lint issues in lines changed in the last commit.
    ///
    /// This flag is similar to `--staged-lines` but operates on the files and lines
    /// from the most recent commit instead of the staging area. This is useful when
    /// you want to fix lint issues only in your changes after committing.
    /// Requires --fix to be enabled.
    ///
    /// Fails if not in a git repository.
    #[arg(long, conflicts_with_all = ["staged", "staged_lines", "path", "list_rules", "explain"])]
    pub last_commit: bool,

    /// Read the file content from stdin and use the given path for baseline and reporting.
    ///
    /// Intended for editor integrations: pipe unsaved buffer content and pass the real file path.
    #[arg(long, conflicts_with_all = ["list_rules", "explain", "staged"])]
    pub stdin_input: bool,

    #[clap(flatten)]
    pub baseline_reporting: BaselineReportingArgs,
}

impl LintCommand {
    /// Executes the lint command.
    ///
    /// This method orchestrates the linting process based on the command's configuration:
    ///
    /// 1. Creates an orchestrator with the configured settings
    /// 2. Applies path overrides if `path` was provided
    /// 3. Loads the database by scanning the file system
    /// 4. Creates the linting service with the database
    /// 5. Handles special modes (`--explain`, `--list-rules`)
    /// 6. Runs linting in the appropriate mode (full or semantics-only)
    /// 7. Processes and reports issues through the baseline processor
    ///
    /// # Arguments
    ///
    /// * `configuration` - The loaded configuration containing linter settings
    /// * `color_choice` - Whether to use colored output
    ///
    /// # Returns
    ///
    /// - `Ok(ExitCode::SUCCESS)` if linting completed successfully (even with issues found)
    /// - `Ok(ExitCode::FAILURE)` if a rule explanation was requested but the rule wasn't found
    /// - `Err(Error)` if database loading, linting, or reporting failed
    ///
    /// # Special Modes
    ///
    /// - **Explain Mode** (`--explain`): Displays detailed rule documentation and exits
    /// - **List Mode** (`--list-rules`): Shows all enabled rules and exits
    /// - **Empty Database**: Logs a message and exits successfully if no files found
    pub fn execute(self, mut configuration: Configuration, color_choice: ColorChoice) -> Result<ExitCode, Error> {
        let trace_enabled = tracing::enabled!(tracing::Level::TRACE);
        let command_start = trace_enabled.then(Instant::now);

        let editor_url = configuration.editor_url.take();

        let orchestrator_init_start = trace_enabled.then(Instant::now);
        let mut orchestrator = create_orchestrator(&configuration, color_choice, self.pedantic, true, false);
        orchestrator.add_exclude_patterns(configuration.linter.excludes.iter());

        let stdin_override = stdin_input::resolve_stdin_override(
            self.stdin_input,
            &self.path,
            &configuration.source.workspace,
            &mut orchestrator,
        )?;

        if !self.stdin_input && self.staged {
            let staged_paths = git::get_staged_file_paths(&configuration.source.workspace)?;
            if staged_paths.is_empty() {
                tracing::info!("No staged files to lint.");
                return Ok(ExitCode::SUCCESS);
            }

            if self.baseline_reporting.reporting.fix {
                git::ensure_staged_files_are_clean(&configuration.source.workspace, &staged_paths)?;
            }

            orchestrator.set_source_paths(staged_paths.iter().map(|p| p.to_string_lossy().to_string()));
        } else if self.staged_lines {
            return self.execute_staged_lines(configuration, color_choice);
        } else if self.last_commit {
            return self.execute_last_commit(configuration, color_choice);
        } else if !self.stdin_input && !self.path.is_empty() {
            stdin_input::set_source_paths_from_paths(&mut orchestrator, &self.path);
        }
        let orchestrator_init_duration = orchestrator_init_start.map(|s| s.elapsed());

        let load_database_start = trace_enabled.then(Instant::now);
        let mut database = orchestrator.load_database(&configuration.source.workspace, false, None, stdin_override)?;
        let load_database_duration = load_database_start.map(|s| s.elapsed());

        let service = orchestrator.get_lint_service(database.read_only());

        if let Some(explain_code) = self.explain {
            let registry = service.create_registry(
                if self.only.is_empty() { None } else { Some(&self.only) },
                self.pedantic, // Enable all rules if pedantic is set
            );

            return explain_rule(&registry, &explain_code);
        }

        if self.list_rules {
            let registry = service.create_registry(
                if self.only.is_empty() { None } else { Some(&self.only) },
                self.pedantic, // Enable all rules if pedantic is set
            );

            return list_rules(registry.rules(), self.json);
        }

        if database.is_empty() {
            tracing::info!("No files found to lint.");

            return Ok(ExitCode::SUCCESS);
        }

        let lint_run_start = trace_enabled.then(Instant::now);
        let issues = service.lint(
            if self.semantics { LintMode::SemanticsOnly } else { LintMode::Full },
            if self.only.is_empty() { None } else { Some(self.only.as_slice()) },
        )?;
        let lint_run_duration = lint_run_start.map(|s| s.elapsed());

        let report_start = trace_enabled.then(Instant::now);
        let baseline = configuration.linter.baseline.as_deref();
        let baseline_variant = configuration.linter.baseline_variant;
        let processor = self.baseline_reporting.get_processor(
            color_choice,
            baseline,
            baseline_variant,
            editor_url,
            configuration.linter.minimum_fail_level,
        );

        let (exit_code, changed_file_ids) = processor.process_issues(&orchestrator, &mut database, issues)?;
        let report_duration = report_start.map(|s| s.elapsed());

        if self.staged && !changed_file_ids.is_empty() {
            git::stage_files(&configuration.source.workspace, &database, changed_file_ids)?;
        }

        let drop_database_start = trace_enabled.then(Instant::now);
        drop(database);
        let drop_database_duration = drop_database_start.map(|s| s.elapsed());

        let drop_orchestrator_start = trace_enabled.then(Instant::now);
        drop(orchestrator);
        let drop_orchestrator_duration = drop_orchestrator_start.map(|s| s.elapsed());

        if let Some(start) = command_start {
            tracing::trace!("Orchestrator initialized in {:?}.", orchestrator_init_duration.unwrap_or_default());
            tracing::trace!("Database loaded in {:?}.", load_database_duration.unwrap_or_default());
            tracing::trace!("Lint service ran in {:?}.", lint_run_duration.unwrap_or_default());
            tracing::trace!("Issues filtered and reported in {:?}.", report_duration.unwrap_or_default());
            tracing::trace!("Database dropped in {:?}.", drop_database_duration.unwrap_or_default());
            tracing::trace!("Orchestrator dropped in {:?}.", drop_orchestrator_duration.unwrap_or_default());
            tracing::trace!("Lint command finished in {:?}.", start.elapsed());
        }

        Ok(exit_code)
    }

    /// Executes the lint command with staged-lines fix mode.
    ///
    /// Only fixes lint issues in lines that are staged for commit.
    fn execute_staged_lines(&self, configuration: Configuration, color_choice: ColorChoice) -> Result<ExitCode, Error> {
        let workspace = &configuration.source.workspace;

        let mut orchestrator = create_orchestrator(&configuration, color_choice, self.pedantic, true, false);
        orchestrator.add_exclude_patterns(configuration.linter.excludes.iter());

        // Get staged file paths
        let staged_paths = git::get_staged_file_paths(workspace)?;
        if staged_paths.is_empty() {
            tracing::info!("No staged files to lint.");
            return Ok(ExitCode::SUCCESS);
        }

        // Check for unstaged changes (required for fixing)
        if self.baseline_reporting.reporting.fix {
            git::ensure_staged_files_are_clean(workspace, &staged_paths)?;
        }

        let database = orchestrator.load_database(workspace, false, None, None)?;

        // Determine safety threshold
        let safety_threshold = if self.baseline_reporting.reporting.r#unsafe {
            Safety::Unsafe
        } else if self.baseline_reporting.reporting.potentially_unsafe {
            Safety::PotentiallyUnsafe
        } else {
            Safety::Safe
        };

        // Process each file with line-level fixing
        let mut changed_file_ids = Vec::new();
        let mut skipped_files = 0;

        for staged_path in &staged_paths {
            // Get line ranges for this file
            let ranges = git::get_staged_line_ranges(workspace, staged_path)?;

            if ranges.is_empty() {
                skipped_files += 1;
                continue;
            }

            // Get file from database
            let absolute_path = workspace.join(staged_path);
            let canonical_path = absolute_path.canonicalize().unwrap_or(absolute_path);
            let file = match database.get_by_path(&canonical_path) {
                Ok(f) => f,
                Err(_) => {
                    skipped_files += 1;
                    continue;
                }
            };

            // Get the lint service for this file
            let service = orchestrator.get_lint_service(database.read_only());
            let issues = service.lint_file(&file, LintMode::Full, None);

            // Filter issues to only those within staged line ranges
            let mut batches: Vec<(Option<String>, Vec<mago_text_edit::TextEdit>)> = Vec::new();

            for issue in issues.into_iter() {
                if issue.edits.is_empty() {
                    continue;
                }

                let span = match issue.primary_span() {
                    Some(s) => s,
                    None => continue,
                };

                let start_line = file.line_number(span.start.offset);
                let end_line = file.line_number(span.end.offset);

                // Check if issue overlaps with any staged range
                let in_range = ranges.iter().any(|range| {
                    (range.start as u32) <= end_line && (range.end as u32) >= start_line
                });

                if in_range {
                    // Get edits for this file
                    if let Some(edits) = issue.edits.get(&file.id) {
                        batches.push((issue.code.clone(), edits.clone()));
                    }
                }
            }

            if batches.is_empty() {
                skipped_files += 1;
                continue;
            }

            // Apply fixes using TextEditor
            let arena = Bump::new();
            let mut editor = TextEditor::with_safety(&file.contents, safety_threshold);
            let parser_settings = orchestrator.config.parser_settings;
            let file_id = file.id;

            let checker = |code: &str| -> bool {
                use mago_syntax::parser::parse_file_content_with_settings;
                !parse_file_content_with_settings(&arena, file_id, code, parser_settings).has_errors()
            };

            let mut applied_any = false;
            for (rule_code, edits) in batches {
                let rule_code = rule_code.as_deref().unwrap_or("unknown");
                match editor.apply_batch(edits, Some(&checker)) {
                    ApplyResult::Applied => {
                        applied_any = true;
                    }
                    _ => {
                        tracing::warn!(
                            "Skipped fix for rule `{}` when fixing staged lines in `{}`",
                            rule_code,
                            file.name.as_ref()
                        );
                    }
                }
            }

            let fixed_content = editor.finish();

            if !applied_any || fixed_content == file.contents {
                skipped_files += 1;
                continue;
            }

            // Extract fixed chunks within ranges and merge back
            let fixed_lines: Vec<&str> = fixed_content.lines().collect();
            let mut formatted_chunks: Vec<String> = Vec::with_capacity(ranges.len());
            let mut successful_ranges = Vec::with_capacity(ranges.len());

            for range in &ranges {
                let start_idx = range.start.saturating_sub(1);
                let end_idx = range.end.saturating_sub(1);

                if start_idx >= fixed_lines.len() || end_idx >= fixed_lines.len() || start_idx > end_idx {
                    continue;
                }

                let chunk_lines: Vec<&str> = fixed_lines[start_idx..=end_idx].iter().copied().collect();
                let chunk_content = chunk_lines.join("\n");

                formatted_chunks.push(chunk_content);
                successful_ranges.push(*range);
            }

            if formatted_chunks.is_empty() {
                skipped_files += 1;
                continue;
            }

            // Merge the fixed chunks back into the original content
            let merged_content = match merge_formatted_lines(&file.contents, &successful_ranges, formatted_chunks) {
                Ok(content) => content,
                Err(e) => {
                    tracing::warn!("Failed to merge fixes in '{}': {}", file.name, e);
                    skipped_files += 1;
                    continue;
                }
            };

            if merged_content == file.contents {
                skipped_files += 1;
                continue;
            }

            // Write to file
            if let Err(e) = std::fs::write(&canonical_path, &merged_content) {
                tracing::warn!("Failed to write fixed content to '{}': {}", file.name, e);
                continue;
            }

            changed_file_ids.push(file.id);
        }

        // Re-stage modified files only if --no-stage is not set
        if !self.no_stage && !changed_file_ids.is_empty() {
            git::stage_files(workspace, &database, changed_file_ids.clone())?;
            tracing::info!("Fixed and re-staged {} file(s).", changed_file_ids.len());
        } else if self.no_stage && !changed_file_ids.is_empty() {
            tracing::info!("Fixed {} file(s). Changes are unstaged (use 'git add' to stage).", changed_file_ids.len());
        } else if skipped_files > 0 {
            tracing::info!("No staged line fixes needed ({} file(s) checked).", skipped_files);
        } else {
            tracing::info!("All staged lines are already lint-free.");
        }

        Ok(ExitCode::SUCCESS)
    }

    /// Executes the lint command with last-commit fix mode.
    ///
    /// Only fixes lint issues in lines changed in the last commit.
    fn execute_last_commit(&self, configuration: Configuration, color_choice: ColorChoice) -> Result<ExitCode, Error> {
        let workspace = &configuration.source.workspace;

        let mut orchestrator = create_orchestrator(&configuration, color_choice, self.pedantic, true, false);
        orchestrator.add_exclude_patterns(configuration.linter.excludes.iter());

        // Get files from last commit with their change status
        let commit_files = git::get_last_commit_files_with_status(workspace)?;
        if commit_files.is_empty() {
            tracing::info!("No files found in the last commit.");
            return Ok(ExitCode::SUCCESS);
        }

        // Filter to only PHP files
        let php_extensions: Vec<&str> = configuration.source.extensions.iter().map(|s| s.as_str()).collect();
        let filtered_files: Vec<(PathBuf, git::FileChangeType)> = commit_files
            .into_iter()
            .filter(|(p, _)| {
                p.extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| php_extensions.contains(&ext))
                    .unwrap_or(false)
            })
            .collect();

        if filtered_files.is_empty() {
            tracing::info!("No PHP files found in the last commit.");
            return Ok(ExitCode::SUCCESS);
        }

        // Override source paths to only files from last commit
        orchestrator.set_source_paths(filtered_files.iter().map(|(p, _)| p.to_string_lossy().to_string()));

        let database = orchestrator.load_database(workspace, false, None, None)?;

        // Determine safety threshold
        let safety_threshold = if self.baseline_reporting.reporting.r#unsafe {
            Safety::Unsafe
        } else if self.baseline_reporting.reporting.potentially_unsafe {
            Safety::PotentiallyUnsafe
        } else {
            Safety::Safe
        };

        // Process each file with line-level fixing
        let mut changed_file_ids = Vec::new();
        let mut skipped_files = 0;

        for (commit_path, change_type) in &filtered_files {
            // For new files, fix the entire file
            // For modified files, fix only the changed lines
            let ranges = if *change_type == git::FileChangeType::Added {
                Vec::new() // Empty ranges = process entire file
            } else {
                git::get_last_commit_line_ranges(workspace, commit_path)?
            };

            if ranges.is_empty() && *change_type != git::FileChangeType::Added {
                // Modified file with no ranges or new file - continue with full file processing
                if *change_type == git::FileChangeType::Added {
                    // For new files with empty ranges, we'll lint the entire file
                } else {
                    skipped_files += 1;
                    continue;
                }
            }

            // Get file from database
            let absolute_path = workspace.join(commit_path);
            let canonical_path = absolute_path.canonicalize().unwrap_or(absolute_path);
            let file = match database.get_by_path(&canonical_path) {
                Ok(f) => f,
                Err(_) => {
                    skipped_files += 1;
                    continue;
                }
            };

            // Get the lint service for this file
            let service = orchestrator.get_lint_service(database.read_only());
            let issues = service.lint_file(&file, LintMode::Full, None);

            // Filter issues to only those within commit line ranges
            // For new files (Added), include ALL issues
            let mut batches: Vec<(Option<String>, Vec<mago_text_edit::TextEdit>)> = Vec::new();

            for issue in issues.into_iter() {
                if issue.edits.is_empty() {
                    continue;
                }

                let span = match issue.primary_span() {
                    Some(s) => s,
                    None => continue,
                };

                let start_line = file.line_number(span.start.offset);
                let end_line = file.line_number(span.end.offset);

                // For new files, include all issues
                // For modified files, check if issue overlaps with any commit range
                let in_range = *change_type == git::FileChangeType::Added || ranges.iter().any(|range| {
                    (range.start as u32) <= end_line && (range.end as u32) >= start_line
                });

                if in_range {
                    // Get edits for this file
                    if let Some(edits) = issue.edits.get(&file.id) {
                        batches.push((issue.code.clone(), edits.clone()));
                    }
                }
            }

            if batches.is_empty() {
                skipped_files += 1;
                continue;
            }

            // Apply fixes using TextEditor
            let arena = Bump::new();
            let mut editor = TextEditor::with_safety(&file.contents, safety_threshold);
            let parser_settings = orchestrator.config.parser_settings;
            let file_id = file.id;

            let checker = |code: &str| -> bool {
                use mago_syntax::parser::parse_file_content_with_settings;
                !parse_file_content_with_settings(&arena, file_id, code, parser_settings).has_errors()
            };

            let mut applied_any = false;
            for (rule_code, edits) in batches {
                let rule_code = rule_code.as_deref().unwrap_or("unknown");
                match editor.apply_batch(edits, Some(&checker)) {
                    ApplyResult::Applied => {
                        applied_any = true;
                    }
                    _ => {
                        tracing::warn!(
                            "Skipped fix for rule `{}` when fixing last commit lines in `{}`",
                            rule_code,
                            file.name.as_ref()
                        );
                    }
                }
            }

            let fixed_content = editor.finish();

            if !applied_any || fixed_content == file.contents {
                skipped_files += 1;
                continue;
            }

            // Extract fixed chunks within ranges and merge back
            let fixed_lines: Vec<&str> = fixed_content.lines().collect();
            let mut formatted_chunks: Vec<String> = Vec::with_capacity(ranges.len());
            let mut successful_ranges = Vec::with_capacity(ranges.len());

            for range in &ranges {
                let start_idx = range.start.saturating_sub(1);
                let end_idx = range.end.saturating_sub(1);

                if start_idx >= fixed_lines.len() || end_idx >= fixed_lines.len() || start_idx > end_idx {
                    continue;
                }

                let chunk_lines: Vec<&str> = fixed_lines[start_idx..=end_idx].iter().copied().collect();
                let chunk_content = chunk_lines.join("\n");

                formatted_chunks.push(chunk_content);
                successful_ranges.push(*range);
            }

            if formatted_chunks.is_empty() {
                skipped_files += 1;
                continue;
            }

            // Merge the fixed chunks back into the original content
            let merged_content = match mago_orchestrator::merge::merge_formatted_lines(&file.contents, &successful_ranges, formatted_chunks) {
                Ok(content) => content,
                Err(e) => {
                    tracing::warn!("Failed to merge fixes in '{}': {}", file.name, e);
                    skipped_files += 1;
                    continue;
                }
            };

            if merged_content == file.contents {
                skipped_files += 1;
                continue;
            }

            // Write to file
            if let Err(e) = std::fs::write(&canonical_path, &merged_content) {
                tracing::warn!("Failed to write fixed content to '{}': {}", file.name, e);
                continue;
            }

            changed_file_ids.push(file.id);
        }

        if !changed_file_ids.is_empty() {
            tracing::info!("Fixed {} file(s) from the last commit.", changed_file_ids.len());
        } else if skipped_files > 0 {
            tracing::info!("No lines from the last commit needed fixing ({} file(s) checked).", skipped_files);
        } else {
            tracing::info!("All lines from the last commit are already lint-free.");
        }

        Ok(ExitCode::SUCCESS)
    }
}

/// Displays detailed documentation for a specific linting rule.
///
/// This function shows comprehensive information about a rule including its
/// description, code, category, and good/bad examples. The output is formatted
/// for terminal display with colors and proper wrapping.
///
/// # Arguments
///
/// * `registry` - The rule registry containing all available rules
/// * `code` - The rule code to explain (e.g., "no-empty", "prefer-while-loop")
///
/// # Returns
///
/// - `Ok(ExitCode::SUCCESS)` if the rule was found and explained
/// - `Ok(ExitCode::FAILURE)` if the rule code doesn't exist in the registry
///
/// # Output Format
///
/// The explanation includes:
/// - Rule name and description (wrapped to 80 characters)
/// - Rule code and category
/// - Good example (if available)
/// - Bad example (if available)
/// - Suggested command to try the rule
pub fn explain_rule(registry: &RuleRegistry, code: &str) -> Result<ExitCode, Error> {
    let Some(rule) = registry.rules().iter().find(|r| r.meta().code == code) else {
        println!();
        println!("  {}", "Error: Rule not found".red().bold());
        println!("  {}", format!("Could not find a rule with the code '{}'.", code).bright_black());
        println!("  {}", "Please check the spelling and try again.".bright_black());
        println!();

        return Ok(ExitCode::FAILURE);
    };

    let meta = rule.meta();

    println!();
    println!("  ╭─ {} {}", "Rule".bold(), meta.name.cyan().bold());
    println!("  │");

    println!("{}", wrap_and_prefix(meta.description, "  │  ", 80));

    println!("  │");
    println!("  │  {}: {}", "Code".bold(), meta.code.yellow());
    println!("  │  {}: {}", "Category".bold(), meta.category.as_str().magenta());

    if !meta.good_example.trim().is_empty() {
        println!("  │");
        println!("  │  {}", "✅ Good Example".green().bold());
        println!("  │");
        println!("{}", colorize_code_block(meta.good_example));
    }

    if !meta.bad_example.trim().is_empty() {
        println!("  │");
        println!("  │  {}", "🚫 Bad Example".red().bold());
        println!("  │");
        println!("{}", colorize_code_block(meta.bad_example));
    }

    println!("  │");
    println!("  │  {}", "Try it out!".bold());
    println!("  │    {}", format!("mago lint --only {}", meta.code).bright_black());
    println!("  ╰─");
    println!();

    Ok(ExitCode::SUCCESS)
}

/// Lists all enabled linting rules.
///
/// This function displays all currently active rules in either human-readable
/// table format or JSON format. The table is formatted with proper column
/// alignment showing rule name, code, severity level, and category.
///
/// # Arguments
///
/// * `rules` - The list of enabled rules to display
/// * `json` - Whether to output in JSON format instead of a table
///
/// # Returns
///
/// Always returns `Ok(ExitCode::SUCCESS)` since listing rules cannot fail.
///
/// # Output Formats
///
/// **Table Format** (default):
/// - Aligned columns for Name, Code, Level, and Category
/// - Color-coded severity levels (Error=red, Warning=yellow, etc.)
/// - Helpful footer with `--explain` command suggestion
///
/// **JSON Format** (`--json`):
/// - Array of rule metadata objects
/// - Pretty-printed for readability
/// - Machine-parseable for tooling integration
pub fn list_rules(rules: &[AnyRule], json: bool) -> Result<ExitCode, Error> {
    if rules.is_empty() && !json {
        println!("{}", "No rules are currently enabled.".yellow());

        return Ok(ExitCode::SUCCESS);
    }

    if json {
        let entries: Vec<_> = rules.iter().map(|r| RuleEntry { meta: r.meta(), level: r.default_level() }).collect();

        println!("{}", serde_json::to_string_pretty(&entries)?);

        return Ok(ExitCode::SUCCESS);
    }

    let max_name = rules.iter().map(|r| r.meta().name.len()).max().unwrap_or(0);
    let max_code = rules.iter().map(|r| r.meta().code.len()).max().unwrap_or(0);

    println!();
    println!(
        "  {: <width_name$}   {: <width_code$}   {: <8}   {}",
        "Name".bold().underline(),
        "Code".bold().underline(),
        "Level".bold().underline(),
        "Category".bold().underline(),
        width_name = max_name,
        width_code = max_code,
    );
    println!();

    for rule in rules {
        let meta = rule.meta();
        let level_str = match rule.default_level() {
            Level::Error => "Error".red(),
            Level::Warning => "Warning".yellow(),
            Level::Help => "Help".green(),
            Level::Note => "Note".blue(),
        };

        println!(
            "  {: <width_name$}   {: <width_code$}   {: <8}   {}",
            meta.name.cyan(),
            meta.code.bright_black(),
            level_str.bold(),
            meta.category.as_str().magenta(),
            width_name = max_name,
            width_code = max_code,
        );
    }

    println!();
    println!("  Run {} to see more information about a specific rule.", "mago lint --explain <CODE>".bold());
    println!();

    Ok(ExitCode::SUCCESS)
}

fn colorize_code_block(code: &str) -> String {
    let mut colored_code = String::new();
    for line in code.trim().lines() {
        let trimmed_line = line.trim_start();
        let indentation = &line[..line.len() - trimmed_line.len()];

        let colored_line =
            if trimmed_line.starts_with("<?php") || trimmed_line.starts_with("<?") || trimmed_line.starts_with("?>") {
                trimmed_line.yellow().bold().to_string()
            } else {
                trimmed_line.to_string()
            };

        colored_code.push_str(&format!("  │    {}{}\n", indentation.bright_black(), colored_line));
    }

    colored_code.trim_end().to_string()
}

fn wrap_and_prefix(text: &str, prefix: &str, width: usize) -> String {
    let mut result = String::new();
    let wrap_width = width.saturating_sub(prefix.len());

    for (i, paragraph) in text.trim().split("\n\n").enumerate() {
        if i > 0 {
            result.push_str(prefix);
            result.push('\n');
        }

        let mut current_line = String::new();
        for word in paragraph.split_whitespace() {
            if !current_line.is_empty() && current_line.len() + word.len() + 1 > wrap_width {
                result.push_str(prefix);
                result.push_str(&current_line);
                result.push('\n');
                current_line.clear();
            }

            if !current_line.is_empty() {
                current_line.push(' ');
            }
            current_line.push_str(word);
        }

        if !current_line.is_empty() {
            result.push_str(prefix);
            result.push_str(&current_line);
            result.push('\n');
        }
    }

    result.trim_end().to_string()
}
