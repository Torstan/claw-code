use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::time::{Duration, Instant};

use api::{
    build_system_blocks_with_cache_controls, detect_provider_kind,
    log_prompt_cache_block_diagnostics, summarize_prompt_cache_controls, AnthropicClient,
    CacheControl, ContentBlockDelta, ContextManagement, InputContentBlock, InputMessage,
    MessageRequest, MessageResponse, OutputConfig, OutputContentBlock, PromptCache,
    ProviderClient as ApiProviderClient, ProviderKind, StreamEvent as ApiStreamEvent,
    ThinkingConfig, ToolChoice, ToolDefinition, ToolResultContentBlock,
};
use runtime::{
    agent_debug_log, ApiClient, ApiRequest, AssistantEvent, ContentBlock, ConversationMessage,
    MessageRole, PromptCacheEvent, RuntimeError,
};
use serde_json::Value;
use tools::{render_tool_result_for_model, GlobalToolRegistry};

use crate::args::filter_tool_specs_for_request;
use crate::auth::resolve_cli_auth_source;
use crate::render::{MarkdownStreamState, TerminalRenderer};
use crate::repl::InternalPromptProgressReporter;
use crate::tool_display::{format_tool_call_start, truncate_for_summary};
use crate::AllowedToolSet;

fn max_tokens_for_model(_model: &str) -> u32 {
    64_000
}

const POST_TOOL_STALL_TIMEOUT: Duration = Duration::from_secs(10);

// NOTE: Despite the historical name `AnthropicRuntimeClient`, this struct
// now holds an `ApiProviderClient` which dispatches to Anthropic, xAI,
// OpenAI, or DashScope at construction time based on
// `detect_provider_kind(&model)`. The struct name is kept to avoid
// churning `BuiltRuntime` and every Deref/DerefMut site that references
// it. See ROADMAP #29 for the provider-dispatch routing fix.
pub(crate) struct AnthropicRuntimeClient {
    runtime: tokio::runtime::Runtime,
    client: ApiProviderClient,
    session_id: String,
    model: String,
    enable_tools: bool,
    emit_output: bool,
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    progress_reporter: Option<InternalPromptProgressReporter>,
    reasoning_effort: Option<String>,
}

impl AnthropicRuntimeClient {
    pub(crate) fn new(
        session_id: &str,
        model: String,
        enable_tools: bool,
        emit_output: bool,
        allowed_tools: Option<AllowedToolSet>,
        tool_registry: GlobalToolRegistry,
        progress_reporter: Option<InternalPromptProgressReporter>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Dispatch to the correct provider at construction time.
        // `ApiProviderClient` (exposed by the api crate as
        // `ProviderClient`) is an enum over Anthropic / xAI / OpenAI
        // variants, where xAI and OpenAI both use the OpenAI-compat
        // wire format under the hood. We consult
        // `detect_provider_kind(&resolved_model)` so model-name prefix
        // routing (`openai/`, `gpt-`, `grok`, `qwen/`) wins over
        // env-var presence.
        //
        // For Anthropic we build the client directly instead of going
        // through `ApiProviderClient::from_model_with_anthropic_auth`
        // so we can explicitly apply `api::read_base_url()` — that
        // reads `ANTHROPIC_BASE_URL` and is required for the local
        // mock-server test harness
        // (`crates/rusty-claude-cli/tests/compact_output.rs`) to point
        // claw at its fake Anthropic endpoint. We also attach a
        // session-scoped prompt cache on the Anthropic path; the
        // prompt cache is Anthropic-only so non-Anthropic variants
        // skip it.
        let resolved_model = api::resolve_model_alias(&model);
        let client = match detect_provider_kind(&resolved_model) {
            ProviderKind::Anthropic => {
                let auth = resolve_cli_auth_source()?;
                let inner = AnthropicClient::from_auth(auth)
                    .with_base_url(api::read_base_url())
                    .with_beta("prompt-caching-scope-2026-01-05")
                    .with_extra_header("X-Claude-Code-Session-Id", session_id.to_string())
                    .with_extra_header("x-app", "cli")
                    .with_extra_header("anthropic-dangerous-direct-browser-access", "true")
                    .with_prompt_cache(PromptCache::new(session_id));
                ApiProviderClient::Anthropic(inner)
            }
            ProviderKind::Xai | ProviderKind::OpenAi => {
                // The api crate's `ProviderClient::from_model_with_anthropic_auth`
                // with `None` for the anthropic auth routes via
                // `detect_provider_kind` and builds an
                // `OpenAiCompatClient::from_env` with the matching
                // `OpenAiCompatConfig` (openai / xai / dashscope).
                // That reads the correct API-key env var and BASE_URL
                // override internally, so this one call covers OpenAI,
                // OpenRouter, xAI, DashScope, Ollama, and any other
                // OpenAI-compat endpoint users configure via
                // `OPENAI_BASE_URL` / `XAI_BASE_URL` / `DASHSCOPE_BASE_URL`.
                ApiProviderClient::from_model_with_anthropic_auth(&resolved_model, None)?
            }
        };
        Ok(Self {
            runtime: tokio::runtime::Runtime::new()?,
            client,
            session_id: session_id.to_string(),
            model,
            enable_tools,
            emit_output,
            allowed_tools,
            tool_registry,
            progress_reporter,
            reasoning_effort: None,
        })
    }

    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.reasoning_effort = effort;
    }
}

