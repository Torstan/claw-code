use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use glob::Pattern;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

/// Maximum file size that can be read (10 MB).
const MAX_READ_SIZE: u64 = 10 * 1024 * 1024;

/// Maximum file size that can be written (10 MB).
const MAX_WRITE_SIZE: usize = 10 * 1024 * 1024;

/// Check whether a file appears to contain binary content by examining
/// the first chunk for NUL bytes.
fn is_binary_file(path: &Path) -> io::Result<bool> {
    use std::io::Read;
    let mut file = fs::File::open(path)?;
    let mut buffer = [0u8; 8192];
    let bytes_read = file.read(&mut buffer)?;
    Ok(buffer[..bytes_read].contains(&0))
}

/// Validate that a resolved path stays within the given workspace root.
/// Returns the canonical path on success, or an error if the path escapes
/// the workspace boundary (e.g. via `../` traversal or symlink).
#[allow(dead_code)]
fn validate_workspace_boundary(resolved: &Path, workspace_root: &Path) -> io::Result<()> {
    if !resolved.starts_with(workspace_root) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "path {} escapes workspace boundary {}",
                resolved.display(),
                workspace_root.display()
            ),
        ));
    }
    Ok(())
}

/// Text payload returned by file-reading operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TextFilePayload {
    #[serde(rename = "filePath")]
    pub file_path: String,
    pub content: String,
    #[serde(rename = "numLines")]
    pub num_lines: usize,
    #[serde(rename = "startLine")]
    pub start_line: usize,
    #[serde(rename = "totalLines")]
    pub total_lines: usize,
}

/// Output envelope for the `read_file` tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadFileOutput {
    #[serde(rename = "type")]
    pub kind: String,
    pub file: TextFilePayload,
}

/// Structured patch hunk emitted by write and edit operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StructuredPatchHunk {
    #[serde(rename = "oldStart")]
    pub old_start: usize,
    #[serde(rename = "oldLines")]
    pub old_lines: usize,
    #[serde(rename = "newStart")]
    pub new_start: usize,
    #[serde(rename = "newLines")]
    pub new_lines: usize,
    pub lines: Vec<String>,
}

/// Output envelope for full-file write operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WriteFileOutput {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "filePath")]
    pub file_path: String,
    pub content: String,
    #[serde(rename = "structuredPatch")]
    pub structured_patch: Vec<StructuredPatchHunk>,
    #[serde(rename = "originalFile")]
    pub original_file: Option<String>,
    #[serde(rename = "gitDiff")]
    pub git_diff: Option<serde_json::Value>,
}

/// Output envelope for targeted string-replacement edits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditFileOutput {
    #[serde(rename = "filePath")]
    pub file_path: String,
    #[serde(rename = "oldString")]
    pub old_string: String,
    #[serde(rename = "newString")]
    pub new_string: String,
    #[serde(rename = "originalFile")]
    pub original_file: String,
    #[serde(rename = "structuredPatch")]
    pub structured_patch: Vec<StructuredPatchHunk>,
    #[serde(rename = "userModified")]
    pub user_modified: bool,
    #[serde(rename = "replaceAll")]
    pub replace_all: bool,
    #[serde(rename = "gitDiff")]
    pub git_diff: Option<serde_json::Value>,
}

/// Result of a glob-based filename search.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GlobSearchOutput {
    #[serde(rename = "durationMs")]
    pub duration_ms: u128,
    #[serde(rename = "numFiles")]
    pub num_files: usize,
    pub filenames: Vec<String>,
    pub truncated: bool,
}

/// Parameters accepted by the grep-style search tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrepSearchInput {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    #[serde(rename = "output_mode")]
    pub output_mode: Option<String>,
    #[serde(rename = "-B")]
    pub before: Option<usize>,
    #[serde(rename = "-A")]
    pub after: Option<usize>,
    #[serde(rename = "-C")]
    pub context_short: Option<usize>,
    pub context: Option<usize>,
    #[serde(rename = "-n")]
    pub line_numbers: Option<bool>,
    #[serde(rename = "-i")]
    pub case_insensitive: Option<bool>,
    #[serde(rename = "type")]
    pub file_type: Option<String>,
    pub head_limit: Option<usize>,
    pub offset: Option<usize>,
    pub multiline: Option<bool>,
}

/// Result payload returned by the grep-style search tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrepSearchOutput {
    pub mode: Option<String>,
    #[serde(rename = "numFiles")]
    pub num_files: usize,
    pub filenames: Vec<String>,
    pub content: Option<String>,
    #[serde(rename = "numLines")]
    pub num_lines: Option<usize>,
    #[serde(rename = "numMatches")]
    pub num_matches: Option<usize>,
    #[serde(rename = "appliedLimit")]
    pub applied_limit: Option<usize>,
    #[serde(rename = "appliedOffset")]
    pub applied_offset: Option<usize>,
}

/// Reads a text file and returns a line-windowed payload.
pub fn read_file(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> io::Result<ReadFileOutput> {
    let absolute_path = normalize_path(path)?;
    read_file_at_path(&absolute_path, offset, limit)
}

fn read_file_at_path(
    absolute_path: &Path,
    offset: Option<usize>,
    limit: Option<usize>,
) -> io::Result<ReadFileOutput> {
    // Check file size before reading
    let metadata = fs::metadata(absolute_path)?;
    if metadata.len() > MAX_READ_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "file is too large ({} bytes, max {} bytes)",
                metadata.len(),
                MAX_READ_SIZE
            ),
        ));
    }

    // Detect binary files
    if is_binary_file(absolute_path)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file appears to be binary",
        ));
    }

    let content = fs::read_to_string(absolute_path)?;
    let lines: Vec<&str> = content.lines().collect();
    let start_index = offset.unwrap_or(0).min(lines.len());
    let end_index = limit.map_or(lines.len(), |limit| {
        start_index.saturating_add(limit).min(lines.len())
    });
    let selected = lines[start_index..end_index].join("\n");

    Ok(ReadFileOutput {
        kind: String::from("text"),
        file: TextFilePayload {
            file_path: absolute_path.to_string_lossy().into_owned(),
            content: selected,
            num_lines: end_index.saturating_sub(start_index),
            start_line: start_index.saturating_add(1),
            total_lines: lines.len(),
        },
    })
}

