use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use runtime::{
    agent_debug_enabled, agent_debug_log, SYSTEM_PROMPT_ATTACHMENT_BOUNDARY,
    SYSTEM_PROMPT_DYNAMIC_BOUNDARY,
};
use serde::{Deserialize, Serialize};

use crate::types::{
    CacheControl, InputContentBlock, InputMessage, MessageRequest, MessageResponse,
    SystemContentBlock, ToolResultContentBlock, Usage,
};

const DEFAULT_COMPLETION_TTL_SECS: u64 = 30;
const DEFAULT_PROMPT_TTL_SECS: u64 = 5 * 60;
const DEFAULT_BREAK_MIN_DROP: u32 = 2_000;
const MAX_SANITIZED_LENGTH: usize = 80;
const REQUEST_FINGERPRINT_VERSION: u32 = 1;
const REQUEST_FINGERPRINT_PREFIX: &str = "v1";
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const OPUS_4_6_MIN_CACHE_TOKENS: usize = 4096;
const ANTHROPIC_BREAKPOINT_LOOKBACK_BLOCKS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCacheControlSummary {
    pub enabled: bool,
    pub cache_control_count: usize,
    pub cache_control_types: Vec<String>,
    pub automatic_cache_control_count: usize,
    pub system_cache_control_count: usize,
    pub tool_cache_control_count: usize,
    pub message_cache_control_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromptCacheBlockDiagnostics {
    pub total_content_blocks: usize,
    pub cache_breakpoints: Vec<PromptCacheBreakpointDiagnostic>,
    pub tool_results: Vec<ToolResultSizeDiagnostic>,
    pub tool_result_model_visible_chars_total: usize,
    pub tool_result_model_visible_bytes_total: usize,
    pub max_tool_result_model_visible_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromptCacheBreakpointDiagnostic {
    pub block_index: usize,
    pub section: &'static str,
    pub block_type: &'static str,
    pub cache_control_source: &'static str,
    pub message_index: Option<usize>,
    pub content_index: Option<usize>,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub distance_from_previous_breakpoint: Option<usize>,
    pub prefix_serialized_chars_estimate: usize,
    pub estimated_prefix_tokens: usize,
    pub below_opus_4_6_min_cache_tokens: bool,
    pub exceeds_anthropic_20_block_lookback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolResultSizeDiagnostic {
    pub block_index: usize,
    pub message_index: usize,
    pub content_index: usize,
    pub tool_use_id: String,
    pub tool_name: Option<String>,
    pub model_visible_chars: usize,
    pub model_visible_bytes: usize,
    pub serialized_block_chars_estimate: usize,
    pub is_error: bool,
    pub has_cache_control: bool,
    pub in_cached_prefix: bool,
}

#[derive(Debug, Clone)]
pub struct PromptCacheConfig {
    pub session_id: String,
    pub completion_ttl: Duration,
    pub prompt_ttl: Duration,
    pub cache_break_min_drop: u32,
}

#[must_use]
pub fn summarize_prompt_cache_controls(request: &MessageRequest) -> PromptCacheControlSummary {
    let mut cache_control_types = BTreeSet::new();

    let mut record_cache_control = |cache_control: &Option<CacheControl>| -> usize {
        let Some(cache_control) = cache_control else {
            return 0;
        };
        cache_control_types.insert(cache_control.kind.clone());
        1
    };

    let automatic_cache_control_count = record_cache_control(&request.cache_control);
    let system_cache_control_count = request.system.as_ref().map_or(0, |blocks| {
        blocks
            .iter()
            .map(|block| record_cache_control(&block.cache_control))
            .sum()
    });
    let tool_cache_control_count = request.tools.as_ref().map_or(0, |tools| {
        tools
            .iter()
            .map(|tool| record_cache_control(&tool.cache_control))
            .sum()
    });
    let message_cache_control_count = request
        .messages
        .iter()
        .map(|message| {
            message
                .content
                .iter()
                .map(|block| match block {
                    InputContentBlock::Text { cache_control, .. }
                    | InputContentBlock::ToolResult { cache_control, .. } => {
                        record_cache_control(cache_control)
                    }
                    InputContentBlock::ToolUse { .. } => 0,
                })
                .sum::<usize>()
        })
        .sum::<usize>();

    let cache_control_count = automatic_cache_control_count
        + system_cache_control_count
        + tool_cache_control_count
        + message_cache_control_count;
    PromptCacheControlSummary {
        enabled: cache_control_count > 0,
        cache_control_count,
        cache_control_types: cache_control_types.into_iter().collect(),
        automatic_cache_control_count,
        system_cache_control_count,
        tool_cache_control_count,
        message_cache_control_count,
    }
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn prompt_cache_block_diagnostics(request: &MessageRequest) -> PromptCacheBlockDiagnostics {
    let mut scanner = PromptCacheBlockScanner::default();
    let mut tool_name_by_id: BTreeMap<String, String> = BTreeMap::new();
    let mut automatic_breakpoint: Option<PromptCacheBreakpointDiagnostic> = None;

    if let Some(tools) = &request.tools {
        for tool in tools {
            let block_index = scanner.next_block_index();
            let block_chars = serialized_len(tool);
            scanner.add_prefix_chars(block_chars);
            automatic_breakpoint = Some(PromptCacheBreakpointDiagnostic {
                block_index,
                section: "tools",
                block_type: "tool",
                cache_control_source: "automatic",
                message_index: None,
                content_index: None,
                tool_name: Some(tool.name.clone()),
                tool_use_id: None,
                distance_from_previous_breakpoint: None,
                prefix_serialized_chars_estimate: 0,
                estimated_prefix_tokens: 0,
                below_opus_4_6_min_cache_tokens: false,
                exceeds_anthropic_20_block_lookback: false,
            });
            if tool.cache_control.is_some() {
                scanner.push_breakpoint(PromptCacheBreakpointDiagnostic {
                    block_index,
                    section: "tools",
                    block_type: "tool",
                    cache_control_source: "block",
                    message_index: None,
                    content_index: None,
                    tool_name: Some(tool.name.clone()),
                    tool_use_id: None,
                    distance_from_previous_breakpoint: None,
                    prefix_serialized_chars_estimate: 0,
                    estimated_prefix_tokens: 0,
                    below_opus_4_6_min_cache_tokens: false,
                    exceeds_anthropic_20_block_lookback: false,
                });
            }
        }
    }

    if let Some(system_blocks) = &request.system {
        for block in system_blocks {
            let block_index = scanner.next_block_index();
            let block_chars = serialized_len(block);
            scanner.add_prefix_chars(block_chars);
            automatic_breakpoint = Some(PromptCacheBreakpointDiagnostic {
                block_index,
                section: "system",
                block_type: "text",
                cache_control_source: "automatic",
                message_index: None,
                content_index: None,
                tool_name: None,
                tool_use_id: None,
                distance_from_previous_breakpoint: None,
                prefix_serialized_chars_estimate: 0,
                estimated_prefix_tokens: 0,
                below_opus_4_6_min_cache_tokens: false,
                exceeds_anthropic_20_block_lookback: false,
            });
            if block.cache_control.is_some() {
                scanner.push_breakpoint(PromptCacheBreakpointDiagnostic {
                    block_index,
                    section: "system",
                    block_type: "text",
                    cache_control_source: "block",
                    message_index: None,
                    content_index: None,
                    tool_name: None,
                    tool_use_id: None,
                    distance_from_previous_breakpoint: None,
                    prefix_serialized_chars_estimate: 0,
                    estimated_prefix_tokens: 0,
                    below_opus_4_6_min_cache_tokens: false,
                    exceeds_anthropic_20_block_lookback: false,
                });
            }
        }
    }

    for (message_index, message) in request.messages.iter().enumerate() {
        for (content_index, block) in message.content.iter().enumerate() {
            let block_index = scanner.next_block_index();
            let block_chars = serialized_len(block);
            scanner.add_prefix_chars(block_chars);
            match block {
                InputContentBlock::Text { cache_control, .. } => {
                    automatic_breakpoint = Some(PromptCacheBreakpointDiagnostic {
                        block_index,
                        section: "messages",
                        block_type: "text",
                        cache_control_source: "automatic",
                        message_index: Some(message_index),
                        content_index: Some(content_index),
                        tool_name: None,
                        tool_use_id: None,
                        distance_from_previous_breakpoint: None,
                        prefix_serialized_chars_estimate: 0,
                        estimated_prefix_tokens: 0,
                        below_opus_4_6_min_cache_tokens: false,
                        exceeds_anthropic_20_block_lookback: false,
                    });
                    if cache_control.is_some() {
                        scanner.push_breakpoint(PromptCacheBreakpointDiagnostic {
                            block_index,
                            section: "messages",
                            block_type: "text",
                            cache_control_source: "block",
                            message_index: Some(message_index),
                            content_index: Some(content_index),
                            tool_name: None,
                            tool_use_id: None,
                            distance_from_previous_breakpoint: None,
                            prefix_serialized_chars_estimate: 0,
                            estimated_prefix_tokens: 0,
                            below_opus_4_6_min_cache_tokens: false,
                            exceeds_anthropic_20_block_lookback: false,
                        });
                    }
                }
                InputContentBlock::ToolUse { id, name, .. } => {
                    automatic_breakpoint = Some(PromptCacheBreakpointDiagnostic {
                        block_index,
                        section: "messages",
                        block_type: "tool_use",
                        cache_control_source: "automatic",
                        message_index: Some(message_index),
                        content_index: Some(content_index),
                        tool_name: Some(name.clone()),
                        tool_use_id: Some(id.clone()),
                        distance_from_previous_breakpoint: None,
                        prefix_serialized_chars_estimate: 0,
                        estimated_prefix_tokens: 0,
                        below_opus_4_6_min_cache_tokens: false,
                        exceeds_anthropic_20_block_lookback: false,
                    });
                    tool_name_by_id.insert(id.clone(), name.clone());
                }
                InputContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                    cache_control,
                } => {
                    let (model_visible_chars, model_visible_bytes) =
                        tool_result_model_visible_size(content);
                    let tool_name = tool_name_by_id.get(tool_use_id).cloned();
                    automatic_breakpoint = Some(PromptCacheBreakpointDiagnostic {
                        block_index,
                        section: "messages",
                        block_type: "tool_result",
                        cache_control_source: "automatic",
                        message_index: Some(message_index),
                        content_index: Some(content_index),
                        tool_name: tool_name.clone(),
                        tool_use_id: Some(tool_use_id.clone()),
                        distance_from_previous_breakpoint: None,
                        prefix_serialized_chars_estimate: 0,
                        estimated_prefix_tokens: 0,
                        below_opus_4_6_min_cache_tokens: false,
                        exceeds_anthropic_20_block_lookback: false,
                    });
                    scanner.tool_results.push(ToolResultSizeDiagnostic {
                        block_index,
                        message_index,
                        content_index,
                        tool_use_id: tool_use_id.clone(),
                        tool_name: tool_name.clone(),
                        model_visible_chars,
                        model_visible_bytes,
                        serialized_block_chars_estimate: block_chars,
                        is_error: *is_error,
                        has_cache_control: cache_control.is_some(),
                        in_cached_prefix: false,
                    });
                    if cache_control.is_some() {
                        scanner.push_breakpoint(PromptCacheBreakpointDiagnostic {
                            block_index,
                            section: "messages",
                            block_type: "tool_result",
                            cache_control_source: "block",
                            message_index: Some(message_index),
                            content_index: Some(content_index),
                            tool_name,
                            tool_use_id: Some(tool_use_id.clone()),
                            distance_from_previous_breakpoint: None,
                            prefix_serialized_chars_estimate: 0,
                            estimated_prefix_tokens: 0,
                            below_opus_4_6_min_cache_tokens: false,
                            exceeds_anthropic_20_block_lookback: false,
                        });
                    }
                }
            }
        }
    }

    if request.cache_control.is_some() {
        if let Some(breakpoint) = automatic_breakpoint {
            scanner.push_breakpoint(breakpoint);
        }
    }

    scanner.finish()
}

#[derive(Default)]
struct PromptCacheBlockScanner {
    total_content_blocks: usize,
    prefix_serialized_chars_estimate: usize,
    last_breakpoint_index: Option<usize>,
    cache_breakpoints: Vec<PromptCacheBreakpointDiagnostic>,
    tool_results: Vec<ToolResultSizeDiagnostic>,
}

impl PromptCacheBlockScanner {
    fn next_block_index(&mut self) -> usize {
        let block_index = self.total_content_blocks;
        self.total_content_blocks += 1;
        block_index
    }

    fn add_prefix_chars(&mut self, chars: usize) {
        self.prefix_serialized_chars_estimate += chars;
    }

    fn push_breakpoint(&mut self, mut breakpoint: PromptCacheBreakpointDiagnostic) {
        let estimated_prefix_tokens =
            estimate_prompt_cache_tokens(self.prefix_serialized_chars_estimate);
        let distance_from_previous_breakpoint = self
            .last_breakpoint_index
            .map(|idx| breakpoint.block_index - idx);
        breakpoint.exceeds_anthropic_20_block_lookback = distance_from_previous_breakpoint
            .is_some_and(|distance| distance > ANTHROPIC_BREAKPOINT_LOOKBACK_BLOCKS);
        breakpoint.distance_from_previous_breakpoint = distance_from_previous_breakpoint;
        breakpoint.prefix_serialized_chars_estimate = self.prefix_serialized_chars_estimate;
        breakpoint.estimated_prefix_tokens = estimated_prefix_tokens;
        breakpoint.below_opus_4_6_min_cache_tokens =
            estimated_prefix_tokens < OPUS_4_6_MIN_CACHE_TOKENS;
        self.last_breakpoint_index = Some(breakpoint.block_index);
        self.cache_breakpoints.push(breakpoint);
    }

    fn finish(mut self) -> PromptCacheBlockDiagnostics {
        if let Some(last_breakpoint_index) = self.last_breakpoint_index {
            for tool_result in &mut self.tool_results {
                tool_result.in_cached_prefix = tool_result.block_index <= last_breakpoint_index;
            }
        }
        let tool_result_model_visible_chars_total = self
            .tool_results
            .iter()
            .map(|result| result.model_visible_chars)
            .sum();
        let tool_result_model_visible_bytes_total = self
            .tool_results
            .iter()
            .map(|result| result.model_visible_bytes)
            .sum();
        let max_tool_result_model_visible_chars = self
            .tool_results
            .iter()
            .map(|result| result.model_visible_chars)
            .max()
            .unwrap_or(0);
        PromptCacheBlockDiagnostics {
            total_content_blocks: self.total_content_blocks,
            cache_breakpoints: self.cache_breakpoints,
            tool_results: self.tool_results,
            tool_result_model_visible_chars_total,
            tool_result_model_visible_bytes_total,
            max_tool_result_model_visible_chars,
        }
    }
}

fn estimate_prompt_cache_tokens(chars: usize) -> usize {
    chars.div_ceil(4)
}

fn serialized_len<T: Serialize>(value: &T) -> usize {
    serde_json::to_string(value).map_or(0, |serialized| serialized.len())
}

fn tool_result_model_visible_size(content: &[ToolResultContentBlock]) -> (usize, usize) {
    content.iter().fold((0, 0), |(chars, bytes), block| {
        let rendered = match block {
            ToolResultContentBlock::Text { text } => text.as_str().to_string(),
            ToolResultContentBlock::Json { value } => {
                serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
            }
        };
        (chars + rendered.chars().count(), bytes + rendered.len())
    })
}

pub fn log_prompt_cache_block_diagnostics(
    log_prefix: &str,
    session_id: &str,
    model: &str,
    request: &MessageRequest,
) {
    if !agent_debug_enabled() {
        return;
    }
    let diagnostics = prompt_cache_block_diagnostics(request);
    let cache_breakpoints_json =
        serde_json::to_string(&diagnostics.cache_breakpoints).unwrap_or_else(|_| "[]".to_string());
    agent_debug_log(
        &format!("{log_prefix}.provider.stream.prompt_cache_blocks"),
        format!(
            "session_id={} model={} total_content_blocks={} cache_breakpoints={} tool_result_count={} tool_result_model_visible_chars_total={} tool_result_model_visible_bytes_total={} max_tool_result_model_visible_chars={}",
            session_id,
            model,
            diagnostics.total_content_blocks,
            cache_breakpoints_json,
            diagnostics.tool_results.len(),
            diagnostics.tool_result_model_visible_chars_total,
            diagnostics.tool_result_model_visible_bytes_total,
            diagnostics.max_tool_result_model_visible_chars
        ),
    );

    if diagnostics.tool_results.is_empty() {
        return;
    }
    let tool_results_json =
        serde_json::to_string(&diagnostics.tool_results).unwrap_or_else(|_| "[]".to_string());
    agent_debug_log(
        &format!("{log_prefix}.provider.stream.tool_result_sizes"),
        format!("session_id={session_id} model={model} tool_results={tool_results_json}"),
    );
}

#[must_use]
pub fn build_system_blocks_with_cache_controls(
    parts: &[String],
) -> Option<Vec<SystemContentBlock>> {
    if parts.is_empty() {
        return None;
    }

    let boundary_pos = parts
        .iter()
        .position(|part| part == SYSTEM_PROMPT_DYNAMIC_BOUNDARY);
    let (stable_parts, dynamic_parts) = if let Some(boundary) = boundary_pos {
        (
            system_prompt_parts(&parts[..boundary]),
            system_prompt_parts(&parts[boundary + 1..]),
        )
    } else {
        (system_prompt_parts(parts), Vec::new())
    };

    let mut blocks = Vec::new();
    push_cacheable_system_blocks(&mut blocks, &stable_parts);

    let dynamic = dynamic_parts.join("\n\n");
    if !dynamic.is_empty() {
        blocks.push(SystemContentBlock::text(dynamic));
    }

    (!blocks.is_empty()).then_some(blocks)
}

fn system_prompt_parts(parts: &[String]) -> Vec<String> {
    parts
        .iter()
        .filter_map(|part| system_prompt_part_text(part))
        .collect()
}

fn system_prompt_part_text(part: &str) -> Option<String> {
    if part.is_empty()
        || part == SYSTEM_PROMPT_DYNAMIC_BOUNDARY
        || part == SYSTEM_PROMPT_ATTACHMENT_BOUNDARY
    {
        return None;
    }
    Some(part.to_string())
}

fn push_cacheable_system_blocks(blocks: &mut Vec<SystemContentBlock>, stable_parts: &[String]) {
    let Some(first) = stable_parts.first() else {
        return;
    };

    blocks.push(
        SystemContentBlock::text(first.clone()).with_cache_control(CacheControl::ephemeral()),
    );

    let rest = stable_parts
        .iter()
        .skip(1)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n");
    if !rest.is_empty() {
        blocks.push(SystemContentBlock::text(rest).with_cache_control(CacheControl::ephemeral()));
    }
}

pub fn apply_message_cache_controls(messages: &mut [InputMessage]) {
    if let Some(idx) = messages.iter().rposition(is_cacheable_user_message) {
        set_user_cache_control(&mut messages[idx]);
    }
}

fn is_cacheable_user_message(message: &InputMessage) -> bool {
    message.role == "user"
        && message
            .content
            .last()
            .is_some_and(is_cacheable_user_content_block)
}

fn is_cacheable_user_content_block(block: &InputContentBlock) -> bool {
    matches!(
        block,
        InputContentBlock::Text { .. } | InputContentBlock::ToolResult { .. }
    )
}

fn set_user_cache_control(message: &mut InputMessage) {
    let Some(last_block) = message.content.last_mut() else {
        return;
    };
    match last_block {
        InputContentBlock::Text { cache_control, .. }
        | InputContentBlock::ToolResult { cache_control, .. } => {
            *cache_control = Some(CacheControl::ephemeral());
        }
        InputContentBlock::ToolUse { .. } => {}
    }
}

impl PromptCacheConfig {
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            completion_ttl: Duration::from_secs(DEFAULT_COMPLETION_TTL_SECS),
            prompt_ttl: Duration::from_secs(DEFAULT_PROMPT_TTL_SECS),
            cache_break_min_drop: DEFAULT_BREAK_MIN_DROP,
        }
    }
}

impl Default for PromptCacheConfig {
    fn default() -> Self {
        Self::new("default")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCachePaths {
    pub root: PathBuf,
    pub session_dir: PathBuf,
    pub completion_dir: PathBuf,
    pub session_state_path: PathBuf,
    pub stats_path: PathBuf,
}

impl PromptCachePaths {
    #[must_use]
    pub fn for_session(session_id: &str) -> Self {
        let root = base_cache_root();
        let session_dir = root.join(sanitize_path_segment(session_id));
        let completion_dir = session_dir.join("completions");
        Self {
            root,
            session_state_path: session_dir.join("session-state.json"),
            stats_path: session_dir.join("stats.json"),
            session_dir,
            completion_dir,
        }
    }

    #[must_use]
    pub fn completion_entry_path(&self, request_hash: &str) -> PathBuf {
        self.completion_dir.join(format!("{request_hash}.json"))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheStats {
    pub tracked_requests: u64,
    pub completion_cache_hits: u64,
    pub completion_cache_misses: u64,
    pub completion_cache_writes: u64,
    pub expected_invalidations: u64,
    pub unexpected_cache_breaks: u64,
    pub total_cache_creation_input_tokens: u64,
    pub total_cache_read_input_tokens: u64,
    pub last_cache_creation_input_tokens: Option<u32>,
    pub last_cache_read_input_tokens: Option<u32>,
    pub last_request_hash: Option<String>,
    pub last_completion_cache_key: Option<String>,
    pub last_break_reason: Option<String>,
    pub last_cache_source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheBreakEvent {
    pub unexpected: bool,
    pub reason: String,
    pub reason_code: String,
    pub diagnostic_scope: String,
    #[serde(default)]
    pub changed_components: Vec<String>,
    pub previous_cache_read_input_tokens: u32,
    pub current_cache_read_input_tokens: u32,
    pub token_drop: u32,
    pub elapsed_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCacheRecord {
    pub cache_break: Option<CacheBreakEvent>,
    pub stats: PromptCacheStats,
}

#[derive(Debug, Clone)]
pub struct PromptCache {
    inner: Arc<Mutex<PromptCacheInner>>,
}

impl PromptCache {
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self::with_config(PromptCacheConfig::new(session_id))
    }

    #[must_use]
    pub fn with_config(config: PromptCacheConfig) -> Self {
        let paths = PromptCachePaths::for_session(&config.session_id);
        let stats = read_json::<PromptCacheStats>(&paths.stats_path).unwrap_or_default();
        let previous = read_json::<TrackedPromptState>(&paths.session_state_path);
        Self {
            inner: Arc::new(Mutex::new(PromptCacheInner {
                config,
                paths,
                stats,
                previous,
            })),
        }
    }

    #[must_use]
    pub fn paths(&self) -> PromptCachePaths {
        self.lock().paths.clone()
    }

    #[must_use]
    pub fn stats(&self) -> PromptCacheStats {
        self.lock().stats.clone()
    }

    #[must_use]
    pub fn lookup_completion(&self, request: &MessageRequest) -> Option<MessageResponse> {
        let request_hash = request_hash_hex(request);
        let (paths, ttl) = {
            let inner = self.lock();
            (inner.paths.clone(), inner.config.completion_ttl)
        };
        let entry_path = paths.completion_entry_path(&request_hash);
        let entry = read_json::<CompletionCacheEntry>(&entry_path);
        let Some(entry) = entry else {
            let mut inner = self.lock();
            inner.stats.completion_cache_misses += 1;
            inner.stats.last_completion_cache_key = Some(request_hash);
            persist_state(&inner);
            return None;
        };

        if entry.fingerprint_version != current_fingerprint_version() {
            let mut inner = self.lock();
            inner.stats.completion_cache_misses += 1;
            inner.stats.last_completion_cache_key = Some(request_hash.clone());
            let _ = fs::remove_file(entry_path);
            persist_state(&inner);
            return None;
        }

        let expired = now_unix_secs().saturating_sub(entry.cached_at_unix_secs) >= ttl.as_secs();
        let mut inner = self.lock();
        inner.stats.last_completion_cache_key = Some(request_hash.clone());
        if expired {
            inner.stats.completion_cache_misses += 1;
            let _ = fs::remove_file(entry_path);
            persist_state(&inner);
            return None;
        }

        inner.stats.completion_cache_hits += 1;
        apply_usage_to_stats(
            &mut inner.stats,
            &entry.response.usage,
            &request_hash,
            "completion-cache",
        );
        inner.previous = Some(TrackedPromptState::from_usage(
            request,
            &entry.response.usage,
        ));
        persist_state(&inner);
        Some(entry.response)
    }

    #[must_use]
    pub fn record_response(
        &self,
        request: &MessageRequest,
        response: &MessageResponse,
    ) -> PromptCacheRecord {
        self.record_usage_internal(request, &response.usage, Some(response))
    }

    #[must_use]
    pub fn record_usage(&self, request: &MessageRequest, usage: &Usage) -> PromptCacheRecord {
        self.record_usage_internal(request, usage, None)
    }

    fn record_usage_internal(
        &self,
        request: &MessageRequest,
        usage: &Usage,
        response: Option<&MessageResponse>,
    ) -> PromptCacheRecord {
        let request_hash = request_hash_hex(request);
        let mut inner = self.lock();
        let previous = inner.previous.clone();
        let current = TrackedPromptState::from_usage(request, usage);
        let cache_break = detect_cache_break(&inner.config, previous.as_ref(), &current);

        inner.stats.tracked_requests += 1;
        apply_usage_to_stats(&mut inner.stats, usage, &request_hash, "api-response");
        if let Some(event) = &cache_break {
            if event.unexpected {
                inner.stats.unexpected_cache_breaks += 1;
            } else {
                inner.stats.expected_invalidations += 1;
            }
            inner.stats.last_break_reason = Some(event.reason.clone());
            log_cache_break(
                &inner.config.session_id,
                &request_hash,
                event,
                previous.as_ref(),
                &current,
            );
        }

        inner.previous = Some(current);
        if let Some(response) = response {
            write_completion_entry(&inner.paths, &request_hash, response);
            inner.stats.completion_cache_writes += 1;
        }
        persist_state(&inner);

        PromptCacheRecord {
            cache_break,
            stats: inner.stats.clone(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PromptCacheInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Debug)]
struct PromptCacheInner {
    config: PromptCacheConfig,
    paths: PromptCachePaths,
    stats: PromptCacheStats,
    previous: Option<TrackedPromptState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompletionCacheEntry {
    cached_at_unix_secs: u64,
    #[serde(default = "current_fingerprint_version")]
    fingerprint_version: u32,
    response: MessageResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TrackedPromptState {
    observed_at_unix_secs: u64,
    #[serde(default = "current_fingerprint_version")]
    fingerprint_version: u32,
    #[serde(default)]
    model_name: String,
    model_hash: u64,
    #[serde(default)]
    system_block_count: usize,
    system_hash: u64,
    #[serde(default)]
    tool_count: usize,
    tools_hash: u64,
    #[serde(default)]
    message_count: usize,
    #[serde(default)]
    last_message_role: Option<String>,
    #[serde(default)]
    last_message_content_types: Vec<String>,
    messages_hash: u64,
    cache_read_input_tokens: u32,
}

impl TrackedPromptState {
    fn from_usage(request: &MessageRequest, usage: &Usage) -> Self {
        let hashes = RequestFingerprints::from_request(request);
        let (last_message_role, last_message_content_types) =
            summarize_last_message(&request.messages);
        Self {
            observed_at_unix_secs: now_unix_secs(),
            fingerprint_version: current_fingerprint_version(),
            model_name: request.model.clone(),
            model_hash: hashes.model,
            system_block_count: request.system.as_ref().map_or(0, Vec::len),
            system_hash: hashes.system,
            tool_count: request.tools.as_ref().map_or(0, Vec::len),
            tools_hash: hashes.tools,
            message_count: request.messages.len(),
            last_message_role,
            last_message_content_types,
            messages_hash: hashes.messages,
            cache_read_input_tokens: usage.cache_read_input_tokens,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RequestFingerprints {
    model: u64,
    system: u64,
    tools: u64,
    messages: u64,
}

impl RequestFingerprints {
    fn from_request(request: &MessageRequest) -> Self {
        Self {
            model: hash_serializable(&request.model),
            system: hash_serializable(&request.system),
            tools: hash_serializable(&request.tools),
            messages: hash_serializable(&request.messages),
        }
    }
}

fn detect_cache_break(
    config: &PromptCacheConfig,
    previous: Option<&TrackedPromptState>,
    current: &TrackedPromptState,
) -> Option<CacheBreakEvent> {
    let previous = previous?;
    if previous.fingerprint_version != current.fingerprint_version {
        let elapsed = current
            .observed_at_unix_secs
            .saturating_sub(previous.observed_at_unix_secs);
        return Some(CacheBreakEvent {
            unexpected: false,
            reason: format!(
                "local prompt-cache fingerprint version changed (v{} -> v{}); historical cache comparisons are no longer comparable. This is not an Anthropic server-provided miss reason.",
                previous.fingerprint_version, current.fingerprint_version
            ),
            reason_code: "fingerprint_version_changed".to_string(),
            diagnostic_scope: "local_fingerprint_version".to_string(),
            changed_components: Vec::new(),
            previous_cache_read_input_tokens: previous.cache_read_input_tokens,
            current_cache_read_input_tokens: current.cache_read_input_tokens,
            token_drop: previous
                .cache_read_input_tokens
                .saturating_sub(current.cache_read_input_tokens),
            elapsed_seconds: elapsed,
        });
    }
    let token_drop = previous
        .cache_read_input_tokens
        .saturating_sub(current.cache_read_input_tokens);
    if token_drop < config.cache_break_min_drop {
        return None;
    }

    let mut changed_components = Vec::new();
    if previous.model_hash != current.model_hash {
        changed_components.push("model".to_string());
    }
    if previous.system_hash != current.system_hash {
        changed_components.push("system".to_string());
    }
    if previous.tools_hash != current.tools_hash {
        changed_components.push("tools".to_string());
    }
    if previous.messages_hash != current.messages_hash {
        changed_components.push("messages".to_string());
    }

    let elapsed = current
        .observed_at_unix_secs
        .saturating_sub(previous.observed_at_unix_secs);

    let (unexpected, reason_code, diagnostic_scope, reason) = if changed_components.is_empty() {
        if elapsed > config.prompt_ttl.as_secs() {
            (
                false,
                "possible_prompt_ttl_expiry".to_string(),
                "local_ttl_heuristic".to_string(),
                format!(
                    "cache_read_input_tokens dropped ({} -> {}) after {elapsed}s while the local request fingerprint stayed stable; possible prompt cache TTL expiry. This is a local heuristic, not an Anthropic server-provided miss reason.",
                    previous.cache_read_input_tokens, current.cache_read_input_tokens
                ),
            )
        } else {
            (
                true,
                "stable_fingerprint_cache_drop".to_string(),
                "local_request_fingerprint".to_string(),
                format!(
                    "cache_read_input_tokens dropped ({} -> {}) while the local request fingerprint stayed stable for {elapsed}s. The client did not observe any model/system/tools/messages change; possible relay or upstream cache-domain drift, server-side invalidation, or another non-payload cache break.",
                    previous.cache_read_input_tokens, current.cache_read_input_tokens
                ),
            )
        }
    } else {
        (
            false,
            "local_request_components_changed".to_string(),
            "local_request_fingerprint".to_string(),
            format_request_change_reason(&changed_components, previous, current),
        )
    };

    Some(CacheBreakEvent {
        unexpected,
        reason,
        reason_code,
        diagnostic_scope,
        changed_components,
        previous_cache_read_input_tokens: previous.cache_read_input_tokens,
        current_cache_read_input_tokens: current.cache_read_input_tokens,
        token_drop,
        elapsed_seconds: elapsed,
    })
}

fn summarize_last_message(
    messages: &[crate::types::InputMessage],
) -> (Option<String>, Vec<String>) {
    let Some(last_message) = messages.last() else {
        return (None, Vec::new());
    };
    let content_types = last_message
        .content
        .iter()
        .map(input_content_block_kind)
        .map(str::to_string)
        .collect();
    (Some(last_message.role.clone()), content_types)
}

fn input_content_block_kind(block: &InputContentBlock) -> &'static str {
    match block {
        InputContentBlock::Text { .. } => "text",
        InputContentBlock::ToolUse { .. } => "tool_use",
        InputContentBlock::ToolResult { .. } => "tool_result",
    }
}

fn format_request_change_reason(
    changed_components: &[String],
    previous: &TrackedPromptState,
    current: &TrackedPromptState,
) -> String {
    let mut details = Vec::new();
    if changed_components
        .iter()
        .any(|component| component == "model")
    {
        details.push(format!(
            "model '{}' -> '{}'",
            display_string_or_placeholder(&previous.model_name),
            display_string_or_placeholder(&current.model_name)
        ));
    }
    if changed_components
        .iter()
        .any(|component| component == "system")
    {
        if previous.system_block_count == current.system_block_count {
            details.push(format!(
                "system prompt changed with the same block count ({})",
                current.system_block_count
            ));
        } else {
            details.push(format!(
                "system_block_count {} -> {}",
                previous.system_block_count, current.system_block_count
            ));
        }
    }
    if changed_components
        .iter()
        .any(|component| component == "tools")
    {
        if previous.tool_count == current.tool_count {
            details.push(format!(
                "tool definitions changed with the same tool count ({})",
                current.tool_count
            ));
        } else {
            details.push(format!(
                "tool_count {} -> {}",
                previous.tool_count, current.tool_count
            ));
        }
    }
    if changed_components
        .iter()
        .any(|component| component == "messages")
    {
        let mut message_details = vec![format!(
            "message_count {} -> {}",
            previous.message_count, current.message_count
        )];
        if previous.last_message_role != current.last_message_role {
            message_details.push(format!(
                "last_message_role {} -> {}",
                display_optional_string(previous.last_message_role.as_deref()),
                display_optional_string(current.last_message_role.as_deref())
            ));
        }
        if previous.last_message_content_types != current.last_message_content_types {
            message_details.push(format!(
                "last_message_content_types {} -> {}",
                display_string_list(&previous.last_message_content_types),
                display_string_list(&current.last_message_content_types)
            ));
        }
        if previous.message_count == current.message_count
            && previous.last_message_role == current.last_message_role
            && previous.last_message_content_types == current.last_message_content_types
        {
            message_details.push(
                "message count and last-message shape stayed the same; the diff is inside message content or ordering"
                    .to_string(),
            );
        }
        details.push(format!("messages: {}", message_details.join(", ")));
    }

    format!(
        "local request fingerprint changed in [{}]: {}. This is a client-side diagnostic, not an Anthropic server-provided miss reason.",
        changed_components.join(", "),
        details.join("; ")
    )
}

fn display_optional_string(value: Option<&str>) -> String {
    value.map_or_else(|| "<none>".to_string(), ToString::to_string)
}

fn display_string_or_placeholder(value: &str) -> String {
    if value.is_empty() {
        "<empty>".to_string()
    } else {
        value.to_string()
    }
}

fn display_string_list(values: &[String]) -> String {
    if values.is_empty() {
        "[]".to_string()
    } else {
        format!("[{}]", values.join(", "))
    }
}

fn log_cache_break(
    session_id: &str,
    request_hash: &str,
    event: &CacheBreakEvent,
    previous: Option<&TrackedPromptState>,
    current: &TrackedPromptState,
) {
    let changed_components = if event.changed_components.is_empty() {
        "<none>".to_string()
    } else {
        event.changed_components.join(",")
    };
    let previous_message_count = previous.map_or(0, |state| state.message_count);
    let previous_tool_count = previous.map_or(0, |state| state.tool_count);
    let previous_system_block_count = previous.map_or(0, |state| state.system_block_count);
    let previous_model = previous.map_or("<none>", |state| state.model_name.as_str());
    let previous_last_message_role = previous
        .and_then(|state| state.last_message_role.as_deref())
        .map_or_else(|| "<none>".to_string(), ToString::to_string);
    let previous_last_message_content_types = previous.map_or_else(
        || "[]".to_string(),
        |state| display_string_list(&state.last_message_content_types),
    );
    agent_debug_log(
        "prompt_cache.break_detected",
        format!(
            "session_id={session_id}\nrequest_hash={request_hash}\nunexpected={}\nreason_code={}\ndiagnostic_scope={}\nchanged_components={changed_components}\nprevious_cache_read_input_tokens={}\ncurrent_cache_read_input_tokens={}\ntoken_drop={}\nelapsed_seconds={}\nprevious_model={}\ncurrent_model={}\nprevious_system_block_count={previous_system_block_count}\ncurrent_system_block_count={}\nprevious_tool_count={previous_tool_count}\ncurrent_tool_count={}\nprevious_message_count={previous_message_count}\ncurrent_message_count={}\nprevious_last_message_role={previous_last_message_role}\ncurrent_last_message_role={}\nprevious_last_message_content_types={previous_last_message_content_types}\ncurrent_last_message_content_types={}\nreason={}",
            event.unexpected,
            event.reason_code,
            event.diagnostic_scope,
            event.previous_cache_read_input_tokens,
            event.current_cache_read_input_tokens,
            event.token_drop,
            event.elapsed_seconds,
            previous_model,
            current.model_name,
            current.system_block_count,
            current.tool_count,
            current.message_count,
            display_optional_string(current.last_message_role.as_deref()),
            display_string_list(&current.last_message_content_types),
            event.reason
        ),
    );
}

fn apply_usage_to_stats(
    stats: &mut PromptCacheStats,
    usage: &Usage,
    request_hash: &str,
    source: &str,
) {
    stats.total_cache_creation_input_tokens += u64::from(usage.cache_creation_input_tokens);
    stats.total_cache_read_input_tokens += u64::from(usage.cache_read_input_tokens);
    stats.last_cache_creation_input_tokens = Some(usage.cache_creation_input_tokens);
    stats.last_cache_read_input_tokens = Some(usage.cache_read_input_tokens);
    stats.last_request_hash = Some(request_hash.to_string());
    stats.last_cache_source = Some(source.to_string());
}

fn persist_state(inner: &PromptCacheInner) {
    let _ = ensure_cache_dirs(&inner.paths);
    let _ = write_json(&inner.paths.stats_path, &inner.stats);
    if let Some(previous) = &inner.previous {
        let _ = write_json(&inner.paths.session_state_path, previous);
    }
}

fn write_completion_entry(
    paths: &PromptCachePaths,
    request_hash: &str,
    response: &MessageResponse,
) {
    let _ = ensure_cache_dirs(paths);
    let entry = CompletionCacheEntry {
        cached_at_unix_secs: now_unix_secs(),
        fingerprint_version: current_fingerprint_version(),
        response: response.clone(),
    };
    let _ = write_json(&paths.completion_entry_path(request_hash), &entry);
}

fn ensure_cache_dirs(paths: &PromptCachePaths) -> std::io::Result<()> {
    fs::create_dir_all(&paths.completion_dir)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    fs::write(path, json)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn request_hash_hex(request: &MessageRequest) -> String {
    format!(
        "{REQUEST_FINGERPRINT_PREFIX}-{:016x}",
        hash_serializable(request)
    )
}

fn hash_serializable<T: Serialize>(value: &T) -> u64 {
    let json = serde_json::to_vec(value).unwrap_or_default();
    stable_hash_bytes(&json)
}

fn sanitize_path_segment(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    if sanitized.len() <= MAX_SANITIZED_LENGTH {
        return sanitized;
    }
    let suffix = format!("-{:x}", hash_string(value));
    format!(
        "{}{}",
        &sanitized[..MAX_SANITIZED_LENGTH.saturating_sub(suffix.len())],
        suffix
    )
}

fn hash_string(value: &str) -> u64 {
    stable_hash_bytes(value.as_bytes())
}

fn base_cache_root() -> PathBuf {
    if let Some(config_home) = std::env::var_os("CLAUDE_CONFIG_HOME") {
        return PathBuf::from(config_home)
            .join("cache")
            .join("prompt-cache");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".claude")
            .join("cache")
            .join("prompt-cache");
    }
    std::env::temp_dir().join("claude-prompt-cache")
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

const fn current_fingerprint_version() -> u32 {
    REQUEST_FINGERPRINT_VERSION
}

fn stable_hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::{
        build_system_blocks_with_cache_controls, detect_cache_break,
        prompt_cache_block_diagnostics, read_json, request_hash_hex, sanitize_path_segment,
        PromptCache, PromptCacheConfig, PromptCachePaths, TrackedPromptState,
        REQUEST_FINGERPRINT_PREFIX,
    };
    use crate::types::{
        CacheControl, InputContentBlock, InputMessage, MessageRequest, MessageResponse,
        OutputContentBlock, SystemContentBlock, ToolDefinition, ToolResultContentBlock, Usage,
    };

    fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn path_builder_sanitizes_session_identifier() {
        let paths = PromptCachePaths::for_session("session:/with spaces");
        let session_dir = paths
            .session_dir
            .file_name()
            .and_then(|value| value.to_str())
            .expect("session dir name");
        assert_eq!(session_dir, "session--with-spaces");
        assert!(paths.completion_dir.ends_with("completions"));
        assert!(paths.stats_path.ends_with("stats.json"));
        assert!(paths.session_state_path.ends_with("session-state.json"));
    }

    #[test]
    fn request_fingerprint_drives_unexpected_break_detection() {
        let request = sample_request("same");
        let previous = TrackedPromptState::from_usage(
            &request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 6_000,
                output_tokens: 0,
                cache_creation: std::collections::BTreeMap::new(),
            },
        );
        let current = TrackedPromptState::from_usage(
            &request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1_000,
                output_tokens: 0,
                cache_creation: std::collections::BTreeMap::new(),
            },
        );
        let event = detect_cache_break(&PromptCacheConfig::default(), Some(&previous), &current)
            .expect("break should be detected");
        assert!(event.unexpected);
        assert_eq!(event.reason_code, "stable_fingerprint_cache_drop");
        assert!(event.changed_components.is_empty());
        assert_eq!(event.diagnostic_scope, "local_request_fingerprint");
        assert!(event
            .reason
            .contains("local request fingerprint stayed stable"));
    }

    #[test]
    fn changed_prompt_marks_break_as_expected() {
        let previous_request = sample_request("first");
        let current_request = sample_request("second");
        let previous = TrackedPromptState::from_usage(
            &previous_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 6_000,
                output_tokens: 0,
                cache_creation: std::collections::BTreeMap::new(),
            },
        );
        let current = TrackedPromptState::from_usage(
            &current_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1_000,
                output_tokens: 0,
                cache_creation: std::collections::BTreeMap::new(),
            },
        );
        let event = detect_cache_break(&PromptCacheConfig::default(), Some(&previous), &current)
            .expect("break should be detected");
        assert!(!event.unexpected);
        assert_eq!(event.reason_code, "local_request_components_changed");
        assert_eq!(event.changed_components, vec!["messages".to_string()]);
        assert_eq!(event.diagnostic_scope, "local_request_fingerprint");
        assert!(event.reason.contains("client-side diagnostic"));
        assert!(event
            .reason
            .contains("message count and last-message shape stayed the same"));
    }

    #[test]
    fn completion_cache_round_trip_persists_recent_response() {
        let _guard = test_env_lock();
        let temp_root = std::env::temp_dir().join(format!(
            "prompt-cache-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::env::set_var("CLAUDE_CONFIG_HOME", &temp_root);
        let cache = PromptCache::new("unit-test-session");
        let request = sample_request("cache me");
        let response = sample_response(42, 12, "cached");

        assert!(cache.lookup_completion(&request).is_none());
        let record = cache.record_response(&request, &response);
        assert!(record.cache_break.is_none());

        let cached = cache
            .lookup_completion(&request)
            .expect("cached response should load");
        assert_eq!(cached.content, response.content);

        let stats = cache.stats();
        assert_eq!(stats.completion_cache_hits, 1);
        assert_eq!(stats.completion_cache_misses, 1);
        assert_eq!(stats.completion_cache_writes, 1);

        let persisted = read_json::<super::PromptCacheStats>(&cache.paths().stats_path)
            .expect("stats should persist");
        assert_eq!(persisted.completion_cache_hits, 1);

        std::fs::remove_dir_all(temp_root).expect("cleanup temp root");
        std::env::remove_var("CLAUDE_CONFIG_HOME");
    }

    #[test]
    fn distinct_requests_do_not_collide_in_completion_cache() {
        let _guard = test_env_lock();
        let temp_root = std::env::temp_dir().join(format!(
            "prompt-cache-distinct-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::env::set_var("CLAUDE_CONFIG_HOME", &temp_root);
        let cache = PromptCache::new("distinct-request-session");
        let first_request = sample_request("first");
        let second_request = sample_request("second");

        let response = sample_response(42, 12, "cached");
        let _ = cache.record_response(&first_request, &response);

        assert!(cache.lookup_completion(&second_request).is_none());

        std::fs::remove_dir_all(temp_root).expect("cleanup temp root");
        std::env::remove_var("CLAUDE_CONFIG_HOME");
    }

    #[test]
    fn expired_completion_entries_are_not_reused() {
        let _guard = test_env_lock();
        let temp_root = std::env::temp_dir().join(format!(
            "prompt-cache-expired-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::env::set_var("CLAUDE_CONFIG_HOME", &temp_root);
        let cache = PromptCache::with_config(PromptCacheConfig {
            session_id: "expired-session".to_string(),
            completion_ttl: Duration::ZERO,
            ..PromptCacheConfig::default()
        });
        let request = sample_request("expire me");
        let response = sample_response(7, 3, "stale");

        let _ = cache.record_response(&request, &response);

        assert!(cache.lookup_completion(&request).is_none());
        let stats = cache.stats();
        assert_eq!(stats.completion_cache_hits, 0);
        assert_eq!(stats.completion_cache_misses, 1);

        std::fs::remove_dir_all(temp_root).expect("cleanup temp root");
        std::env::remove_var("CLAUDE_CONFIG_HOME");
    }

    #[test]
    fn cache_break_writes_plaintext_debug_log() {
        let _guard = test_env_lock();
        let temp_root = std::env::temp_dir().join(format!(
            "prompt-cache-debug-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let debug_root = temp_root.join("debug");
        std::env::set_var("CLAUDE_CONFIG_HOME", &temp_root);
        std::env::set_var("CLAWD_AGENT_DEBUG", &debug_root);

        let cache = PromptCache::new("debug-session");
        let first_request = sample_request("first");
        let second_request = sample_request("second");

        let _ = cache.record_usage(
            &first_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 6_000,
                output_tokens: 0,
                cache_creation: std::collections::BTreeMap::new(),
            },
        );
        let record = cache.record_usage(
            &second_request,
            &Usage {
                input_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 1_000,
                output_tokens: 0,
                cache_creation: std::collections::BTreeMap::new(),
            },
        );

        let event = record.cache_break.expect("break should be recorded");
        assert_eq!(event.reason_code, "local_request_components_changed");

        let log_path = debug_root.join("clawd-agent-debug.log");
        let log_contents = std::fs::read_to_string(&log_path).expect("debug log should exist");
        assert!(log_contents.contains("prompt_cache.break_detected"));
        assert!(log_contents.contains("session_id=debug-session"));
        assert!(log_contents.contains("reason_code=local_request_components_changed"));
        assert!(log_contents.contains("changed_components=messages"));
        assert!(log_contents.contains("current_message_count=1"));

        std::fs::remove_dir_all(temp_root).expect("cleanup temp root");
        std::env::remove_var("CLAUDE_CONFIG_HOME");
        std::env::remove_var("CLAWD_AGENT_DEBUG");
    }

    #[test]
    fn sanitize_path_caps_long_values() {
        let long_value = "x".repeat(200);
        let sanitized = sanitize_path_segment(&long_value);
        assert!(sanitized.len() <= 80);
    }

    #[test]
    fn request_hashes_are_versioned_and_stable() {
        let request = sample_request("stable");
        let first = request_hash_hex(&request);
        let second = request_hash_hex(&request);
        assert_eq!(first, second);
        assert!(first.starts_with(REQUEST_FINGERPRINT_PREFIX));
    }

    #[test]
    fn system_blocks_use_two_stable_cache_markers_and_uncached_dynamic_tail() {
        let parts = vec![
            "stable prefix".to_string(),
            "stable instructions".to_string(),
            runtime::SYSTEM_PROMPT_DYNAMIC_BOUNDARY.to_string(),
            "dynamic environment".to_string(),
            runtime::SYSTEM_PROMPT_ATTACHMENT_BOUNDARY.to_string(),
            "dynamic attachments".to_string(),
        ];

        let blocks =
            build_system_blocks_with_cache_controls(&parts).expect("system blocks should build");

        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].text, "stable prefix");
        assert!(blocks[0].cache_control.is_some());
        assert_eq!(blocks[1].text, "stable instructions");
        assert!(blocks[1].cache_control.is_some());
        assert_eq!(blocks[2].text, "dynamic environment\n\ndynamic attachments");
        assert!(blocks[2].cache_control.is_none());
        assert!(!blocks
            .iter()
            .any(|block| block.text.contains("SYSTEM_PROMPT")));
    }

    #[test]
    fn prompt_cache_block_diagnostics_flags_breakpoints_beyond_twenty_block_lookback() {
        let mut messages = (0..42)
            .map(|index| InputMessage::user_text(format!("message-{index}")))
            .collect::<Vec<_>>();
        for index in [0usize, 20, 41] {
            let Some(InputContentBlock::Text { cache_control, .. }) =
                messages[index].content.first_mut()
            else {
                panic!("test messages should contain text blocks");
            };
            *cache_control = Some(CacheControl::ephemeral());
        }
        let request = MessageRequest {
            model: "claude-opus-4-6".to_string(),
            max_tokens: 1024,
            messages,
            ..Default::default()
        };

        let diagnostics = prompt_cache_block_diagnostics(&request);

        assert_eq!(
            diagnostics
                .cache_breakpoints
                .iter()
                .map(|breakpoint| breakpoint.block_index)
                .collect::<Vec<_>>(),
            vec![0, 20, 41]
        );
        assert_eq!(
            diagnostics
                .cache_breakpoints
                .iter()
                .map(|breakpoint| breakpoint.distance_from_previous_breakpoint)
                .collect::<Vec<_>>(),
            vec![None, Some(20), Some(21)]
        );
        assert_eq!(
            diagnostics
                .cache_breakpoints
                .iter()
                .map(|breakpoint| breakpoint.exceeds_anthropic_20_block_lookback)
                .collect::<Vec<_>>(),
            vec![false, false, true]
        );
    }

    #[test]
    fn prompt_cache_block_diagnostics_reports_model_visible_tool_result_size() {
        let request = MessageRequest {
            model: "claude-opus-4-6".to_string(),
            max_tokens: 1024,
            tools: Some(vec![ToolDefinition {
                name: "edit_file".to_string(),
                description: Some("Edit a file".to_string()),
                input_schema: serde_json::json!({"type": "object"}),
                cache_control: Some(CacheControl::ephemeral()),
            }]),
            system: Some(vec![SystemContentBlock::text("stable system")
                .with_cache_control(CacheControl::ephemeral())]),
            messages: vec![
                InputMessage {
                    role: "assistant".to_string(),
                    content: vec![InputContentBlock::ToolUse {
                        id: "tool-1".to_string(),
                        name: "edit_file".to_string(),
                        input: serde_json::json!({"path": "a.txt"}),
                    }],
                },
                InputMessage {
                    role: "user".to_string(),
                    content: vec![InputContentBlock::ToolResult {
                        tool_use_id: "tool-1".to_string(),
                        content: vec![ToolResultContentBlock::Text {
                            text: "visible output".to_string(),
                        }],
                        is_error: false,
                        cache_control: Some(CacheControl::ephemeral()),
                    }],
                },
            ],
            ..Default::default()
        };

        let diagnostics = prompt_cache_block_diagnostics(&request);

        assert_eq!(diagnostics.total_content_blocks, 4);
        assert_eq!(
            diagnostics
                .cache_breakpoints
                .iter()
                .map(|breakpoint| breakpoint.block_index)
                .collect::<Vec<_>>(),
            vec![0, 1, 3]
        );
        assert_eq!(
            diagnostics
                .cache_breakpoints
                .iter()
                .map(|breakpoint| breakpoint.distance_from_previous_breakpoint)
                .collect::<Vec<_>>(),
            vec![None, Some(1), Some(2)]
        );
        assert_eq!(diagnostics.tool_results.len(), 1);
        let result = diagnostics
            .tool_results
            .first()
            .expect("tool result diagnostic should be recorded");
        assert_eq!(result.block_index, 3);
        assert_eq!(result.tool_use_id, "tool-1");
        assert_eq!(result.tool_name.as_deref(), Some("edit_file"));
        assert_eq!(result.model_visible_chars, "visible output".chars().count());
        assert_eq!(result.model_visible_bytes, "visible output".len());
        assert!(result.has_cache_control);
        assert!(result.in_cached_prefix);
    }

    fn sample_request(text: &str) -> MessageRequest {
        MessageRequest {
            model: "claude-3-7-sonnet-latest".to_string(),
            max_tokens: 64,
            messages: vec![InputMessage::user_text(text)],
            system: Some(vec![SystemContentBlock::text("system")]),
            tools: None,
            tool_choice: None,
            stream: false,
            ..Default::default()
        }
    }

    fn sample_response(
        cache_read_input_tokens: u32,
        output_tokens: u32,
        text: &str,
    ) -> MessageResponse {
        MessageResponse {
            id: "msg_test".to_string(),
            kind: "message".to_string(),
            role: "assistant".to_string(),
            content: vec![OutputContentBlock::Text {
                text: text.to_string(),
            }],
            model: "claude-3-7-sonnet-latest".to_string(),
            stop_reason: Some("end_turn".to_string()),
            stop_sequence: None,
            usage: Usage {
                input_tokens: 10,
                cache_creation_input_tokens: 5,
                cache_read_input_tokens,
                output_tokens,
                cache_creation: std::collections::BTreeMap::new(),
            },
            request_id: Some("req_test".to_string()),
        }
    }
}