impl ApiClient for AnthropicRuntimeClient {
    #[allow(clippy::too_many_lines)]
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        if let Some(progress_reporter) = &self.progress_reporter {
            progress_reporter.mark_model_phase();
        }
        let is_post_tool = request_ends_with_tool_result(&request);
        let system_blocks = build_system_blocks_with_cache_controls(&request.system_prompt);
        let mut tools = self.enable_tools.then(|| {
            filter_tool_specs_for_request(
                &self.tool_registry,
                self.allowed_tools.as_ref(),
                request.suppress_background_task_tools,
            )
        });
        apply_tool_cache_controls(&mut tools);
        let messages = convert_messages(&request.messages);
        let message_request = MessageRequest {
            model: self.model.clone(),
            max_tokens: max_tokens_for_model(&self.model),
            messages,
            cache_control: Some(CacheControl::ephemeral()),
            system: system_blocks,
            tools,
            tool_choice: self.enable_tools.then_some(ToolChoice::Auto),
            stream: true,
            reasoning_effort: self.reasoning_effort.clone(),
            context_management: Some(ContextManagement::default()),
            thinking: Some(ThinkingConfig::adaptive()),
            output_config: Some(OutputConfig::high()),
            ..Default::default()
        };
        let prompt_cache_summary = summarize_prompt_cache_controls(&message_request);
        let cache_control_types_json =
            serde_json::to_string(&prompt_cache_summary.cache_control_types)
                .unwrap_or_else(|_| "[]".to_string());
        agent_debug_log(
            "cli.provider.stream.prompt_cache",
            format!(
                "session_id={} model={} message_count={} cache_enabled={} cache_control_count={} cache_control_types={} automatic_cache_control_count={} system_cache_control_count={} tool_cache_control_count={} message_cache_control_count={}",
                self.session_id,
                self.model,
                message_request.messages.len(),
                prompt_cache_summary.enabled,
                prompt_cache_summary.cache_control_count,
                cache_control_types_json,
                prompt_cache_summary.automatic_cache_control_count,
                prompt_cache_summary.system_cache_control_count,
                prompt_cache_summary.tool_cache_control_count,
                prompt_cache_summary.message_cache_control_count
            ),
        );
        log_prompt_cache_block_diagnostics("cli", &self.session_id, &self.model, &message_request);
        let tool_count = message_request.tools.as_ref().map_or(0, Vec::len);
        let max_attempts: usize = if is_post_tool { 2 } else { 1 };
        let total_started_at = Instant::now();
        agent_debug_log(
            "cli.provider.stream.begin",
            format!(
                "session_id={}\nmodel={}\nmessage_count={}\ntool_count={}\nsystem_prompt_parts={}\npost_tool={}\nmax_attempts={}\nreasoning_effort={:?}",
                self.session_id,
                self.model,
                request.messages.len(),
                tool_count,
                request.system_prompt.len(),
                is_post_tool,
                max_attempts,
                self.reasoning_effort
            ),
        );

        self.runtime.block_on(async {
            // When resuming after tool execution, apply a stall timeout on the
            // first stream event.  If the model does not respond within the
            // deadline we drop the stalled connection and re-send the request as
            // a continuation nudge (one retry only).
            for attempt in 1..=max_attempts {
                let attempt_started_at = Instant::now();
                let apply_stall_timeout = is_post_tool && attempt == 1;
                agent_debug_log(
                    "cli.provider.stream.attempt.begin",
                    format!(
                        "session_id={}\nmodel={}\nattempt={}\nmax_attempts={}\napply_stall_timeout={}",
                        self.session_id, self.model, attempt, max_attempts, apply_stall_timeout
                    ),
                );
                let result = self
                    .consume_stream(&message_request, apply_stall_timeout)
                    .await;
                match result {
                    Ok(events) => {
                        agent_debug_log(
                            "cli.provider.stream.success",
                            format!(
                                "session_id={}\nmodel={}\nattempt={}\nevent_count={}\nattempt_elapsed_ms={}\ntotal_elapsed_ms={}",
                                self.session_id,
                                self.model,
                                attempt,
                                events.len(),
                                attempt_started_at.elapsed().as_millis(),
                                total_started_at.elapsed().as_millis()
                            ),
                        );
                        return Ok(events);
                    }
                    Err(error)
                        if error.to_string().contains("post-tool stall")
                            && attempt < max_attempts =>
                    {
                        // Stalled after tool completion — nudge the model by
                        // re-sending the same request.
                        agent_debug_log(
                            "cli.provider.stream.retry",
                            format!(
                                "session_id={}\nmodel={}\nattempt={}\nreason=post_tool_stall\nattempt_elapsed_ms={}\ntotal_elapsed_ms={}\nerror={error}",
                                self.session_id,
                                self.model,
                                attempt,
                                attempt_started_at.elapsed().as_millis(),
                                total_started_at.elapsed().as_millis()
                            ),
                        );
                    }
                    Err(error) => {
                        agent_debug_log(
                            "cli.provider.stream.error",
                            format!(
                                "session_id={}\nmodel={}\nattempt={}\nattempt_elapsed_ms={}\ntotal_elapsed_ms={}\nerror={error}",
                                self.session_id,
                                self.model,
                                attempt,
                                attempt_started_at.elapsed().as_millis(),
                                total_started_at.elapsed().as_millis()
                            ),
                        );
                        return Err(error);
                    }
                }
            }

            let error = RuntimeError::new("post-tool continuation nudge exhausted");
            agent_debug_log(
                "cli.provider.stream.error",
                format!(
                    "session_id={}\nmodel={}\nattempt={}\ntotal_elapsed_ms={}\nerror={error}",
                    self.session_id,
                    self.model,
                    max_attempts,
                    total_started_at.elapsed().as_millis()
                ),
            );
            Err(error)
        })
    }
}

