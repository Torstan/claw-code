// File operations module
// Extracted from lib.rs for better modularity

use runtime::{edit_file, glob_search, grep_search, read_file, write_file, GrepSearchInput};
use serde::Deserialize;

// Input types
#[derive(Debug, Clone, Deserialize)]
pub struct ReadFileInput {
    pub path: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WriteFileInput {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditFileInput {
    pub path: String,
    pub old_string: String,
    pub new_string: String,
    pub replace_all: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GlobSearchInputValue {
    pub pattern: String,
    pub path: Option<String>,
}

// Helper functions (imported from parent)
use crate::{io_to_string, to_pretty_json};

// Tool execution functions
#[allow(clippy::needless_pass_by_value)]
pub fn run_read_file(input: ReadFileInput) -> Result<String, String> {
    let offset = input.offset.unwrap_or(1);
    let limit = input.limit.unwrap_or(500);
    to_pretty_json(read_file(&input.path, offset, limit).map_err(io_to_string)?)
}

#[allow(clippy::needless_pass_by_value)]
pub fn run_write_file(input: WriteFileInput) -> Result<String, String> {
    to_pretty_json(write_file(&input.path, &input.content).map_err(io_to_string)?)
}

#[allow(clippy::needless_pass_by_value)]
pub fn run_edit_file(input: EditFileInput) -> Result<String, String> {
    to_pretty_json(
        edit_file(
            &input.path,
            &input.old_string,
            &input.new_string,
            input.replace_all.unwrap_or(false),
        )
        .map_err(io_to_string)?,
    )
}

#[allow(clippy::needless_pass_by_value)]
pub fn run_glob_search(input: GlobSearchInputValue) -> Result<String, String> {
    to_pretty_json(glob_search(&input.pattern, input.path.as_deref()).map_err(io_to_string)?)
}

#[allow(clippy::needless_pass_by_value)]
pub fn run_grep_search(input: GrepSearchInput) -> Result<String, String> {
    to_pretty_json(grep_search(&input).map_err(io_to_string)?)
}
