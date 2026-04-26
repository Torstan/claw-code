// Simple tool operations module
// Extracted from lib.rs for better modularity

use runtime::{execute_todo_write, execute_skill, execute_agent};
use serde::{Deserialize, Serialize};

// Helper function
fn to_pretty_json<T: serde::Serialize>(value: T) -> Result<String, String> {
    serde_json::to_string_pretty(&value).map_err(|e| e.to_string())
}

// Input types
#[derive(Debug, Deserialize)]
pub struct TodoWriteInput {
    pub todos: Vec<TodoItem>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub content: String,
    #[serde(rename = "activeForm")]
    pub active_form: String,
    pub status: TodoStatus,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Deserialize)]
pub struct SkillInput {
    pub skill: String,
    pub args: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AgentInput {
    pub description: String,
    pub prompt: String,
    pub subagent_type: Option<String>,
    pub name: Option<String>,
    pub model: Option<String>,
    pub run_in_background: Option<bool>,
}

// Tool execution functions
pub fn run_todo_write(input: TodoWriteInput) -> Result<String, String> {
    to_pretty_json(execute_todo_write(input)?)
}

pub fn run_skill(input: SkillInput) -> Result<String, String> {
    to_pretty_json(execute_skill(input)?)
}

pub fn run_agent(input: AgentInput) -> Result<String, String> {
    to_pretty_json(execute_agent(input)?)
}
