// Web operations module (fetch and search)
// Extracted from lib.rs for better modularity

// Re-export types from parent module
pub use super::{WebFetchInput, WebSearchInput};

// Import helper functions from parent
use super::{to_pretty_json, execute_web_fetch, execute_web_search};

// Web fetch operation
#[allow(clippy::needless_pass_by_value)]
pub fn run_web_fetch(input: WebFetchInput) -> Result<String, String> {
    to_pretty_json(execute_web_fetch(&input)?)
}

// Web search operation
#[allow(clippy::needless_pass_by_value)]
pub fn run_web_search(input: WebSearchInput) -> Result<String, String> {
    to_pretty_json(execute_web_search(&input)?)
}