/// Replaces a file's contents and returns patch metadata.
pub fn write_file(path: &str, content: &str) -> io::Result<WriteFileOutput> {
    let absolute_path = normalize_path_allow_missing(path)?;
    write_file_at_path(&absolute_path, content)
}

fn write_file_at_path(absolute_path: &Path, content: &str) -> io::Result<WriteFileOutput> {
    if content.len() > MAX_WRITE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "content is too large ({} bytes, max {} bytes)",
                content.len(),
                MAX_WRITE_SIZE
            ),
        ));
    }

    let original_file = fs::read_to_string(absolute_path).ok();
    if let Some(parent) = absolute_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(absolute_path, content)?;

    Ok(WriteFileOutput {
        kind: if original_file.is_some() {
            String::from("update")
        } else {
            String::from("create")
        },
        file_path: absolute_path.to_string_lossy().into_owned(),
        content: content.to_owned(),
        structured_patch: make_patch(original_file.as_deref().unwrap_or(""), content),
        original_file,
        git_diff: None,
    })
}

/// Performs an in-file string replacement and returns patch metadata.
pub fn edit_file(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> io::Result<EditFileOutput> {
    let absolute_path = normalize_path(path)?;
    edit_file_at_path(&absolute_path, old_string, new_string, replace_all)
}

fn edit_file_at_path(
    absolute_path: &Path,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> io::Result<EditFileOutput> {
    let original_file = fs::read_to_string(absolute_path)?;
    if old_string == new_string {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "old_string and new_string must differ",
        ));
    }
    let match_count = original_file.matches(old_string).count();
    if match_count == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "old_string not found in file",
        ));
    }
    if !replace_all && match_count != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "old_string matched {match_count} times; expected exactly one match when replace_all is false"
            ),
        ));
    }

    let updated = if replace_all {
        original_file.replace(old_string, new_string)
    } else {
        original_file.replacen(old_string, new_string, 1)
    };
    fs::write(absolute_path, &updated)?;

    Ok(EditFileOutput {
        file_path: absolute_path.to_string_lossy().into_owned(),
        old_string: old_string.to_owned(),
        new_string: new_string.to_owned(),
        original_file: original_file.clone(),
        structured_patch: make_patch(&original_file, &updated),
        user_modified: false,
        replace_all,
        git_diff: None,
    })
}

/// Expands a glob pattern and returns matching filenames.
pub fn glob_search(pattern: &str, path: Option<&str>) -> io::Result<GlobSearchOutput> {
    let started = Instant::now();
    let base_dir = path
        .map(normalize_path)
        .transpose()?
        .unwrap_or(std::env::current_dir()?);
    let search_pattern = if Path::new(pattern).is_absolute() {
        pattern.to_owned()
    } else {
        base_dir.join(pattern).to_string_lossy().into_owned()
    };

    // The `glob` crate does not support brace expansion ({a,b,c}).
    // Expand braces into multiple patterns so patterns like
    // `Assets/**/*.{cs,uxml,uss}` work correctly.
    let expanded = expand_braces(&search_pattern);

    let mut seen = std::collections::HashSet::new();
    let mut matches = Vec::new();
    for pat in &expanded {
        let entries = glob::glob(pat)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        for entry in entries.flatten() {
            if entry.is_file() && seen.insert(entry.clone()) {
                matches.push(entry);
            }
        }
    }

    matches.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .map(Reverse)
    });

    let truncated = matches.len() > 100;
    let filenames = matches
        .into_iter()
        .take(100)
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    Ok(GlobSearchOutput {
        duration_ms: started.elapsed().as_millis(),
        num_files: filenames.len(),
        filenames,
        truncated,
    })
}

/// Runs a regex search over workspace files with optional context lines.
pub fn grep_search(input: &GrepSearchInput) -> io::Result<GrepSearchOutput> {
    let base_path = input
        .path
        .as_deref()
        .map(normalize_path)
        .transpose()?
        .unwrap_or(std::env::current_dir()?);

    let regex = RegexBuilder::new(&input.pattern)
        .case_insensitive(input.case_insensitive.unwrap_or(false))
        .dot_matches_new_line(input.multiline.unwrap_or(false))
        .build()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;

    let glob_filter = input
        .glob
        .as_deref()
        .map(Pattern::new)
        .transpose()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
    let file_type = input.file_type.as_deref();
    let output_mode = input
        .output_mode
        .clone()
        .unwrap_or_else(|| String::from("files_with_matches"));
    let context = input.context.or(input.context_short).unwrap_or(0);

    let mut filenames = Vec::new();
    let mut content_lines = Vec::new();
    let mut total_matches = 0usize;

    for file_path in collect_search_files(&base_path)? {
        if !matches_optional_filters(&file_path, glob_filter.as_ref(), file_type) {
            continue;
        }

        let Ok(file_contents) = fs::read_to_string(&file_path) else {
            continue;
        };

        if output_mode == "count" {
            let count = regex.find_iter(&file_contents).count();
            if count > 0 {
                filenames.push(file_path.to_string_lossy().into_owned());
                total_matches += count;
            }
            continue;
        }

        let lines: Vec<&str> = file_contents.lines().collect();
        let (matched_lines, match_count) =
            matched_line_indices(&regex, &file_contents, input.multiline.unwrap_or(false));
        total_matches += match_count;

        if matched_lines.is_empty() {
            continue;
        }

        filenames.push(file_path.to_string_lossy().into_owned());
        if output_mode == "content" {
            for index in matched_lines {
                let start = index.saturating_sub(input.before.unwrap_or(context));
                let end = (index + input.after.unwrap_or(context) + 1).min(lines.len());
                for (current, line) in lines.iter().enumerate().take(end).skip(start) {
                    let prefix = if input.line_numbers.unwrap_or(true) {
                        format!("{}:{}:", file_path.to_string_lossy(), current + 1)
                    } else {
                        format!("{}:", file_path.to_string_lossy())
                    };
                    content_lines.push(format!("{prefix}{line}"));
                }
            }
        }
    }

    let (filenames, applied_limit, applied_offset) =
        apply_limit(filenames, input.head_limit, input.offset);
    let content_output = if output_mode == "content" {
        let (lines, limit, offset) = apply_limit(content_lines, input.head_limit, input.offset);
        return Ok(GrepSearchOutput {
            mode: Some(output_mode),
            num_files: filenames.len(),
            filenames,
            num_lines: Some(lines.len()),
            content: Some(lines.join("\n")),
            num_matches: None,
            applied_limit: limit,
            applied_offset: offset,
        });
    } else {
        None
    };

    Ok(GrepSearchOutput {
        mode: Some(output_mode.clone()),
        num_files: filenames.len(),
        filenames,
        content: content_output,
        num_lines: None,
        num_matches: (output_mode == "count").then_some(total_matches),
        applied_limit,
        applied_offset,
    })
}