impl AnthropicRuntimeClient {
    /// Consume a single streaming response, optionally applying a stall
    /// timeout on the first event for post-tool continuations.
    #[allow(clippy::too_many_lines)]
    async fn consume_stream(
        &self,
        message_request: &MessageRequest,
        apply_stall_timeout: bool,
    ) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let consume_started_at = Instant::now();
        let mut stream = self
            .client
            .stream_message(message_request)
            .await
            .map_err(|error| {
                RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
            })?;
        let mut stdout = io::stdout();
        let mut sink = io::sink();
        let out: &mut dyn Write = if self.emit_output {
            &mut stdout
        } else {
            &mut sink
        };
        let renderer = TerminalRenderer::new();
        let mut markdown_stream = MarkdownStreamState::default();
        let mut events = Vec::new();
        let mut pending_tools: BTreeMap<u32, (String, String, String)> = BTreeMap::new();
        let mut tool_block_indices: BTreeSet<u32> = BTreeSet::new();
        let mut tool_input_delta_counts: BTreeMap<u32, usize> = BTreeMap::new();
        let mut block_has_thinking_summary = false;
        let mut saw_stop = false;
        let mut received_any_event = false;
        let mut stream_event_seq = 0_u64;

        loop {
            let next = if apply_stall_timeout && !received_any_event {
                match tokio::time::timeout(POST_TOOL_STALL_TIMEOUT, stream.next_event()).await {
                    Ok(inner) => inner.map_err(|error| {
                        RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
                    })?,
                    Err(_elapsed) => {
                        agent_debug_log(
                            "cli.provider.stream.stall_timeout",
                            format!(
                                "session_id={}\nmodel={}\ntimeout_ms={}",
                                self.session_id,
                                self.model,
                                POST_TOOL_STALL_TIMEOUT.as_millis()
                            ),
                        );
                        return Err(RuntimeError::new(
                            "post-tool stall: model did not respond within timeout",
                        ));
                    }
                }
            } else {
                stream.next_event().await.map_err(|error| {
                    RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
                })?
            };

            let Some(event) = next else {
                break;
            };
            stream_event_seq += 1;
            if !received_any_event {
                let event_name = match &event {
                    ApiStreamEvent::MessageStart(_) => "message_start",
                    ApiStreamEvent::MessageDelta(_) => "message_delta",
                    ApiStreamEvent::ContentBlockStart(_) => "content_block_start",
                    ApiStreamEvent::ContentBlockDelta(_) => "content_block_delta",
                    ApiStreamEvent::ContentBlockStop(_) => "content_block_stop",
                    ApiStreamEvent::MessageStop(_) => "message_stop",
                };
                agent_debug_log(
                    "cli.provider.stream.first_event",
                    format!(
                        "session_id={}\nmodel={}\nelapsed_ms={}\nevent={event_name}",
                        self.session_id,
                        self.model,
                        consume_started_at.elapsed().as_millis()
                    ),
                );
            }
            if should_log_stream_event_for_tool_diagnostics(&event) {
                cli_agent_debug_log(
                    "cli.provider.stream.event",
                    format!(
                        "session_id={}\nmodel={}\nevent_seq={}\nelapsed_ms={}\n{}\n{}",
                        self.session_id,
                        self.model,
                        stream_event_seq,
                        consume_started_at.elapsed().as_millis(),
                        stream_event_debug_summary(&event, 4000),
                        pending_tools_debug_summary(&pending_tools, 240)
                    ),
                );
            }
            received_any_event = true;

            match event {
                ApiStreamEvent::MessageStart(start) => {
                    for (index, block) in start.message.content.into_iter().enumerate() {
                        let index =
                            u32::try_from(index).expect("stream message block index overflow");
                        if matches!(&block, OutputContentBlock::ToolUse { .. }) {
                            tool_input_delta_counts.insert(index, 0);
                        }
                        track_tool_block_index(&block, index, &mut tool_block_indices);
                        push_output_block(
                            block,
                            index,
                            out,
                            &mut events,
                            &mut pending_tools,
                            true,
                            &mut block_has_thinking_summary,
                        )?;
                    }
                }
                ApiStreamEvent::ContentBlockStart(start) => {
                    track_tool_block_index(
                        &start.content_block,
                        start.index,
                        &mut tool_block_indices,
                    );
                    if let OutputContentBlock::ToolUse { id, name, input } = &start.content_block {
                        tool_input_delta_counts.insert(start.index, 0);
                        cli_agent_debug_log(
                            "cli.provider.stream.tool_start",
                            format!(
                                "session_id={}\nmodel={}\nindex={}\ntool_id={id}\ntool_name={name}\n{}",
                                self.session_id,
                                self.model,
                                start.index,
                                debug_json_value_summary(input, 160)
                            ),
                        );
                    }
                    push_output_block(
                        start.content_block,
                        start.index,
                        out,
                        &mut events,
                        &mut pending_tools,
                        true,
                        &mut block_has_thinking_summary,
                    )?;
                }
                ApiStreamEvent::ContentBlockDelta(delta) => match delta.delta {
                    ContentBlockDelta::TextDelta { text } => {
                        if !text.is_empty() {
                            if let Some(progress_reporter) = &self.progress_reporter {
                                progress_reporter.mark_text_phase(&text);
                            }
                            if let Some(rendered) = markdown_stream.push(&renderer, &text) {
                                write!(out, "{rendered}")
                                    .and_then(|()| out.flush())
                                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                            }
                            events.push(AssistantEvent::TextDelta(text));
                        }
                    }
                    ContentBlockDelta::InputJsonDelta { partial_json } => {
                        if let Some((id, name, input)) = pending_tools.get_mut(&delta.index) {
                            input.push_str(&partial_json);
                            let delta_count = tool_input_delta_counts
                                .entry(delta.index)
                                .and_modify(|count| *count += 1)
                                .or_insert(1);
                            if should_log_streamed_tool_input_delta(*delta_count) {
                                cli_agent_debug_log(
                                    "cli.provider.stream.tool_input_delta",
                                    format!(
                                        "session_id={}\nmodel={}\nindex={}\ntool_id={id}\ntool_name={name}\ndelta_count={}\npartial_bytes={}\npartial_chars={}\naccumulated_bytes={}\naccumulated_chars={}\npartial={}\naccumulated_suffix={}",
                                        self.session_id,
                                        self.model,
                                        delta.index,
                                        delta_count,
                                        partial_json.len(),
                                        partial_json.chars().count(),
                                        input.len(),
                                        input.chars().count(),
                                        json_debug_string(&partial_json, 160),
                                        json_debug_suffix(input, 160)
                                    ),
                                );
                            }
                        } else {
                            cli_agent_debug_log(
                                "cli.provider.stream.tool_input_delta_without_start",
                                format!(
                                    "session_id={}\nmodel={}\nindex={}\npartial_bytes={}\npartial_chars={}\npartial={}",
                                    self.session_id,
                                    self.model,
                                    delta.index,
                                    partial_json.len(),
                                    partial_json.chars().count(),
                                    json_debug_string(&partial_json, 160)
                                ),
                            );
                        }
                    }
                    ContentBlockDelta::ThinkingDelta { .. } => {
                        if !block_has_thinking_summary {
                            render_thinking_block_summary(out, None, false)?;
                            block_has_thinking_summary = true;
                        }
                    }
                    ContentBlockDelta::SignatureDelta { .. } => {}
                },
                ApiStreamEvent::ContentBlockStop(stop) => {
                    block_has_thinking_summary = false;
                    if let Some(rendered) = markdown_stream.flush(&renderer) {
                        write!(out, "{rendered}")
                            .and_then(|()| out.flush())
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                    }
                    if let Some((id, name, input)) = pending_tools.remove(&stop.index) {
                        tool_block_indices.remove(&stop.index);
                        tool_input_delta_counts.remove(&stop.index);
                        let normalized_empty_to_object = input.trim().is_empty();
                        let raw_summary =
                            debug_labeled_json_input_summary("raw_input", &input, 240);
                        let input = normalize_tool_input_string(input);
                        cli_agent_debug_log(
                            "cli.provider.stream.tool_stop",
                            format!(
                                "session_id={}\nmodel={}\nindex={}\ntool_id={id}\ntool_name={name}\nnormalized_empty_to_object={}\n{}\n{}",
                                self.session_id,
                                self.model,
                                stop.index,
                                normalized_empty_to_object,
                                raw_summary,
                                debug_labeled_json_input_summary("normalized_input", &input, 240)
                            ),
                        );
                        if let Err(error) = validate_tool_input_json(&input) {
                            cli_agent_debug_log(
                                "cli.provider.stream.tool_input_json_invalid",
                                format!(
                                    "session_id={}\nmodel={}\nindex={}\ntool_id={id}\ntool_name={name}\nerror={error}\n{}\n{}",
                                    self.session_id,
                                    self.model,
                                    stop.index,
                                    debug_labeled_json_input_summary("invalid_input", &input, 4000),
                                    pending_tools_debug_summary(&pending_tools, 240)
                                ),
                            );
                            return self
                                .non_streaming_fallback(
                                    message_request,
                                    out,
                                    &format!(
                                        "invalid_streamed_tool_input_json tool_id={id} tool_name={name} error={error}"
                                    ),
                                )
                                .await;
                        }
                        if let Some(progress_reporter) = &self.progress_reporter {
                            progress_reporter.mark_tool_phase(&name, &input);
                        }
                        // Display tool call now that input is fully accumulated
                        writeln!(out, "\n{}", format_tool_call_start(&name, &input))
                            .and_then(|()| out.flush())
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                        events.push(AssistantEvent::ToolUse { id, name, input });
                    } else if should_log_tool_stop_without_start(
                        &mut tool_block_indices,
                        stop.index,
                    ) {
                        tool_input_delta_counts.remove(&stop.index);
                        cli_agent_debug_log(
                            "cli.provider.stream.tool_stop_without_start",
                            format!(
                                "session_id={}\nmodel={}\nindex={}\n{}",
                                self.session_id,
                                self.model,
                                stop.index,
                                pending_tools_debug_summary(&pending_tools, 240)
                            ),
                        );
                    }
                }
                ApiStreamEvent::MessageDelta(delta) => {
                    let cache_creation_json = serde_json::to_string(&delta.usage.cache_creation)
                        .unwrap_or_else(|_| "{}".to_string());
                    agent_debug_log(
                        "cli.provider.stream.usage",
                        format!(
                            "session_id={} model={} input_tokens={} output_tokens={} cache_creation_input_tokens={} cache_read_input_tokens={} cache_creation={}",
                            self.session_id,
                            self.model,
                            delta.usage.input_tokens,
                            delta.usage.output_tokens,
                            delta.usage.cache_creation_input_tokens,
                            delta.usage.cache_read_input_tokens,
                            cache_creation_json,
                        ),
                    );
                    events.push(AssistantEvent::Usage(delta.usage.token_usage()));
                }
                ApiStreamEvent::MessageStop(_) => {
                    saw_stop = true;
                    if !pending_tools.is_empty() {
                        cli_agent_debug_log(
                            "cli.provider.stream.message_stop_with_pending_tools",
                            format!(
                                "session_id={}\nmodel={}\n{}",
                                self.session_id,
                                self.model,
                                pending_tools_debug_summary(&pending_tools, 240)
                            ),
                        );
                    }
                    if let Some(rendered) = markdown_stream.flush(&renderer) {
                        write!(out, "{rendered}")
                            .and_then(|()| out.flush())
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                    }
                    events.push(AssistantEvent::MessageStop);
                }
            }
        }

