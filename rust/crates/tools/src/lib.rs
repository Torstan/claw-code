use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use api::{
    build_system_blocks_with_cache_controls, detect_provider_kind,
    log_prompt_cache_block_diagnostics, max_tokens_for_model, read_base_url, resolve_model_alias,
    resolve_startup_auth_source, summarize_prompt_cache_controls, AnthropicClient, ApiError,
    AuthSource, CacheControl, ContentBlockDelta, InputContentBlock, InputMessage, MessageRequest,
    MessageResponse, OutputContentBlock, PromptCache, ProviderClient, ProviderKind,
    StreamEvent as ApiStreamEvent, ToolChoice, ToolDefinition, ToolResultContentBlock,
};
use plugins::PluginTool;
use runtime::{
    active_tool_session_id, check_freshness, dedupe_superseded_commit_events,
    enqueue_session_notification, execute_bash, glob_search, grep_search, load_system_prompt,
    permission_enforcer::{EnforcementResult, PermissionEnforcer},
    read_file,
    summary_compression::compress_summary_text,
    task_registry::TaskStatus,
    worker_boot::WorkerReadySnapshot,
    write_file, ApiClient, ApiRequest, AssistantEvent, BashCommandInput, BashCommandOutput,
    BranchFreshness, ConfigLoader, ContentBlock, ConversationMessage, ConversationRuntime,
    GrepSearchInput, LaneCommitProvenance, LaneEvent, LaneEventBlocker, LaneFailureClass,
    McpDegradedReport, MessageRole, OAuthConfig, PermissionMode, PermissionPolicy,
    PromptCacheEvent, ProviderFallbackConfig, RuntimeError, Session, TaskPacket, ToolError,
    ToolExecutor, SYSTEM_PROMPT_DYNAMIC_BOUNDARY,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod agent;
mod config;
mod dispatch;
mod notebook;
mod registries;
mod registry;
mod skill;
mod specs;
mod todo;
mod tool_search;
mod web;

pub mod lane_completion;
pub mod pdf_extract;

pub use dispatch::{enforce_permission_check, execute_tool, render_tool_result_for_model};
pub use registry::{
    GlobalToolRegistry, RuntimeToolDefinition, ToolManifestEntry, ToolRegistry, ToolSource,
    ToolSpec,
};
pub use specs::{is_background_task_tool_name, mvp_tool_specs};

#[cfg(test)]
pub(crate) use agent::{
    agent_permission_policy, allowed_tools_for_subagent, apply_tool_cache_controls,
    build_agent_system_prompt, classify_lane_failure, convert_messages, debug_json_input_summary,
    derive_agent_state, enqueue_background_agent_notification, execute_agent_with_mode,
    execute_agent_with_spawn, final_assistant_text, maybe_commit_provenance, new_agent_session,
    normalize_tool_input_string, persist_agent_terminal_state, push_output_block,
    should_log_tool_stop_without_start, tool_specs_for_allowed_tools, track_tool_block_index,
    ProviderRuntimeClient, SubagentToolExecutor,
};
pub(crate) use agent::{execute_agent, iso8601_now};
pub(crate) use config::{
    execute_brief, execute_config, execute_enter_plan_mode, execute_exit_plan_mode,
    execute_powershell, execute_repl, execute_sleep, execute_structured_output,
};
pub(crate) use dispatch::{execute_tool_with_enforcer, workspace_test_branch_preflight};
#[cfg(test)]
pub(crate) use dispatch::{run_task_output, run_task_packet};
pub(crate) use notebook::execute_notebook_edit;
pub(crate) use registries::{
    agent_debug_log, global_cron_registry, global_lsp_registry, global_mcp_registry,
    global_task_registry, global_team_registry, global_worker_registry,
};
#[cfg(test)]
pub(crate) use registry::permission_mode_from_plugin;
pub(crate) use skill::execute_skill;
pub(crate) use todo::execute_todo_write;
pub(crate) use tool_search::{
    canonical_tool_token, deferred_tool_specs, execute_tool_search, normalize_tool_search_query,
    search_tool_specs,
};
pub(crate) use web::{execute_web_fetch, execute_web_search};

#[derive(Debug, Deserialize)]
struct ReadFileInput {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct WriteFileInput {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct EditFileInput {
    path: String,
    old_string: String,
    new_string: String,
    replace_all: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct GlobSearchInputValue {
    pattern: String,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WebFetchInput {
    url: String,
    prompt: String,
}

#[derive(Debug, Deserialize)]
struct WebSearchInput {
    query: String,
    allowed_domains: Option<Vec<String>>,
    blocked_domains: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct TodoWriteInput {
    todos: Vec<TodoItem>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
struct TodoItem {
    content: String,
    #[serde(rename = "activeForm")]
    active_form: String,
    status: TodoStatus,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Deserialize)]
struct SkillInput {
    skill: String,
    args: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AgentInput {
    description: String,
    prompt: String,
    subagent_type: Option<String>,
    name: Option<String>,
    model: Option<String>,
    run_in_background: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ToolSearchInput {
    query: String,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct NotebookEditInput {
    notebook_path: String,
    cell_id: Option<String>,
    new_source: Option<String>,
    cell_type: Option<NotebookCellType>,
    edit_mode: Option<NotebookEditMode>,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum NotebookCellType {
    Code,
    Markdown,
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum NotebookEditMode {
    Replace,
    Insert,
    Delete,
}

#[derive(Debug, Deserialize)]
struct SleepInput {
    duration_ms: u64,
}

#[derive(Debug, Deserialize)]
struct BriefInput {
    message: String,
    attachments: Option<Vec<String>>,
    status: BriefStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BriefStatus {
    Normal,
    Proactive,
}

#[derive(Debug, Deserialize)]
struct ConfigInput {
    setting: String,
    value: Option<ConfigValue>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct EnterPlanModeInput {}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExitPlanModeInput {}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ConfigValue {
    String(String),
    Bool(bool),
    Number(f64),
}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
struct StructuredOutputInput(BTreeMap<String, Value>);

#[derive(Debug, Deserialize)]
struct ReplInput {
    code: String,
    language: String,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PowerShellInput {
    command: String,
    timeout: Option<u64>,
    description: Option<String>,
    run_in_background: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct AskUserQuestionInput {
    question: String,
    #[serde(default)]
    options: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct TaskCreateInput {
    prompt: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaskIdInput {
    task_id: String,
}

#[derive(Debug, Deserialize)]
struct TaskOutputInput {
    task_id: String,
    #[serde(default = "default_task_output_block")]
    block: bool,
    #[serde(default = "default_task_output_timeout_ms", alias = "timeout")]
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
struct TaskUpdateInput {
    task_id: String,
    message: String,
}

#[derive(Debug, Deserialize)]
struct WorkerCreateInput {
    cwd: String,
    #[serde(default)]
    trusted_roots: Vec<String>,
    #[serde(default = "default_auto_recover_prompt_misdelivery")]
    auto_recover_prompt_misdelivery: bool,
}

#[derive(Debug, Deserialize)]
struct WorkerIdInput {
    worker_id: String,
}

#[derive(Debug, Deserialize)]
struct WorkerObserveCompletionInput {
    worker_id: String,
    finish_reason: String,
    tokens_output: u64,
}

#[derive(Debug, Deserialize)]
struct WorkerObserveInput {
    worker_id: String,
    screen_text: String,
}

#[derive(Debug, Deserialize)]
struct WorkerSendPromptInput {
    worker_id: String,
    #[serde(default)]
    prompt: Option<String>,
}

const fn default_auto_recover_prompt_misdelivery() -> bool {
    true
}

const fn default_task_output_block() -> bool {
    true
}

const fn default_task_output_timeout_ms() -> u64 {
    30_000
}

#[derive(Debug, Deserialize)]
struct TeamCreateInput {
    name: String,
    tasks: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct TeamDeleteInput {
    team_id: String,
}

#[derive(Debug, Deserialize)]
struct CronCreateInput {
    schedule: String,
    prompt: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CronDeleteInput {
    cron_id: String,
}

#[derive(Debug, Deserialize)]
struct LspInput {
    action: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    character: Option<u32>,
    #[serde(default)]
    query: Option<String>,
}

#[derive(Debug, Deserialize)]
struct McpResourceInput {
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct McpAuthInput {
    server: String,
}

#[derive(Debug, Deserialize)]
struct RemoteTriggerInput {
    url: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: Option<Value>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct McpToolInput {
    server: String,
    tool: String,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct TestingPermissionInput {
    action: String,
}

#[derive(Debug, Serialize)]
struct WebFetchOutput {
    bytes: usize,
    code: u16,
    #[serde(rename = "codeText")]
    code_text: String,
    result: String,
    #[serde(rename = "durationMs")]
    duration_ms: u128,
    url: String,
}

#[derive(Debug, Serialize)]
struct WebSearchOutput {
    query: String,
    results: Vec<WebSearchResultItem>,
    #[serde(rename = "durationSeconds")]
    duration_seconds: f64,
}

#[derive(Debug, Serialize)]
struct TodoWriteOutput {
    #[serde(rename = "oldTodos")]
    old_todos: Vec<TodoItem>,
    #[serde(rename = "newTodos")]
    new_todos: Vec<TodoItem>,
    #[serde(rename = "verificationNudgeNeeded")]
    verification_nudge_needed: Option<bool>,
}

#[derive(Debug, Serialize)]
struct SkillOutput {
    skill: String,
    path: String,
    args: Option<String>,
    description: Option<String>,
    prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentOutput {
    #[serde(rename = "agentId")]
    agent_id: String,
    name: String,
    description: String,
    #[serde(rename = "subagentType")]
    subagent_type: Option<String>,
    model: Option<String>,
    status: String,
    #[serde(rename = "outputFile")]
    output_file: String,
    #[serde(rename = "manifestFile")]
    manifest_file: String,
    #[serde(rename = "createdAt")]
    created_at: String,
    #[serde(rename = "startedAt", skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(rename = "completedAt", skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    #[serde(rename = "laneEvents", default, skip_serializing_if = "Vec::is_empty")]
    lane_events: Vec<LaneEvent>,
    #[serde(rename = "currentBlocker", skip_serializing_if = "Option::is_none")]
    current_blocker: Option<LaneEventBlocker>,
    #[serde(rename = "derivedState")]
    derived_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct AsyncAgentLaunchOutput {
    status: &'static str,
    #[serde(rename = "agentId")]
    agent_id: String,
    description: String,
    prompt: String,
    #[serde(rename = "outputFile")]
    output_file: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
enum AgentToolOutput {
    Completed(AgentOutput),
    AsyncLaunched(AsyncAgentLaunchOutput),
}

#[derive(Debug, Clone, Serialize)]
struct TaskOutputPayload {
    task_id: String,
    status: String,
    prompt: String,
    description: Option<String>,
    created_at: u64,
    updated_at: u64,
    messages: Vec<runtime::task_registry::TaskMessage>,
    output: String,
    has_output: bool,
    team_id: Option<String>,
}

#[derive(Debug, Clone)]
struct AgentJob {
    manifest: AgentOutput,
    prompt: String,
    system_prompt: Vec<String>,
    allowed_tools: BTreeSet<String>,
    parent_session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ToolSearchOutput {
    matches: Vec<String>,
    query: String,
    normalized_query: String,
    #[serde(rename = "total_deferred_tools")]
    total_deferred_tools: usize,
    #[serde(rename = "pending_mcp_servers")]
    pending_mcp_servers: Option<Vec<String>>,
    #[serde(rename = "mcp_degraded", skip_serializing_if = "Option::is_none")]
    mcp_degraded: Option<McpDegradedReport>,
}

#[derive(Debug, Serialize)]
struct NotebookEditOutput {
    new_source: String,
    cell_id: Option<String>,
    cell_type: Option<NotebookCellType>,
    language: String,
    edit_mode: String,
    error: Option<String>,
    notebook_path: String,
    original_file: String,
    updated_file: String,
}

#[derive(Debug, Serialize)]
struct SleepOutput {
    duration_ms: u64,
    message: String,
}

#[derive(Debug, Serialize)]
struct BriefOutput {
    message: String,
    attachments: Option<Vec<ResolvedAttachment>>,
    #[serde(rename = "sentAt")]
    sent_at: String,
}

#[derive(Debug, Serialize)]
struct ResolvedAttachment {
    path: String,
    size: u64,
    #[serde(rename = "isImage")]
    is_image: bool,
}

#[derive(Debug, Serialize)]
struct ConfigOutput {
    success: bool,
    operation: Option<String>,
    setting: Option<String>,
    value: Option<Value>,
    #[serde(rename = "previousValue")]
    previous_value: Option<Value>,
    #[serde(rename = "newValue")]
    new_value: Option<Value>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlanModeState {
    #[serde(rename = "hadLocalOverride")]
    had_local_override: bool,
    #[serde(rename = "previousLocalMode")]
    previous_local_mode: Option<Value>,
}

#[derive(Debug, Serialize)]
#[allow(clippy::struct_excessive_bools)]
struct PlanModeOutput {
    success: bool,
    operation: String,
    changed: bool,
    active: bool,
    managed: bool,
    message: String,
    #[serde(rename = "settingsPath")]
    settings_path: String,
    #[serde(rename = "statePath")]
    state_path: String,
    #[serde(rename = "previousLocalMode")]
    previous_local_mode: Option<Value>,
    #[serde(rename = "currentLocalMode")]
    current_local_mode: Option<Value>,
}

#[derive(Debug, Clone)]
struct SearchableToolSpec {
    name: String,
    description: String,
}

#[derive(Debug, Serialize)]
struct StructuredOutputResult {
    data: String,
    structured_output: BTreeMap<String, Value>,
}

#[derive(Debug, Serialize)]
struct ReplOutput {
    language: String,
    stdout: String,
    stderr: String,
    #[serde(rename = "exitCode")]
    exit_code: i32,
    #[serde(rename = "durationMs")]
    duration_ms: u128,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum WebSearchResultItem {
    SearchResult {
        tool_use_id: String,
        content: Vec<SearchHit>,
    },
    Commentary(String),
}

#[derive(Debug, Serialize)]
struct SearchHit {
    title: String,
    url: String,
}

#[cfg(test)]
mod tests;