fn collect_search_files(base_path: &Path) -> io::Result<Vec<PathBuf>> {
    if base_path.is_file() {
        return Ok(vec![base_path.to_path_buf()]);
    }

    let mut files = Vec::new();
    for entry in WalkDir::new(base_path) {
        let entry = entry.map_err(|error| io::Error::other(error.to_string()))?;
        if entry.file_type().is_file() {
            files.push(entry.path().to_path_buf());
        }
    }
    Ok(files)
}

fn matches_optional_filters(
    path: &Path,
    glob_filter: Option<&Pattern>,
    file_type: Option<&str>,
) -> bool {
    if let Some(glob_filter) = glob_filter {
        let path_string = path.to_string_lossy();
        if !glob_filter.matches(&path_string) && !glob_filter.matches_path(path) {
            return false;
        }
    }

    if let Some(file_type) = file_type {
        let extension = path.extension().and_then(|extension| extension.to_str());
        if extension != Some(file_type) {
            return false;
        }
    }

    true
}

fn matched_line_indices(
    regex: &regex::Regex,
    contents: &str,
    multiline: bool,
) -> (Vec<usize>, usize) {
    if !multiline {
        let lines = contents.lines().collect::<Vec<_>>();
        let matched = lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| regex.is_match(line).then_some(index))
            .collect::<Vec<_>>();
        return (matched.clone(), matched.len());
    }

    let line_starts = line_start_offsets(contents);
    let mut lines = BTreeSet::new();
    let mut matches = 0usize;
    for found in regex.find_iter(contents) {
        matches += 1;
        let start_line = line_index_for_offset(&line_starts, found.start());
        let end_offset = found.end().saturating_sub(1);
        let end_line = line_index_for_offset(&line_starts, end_offset);
        for line in start_line..=end_line {
            lines.insert(line);
        }
    }
    (lines.into_iter().collect(), matches)
}

fn line_start_offsets(contents: &str) -> Vec<usize> {
    let mut offsets = vec![0usize];
    for (index, ch) in contents.char_indices() {
        if ch == '\n' && index + 1 < contents.len() {
            offsets.push(index + 1);
        }
    }
    offsets
}

fn line_index_for_offset(line_starts: &[usize], offset: usize) -> usize {
    match line_starts.binary_search(&offset) {
        Ok(index) => index,
        Err(index) => index.saturating_sub(1),
    }
}