        if !pending_tools.is_empty() {
            cli_agent_debug_log(
                "cli.provider.stream.ended_with_pending_tools",
                format!(
                    "session_id={}\nmodel={}\nsaw_stop={}\n{}",
                    self.session_id,
                    self.model,
                    saw_stop,
                    pending_tools_debug_summary(&pending_tools, 240)
                ),
            );
        }

        push_prompt_cache_record(&self.client, &mut events);

        if !saw_stop
            && events.iter().any(|event| {
                matches!(event, AssistantEvent::TextDelta(text) if !text.is_empty())
                    || matches!(event, AssistantEvent::ToolUse { .. })
            })
        {
            events.push(AssistantEvent::MessageStop);
        }

        if events
            .iter()
            .any(|event| matches!(event, AssistantEvent::MessageStop))
        {
            return Ok(events);
        }

        self.non_streaming_fallback(message_request, out, "stream_ended_without_message_stop")
            .await
    }

    async fn non_streaming_fallback(
        &self,
        message_request: &MessageRequest,
        out: &mut (impl Write + ?Sized),
        reason: &str,
    ) -> Result<Vec<AssistantEvent>, RuntimeError> {
        agent_debug_log(
            "cli.provider.stream.fallback_non_streaming",
            format!(
                "session_id={}\nmodel={}\nreason={}",
                self.session_id, self.model, reason
            ),
        );
        let response = self
            .client
            .send_message(&MessageRequest {
                stream: false,
                ..message_request.clone()
            })
            .await
            .map_err(|error| {
                RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
            })?;
        let mut events = response_to_events(response, out)?;
        push_prompt_cache_record(&self.client, &mut events);
        Ok(events)
    }
}