fn apply_limit<T>(
    items: Vec<T>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> (Vec<T>, Option<usize>, Option<usize>) {
    let offset_value = offset.unwrap_or(0);
    let mut items = items.into_iter().skip(offset_value).collect::<Vec<_>>();
    let explicit_limit = limit.unwrap_or(250);
    if explicit_limit == 0 {
        return (items, None, (offset_value > 0).then_some(offset_value));
    }

    let truncated = items.len() > explicit_limit;
    items.truncate(explicit_limit);
    (
        items,
        truncated.then_some(explicit_limit),
        (offset_value > 0).then_some(offset_value),
    )
}

const PATCH_CONTEXT_LINES: usize = 2;
const MAX_LCS_PATCH_CELLS: usize = 4_000_000;

#[derive(Clone, Copy)]
enum PatchLine<'a> {
    Equal(&'a str),
    Delete(&'a str),
    Insert(&'a str),
}

#[derive(Clone, Copy)]
struct PatchOp<'a> {
    line: PatchLine<'a>,
    old_index: usize,
    new_index: usize,
}

impl PatchOp<'_> {
    fn is_equal(self) -> bool {
        matches!(self.line, PatchLine::Equal(_))
    }

    fn is_change(self) -> bool {
        !self.is_equal()
    }

    fn consumes_old(self) -> bool {
        matches!(self.line, PatchLine::Equal(_) | PatchLine::Delete(_))
    }

    fn consumes_new(self) -> bool {
        matches!(self.line, PatchLine::Equal(_) | PatchLine::Insert(_))
    }

    fn serialized_line(self) -> String {
        match self.line {
            PatchLine::Equal(line) => format!(" {line}"),
            PatchLine::Delete(line) => format!("-{line}"),
            PatchLine::Insert(line) => format!("+{line}"),
        }
    }
}

fn make_patch(original: &str, updated: &str) -> Vec<StructuredPatchHunk> {
    if original == updated {
        return Vec::new();
    }

    let original_lines = original.lines().collect::<Vec<_>>();
    let updated_lines = updated.lines().collect::<Vec<_>>();
    if original_lines.len() == updated_lines.len() {
        let changed_indices = original_lines
            .iter()
            .zip(&updated_lines)
            .enumerate()
            .filter_map(|(index, (old, new))| (old != new).then_some(index))
            .collect::<Vec<_>>();
        if !changed_indices.is_empty() {
            return make_line_aligned_patch(&original_lines, &updated_lines, &changed_indices);
        }
    }

    if let Some(hunks) = make_lcs_patch(&original_lines, &updated_lines) {
        return hunks;
    }

    make_single_hunk_patch(&original_lines, &updated_lines)
}

fn make_line_aligned_patch(
    original_lines: &[&str],
    updated_lines: &[&str],
    changed_indices: &[usize],
) -> Vec<StructuredPatchHunk> {
    let mut hunks = Vec::new();
    let mut hunk_start = changed_indices[0].saturating_sub(PATCH_CONTEXT_LINES);
    let mut hunk_end = (changed_indices[0] + PATCH_CONTEXT_LINES + 1).min(original_lines.len());

    for &index in &changed_indices[1..] {
        let next_start = index.saturating_sub(PATCH_CONTEXT_LINES);
        let next_end = (index + PATCH_CONTEXT_LINES + 1).min(original_lines.len());
        if next_start <= hunk_end {
            hunk_end = hunk_end.max(next_end);
        } else {
            hunks.push(make_line_aligned_hunk(
                original_lines,
                updated_lines,
                hunk_start,
                hunk_end,
            ));
            hunk_start = next_start;
            hunk_end = next_end;
        }
    }

    hunks.push(make_line_aligned_hunk(
        original_lines,
        updated_lines,
        hunk_start,
        hunk_end,
    ));
    hunks
}

fn make_lcs_patch<'a>(
    original_lines: &[&'a str],
    updated_lines: &[&'a str],
) -> Option<Vec<StructuredPatchHunk>> {
    let row_count = original_lines.len().checked_add(1)?;
    let column_count = updated_lines.len().checked_add(1)?;
    if row_count.checked_mul(column_count)? > MAX_LCS_PATCH_CELLS {
        return None;
    }

    let mut lcs = vec![0usize; row_count * column_count];
    for old_index in (0..original_lines.len()).rev() {
        for new_index in (0..updated_lines.len()).rev() {
            let index = old_index * column_count + new_index;
            lcs[index] = if original_lines[old_index] == updated_lines[new_index] {
                lcs[(old_index + 1) * column_count + new_index + 1] + 1
            } else {
                lcs[(old_index + 1) * column_count + new_index]
                    .max(lcs[old_index * column_count + new_index + 1])
            };
        }
    }

    let mut ops = Vec::with_capacity(original_lines.len() + updated_lines.len());
    let mut old_index = 0usize;
    let mut new_index = 0usize;
    while old_index < original_lines.len() && new_index < updated_lines.len() {
        if original_lines[old_index] == updated_lines[new_index] {
            ops.push(PatchOp {
                line: PatchLine::Equal(original_lines[old_index]),
                old_index,
                new_index,
            });
            old_index += 1;
            new_index += 1;
        } else if lcs[(old_index + 1) * column_count + new_index]
            >= lcs[old_index * column_count + new_index + 1]
        {
            ops.push(PatchOp {
                line: PatchLine::Delete(original_lines[old_index]),
                old_index,
                new_index,
            });
            old_index += 1;
        } else {
            ops.push(PatchOp {
                line: PatchLine::Insert(updated_lines[new_index]),
                old_index,
                new_index,
            });
            new_index += 1;
        }
    }

    while old_index < original_lines.len() {
        ops.push(PatchOp {
            line: PatchLine::Delete(original_lines[old_index]),
            old_index,
            new_index,
        });
        old_index += 1;
    }
    while new_index < updated_lines.len() {
        ops.push(PatchOp {
            line: PatchLine::Insert(updated_lines[new_index]),
            old_index,
            new_index,
        });
        new_index += 1;
    }

    Some(make_hunks_from_ops(&ops))
}

fn make_hunks_from_ops(ops: &[PatchOp<'_>]) -> Vec<StructuredPatchHunk> {
    let mut windows = Vec::<(usize, usize)>::new();

    for (index, op) in ops.iter().enumerate() {
        if !op.is_change() {
            continue;
        }

        let start = hunk_start_for_change(ops, index);
        let end = hunk_end_for_change(ops, index);
        if let Some((_, last_end)) = windows.last_mut() {
            if start <= *last_end {
                *last_end = (*last_end).max(end);
                continue;
            }
        }
        windows.push((start, end));
    }

    windows
        .into_iter()
        .map(|(start, end)| make_hunk_from_ops(&ops[start..end]))
        .collect()
}

fn hunk_start_for_change(ops: &[PatchOp<'_>], change_index: usize) -> usize {
    let mut start = change_index;
    let mut context_lines = 0usize;
    while start > 0 && context_lines < PATCH_CONTEXT_LINES {
        start -= 1;
        if ops[start].is_equal() {
            context_lines += 1;
        }
    }
    start
}

fn hunk_end_for_change(ops: &[PatchOp<'_>], change_index: usize) -> usize {
    let mut end = change_index + 1;
    let mut context_lines = 0usize;
    while end < ops.len() && context_lines < PATCH_CONTEXT_LINES {
        if ops[end].is_equal() {
            context_lines += 1;
        }
        end += 1;
    }
    end
}

fn make_hunk_from_ops(ops: &[PatchOp<'_>]) -> StructuredPatchHunk {
    let first = ops
        .first()
        .expect("patch hunk should contain at least one operation");
    let mut old_lines = 0usize;
    let mut new_lines = 0usize;
    let mut lines = Vec::new();

    for op in ops {
        if op.consumes_old() {
            old_lines += 1;
        }
        if op.consumes_new() {
            new_lines += 1;
        }
        lines.push(op.serialized_line());
    }

    StructuredPatchHunk {
        old_start: first.old_index.saturating_add(1),
        old_lines,
        new_start: first.new_index.saturating_add(1),
        new_lines,
        lines,
    }
}

fn make_line_aligned_hunk(
    original_lines: &[&str],
    updated_lines: &[&str],
    start: usize,
    end: usize,
) -> StructuredPatchHunk {
    let mut lines = Vec::new();
    for index in start..end {
        if original_lines[index] == updated_lines[index] {
            lines.push(format!(" {}", original_lines[index]));
        } else {
            lines.push(format!("-{}", original_lines[index]));
            lines.push(format!("+{}", updated_lines[index]));
        }
    }

    StructuredPatchHunk {
        old_start: start.saturating_add(1),
        old_lines: end.saturating_sub(start),
        new_start: start.saturating_add(1),
        new_lines: end.saturating_sub(start),
        lines,
    }
}

fn make_single_hunk_patch(
    original_lines: &[&str],
    updated_lines: &[&str],
) -> Vec<StructuredPatchHunk> {
    let mut prefix_len = 0usize;
    while prefix_len < original_lines.len()
        && prefix_len < updated_lines.len()
        && original_lines[prefix_len] == updated_lines[prefix_len]
    {
        prefix_len += 1;
    }

    let mut suffix_len = 0usize;
    while suffix_len < original_lines.len().saturating_sub(prefix_len)
        && suffix_len < updated_lines.len().saturating_sub(prefix_len)
        && original_lines[original_lines.len() - 1 - suffix_len]
            == updated_lines[updated_lines.len() - 1 - suffix_len]
    {
        suffix_len += 1;
    }

    let old_change_end = original_lines.len().saturating_sub(suffix_len);
    let new_change_end = updated_lines.len().saturating_sub(suffix_len);
    let context_before_start = prefix_len.saturating_sub(2);
    let context_after_end = (old_change_end + 2).min(original_lines.len());
    let context_after_len = context_after_end.saturating_sub(old_change_end);

    let mut lines = Vec::new();
    for line in &original_lines[context_before_start..prefix_len] {
        lines.push(format!(" {line}"));
    }
    for line in &original_lines[prefix_len..old_change_end] {
        lines.push(format!("-{line}"));
    }
    for line in &updated_lines[prefix_len..new_change_end] {
        lines.push(format!("+{line}"));
    }
    for line in &original_lines[old_change_end..context_after_end] {
        lines.push(format!(" {line}"));
    }

    let old_lines = context_after_end.saturating_sub(context_before_start);
    let new_lines = prefix_len.saturating_sub(context_before_start)
        + new_change_end.saturating_sub(prefix_len)
        + context_after_len;

    vec![StructuredPatchHunk {
        old_start: context_before_start.saturating_add(1),
        old_lines,
        new_start: context_before_start.saturating_add(1),
        new_lines,
        lines,
    }]
}

fn path_candidate_from(path: &str, base: &Path) -> PathBuf {
    if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        base.join(path)
    }
}

fn normalize_path(path: &str) -> io::Result<PathBuf> {
    let base = std::env::current_dir()?;
    normalize_path_from(path, &base)
}

fn normalize_path_from(path: &str, base: &Path) -> io::Result<PathBuf> {
    let candidate = path_candidate_from(path, base);
    candidate.canonicalize()
}

fn normalize_path_allow_missing(path: &str) -> io::Result<PathBuf> {
    let base = std::env::current_dir()?;
    normalize_path_allow_missing_from(path, &base)
}

fn normalize_path_allow_missing_from(path: &str, base: &Path) -> io::Result<PathBuf> {
    let candidate = path_candidate_from(path, base);

    if let Ok(canonical) = candidate.canonicalize() {
        return Ok(canonical);
    }

    if let Some(parent) = candidate.parent() {
        let canonical_parent = parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf());
        if let Some(name) = candidate.file_name() {
            return Ok(canonical_parent.join(name));
        }
    }

    Ok(candidate)
}

fn normalize_missing_path_with_existing_parent_from(
    path: &str,
    base: &Path,
) -> io::Result<(PathBuf, PathBuf)> {
    let candidate = path_candidate_from(path, base);

    let mut current = candidate.as_path();
    let mut missing_components = Vec::<OsString>::new();
    let mut missing_contains_parent_dir = false;

    loop {
        if current.exists() {
            let canonical_existing = current.canonicalize()?;
            if missing_contains_parent_dir {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "path {} traverses through a missing parent directory",
                        candidate.display()
                    ),
                ));
            }

            let mut resolved = canonical_existing.clone();
            for component in missing_components.iter().rev() {
                resolved.push(component);
            }
            return Ok((resolved, canonical_existing));
        }

        match current.components().next_back() {
            Some(Component::Normal(component)) => {
                missing_components.push(component.to_os_string());
            }
            Some(Component::CurDir) => {}
            Some(Component::ParentDir) => {
                missing_contains_parent_dir = true;
            }
            Some(Component::RootDir | Component::Prefix(_)) | None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no existing parent found for {}", candidate.display()),
                ));
            }
        }

        current = current.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no existing parent found for {}", candidate.display()),
            )
        })?;
    }
}