/// Returns `true` when the conversation ends with a tool-result message,
/// meaning the model is expected to continue after tool execution.
fn request_ends_with_tool_result(request: &ApiRequest) -> bool {
    request
        .messages
        .last()
        .is_some_and(|message| message.role == MessageRole::Tool)
}

pub(crate) fn format_user_visible_api_error(session_id: &str, error: &api::ApiError) -> String {
    if error.is_context_window_failure() {
        format_context_window_blocked_error(session_id, error)
    } else if error.is_generic_fatal_wrapper() {
        let mut qualifiers = vec![format!("session {session_id}")];
        if let Some(request_id) = error.request_id() {
            qualifiers.push(format!("trace {request_id}"));
        }
        format!(
            "{} ({}): {}",
            error.safe_failure_class(),
            qualifiers.join(", "),
            error
        )
    } else {
        error.to_string()
    }
}

fn format_context_window_blocked_error(session_id: &str, error: &api::ApiError) -> String {
    let mut lines = vec![
        "Context window blocked".to_string(),
        "  Failure class    context_window_blocked".to_string(),
        format!("  Session          {session_id}"),
    ];

    if let Some(request_id) = error.request_id() {
        lines.push(format!("  Trace            {request_id}"));
    }

    match error {
        api::ApiError::ContextWindowExceeded {
            model,
            estimated_input_tokens,
            requested_output_tokens,
            estimated_total_tokens,
            context_window_tokens,
        } => {
            lines.push(format!("  Model            {model}"));
            lines.push(format!(
                "  Input estimate   ~{estimated_input_tokens} tokens (heuristic)"
            ));
            lines.push(format!(
                "  Requested output {requested_output_tokens} tokens"
            ));
            lines.push(format!(
                "  Total estimate   ~{estimated_total_tokens} tokens (heuristic)"
            ));
            lines.push(format!("  Context window   {context_window_tokens} tokens"));
        }
        api::ApiError::Api { message, body, .. } => {
            let detail = message.as_deref().unwrap_or(body).trim();
            if !detail.is_empty() {
                lines.push(format!(
                    "  Detail           {}",
                    truncate_for_summary(detail, 120)
                ));
            }
        }
        api::ApiError::RetriesExhausted { last_error, .. } => {
            let detail = match last_error.as_ref() {
                api::ApiError::Api { message, body, .. } => message.as_deref().unwrap_or(body),
                other => return format_context_window_blocked_error(session_id, other),
            }
            .trim();
            if !detail.is_empty() {
                lines.push(format!(
                    "  Detail           {}",
                    truncate_for_summary(detail, 120)
                ));
            }
        }
        _ => {}
    }

    lines.push(String::new());
    lines.push("Recovery".to_string());
    lines.push("  Compact          /compact".to_string());
    lines.push(format!(
        "  Resume compact   claw --resume {session_id} /compact"
    ));
    lines.push("  Fresh session    /clear --confirm".to_string());
    lines.push(
        "  Reduce scope     remove large pasted context/files or ask for a smaller slice"
            .to_string(),
    );
    lines.push("  Retry            rerun after compacting or reducing the request".to_string());

    lines.join("\n")
}