fn glob_pattern_contains_parent_traversal(pattern: &str) -> bool {
    Path::new(pattern)
        .components()
        .any(|component| matches!(component, Component::ParentDir))
}

fn validate_glob_matches_in_workspace(
    output: GlobSearchOutput,
    workspace_root: &Path,
) -> io::Result<GlobSearchOutput> {
    for filename in &output.filenames {
        let resolved = Path::new(filename).canonicalize()?;
        validate_workspace_boundary(&resolved, workspace_root)?;
    }
    Ok(output)
}

/// Read a file with workspace boundary enforcement.
#[allow(dead_code)]
pub fn read_file_in_workspace(
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    workspace_root: &Path,
) -> io::Result<ReadFileOutput> {
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let absolute_path = normalize_path_from(path, &canonical_root)?;
    validate_workspace_boundary(&absolute_path, &canonical_root)?;
    read_file_at_path(&absolute_path, offset, limit)
}

/// Write a file with workspace boundary enforcement.
#[allow(dead_code)]
pub fn write_file_in_workspace(
    path: &str,
    content: &str,
    workspace_root: &Path,
) -> io::Result<WriteFileOutput> {
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let (absolute_path, existing_parent) =
        normalize_missing_path_with_existing_parent_from(path, &canonical_root)?;
    validate_workspace_boundary(&existing_parent, &canonical_root)?;
    validate_workspace_boundary(&absolute_path, &canonical_root)?;
    write_file_at_path(&absolute_path, content)
}

/// Edit a file with workspace boundary enforcement.
#[allow(dead_code)]
pub fn edit_file_in_workspace(
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    workspace_root: &Path,
) -> io::Result<EditFileOutput> {
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let absolute_path = normalize_path_from(path, &canonical_root)?;
    validate_workspace_boundary(&absolute_path, &canonical_root)?;
    edit_file_at_path(&absolute_path, old_string, new_string, replace_all)
}

/// Run a glob search with workspace boundary enforcement.
#[allow(dead_code)]
pub fn glob_search_in_workspace(
    pattern: &str,
    path: Option<&str>,
    workspace_root: &Path,
) -> io::Result<GlobSearchOutput> {
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let base_path = path
        .map(|path| normalize_path_from(path, &canonical_root))
        .transpose()?
        .unwrap_or_else(|| canonical_root.clone());
    validate_workspace_boundary(&base_path, &canonical_root)?;
    if glob_pattern_contains_parent_traversal(pattern) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("glob pattern {pattern} escapes workspace boundary"),
        ));
    }

    if Path::new(pattern).is_absolute() {
        let resolved_pattern = normalize_path_allow_missing_from(pattern, &canonical_root)?;
        validate_workspace_boundary(&resolved_pattern, &canonical_root)?;
        return validate_glob_matches_in_workspace(glob_search(pattern, None)?, &canonical_root);
    }

    validate_glob_matches_in_workspace(
        glob_search(pattern, Some(base_path.to_string_lossy().as_ref()))?,
        &canonical_root,
    )
}