fn render_thinking_block_summary(
    out: &mut (impl Write + ?Sized),
    char_count: Option<usize>,
    redacted: bool,
) -> Result<(), RuntimeError> {
    let summary = if redacted {
        "\n▶ Thinking block hidden by provider\n".to_string()
    } else if let Some(char_count) = char_count {
        format!("\n▶ Thinking ({char_count} chars hidden)\n")
    } else {
        "\n▶ Thinking hidden\n".to_string()
    };
    write!(out, "{summary}")
        .and_then(|()| out.flush())
        .map_err(|error| RuntimeError::new(error.to_string()))
}

pub(crate) fn push_output_block(
    block: OutputContentBlock,
    block_index: u32,
    out: &mut (impl Write + ?Sized),
    events: &mut Vec<AssistantEvent>,
    pending_tools: &mut BTreeMap<u32, (String, String, String)>,
    streaming_tool_input: bool,
    block_has_thinking_summary: &mut bool,
) -> Result<(), RuntimeError> {
    match block {
        OutputContentBlock::Text { text } => {
            if !text.is_empty() {
                let rendered = TerminalRenderer::new().markdown_to_ansi(&text);
                write!(out, "{rendered}")
                    .and_then(|()| out.flush())
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                events.push(AssistantEvent::TextDelta(text));
            }
        }
        OutputContentBlock::ToolUse { id, name, input } => {
            // During streaming, the initial content_block_start has an empty input ({}).
            // The real input arrives via input_json_delta events. In
            // non-streaming responses, preserve a legitimate empty object.
            let initial_input = if streaming_tool_input
                && input.is_object()
                && input.as_object().is_some_and(serde_json::Map::is_empty)
            {
                String::new()
            } else {
                input.to_string()
            };
            pending_tools.insert(block_index, (id, name, initial_input));
        }
        OutputContentBlock::Thinking { thinking, .. } => {
            render_thinking_block_summary(out, Some(thinking.chars().count()), false)?;
            *block_has_thinking_summary = true;
        }
        OutputContentBlock::RedactedThinking { .. } => {
            render_thinking_block_summary(out, None, true)?;
            *block_has_thinking_summary = true;
        }
    }
    Ok(())
}

pub(crate) fn track_tool_block_index(
    block: &OutputContentBlock,
    block_index: u32,
    tool_block_indices: &mut BTreeSet<u32>,
) {
    if matches!(block, OutputContentBlock::ToolUse { .. }) {
        tool_block_indices.insert(block_index);
    }
}

pub(crate) fn should_log_tool_stop_without_start(
    tool_block_indices: &mut BTreeSet<u32>,
    block_index: u32,
) -> bool {
    tool_block_indices.remove(&block_index)
}

pub(crate) fn response_to_events(
    response: MessageResponse,
    out: &mut (impl Write + ?Sized),
) -> Result<Vec<AssistantEvent>, RuntimeError> {
    let mut events = Vec::new();
    let mut pending_tools = BTreeMap::new();

    for (index, block) in response.content.into_iter().enumerate() {
        let index = u32::try_from(index).expect("response block index overflow");
        let mut block_has_thinking_summary = false;
        push_output_block(
            block,
            index,
            out,
            &mut events,
            &mut pending_tools,
            false,
            &mut block_has_thinking_summary,
        )?;
        if let Some((id, name, input)) = pending_tools.remove(&index) {
            events.push(AssistantEvent::ToolUse { id, name, input });
        }
    }

    events.push(AssistantEvent::Usage(response.usage.token_usage()));
    events.push(AssistantEvent::MessageStop);
    Ok(events)
}

pub(crate) fn normalize_tool_input_json(input: &str) -> &str {
    if input.trim().is_empty() {
        "{}"
    } else {
        input
    }
}

pub(crate) fn normalize_tool_input_string(input: String) -> String {
    if input.trim().is_empty() {
        "{}".to_string()
    } else {
        input
    }
}

pub(crate) fn validate_tool_input_json(input: &str) -> Result<(), String> {
    serde_json::from_str::<Value>(normalize_tool_input_json(input))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn debug_json_value_summary(value: &serde_json::Value, limit: usize) -> String {
    let rendered = serde_json::to_string(value).unwrap_or_else(|_| value.to_string());
    debug_json_input_summary(&rendered, limit)
}

pub(crate) fn debug_json_input_summary(input: &str, limit: usize) -> String {
    debug_labeled_json_input_summary("input", input, limit)
}

fn debug_labeled_json_input_summary(label: &str, input: &str, limit: usize) -> String {
    format!(
        "{label}_bytes={}\n{label}_chars={}\n{label}_trimmed_empty={}\n{label}_full_available={}\n{label}_prefix={}\n{label}_suffix={}",
        input.len(),
        input.chars().count(),
        input.trim().is_empty(),
        input.chars().count() <= limit,
        json_debug_string(input, limit),
        json_debug_suffix(input, limit)
    )
}

pub(crate) fn should_log_stream_event_for_tool_diagnostics(event: &ApiStreamEvent) -> bool {
    !matches!(
        event,
        ApiStreamEvent::ContentBlockDelta(api::ContentBlockDeltaEvent {
            delta: ContentBlockDelta::TextDelta { .. } | ContentBlockDelta::InputJsonDelta { .. },
            ..
        })
    )
}

pub(crate) fn should_log_streamed_tool_input_delta(delta_count: usize) -> bool {
    delta_count > 0 && (delta_count == 1 || delta_count.is_multiple_of(32))
}

fn stream_event_debug_summary(event: &ApiStreamEvent, limit: usize) -> String {
    match event {
        ApiStreamEvent::MessageStart(start) => format!(
            "event=message_start\ncontent_blocks={}",
            start.message.content.len()
        ),
        ApiStreamEvent::ContentBlockStart(start) => match &start.content_block {
            OutputContentBlock::ToolUse { id, name, input } => format!(
                "event=content_block_start\nindex={}\nblock_type=tool_use\ntool_id={id}\ntool_name={name}\n{}",
                start.index,
                debug_json_value_summary(input, limit)
            ),
            OutputContentBlock::Text { text } => format!(
                "event=content_block_start\nindex={}\nblock_type=text\ntext_bytes={}\ntext_chars={}",
                start.index,
                text.len(),
                text.chars().count()
            ),
            OutputContentBlock::Thinking { thinking, .. } => format!(
                "event=content_block_start\nindex={}\nblock_type=thinking\nthinking_chars={}",
                start.index,
                thinking.chars().count()
            ),
            OutputContentBlock::RedactedThinking { .. } => format!(
                "event=content_block_start\nindex={}\nblock_type=redacted_thinking",
                start.index
            ),
        },
        ApiStreamEvent::ContentBlockDelta(delta) => match &delta.delta {
            ContentBlockDelta::InputJsonDelta { partial_json } => format!(
                "event=content_block_delta\nindex={}\ndelta_type=input_json_delta\n{}",
                delta.index,
                debug_labeled_json_input_summary("partial_json", partial_json, limit)
            ),
            ContentBlockDelta::TextDelta { text } => format!(
                "event=content_block_delta\nindex={}\ndelta_type=text_delta\ntext_bytes={}\ntext_chars={}",
                delta.index,
                text.len(),
                text.chars().count()
            ),
            ContentBlockDelta::ThinkingDelta { thinking } => format!(
                "event=content_block_delta\nindex={}\ndelta_type=thinking_delta\nthinking_chars={}",
                delta.index,
                thinking.chars().count()
            ),
            ContentBlockDelta::SignatureDelta { signature } => format!(
                "event=content_block_delta\nindex={}\ndelta_type=signature_delta\nsignature_chars={}",
                delta.index,
                signature.chars().count()
            ),
        },
        ApiStreamEvent::ContentBlockStop(stop) => {
            format!("event=content_block_stop\nindex={}", stop.index)
        }
        ApiStreamEvent::MessageDelta(delta) => format!(
            "event=message_delta\nstop_reason={}\nstop_sequence={}\ninput_tokens={}\noutput_tokens={}\ncache_creation_input_tokens={}\ncache_read_input_tokens={}",
            delta.delta.stop_reason.as_deref().unwrap_or("none"),
            delta.delta.stop_sequence.as_deref().unwrap_or("none"),
            delta.usage.input_tokens,
            delta.usage.output_tokens,
            delta.usage.cache_creation_input_tokens,
            delta.usage.cache_read_input_tokens
        ),
        ApiStreamEvent::MessageStop(_) => "event=message_stop".to_string(),
    }
}

fn pending_tools_debug_summary(
    pending_tools: &BTreeMap<u32, (String, String, String)>,
    limit: usize,
) -> String {
    let indices = pending_tools
        .keys()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let entries = pending_tools
        .iter()
        .map(|(index, (id, name, input))| {
            format!(
                "index={index} id={id} name={name} bytes={} chars={} suffix={}",
                input.len(),
                input.chars().count(),
                json_debug_suffix(input, limit)
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    format!(
        "pending_tool_count={}\npending_tool_indices={}\npending_tool_entries={}",
        pending_tools.len(),
        json_debug_string(&indices, limit),
        json_debug_string(&entries, limit * pending_tools.len().max(1))
    )
}

fn json_debug_string(input: &str, limit: usize) -> String {
    let value = input.chars().take(limit).collect::<String>();
    serde_json::to_string(&value).unwrap_or_else(|_| "\"<unprintable>\"".to_string())
}

fn json_debug_suffix(input: &str, limit: usize) -> String {
    let mut suffix = input.chars().rev().take(limit).collect::<Vec<_>>();
    suffix.reverse();
    let value = suffix.into_iter().collect::<String>();
    serde_json::to_string(&value).unwrap_or_else(|_| "\"<unprintable>\"".to_string())
}

fn push_prompt_cache_record(client: &ApiProviderClient, events: &mut Vec<AssistantEvent>) {
    // `ApiProviderClient::take_last_prompt_cache_record` is a pass-through
    // to the Anthropic variant and returns `None` for OpenAI-compat /
    // xAI variants, which do not have a prompt cache. So this helper
    // remains a no-op on non-Anthropic providers without any extra
    // branching here.
    if let Some(record) = client.take_last_prompt_cache_record() {
        if let Some(event) = prompt_cache_record_to_runtime_event(record) {
            events.push(AssistantEvent::PromptCache(event));
        }
    }
}

fn prompt_cache_record_to_runtime_event(
    record: api::PromptCacheRecord,
) -> Option<PromptCacheEvent> {
    let cache_break = record.cache_break?;
    Some(PromptCacheEvent {
        unexpected: cache_break.unexpected,
        reason: cache_break.reason,
        reason_code: cache_break.reason_code,
        diagnostic_scope: cache_break.diagnostic_scope,
        changed_components: cache_break.changed_components,
        previous_cache_read_input_tokens: cache_break.previous_cache_read_input_tokens,
        current_cache_read_input_tokens: cache_break.current_cache_read_input_tokens,
        token_drop: cache_break.token_drop,
        elapsed_seconds: cache_break.elapsed_seconds,
    })
}

#[track_caller]
fn cli_agent_debug_log(event: &str, detail: impl AsRef<str>) {
    runtime::agent_debug_log(event, detail);
}

pub(crate) fn convert_messages(messages: &[ConversationMessage]) -> Vec<InputMessage> {
    let mut converted: Vec<InputMessage> = Vec::new();
    let mut previous_source_was_tool = false;

    for message in messages {
        let source_is_tool = message.role == MessageRole::Tool;
        let role = match message.role {
            MessageRole::System | MessageRole::User | MessageRole::Tool => "user",
            MessageRole::Assistant => "assistant",
        };
        let content = message
            .blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => InputContentBlock::Text {
                    text: text.clone(),
                    cache_control: None,
                },
                ContentBlock::ToolUse { id, name, input } => InputContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: serde_json::from_str(input)
                        .unwrap_or_else(|_| serde_json::json!({ "raw": input })),
                },
                ContentBlock::ToolResult {
                    tool_use_id,
                    tool_name,
                    output,
                    is_error,
                } => InputContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: vec![ToolResultContentBlock::Text {
                        text: render_tool_result_for_model(tool_name, output),
                    }],
                    is_error: *is_error,
                    cache_control: None,
                },
            })
            .collect::<Vec<_>>();
        if content.is_empty() {
            continue;
        }

        if source_is_tool && previous_source_was_tool {
            if let Some(last) = converted.last_mut() {
                if last.role == "user" {
                    last.content.extend(content);
                    previous_source_was_tool = true;
                    continue;
                }
            }
        }

        converted.push(InputMessage {
            role: role.to_string(),
            content,
        });
        previous_source_was_tool = source_is_tool;
    }

    converted
}

fn apply_tool_cache_controls(tools: &mut Option<Vec<ToolDefinition>>) {
    let Some(tool_list) = tools.as_mut() else {
        return;
    };
    if let Some(last_tool) = tool_list.last_mut() {
        last_tool.cache_control = Some(CacheControl::ephemeral());
    }
}