/// Run a grep search with workspace boundary enforcement.
#[allow(dead_code)]
pub fn grep_search_in_workspace(
    input: &GrepSearchInput,
    workspace_root: &Path,
) -> io::Result<GrepSearchOutput> {
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let base_path = input
        .path
        .as_deref()
        .map(|path| normalize_path_from(path, &canonical_root))
        .transpose()?
        .unwrap_or_else(|| canonical_root.clone());
    validate_workspace_boundary(&base_path, &canonical_root)?;

    let mut scoped = input.clone();
    scoped.path = Some(base_path.to_string_lossy().into_owned());
    grep_search(&scoped)
}

/// Check whether a path is a symlink that resolves outside the workspace.
#[allow(dead_code)]
pub fn is_symlink_escape(path: &Path, workspace_root: &Path) -> io::Result<bool> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_symlink() {
        return Ok(false);
    }
    let resolved = path.canonicalize()?;
    let canonical_root = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    Ok(!resolved.starts_with(&canonical_root))
}

/// Expand shell-style brace groups in a glob pattern.
///
/// Handles one level of braces: `foo.{a,b,c}` → `["foo.a", "foo.b", "foo.c"]`.
/// Nested braces are not expanded (uncommon in practice).
/// Patterns without braces pass through unchanged.
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_owned()];
    };
    let Some(close) = pattern[open..].find('}').map(|i| open + i) else {
        // Unmatched brace — treat as literal.
        return vec![pattern.to_owned()];
    };
    let prefix = &pattern[..open];
    let suffix = &pattern[close + 1..];
    let alternatives = &pattern[open + 1..close];
    alternatives
        .split(',')
        .flat_map(|alt| expand_braces(&format!("{prefix}{alt}{suffix}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        edit_file, edit_file_in_workspace, expand_braces, glob_search, glob_search_in_workspace,
        grep_search, grep_search_in_workspace, is_symlink_escape, read_file,
        read_file_in_workspace, write_file, write_file_in_workspace, GrepSearchInput,
        MAX_WRITE_SIZE,
    };

    fn temp_path(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        std::env::temp_dir().join(format!("clawd-native-{name}-{unique}"))
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct CwdGuard {
        previous: std::path::PathBuf,
    }

    impl CwdGuard {
        fn set(path: &std::path::Path) -> Self {
            let previous = std::env::current_dir().expect("cwd should be readable");
            std::env::set_current_dir(path).expect("cwd should change");
            Self { previous }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.previous).expect("cwd should restore");
        }
    }

    #[test]
    fn reads_and_writes_files() {
        let path = temp_path("read-write.txt");
        let write_output = write_file(path.to_string_lossy().as_ref(), "one\ntwo\nthree")
            .expect("write should succeed");
        assert_eq!(write_output.kind, "create");

        let read_output = read_file(path.to_string_lossy().as_ref(), Some(1), Some(1))
            .expect("read should succeed");
        assert_eq!(read_output.file.content, "two");
    }

    #[test]
    fn edits_file_contents() {
        let path = temp_path("edit.txt");
        write_file(path.to_string_lossy().as_ref(), "alpha beta alpha")
            .expect("initial write should succeed");
        let output = edit_file(path.to_string_lossy().as_ref(), "alpha", "omega", true)
            .expect("edit should succeed");
        assert!(output.replace_all);
    }

    #[test]
    fn confirms_issue_13_edit_file_requires_unique_match_when_not_replace_all() {
        let path = temp_path("issue-13-duplicate-edit.txt");
        write_file(path.to_string_lossy().as_ref(), "alpha\nbeta\nalpha\n")
            .expect("initial file should write");

        let result = edit_file(path.to_string_lossy().as_ref(), "alpha", "omega", false);

        assert!(
            result.is_err(),
            "non-replace-all edit should reject ambiguous duplicate old_string matches"
        );
    }

    #[test]
    fn confirms_issue_14_structured_patch_is_localized() {
        let path = temp_path("issue-14-patch.txt");
        write_file(path.to_string_lossy().as_ref(), "one\ntwo\nthree\nfour\n")
            .expect("initial file should write");

        let output = edit_file(path.to_string_lossy().as_ref(), "two", "TWO", false)
            .expect("edit should execute");
        let changed_lines = output
            .structured_patch
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .filter(|line| line.starts_with('+') || line.starts_with('-'))
            .count();

        assert!(
            changed_lines <= 4,
            "single-line edits should not serialize the entire file as removed and re-added"
        );
    }

    #[test]
    fn confirms_issue_14_replace_all_emits_separated_localized_hunks() {
        let path = temp_path("issue-14-separated-patch.txt");
        let mut lines = (1..=40)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>();
        lines[4] = "target".to_string();
        lines[34] = "target".to_string();
        let original = format!("{}\n", lines.join("\n"));
        write_file(path.to_string_lossy().as_ref(), &original).expect("initial file should write");

        let output = edit_file(path.to_string_lossy().as_ref(), "target", "TARGET", true)
            .expect("replace_all edit should execute");
        let serialized_lines = output
            .structured_patch
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .collect::<Vec<_>>();

        assert!(
            output.structured_patch.len() >= 2,
            "separated edits should be emitted as multiple localized hunks"
        );
        assert!(
            serialized_lines.len() <= 16,
            "separated edits should not serialize the unchanged middle of the file"
        );
        assert!(
            !serialized_lines
                .iter()
                .any(|line| line.as_str() == "-line 20" || line.as_str() == "+line 20"),
            "unchanged middle lines should not be represented as removals/additions"
        );
    }

    #[test]
    fn confirms_issue_14_replace_all_multiline_insert_delete_uses_local_hunks() {
        let assert_localized_patch =
            |name: &str, old_string: &str, new_string: &str, expected_change_line: &str| {
                let path = temp_path(name);
                let mut lines = (1..=50)
                    .map(|line| format!("line {line}"))
                    .collect::<Vec<_>>();
                lines.splice(
                    4..4,
                    [
                        "target".to_string(),
                        "middle".to_string(),
                        "tail".to_string(),
                    ],
                );
                lines.splice(
                    34..34,
                    [
                        "target".to_string(),
                        "middle".to_string(),
                        "tail".to_string(),
                    ],
                );
                let original = format!("{}\n", lines.join("\n"));
                write_file(path.to_string_lossy().as_ref(), &original)
                    .expect("initial file should write");

                let output = edit_file(
                    path.to_string_lossy().as_ref(),
                    old_string,
                    new_string,
                    true,
                )
                .expect("replace_all edit should execute");
                let serialized_lines = output
                    .structured_patch
                    .iter()
                    .flat_map(|hunk| hunk.lines.iter())
                    .collect::<Vec<_>>();

                assert!(
                    output.structured_patch.len() >= 2,
                    "{name}: separated multiline edits should emit multiple localized hunks"
                );
                assert!(
                    !serialized_lines
                        .iter()
                        .any(|line| line.as_str() == "-line 25" || line.as_str() == "+line 25"),
                    "{name}: unchanged middle lines should not be removed or re-added"
                );
                assert!(
                    serialized_lines
                        .iter()
                        .any(|line| line.as_str() == expected_change_line),
                    "{name}: expected changed line should appear in the structured patch"
                );
            };

        assert_localized_patch(
            "issue-14-multiline-insert-patch.txt",
            "target\nmiddle",
            "target\ninserted\nmiddle",
            "+inserted",
        );
        assert_localized_patch(
            "issue-14-multiline-delete-patch.txt",
            "target\nmiddle\ntail",
            "target\ntail",
            "-middle",
        );
    }

    #[test]
    fn confirms_issue_15_multiline_grep_matches_across_lines_in_content_mode() {
        let dir = temp_path("issue-15-grep");
        std::fs::create_dir_all(&dir).expect("directory should create");
        let file = dir.join("sample.txt");
        write_file(file.to_string_lossy().as_ref(), "first\nsecond\nthird\n")
            .expect("file should write");

        let result = grep_search(&GrepSearchInput {
            pattern: "first\\nsecond".to_string(),
            path: Some(dir.to_string_lossy().into_owned()),
            glob: Some("**/*.txt".to_string()),
            output_mode: Some("content".to_string()),
            before: None,
            after: None,
            context_short: None,
            context: None,
            line_numbers: Some(true),
            case_insensitive: Some(false),
            file_type: None,
            head_limit: Some(10),
            offset: Some(0),
            multiline: Some(true),
        })
        .expect("grep should run");

        assert!(
            result.content.unwrap_or_default().contains("first"),
            "multiline content mode should return a match spanning newline characters"
        );
    }

    #[test]
    fn workspace_scoped_search_rejects_outside_absolute_paths() {
        let workspace = temp_path("workspace-search-root");
        let outside = temp_path("workspace-search-outside");
        std::fs::create_dir_all(&workspace).expect("workspace should create");
        std::fs::create_dir_all(&outside).expect("outside should create");
        let outside_file = outside.join("secret.txt");
        std::fs::write(&outside_file, "needle\n").expect("outside file should write");

        let glob_error = glob_search_in_workspace(
            "**/*.txt",
            Some(outside.to_string_lossy().as_ref()),
            &workspace,
        )
        .expect_err("outside glob base should be rejected");
        assert_eq!(glob_error.kind(), std::io::ErrorKind::PermissionDenied);

        let grep_error = grep_search_in_workspace(
            &GrepSearchInput {
                pattern: "needle".to_string(),
                path: Some(outside.to_string_lossy().into_owned()),
                glob: Some("**/*.txt".to_string()),
                output_mode: Some("content".to_string()),
                before: None,
                after: None,
                context_short: None,
                context: None,
                line_numbers: Some(true),
                case_insensitive: Some(false),
                file_type: None,
                head_limit: Some(10),
                offset: Some(0),
                multiline: Some(false),
            },
            &workspace,
        )
        .expect_err("outside grep base should be rejected");
        assert_eq!(grep_error.kind(), std::io::ErrorKind::PermissionDenied);

        let _ = std::fs::remove_dir_all(workspace);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn workspace_scoped_search_rejects_relative_parent_traversal_patterns() {
        let workspace = temp_path("workspace-search-relative-root");
        let outside_name = workspace
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| format!("{name}-outside"))
            .expect("workspace should have a file name");
        let outside = workspace
            .parent()
            .expect("workspace should have a parent")
            .join(&outside_name);
        std::fs::create_dir_all(&workspace).expect("workspace should create");
        std::fs::create_dir_all(&outside).expect("outside should create");
        std::fs::write(outside.join("secret.txt"), "needle\n").expect("outside file should write");

        let pattern = format!("../{outside_name}/**/*.txt");
        let error = glob_search_in_workspace(&pattern, None, &workspace)
            .expect_err("relative glob traversal should be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);

        let _ = std::fs::remove_dir_all(workspace);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn workspace_scoped_write_rejects_missing_parent_traversal() {
        let workspace = temp_path("workspace-write-root");
        let outside_name = workspace
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| format!("{name}-outside"))
            .expect("workspace should have a file name");
        let outside = workspace
            .parent()
            .expect("workspace should have a parent")
            .join(&outside_name);
        std::fs::create_dir_all(&workspace).expect("workspace should create");

        let traversal = workspace.join("..").join(&outside_name).join("new.txt");
        let error =
            write_file_in_workspace(traversal.to_string_lossy().as_ref(), "nope", &workspace)
                .expect_err("missing outside parent traversal should be rejected");

        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            !outside.join("new.txt").exists(),
            "rejected workspace write should not create the outside file"
        );

        let _ = std::fs::remove_dir_all(workspace);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn rejects_binary_files() {
        let path = temp_path("binary-test.bin");
        std::fs::write(&path, b"\x00\x01\x02\x03binary content").expect("write should succeed");
        let result = read_file(path.to_string_lossy().as_ref(), None, None);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("binary"));
    }

    #[test]
    fn rejects_oversized_writes() {
        let path = temp_path("oversize-write.txt");
        let huge = "x".repeat(MAX_WRITE_SIZE + 1);
        let result = write_file(path.to_string_lossy().as_ref(), &huge);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("too large"));
    }

    #[test]
    fn enforces_workspace_boundary() {
        let workspace = temp_path("workspace-boundary");
        std::fs::create_dir_all(&workspace).expect("workspace dir should be created");
        let inside = workspace.join("inside.txt");
        write_file(inside.to_string_lossy().as_ref(), "safe content")
            .expect("write inside workspace should succeed");

        // Reading inside workspace should succeed
        let result =
            read_file_in_workspace(inside.to_string_lossy().as_ref(), None, None, &workspace);
        assert!(result.is_ok());

        // Reading outside workspace should fail
        let outside = temp_path("outside-boundary.txt");
        write_file(outside.to_string_lossy().as_ref(), "unsafe content")
            .expect("write outside should succeed");
        let result =
            read_file_in_workspace(outside.to_string_lossy().as_ref(), None, None, &workspace);
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("escapes workspace"));
    }

    #[test]
    fn workspace_scoped_file_ops_resolve_relative_paths_from_workspace_root() {
        let _lock = env_lock();
        let workspace = temp_path("workspace-relative-root");
        let process_cwd = temp_path("workspace-relative-cwd");
        std::fs::create_dir_all(&workspace).expect("workspace dir should be created");
        std::fs::create_dir_all(&process_cwd).expect("cwd dir should be created");
        std::fs::write(workspace.join("target.txt"), "alpha\n").expect("workspace file");
        std::fs::write(process_cwd.join("target.txt"), "wrong\n").expect("cwd file");
        let _cwd = CwdGuard::set(&process_cwd);

        let read_output = read_file_in_workspace("target.txt", None, None, &workspace)
            .expect("relative read should resolve from workspace root");
        assert_eq!(read_output.file.content, "alpha");

        let write_output = write_file_in_workspace("created.txt", "created\n", &workspace)
            .expect("relative write should resolve from workspace root");
        assert_eq!(
            write_output.file_path,
            workspace.join("created.txt").display().to_string()
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("created.txt")).expect("workspace created file"),
            "created\n"
        );
        assert!(
            !process_cwd.join("created.txt").exists(),
            "relative workspace writes must not target process cwd"
        );

        let edit_output = edit_file_in_workspace("target.txt", "alpha", "beta", false, &workspace)
            .expect("relative edit should resolve from workspace root");
        assert_eq!(
            edit_output.file_path,
            workspace.join("target.txt").display().to_string()
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("target.txt")).expect("workspace edited file"),
            "beta\n"
        );
        assert_eq!(
            std::fs::read_to_string(process_cwd.join("target.txt")).expect("cwd file unchanged"),
            "wrong\n"
        );

        let _ = std::fs::remove_dir_all(workspace);
        let _ = std::fs::remove_dir_all(process_cwd);
    }

    #[test]
    fn detects_symlink_escape() {
        let workspace = temp_path("symlink-workspace");
        std::fs::create_dir_all(&workspace).expect("workspace dir should be created");
        let outside = temp_path("symlink-target.txt");
        std::fs::write(&outside, "target content").expect("target should write");

        let link_path = workspace.join("escape-link.txt");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, &link_path).expect("symlink should create");
            assert!(is_symlink_escape(&link_path, &workspace).expect("check should succeed"));
        }

        // Non-symlink file should not be an escape
        let normal = workspace.join("normal.txt");
        std::fs::write(&normal, "normal content").expect("normal file should write");
        assert!(!is_symlink_escape(&normal, &workspace).expect("check should succeed"));
    }

    #[test]
    fn globs_and_greps_directory() {
        let dir = temp_path("search-dir");
        std::fs::create_dir_all(&dir).expect("directory should be created");
        let file = dir.join("demo.rs");
        write_file(
            file.to_string_lossy().as_ref(),
            "fn main() {\n println!(\"hello\");\n}\n",
        )
        .expect("file write should succeed");

        let globbed = glob_search("**/*.rs", Some(dir.to_string_lossy().as_ref()))
            .expect("glob should succeed");
        assert_eq!(globbed.num_files, 1);

        let grep_output = grep_search(&GrepSearchInput {
            pattern: String::from("hello"),
            path: Some(dir.to_string_lossy().into_owned()),
            glob: Some(String::from("**/*.rs")),
            output_mode: Some(String::from("content")),
            before: None,
            after: None,
            context_short: None,
            context: None,
            line_numbers: Some(true),
            case_insensitive: Some(false),
            file_type: None,
            head_limit: Some(10),
            offset: Some(0),
            multiline: Some(false),
        })
        .expect("grep should succeed");
        assert!(grep_output.content.unwrap_or_default().contains("hello"));
    }

    #[test]
    fn expand_braces_no_braces() {
        assert_eq!(expand_braces("*.rs"), vec!["*.rs"]);
    }

    #[test]
    fn expand_braces_single_group() {
        let mut result = expand_braces("Assets/**/*.{cs,uxml,uss}");
        result.sort();
        assert_eq!(
            result,
            vec!["Assets/**/*.cs", "Assets/**/*.uss", "Assets/**/*.uxml",]
        );
    }

    #[test]
    fn expand_braces_nested() {
        let mut result = expand_braces("src/{a,b}.{rs,toml}");
        result.sort();
        assert_eq!(
            result,
            vec!["src/a.rs", "src/a.toml", "src/b.rs", "src/b.toml"]
        );
    }

    #[test]
    fn expand_braces_unmatched() {
        assert_eq!(expand_braces("foo.{bar"), vec!["foo.{bar"]);
    }

    #[test]
    fn glob_search_with_braces_finds_files() {
        let dir = temp_path("glob-braces");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("b.toml"), "[package]").unwrap();
        std::fs::write(dir.join("c.txt"), "hello").unwrap();

        let result =
            glob_search("*.{rs,toml}", Some(dir.to_str().unwrap())).expect("glob should succeed");
        assert_eq!(
            result.num_files, 2,
            "should match .rs and .toml but not .txt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
