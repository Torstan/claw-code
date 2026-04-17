use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

use serde_json::{Map, Value};
use telemetry::SessionTracer;

use crate::agent_debug::agent_debug_log;
use crate::compact::{estimate_session_tokens, CompactionConfig, CompactionResult};
use crate::compact_session_with_memory;
use crate::config::RuntimeFeatureConfig;
use crate::hooks::{HookAbortSignal, HookProgressReporter, HookRunResult, HookRunner};
use crate::micro_compact::{micro_compact_session, MicroCompactionConfig};
use crate::permissions::{
    PermissionContext, PermissionOutcome, PermissionPolicy, PermissionPrompter,
};
use crate::session::{ContentBlock, ConversationMessage, Session};
use crate::session_memory_compact::refresh_session_memory;
use crate::session_notifications::{drain_session_notifications, with_active_tool_session};
use crate::snip_compact::{snip_compact_session, SnipCompactionConfig};
use crate::usage::{TokenUsage, UsageTracker};

const DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD: u32 = 100_000;
const AUTO_COMPACTION_THRESHOLD_ENV_VAR: &str = "CLAUDE_CODE_AUTO_COMPACT_INPUT_TOKENS";

/// Fully assembled request payload sent to the upstream model client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiRequest {
    pub system_prompt: Vec<String>,
    pub messages: Vec<ConversationMessage>,
}

/// Streamed events emitted while processing a single assistant turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantEvent {
    TextDelta(String),
    ToolUse {
        id: String,
        name: String,
        input: String,
    },
    Usage(TokenUsage),
    PromptCache(PromptCacheEvent),
    MessageStop,
}

/// Prompt-cache telemetry captured from the provider response stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptCacheEvent {
    pub unexpected: bool,
    pub reason: String,
    pub previous_cache_read_input_tokens: u32,
    pub current_cache_read_input_tokens: u32,
    pub token_drop: u32,
}

/// Minimal streaming API contract required by [`ConversationRuntime`].
pub trait ApiClient {
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError>;
}

/// Trait implemented by tool dispatchers that execute model-requested tools.
pub trait ToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError>;

    fn execute_many(&mut self, invocations: &[ToolInvocation]) -> Vec<Result<String, ToolError>> {
        invocations
            .iter()
            .map(|invocation| self.execute(&invocation.tool_name, &invocation.input))
            .collect()
    }

    fn supports_parallel_execution(&self, _tool_name: &str) -> bool {
        false
    }
}

/// One tool call prepared for executor dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInvocation {
    pub tool_name: String,
    pub input: String,
}

/// Error returned when a tool invocation fails locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    message: String,
}

impl ToolError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for ToolError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ToolError {}

/// Error returned when a conversation turn cannot be completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    message: String,
}

impl RuntimeError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for RuntimeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RuntimeError {}

/// Summary of one completed runtime turn, including tool results and usage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSummary {
    pub assistant_messages: Vec<ConversationMessage>,
    pub tool_results: Vec<ConversationMessage>,
    pub prompt_cache_events: Vec<PromptCacheEvent>,
    pub iterations: usize,
    pub usage: TokenUsage,
    pub auto_compaction: Option<AutoCompactionEvent>,
}

/// Details about automatic session compaction applied during a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoCompactionEvent {
    pub removed_message_count: usize,
}

/// Coordinates the model loop, tool execution, hooks, and session updates.
pub struct ConversationRuntime<C, T> {
    session: Session,
    api_client: C,
    tool_executor: T,
    permission_policy: PermissionPolicy,
    system_prompt: Vec<String>,
    max_iterations: usize,
    usage_tracker: UsageTracker,
    hook_runner: HookRunner,
    auto_compaction_input_tokens_threshold: u32,
    hook_abort_signal: HookAbortSignal,
    hook_progress_reporter: Option<Box<dyn HookProgressReporter>>,
    session_tracer: Option<SessionTracer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedToolUse {
    tool_use_id: String,
    tool_name: String,
    effective_input: String,
    pre_hook_messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ToolDispatch {
    Immediate(ConversationMessage),
    Ready(PreparedToolUse),
}

impl<C, T> ConversationRuntime<C, T>
where
    C: ApiClient,
    T: ToolExecutor,
{
    #[must_use]
    pub fn new(
        session: Session,
        api_client: C,
        tool_executor: T,
        permission_policy: PermissionPolicy,
        system_prompt: Vec<String>,
    ) -> Self {
        Self::new_with_features(
            session,
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
            &RuntimeFeatureConfig::default(),
        )
    }

    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn new_with_features(
        session: Session,
        api_client: C,
        tool_executor: T,
        permission_policy: PermissionPolicy,
        system_prompt: Vec<String>,
        feature_config: &RuntimeFeatureConfig,
    ) -> Self {
        let usage_tracker = UsageTracker::from_session(&session);
        Self {
            session,
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
            max_iterations: usize::MAX,
            usage_tracker,
            hook_runner: HookRunner::from_feature_config(feature_config),
            auto_compaction_input_tokens_threshold: auto_compaction_threshold_from_env(),
            hook_abort_signal: HookAbortSignal::default(),
            hook_progress_reporter: None,
            session_tracer: None,
        }
    }

    #[must_use]
    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    #[must_use]
    pub fn with_auto_compaction_input_tokens_threshold(mut self, threshold: u32) -> Self {
        self.auto_compaction_input_tokens_threshold = threshold;
        self
    }

    #[must_use]
    pub fn with_hook_abort_signal(mut self, hook_abort_signal: HookAbortSignal) -> Self {
        self.hook_abort_signal = hook_abort_signal;
        self
    }

    #[must_use]
    pub fn with_hook_progress_reporter(
        mut self,
        hook_progress_reporter: Box<dyn HookProgressReporter>,
    ) -> Self {
        self.hook_progress_reporter = Some(hook_progress_reporter);
        self
    }

    #[must_use]
    pub fn with_session_tracer(mut self, session_tracer: SessionTracer) -> Self {
        self.session_tracer = Some(session_tracer);
        self
    }

    fn run_pre_tool_use_hook(&mut self, tool_name: &str, input: &str) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_pre_tool_use_with_context(
                tool_name,
                input,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_pre_tool_use_with_context(
                tool_name,
                input,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    fn run_post_tool_use_hook(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
        is_error: bool,
    ) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_post_tool_use_with_context(
                tool_name,
                input,
                output,
                is_error,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_post_tool_use_with_context(
                tool_name,
                input,
                output,
                is_error,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    fn run_post_tool_use_failure_hook(
        &mut self,
        tool_name: &str,
        input: &str,
        output: &str,
    ) -> HookRunResult {
        if let Some(reporter) = self.hook_progress_reporter.as_mut() {
            self.hook_runner.run_post_tool_use_failure_with_context(
                tool_name,
                input,
                output,
                Some(&self.hook_abort_signal),
                Some(reporter.as_mut()),
            )
        } else {
            self.hook_runner.run_post_tool_use_failure_with_context(
                tool_name,
                input,
                output,
                Some(&self.hook_abort_signal),
                None,
            )
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn run_turn(
        &mut self,
        user_input: impl Into<String>,
        prompter: Option<&mut dyn PermissionPrompter>,
    ) -> Result<TurnSummary, RuntimeError> {
        let user_input = user_input.into();
        self.run_turn_with_messages(
            user_input.clone(),
            vec![ConversationMessage::user_text(user_input)],
            prompter,
        )
    }

    #[allow(clippy::too_many_lines)]
    pub fn run_turn_with_messages(
        &mut self,
        user_input: impl Into<String>,
        initial_messages: Vec<ConversationMessage>,
        mut prompter: Option<&mut dyn PermissionPrompter>,
    ) -> Result<TurnSummary, RuntimeError> {
        let user_input = user_input.into();
        self.record_turn_started(&user_input);
        self.inject_pending_session_notifications()?;
        for message in initial_messages {
            self.session
                .push_message(message)
                .map_err(|error| RuntimeError::new(error.to_string()))?;
        }

        let mut assistant_messages = Vec::new();
        let mut tool_results = Vec::new();
        let mut prompt_cache_events = Vec::new();
        let mut iterations = 0;
        let mut auto_compaction: Option<AutoCompactionEvent> = None;

        loop {
            iterations += 1;
            if iterations > self.max_iterations {
                let error = RuntimeError::new(
                    "conversation loop exceeded the maximum number of iterations",
                );
                self.record_turn_failed(iterations, &error);
                return Err(error);
            }

            self.inject_pending_session_notifications()?;
            if iterations == 1 {
                if let Err(error) = self.maybe_snip_compact() {
                    self.record_turn_failed(iterations, &error);
                    return Err(error);
                }
                if let Err(error) = self.maybe_micro_compact() {
                    self.record_turn_failed(iterations, &error);
                    return Err(error);
                }
            }
            if let Some(event) = match self.maybe_auto_compact() {
                Ok(event) => event,
                Err(error) => {
                    self.record_turn_failed(iterations, &error);
                    return Err(error);
                }
            } {
                accumulate_auto_compaction_event(&mut auto_compaction, event.removed_message_count);
            }

            let events =
                match self.stream_with_reactive_compaction(iterations, &mut auto_compaction) {
                    Ok(events) => events,
                    Err(error) => {
                        self.record_turn_failed(iterations, &error);
                        return Err(error);
                    }
                };
            let (assistant_message, usage, turn_prompt_cache_events) =
                match build_assistant_message(events) {
                    Ok(result) => result,
                    Err(error) => {
                        self.record_turn_failed(iterations, &error);
                        return Err(error);
                    }
                };
            if let Some(usage) = usage {
                self.usage_tracker.record(usage);
            }
            prompt_cache_events.extend(turn_prompt_cache_events);
            let pending_tool_uses = assistant_message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            self.record_assistant_iteration(
                iterations,
                &assistant_message,
                pending_tool_uses.len(),
            );

            self.session
                .push_message(assistant_message.clone())
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            assistant_messages.push(assistant_message);

            if pending_tool_uses.is_empty() {
                break;
            }

            let mut planned_dispatches = Vec::new();
            for (tool_use_id, tool_name, input) in pending_tool_uses {
                planned_dispatches.push(self.plan_tool_dispatch(
                    tool_use_id,
                    tool_name,
                    input,
                    &mut prompter,
                ));
            }
            self.dispatch_tool_plan(iterations, planned_dispatches, &mut tool_results)?;
        }

        let summary = TurnSummary {
            assistant_messages,
            tool_results,
            prompt_cache_events,
            iterations,
            usage: self.usage_tracker.cumulative_usage(),
            auto_compaction,
        };
        let _ = refresh_session_memory(&self.session);
        self.record_turn_completed(&summary);

        Ok(summary)
    }

    #[must_use]
    pub fn compact(&self, config: CompactionConfig) -> CompactionResult {
        compact_session_with_memory(&self.session, config)
    }

    #[must_use]
    pub fn estimated_tokens(&self) -> usize {
        estimate_session_tokens(&self.session)
    }

    #[must_use]
    pub fn usage(&self) -> &UsageTracker {
        &self.usage_tracker
    }

    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn api_client_mut(&mut self) -> &mut C {
        &mut self.api_client
    }

    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    #[must_use]
    pub fn fork_session(&self, branch_name: Option<String>) -> Session {
        self.session.fork(branch_name)
    }

    #[must_use]
    pub fn into_session(self) -> Session {
        self.session
    }

    fn inject_pending_session_notifications(&mut self) -> Result<(), RuntimeError> {
        for message in drain_session_notifications(&self.session.session_id) {
            self.session
                .push_message(ConversationMessage::user_text(message))
                .map_err(|error| RuntimeError::new(error.to_string()))?;
        }
        Ok(())
    }

    fn plan_tool_dispatch(
        &mut self,
        tool_use_id: String,
        tool_name: String,
        input: String,
        prompter: &mut Option<&mut dyn PermissionPrompter>,
    ) -> ToolDispatch {
        let pre_hook_result = self.run_pre_tool_use_hook(&tool_name, &input);
        let effective_input = pre_hook_result
            .updated_input()
            .map_or_else(|| input.clone(), ToOwned::to_owned);
        let permission_context = PermissionContext::new(
            pre_hook_result.permission_override(),
            pre_hook_result.permission_reason().map(ToOwned::to_owned),
        );

        let permission_outcome = if pre_hook_result.is_cancelled() {
            PermissionOutcome::Deny {
                reason: format_hook_message(
                    &pre_hook_result,
                    &format!("PreToolUse hook cancelled tool `{tool_name}`"),
                ),
            }
        } else if pre_hook_result.is_failed() {
            PermissionOutcome::Deny {
                reason: format_hook_message(
                    &pre_hook_result,
                    &format!("PreToolUse hook failed for tool `{tool_name}`"),
                ),
            }
        } else if pre_hook_result.is_denied() {
            PermissionOutcome::Deny {
                reason: format_hook_message(
                    &pre_hook_result,
                    &format!("PreToolUse hook denied tool `{tool_name}`"),
                ),
            }
        } else if let Some(prompt) = prompter.as_deref_mut() {
            self.permission_policy.authorize_with_context(
                &tool_name,
                &effective_input,
                &permission_context,
                Some(prompt),
            )
        } else {
            self.permission_policy.authorize_with_context(
                &tool_name,
                &effective_input,
                &permission_context,
                None,
            )
        };

        match permission_outcome {
            PermissionOutcome::Allow => ToolDispatch::Ready(PreparedToolUse {
                tool_use_id,
                tool_name,
                effective_input,
                pre_hook_messages: pre_hook_result.messages().to_vec(),
            }),
            PermissionOutcome::Deny { reason } => {
                ToolDispatch::Immediate(ConversationMessage::tool_result(
                    tool_use_id,
                    tool_name,
                    merge_hook_feedback(pre_hook_result.messages(), reason, true),
                    true,
                ))
            }
        }
    }

    fn dispatch_tool_plan(
        &mut self,
        iteration: usize,
        planned_dispatches: Vec<ToolDispatch>,
        tool_results: &mut Vec<ConversationMessage>,
    ) -> Result<(), RuntimeError> {
        let mut dispatches = std::collections::VecDeque::from(planned_dispatches);

        while let Some(dispatch) = dispatches.pop_front() {
            match dispatch {
                ToolDispatch::Immediate(message) => {
                    self.push_tool_result_message(iteration, message, tool_results)?
                }
                ToolDispatch::Ready(prepared) => {
                    if self
                        .tool_executor
                        .supports_parallel_execution(&prepared.tool_name)
                    {
                        let mut batch = vec![prepared];
                        while dispatches.front().is_some_and(|next| match next {
                            ToolDispatch::Ready(next_prepared) => self
                                .tool_executor
                                .supports_parallel_execution(&next_prepared.tool_name),
                            ToolDispatch::Immediate(_) => false,
                        }) {
                            let ToolDispatch::Ready(next_prepared) = dispatches
                                .pop_front()
                                .expect("front item should still be present")
                            else {
                                unreachable!("front readiness already checked");
                            };
                            batch.push(next_prepared);
                        }

                        if batch.len() > 1 {
                            self.execute_prepared_batch(iteration, batch, tool_results)?;
                        } else {
                            self.execute_prepared_tool(
                                iteration,
                                batch.pop().expect("single-item batch"),
                                tool_results,
                            )?;
                        }
                    } else {
                        self.execute_prepared_tool(iteration, prepared, tool_results)?;
                    }
                }
            }
        }

        Ok(())
    }

    fn execute_prepared_batch(
        &mut self,
        iteration: usize,
        batch: Vec<PreparedToolUse>,
        tool_results: &mut Vec<ConversationMessage>,
    ) -> Result<(), RuntimeError> {
        for prepared in &batch {
            self.record_tool_started(iteration, &prepared.tool_name);
        }

        let invocations = batch
            .iter()
            .map(|prepared| ToolInvocation {
                tool_name: prepared.tool_name.clone(),
                input: prepared.effective_input.clone(),
            })
            .collect::<Vec<_>>();
        let results = with_active_tool_session(Some(self.session.session_id.as_str()), || {
            self.tool_executor.execute_many(&invocations)
        });

        for (prepared, result) in batch.into_iter().zip(results) {
            let result_message = self.finalize_prepared_tool_result(prepared, result);
            self.push_tool_result_message(iteration, result_message, tool_results)?;
        }

        Ok(())
    }

    fn execute_prepared_tool(
        &mut self,
        iteration: usize,
        prepared: PreparedToolUse,
        tool_results: &mut Vec<ConversationMessage>,
    ) -> Result<(), RuntimeError> {
        self.record_tool_started(iteration, &prepared.tool_name);
        let result = with_active_tool_session(Some(self.session.session_id.as_str()), || {
            self.tool_executor
                .execute(&prepared.tool_name, &prepared.effective_input)
        });
        let result_message = self.finalize_prepared_tool_result(prepared, result);
        self.push_tool_result_message(iteration, result_message, tool_results)
    }

    fn finalize_prepared_tool_result(
        &mut self,
        prepared: PreparedToolUse,
        result: Result<String, ToolError>,
    ) -> ConversationMessage {
        let (mut output, mut is_error) = match result {
            Ok(output) => (output, false),
            Err(error) => (error.to_string(), true),
        };
        output = merge_hook_feedback(&prepared.pre_hook_messages, output, false);

        let post_hook_result = if is_error {
            self.run_post_tool_use_failure_hook(
                &prepared.tool_name,
                &prepared.effective_input,
                &output,
            )
        } else {
            self.run_post_tool_use_hook(
                &prepared.tool_name,
                &prepared.effective_input,
                &output,
                false,
            )
        };
        if post_hook_result.is_denied()
            || post_hook_result.is_failed()
            || post_hook_result.is_cancelled()
        {
            is_error = true;
        }
        output = merge_hook_feedback(
            post_hook_result.messages(),
            output,
            post_hook_result.is_denied()
                || post_hook_result.is_failed()
                || post_hook_result.is_cancelled(),
        );

        ConversationMessage::tool_result(prepared.tool_use_id, prepared.tool_name, output, is_error)
    }

    fn push_tool_result_message(
        &mut self,
        iteration: usize,
        result_message: ConversationMessage,
        tool_results: &mut Vec<ConversationMessage>,
    ) -> Result<(), RuntimeError> {
        self.session
            .push_message(result_message.clone())
            .map_err(|error| RuntimeError::new(error.to_string()))?;
        self.record_tool_finished(iteration, &result_message);
        tool_results.push(result_message);
        Ok(())
    }

    fn maybe_snip_compact(&mut self) -> Result<(), RuntimeError> {
        let config = SnipCompactionConfig::for_auto_compaction_threshold(
            self.auto_compaction_input_tokens_threshold as usize,
        );
        let result = snip_compact_session(&self.session, config);
        if result.snipped_message_ids.is_empty() {
            return Ok(());
        }

        self.replace_session_and_persist(result.compacted_session)
    }

    fn maybe_micro_compact(&mut self) -> Result<(), RuntimeError> {
        let result = micro_compact_session(&self.session, MicroCompactionConfig::default());
        if result.cleared_tool_result_count == 0 {
            return Ok(());
        }
        self.replace_session_and_persist(result.compacted_session)
    }

    fn maybe_auto_compact(&mut self) -> Result<Option<AutoCompactionEvent>, RuntimeError> {
        if estimate_session_tokens(&self.session)
            < self.auto_compaction_input_tokens_threshold as usize
        {
            return Ok(None);
        }

        let config = CompactionConfig {
            max_estimated_tokens: self.auto_compaction_input_tokens_threshold as usize,
            ..CompactionConfig::default()
        };
        let result = compact_session_with_memory(&self.session, config);

        if result.removed_message_count == 0 {
            return Ok(None);
        }

        self.replace_session_and_persist(result.compacted_session)?;
        Ok(Some(AutoCompactionEvent {
            removed_message_count: result.removed_message_count,
        }))
    }

    fn stream_with_reactive_compaction(
        &mut self,
        iteration: usize,
        auto_compaction: &mut Option<AutoCompactionEvent>,
    ) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let mut reactive_retry_attempted = false;

        loop {
            let request = ApiRequest {
                system_prompt: self.system_prompt.clone(),
                messages: self.session.messages.clone(),
            };
            agent_debug_log(
                "llm.request",
                format!(
                    "session_id={}\niteration={iteration}\nsystem_prompt_parts={}\nmessage_count={}\nrequest={request:#?}",
                    self.session.session_id,
                    request.system_prompt.len(),
                    request.messages.len()
                ),
            );

            match self.api_client.stream(request) {
                Ok(events) => {
                    agent_debug_log(
                        "llm.response",
                        format!(
                            "session_id={}\niteration={iteration}\nevent_count={}\nresponse={events:#?}",
                            self.session.session_id,
                            events.len()
                        ),
                    );
                    return Ok(events);
                }
                Err(error) => {
                    agent_debug_log(
                        "llm.response.error",
                        format!(
                            "session_id={}\niteration={iteration}\nreactive_retry_attempted={reactive_retry_attempted}\nerror={error}",
                            self.session.session_id
                        ),
                    );

                    if reactive_retry_attempted || !is_context_window_runtime_error(&error) {
                        return Err(error);
                    }

                    let recovered = self.maybe_reactive_compact(auto_compaction)?;
                    if !recovered {
                        return Err(error);
                    }
                    reactive_retry_attempted = true;
                }
            }
        }
    }

    fn maybe_reactive_compact(
        &mut self,
        auto_compaction: &mut Option<AutoCompactionEvent>,
    ) -> Result<bool, RuntimeError> {
        let mut changed = false;

        if self.force_snip_compact()? {
            changed = true;
        }
        if self.force_micro_compact()? {
            changed = true;
        }

        if !changed
            || estimate_session_tokens(&self.session)
                > self.auto_compaction_input_tokens_threshold as usize
        {
            let result = compact_session_with_memory(
                &self.session,
                CompactionConfig {
                    max_estimated_tokens: 1,
                    ..CompactionConfig::default()
                },
            );
            if result.removed_message_count > 0 {
                self.replace_session_and_persist(result.compacted_session)?;
                accumulate_auto_compaction_event(auto_compaction, result.removed_message_count);
                changed = true;
            }
        }

        Ok(changed)
    }

    fn force_snip_compact(&mut self) -> Result<bool, RuntimeError> {
        let result = snip_compact_session(
            &self.session,
            SnipCompactionConfig {
                trigger_threshold_tokens: 1,
                target_tokens: 0,
                protected_recent_messages: 0,
                min_candidate_tokens: 20,
            },
        );
        if result.snipped_message_ids.is_empty() {
            return Ok(false);
        }

        self.replace_session_and_persist(result.compacted_session)?;
        Ok(true)
    }

    fn force_micro_compact(&mut self) -> Result<bool, RuntimeError> {
        let result = micro_compact_session(
            &self.session,
            MicroCompactionConfig {
                trigger_count: 0,
                keep_recent: 0,
                gap_threshold_minutes: u64::MAX,
            },
        );
        if result.cleared_tool_result_count == 0 {
            return Ok(false);
        }

        self.replace_session_and_persist(result.compacted_session)?;
        Ok(true)
    }

    fn replace_session_and_persist(&mut self, session: Session) -> Result<(), RuntimeError> {
        if let Some(path) = session.persistence_path().map(|path| path.to_path_buf()) {
            session
                .save_to_path(&path)
                .map_err(|error| RuntimeError::new(error.to_string()))?;
        }
        self.session = session;
        Ok(())
    }

    fn record_turn_started(&self, user_input: &str) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert(
            "user_input".to_string(),
            Value::String(user_input.to_string()),
        );
        session_tracer.record("turn_started", attributes);
    }

    fn record_assistant_iteration(
        &self,
        iteration: usize,
        assistant_message: &ConversationMessage,
        pending_tool_use_count: usize,
    ) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert(
            "assistant_blocks".to_string(),
            Value::from(assistant_message.blocks.len() as u64),
        );
        attributes.insert(
            "pending_tool_use_count".to_string(),
            Value::from(pending_tool_use_count as u64),
        );
        session_tracer.record("assistant_iteration_completed", attributes);
    }

    fn record_tool_started(&self, iteration: usize, tool_name: &str) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert(
            "tool_name".to_string(),
            Value::String(tool_name.to_string()),
        );
        session_tracer.record("tool_execution_started", attributes);
    }

    fn record_tool_finished(&self, iteration: usize, result_message: &ConversationMessage) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let Some(ContentBlock::ToolResult {
            tool_name,
            is_error,
            ..
        }) = result_message.blocks.first()
        else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert("tool_name".to_string(), Value::String(tool_name.clone()));
        attributes.insert("is_error".to_string(), Value::Bool(*is_error));
        session_tracer.record("tool_execution_finished", attributes);
    }

    fn record_turn_completed(&self, summary: &TurnSummary) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert(
            "iterations".to_string(),
            Value::from(summary.iterations as u64),
        );
        attributes.insert(
            "assistant_messages".to_string(),
            Value::from(summary.assistant_messages.len() as u64),
        );
        attributes.insert(
            "tool_results".to_string(),
            Value::from(summary.tool_results.len() as u64),
        );
        attributes.insert(
            "prompt_cache_events".to_string(),
            Value::from(summary.prompt_cache_events.len() as u64),
        );
        session_tracer.record("turn_completed", attributes);
    }

    fn record_turn_failed(&self, iteration: usize, error: &RuntimeError) {
        let Some(session_tracer) = &self.session_tracer else {
            return;
        };

        let mut attributes = Map::new();
        attributes.insert("iteration".to_string(), Value::from(iteration as u64));
        attributes.insert("error".to_string(), Value::String(error.to_string()));
        session_tracer.record("turn_failed", attributes);
    }
}

/// Reads the automatic compaction threshold from the environment.
#[must_use]
pub fn auto_compaction_threshold_from_env() -> u32 {
    parse_auto_compaction_threshold(
        std::env::var(AUTO_COMPACTION_THRESHOLD_ENV_VAR)
            .ok()
            .as_deref(),
    )
}

#[must_use]
fn parse_auto_compaction_threshold(value: Option<&str>) -> u32 {
    value
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|threshold| *threshold > 0)
        .unwrap_or(DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD)
}

fn is_context_window_runtime_error(error: &RuntimeError) -> bool {
    let message = error.to_string().to_lowercase();
    [
        "maximum context length",
        "context window",
        "context length",
        "too many tokens",
        "prompt is too long",
        "input is too long",
        "request is too large",
        "context_window_blocked",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

fn accumulate_auto_compaction_event(
    auto_compaction: &mut Option<AutoCompactionEvent>,
    removed_message_count: usize,
) {
    if let Some(existing) = auto_compaction.as_mut() {
        existing.removed_message_count += removed_message_count;
    } else {
        *auto_compaction = Some(AutoCompactionEvent {
            removed_message_count,
        });
    }
}

fn build_assistant_message(
    events: Vec<AssistantEvent>,
) -> Result<
    (
        ConversationMessage,
        Option<TokenUsage>,
        Vec<PromptCacheEvent>,
    ),
    RuntimeError,
> {
    let mut text = String::new();
    let mut blocks = Vec::new();
    let mut prompt_cache_events = Vec::new();
    let mut finished = false;
    let mut usage = None;

    for event in events {
        match event {
            AssistantEvent::TextDelta(delta) => text.push_str(&delta),
            AssistantEvent::ToolUse { id, name, input } => {
                flush_text_block(&mut text, &mut blocks);
                blocks.push(ContentBlock::ToolUse { id, name, input });
            }
            AssistantEvent::Usage(value) => usage = Some(value),
            AssistantEvent::PromptCache(event) => prompt_cache_events.push(event),
            AssistantEvent::MessageStop => {
                finished = true;
            }
        }
    }

    flush_text_block(&mut text, &mut blocks);

    if !finished {
        return Err(RuntimeError::new(
            "assistant stream ended without a message stop event",
        ));
    }
    if blocks.is_empty() {
        return Err(RuntimeError::new("assistant stream produced no content"));
    }

    Ok((
        ConversationMessage::assistant_with_usage(blocks, usage),
        usage,
        prompt_cache_events,
    ))
}

fn flush_text_block(text: &mut String, blocks: &mut Vec<ContentBlock>) {
    if !text.is_empty() {
        blocks.push(ContentBlock::Text {
            text: std::mem::take(text),
        });
    }
}

fn format_hook_message(result: &HookRunResult, fallback: &str) -> String {
    if result.messages().is_empty() {
        fallback.to_string()
    } else {
        result.messages().join("\n")
    }
}

fn merge_hook_feedback(messages: &[String], output: String, is_error: bool) -> String {
    if messages.is_empty() {
        return output;
    }

    let mut sections = Vec::new();
    if !output.trim().is_empty() {
        sections.push(output);
    }
    let label = if is_error {
        "Hook feedback (error)"
    } else {
        "Hook feedback"
    };
    sections.push(format!("{label}:\n{}", messages.join("\n")));
    sections.join("\n\n")
}

type ToolHandler = Box<dyn FnMut(&str) -> Result<String, ToolError>>;

/// Simple in-memory tool executor for tests and lightweight integrations.
#[derive(Default)]
pub struct StaticToolExecutor {
    handlers: BTreeMap<String, ToolHandler>,
}

impl StaticToolExecutor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn register(
        mut self,
        tool_name: impl Into<String>,
        handler: impl FnMut(&str) -> Result<String, ToolError> + 'static,
    ) -> Self {
        self.handlers.insert(tool_name.into(), Box::new(handler));
        self
    }
}

impl ToolExecutor for StaticToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        self.handlers
            .get_mut(tool_name)
            .ok_or_else(|| ToolError::new(format!("unknown tool: {tool_name}")))?(input)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_assistant_message, parse_auto_compaction_threshold, ApiClient, ApiRequest,
        AssistantEvent, AutoCompactionEvent, ConversationRuntime, PromptCacheEvent, RuntimeError,
        StaticToolExecutor, ToolExecutor, ToolInvocation,
        DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD,
    };
    use crate::compact::CompactionConfig;
    use crate::config::{RuntimeFeatureConfig, RuntimeHookConfig};
    use crate::micro_compact::{
        micro_compact_session, MicroCompactionConfig, MICROCOMPACT_CLEARED_SENTINEL,
    };
    use crate::permissions::{
        PermissionMode, PermissionPolicy, PermissionPromptDecision, PermissionPrompter,
        PermissionRequest,
    };
    use crate::prompt::{ProjectContext, SystemPromptBuilder};
    use crate::session::{CompactionMarkerKind, ContentBlock, MessageRole, Session};
    use crate::session_memory_compact::{refresh_session_memory, session_memory_path};
    use crate::session_notifications::{active_tool_session_id, enqueue_session_notification};
    use crate::snip_compact::{
        SNIP_CLEARED_ASSISTANT_TEXT_SENTINEL, SNIP_CLEARED_TOOL_RESULT_SENTINEL,
    };
    use crate::usage::TokenUsage;
    use crate::ToolError;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};
    use telemetry::{MemoryTelemetrySink, SessionTracer, TelemetryEvent};

    struct ScriptedApiClient {
        call_count: usize,
    }

    impl ApiClient for ScriptedApiClient {
        fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
            self.call_count += 1;
            match self.call_count {
                1 => {
                    assert!(request
                        .messages
                        .iter()
                        .any(|message| message.role == MessageRole::User));
                    Ok(vec![
                        AssistantEvent::TextDelta("Let me calculate that.".to_string()),
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "add".to_string(),
                            input: "2,2".to_string(),
                        },
                        AssistantEvent::Usage(TokenUsage {
                            input_tokens: 20,
                            output_tokens: 6,
                            cache_creation_input_tokens: 1,
                            cache_read_input_tokens: 2,
                        }),
                        AssistantEvent::MessageStop,
                    ])
                }
                2 => {
                    let last_message = request
                        .messages
                        .last()
                        .expect("tool result should be present");
                    assert_eq!(last_message.role, MessageRole::Tool);
                    Ok(vec![
                        AssistantEvent::TextDelta("The answer is 4.".to_string()),
                        AssistantEvent::Usage(TokenUsage {
                            input_tokens: 24,
                            output_tokens: 4,
                            cache_creation_input_tokens: 1,
                            cache_read_input_tokens: 3,
                        }),
                        AssistantEvent::PromptCache(PromptCacheEvent {
                            unexpected: true,
                            reason:
                                "cache read tokens dropped while prompt fingerprint remained stable"
                                    .to_string(),
                            previous_cache_read_input_tokens: 6_000,
                            current_cache_read_input_tokens: 1_000,
                            token_drop: 5_000,
                        }),
                        AssistantEvent::MessageStop,
                    ])
                }
                _ => unreachable!("extra API call"),
            }
        }
    }

    struct BatchAwareToolExecutor {
        batched: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl ToolExecutor for BatchAwareToolExecutor {
        fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
            Ok(format!("{tool_name}:{input}"))
        }

        fn execute_many(
            &mut self,
            invocations: &[ToolInvocation],
        ) -> Vec<Result<String, ToolError>> {
            self.batched
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(
                    invocations
                        .iter()
                        .map(|invocation| invocation.tool_name.clone())
                        .collect(),
                );
            invocations
                .iter()
                .map(|invocation| Ok(format!("{}:{}", invocation.tool_name, invocation.input)))
                .collect()
        }

        fn supports_parallel_execution(&self, tool_name: &str) -> bool {
            tool_name == "Agent"
        }
    }

    struct PromptAllowOnce;

    impl PermissionPrompter for PromptAllowOnce {
        fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
            assert_eq!(request.tool_name, "add");
            PermissionPromptDecision::Allow
        }
    }

    #[test]
    fn runs_user_to_tool_to_result_loop_end_to_end_and_tracks_usage() {
        let api_client = ScriptedApiClient { call_count: 0 };
        let tool_executor = StaticToolExecutor::new().register("add", |input| {
            let total = input
                .split(',')
                .map(|part| part.parse::<i32>().expect("input must be valid integer"))
                .sum::<i32>();
            Ok(total.to_string())
        });
        let permission_policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite);
        let system_prompt = SystemPromptBuilder::new()
            .with_project_context(ProjectContext {
                cwd: PathBuf::from("/tmp/project"),
                current_date: "2026-03-31".to_string(),
                git_status: None,
                git_diff: None,
                git_context: None,
                instruction_files: Vec::new(),
            })
            .with_os("linux", "6.8")
            .build();
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            api_client,
            tool_executor,
            permission_policy,
            system_prompt,
        );

        let summary = runtime
            .run_turn("what is 2 + 2?", Some(&mut PromptAllowOnce))
            .expect("conversation loop should succeed");

        assert_eq!(summary.iterations, 2);
        assert_eq!(summary.assistant_messages.len(), 2);
        assert_eq!(summary.tool_results.len(), 1);
        assert_eq!(summary.prompt_cache_events.len(), 1);
        assert_eq!(runtime.session().messages.len(), 4);
        assert_eq!(summary.usage.output_tokens, 10);
        assert_eq!(summary.auto_compaction, None);
        assert!(matches!(
            runtime.session().messages[1].blocks[1],
            ContentBlock::ToolUse { .. }
        ));
        assert!(matches!(
            runtime.session().messages[2].blocks[0],
            ContentBlock::ToolResult {
                is_error: false,
                ..
            }
        ));
    }

    #[test]
    fn batches_parallel_safe_tool_calls_in_one_iteration() {
        struct ParallelApiClient {
            calls: usize,
        }

        impl ApiClient for ParallelApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Ok(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "Agent".to_string(),
                            input: r#"{"prompt":"one"}"#.to_string(),
                        },
                        AssistantEvent::ToolUse {
                            id: "tool-2".to_string(),
                            name: "Agent".to_string(),
                            input: r#"{"prompt":"two"}"#.to_string(),
                        },
                        AssistantEvent::MessageStop,
                    ]),
                    2 => {
                        let tool_messages = request
                            .messages
                            .iter()
                            .filter(|message| message.role == MessageRole::Tool)
                            .collect::<Vec<_>>();
                        assert_eq!(tool_messages.len(), 2);
                        Ok(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("extra API call"),
                }
            }
        }

        let batched = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let tool_executor = BatchAwareToolExecutor {
            batched: Arc::clone(&batched),
        };
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            ParallelApiClient { calls: 0 },
            tool_executor,
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("launch both", None)
            .expect("parallel batch should succeed");

        assert_eq!(summary.tool_results.len(), 2);
        assert_eq!(
            batched
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            &[vec!["Agent".to_string(), "Agent".to_string()]]
        );
    }

    #[test]
    fn injects_pending_session_notifications_before_first_request() {
        struct NotificationAwareApiClient;

        impl ApiClient for NotificationAwareApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                assert_eq!(request.messages.len(), 2);
                assert_eq!(request.messages[0].role, MessageRole::User);
                assert_eq!(request.messages[1].role, MessageRole::User);
                let ContentBlock::Text { text } = &request.messages[0].blocks[0] else {
                    panic!("expected notification text block");
                };
                assert!(text.contains("Background agent finished."));
                Ok(vec![
                    AssistantEvent::TextDelta("acknowledged".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut session = Session::new();
        session.session_id = "notif-start".to_string();
        enqueue_session_notification("notif-start", "Background agent finished.");

        let mut runtime = ConversationRuntime::new(
            session,
            NotificationAwareApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        runtime
            .run_turn("hello", None)
            .expect("notification injection should succeed");
    }

    #[test]
    fn starts_turn_from_prebuilt_messages() {
        struct PrebuiltMessageApiClient;

        impl ApiClient for PrebuiltMessageApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                assert_eq!(request.messages.len(), 2);
                assert_eq!(request.messages[0].role, MessageRole::User);
                assert_eq!(request.messages[1].role, MessageRole::User);

                let ContentBlock::Text { text: metadata } = &request.messages[0].blocks[0] else {
                    panic!("expected slash command metadata");
                };
                assert_eq!(
                    metadata,
                    "<command-message>simplify</command-message>\n<command-name>/simplify</command-name>"
                );

                let ContentBlock::Text { text: prompt } = &request.messages[1].blocks[0] else {
                    panic!("expected slash command prompt");
                };
                assert_eq!(prompt, "# Simplify");

                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            PrebuiltMessageApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        runtime
            .run_turn_with_messages(
                "/simplify",
                vec![
                    crate::session::ConversationMessage::user_text(
                        "<command-message>simplify</command-message>\n<command-name>/simplify</command-name>",
                    ),
                    crate::session::ConversationMessage::user_text("# Simplify"),
                ],
                None,
            )
            .expect("prebuilt message turn should succeed");

        assert_eq!(runtime.session().messages.len(), 3);
    }

    #[test]
    fn injects_notifications_between_tool_iterations() {
        struct TwoStepApiClient {
            calls: usize,
        }

        impl ApiClient for TwoStepApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Ok(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "notify".to_string(),
                            input: "{}".to_string(),
                        },
                        AssistantEvent::MessageStop,
                    ]),
                    2 => {
                        let last_message = request.messages.last().expect("notification present");
                        assert_eq!(last_message.role, MessageRole::User);
                        let ContentBlock::Text { text } = &last_message.blocks[0] else {
                            panic!("expected notification text block");
                        };
                        assert!(text.contains("async review done"));
                        Ok(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("extra API call"),
                }
            }
        }

        let mut session = Session::new();
        session.session_id = "notif-iter".to_string();
        let executor = StaticToolExecutor::new().register("notify", |_input| {
            let session_id = active_tool_session_id()
                .expect("tool execution should have active session context");
            enqueue_session_notification(session_id, "async review done");
            Ok("launched".to_string())
        });
        let mut runtime = ConversationRuntime::new(
            session,
            TwoStepApiClient { calls: 0 },
            executor,
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("launch notification", None)
            .expect("notification loop should succeed");

        assert_eq!(summary.iterations, 2);
    }

    #[test]
    fn records_runtime_session_trace_events() {
        let sink = Arc::new(MemoryTelemetrySink::default());
        let tracer = SessionTracer::new("session-runtime", sink.clone());
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            ScriptedApiClient { call_count: 0 },
            StaticToolExecutor::new().register("add", |_input| Ok("4".to_string())),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            vec!["system".to_string()],
        )
        .with_session_tracer(tracer);

        runtime
            .run_turn("what is 2 + 2?", Some(&mut PromptAllowOnce))
            .expect("conversation loop should succeed");

        let events = sink.events();
        let trace_names = events
            .iter()
            .filter_map(|event| match event {
                TelemetryEvent::SessionTrace(trace) => Some(trace.name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(trace_names.contains(&"turn_started"));
        assert!(trace_names.contains(&"assistant_iteration_completed"));
        assert!(trace_names.contains(&"tool_execution_started"));
        assert!(trace_names.contains(&"tool_execution_finished"));
        assert!(trace_names.contains(&"turn_completed"));
    }

    #[test]
    fn records_denied_tool_results_when_prompt_rejects() {
        struct RejectPrompter;
        impl PermissionPrompter for RejectPrompter {
            fn decide(&mut self, _request: &PermissionRequest) -> PermissionPromptDecision {
                PermissionPromptDecision::Deny {
                    reason: "not now".to_string(),
                }
            }
        }

        struct SingleCallApiClient;
        impl ApiClient for SingleCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("I could not use the tool.".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: "secret".to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::WorkspaceWrite),
            vec!["system".to_string()],
        );

        let summary = runtime
            .run_turn("use the tool", Some(&mut RejectPrompter))
            .expect("conversation should continue after denied tool");

        assert_eq!(summary.tool_results.len(), 1);
        assert!(matches!(
            &summary.tool_results[0].blocks[0],
            ContentBlock::ToolResult { is_error: true, output, .. } if output == "not now"
        ));
    }

    #[test]
    fn denies_tool_use_when_pre_tool_hook_blocks() {
        struct SingleCallApiClient;
        impl ApiClient for SingleCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("blocked".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: r#"{"path":"secret.txt"}"#.to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new().register("blocked", |_input| {
                panic!("tool should not execute when hook denies")
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'blocked by hook'; exit 2")],
                Vec::new(),
                Vec::new(),
            )),
        );

        let summary = runtime
            .run_turn("use the tool", None)
            .expect("conversation should continue after hook denial");

        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "hook denial should produce an error result: {output}"
        );
        assert!(
            output.contains("denied tool") || output.contains("blocked by hook"),
            "unexpected hook denial output: {output:?}"
        );
    }

    #[test]
    fn denies_tool_use_when_pre_tool_hook_fails() {
        struct SingleCallApiClient;
        impl ApiClient for SingleCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                if request
                    .messages
                    .iter()
                    .any(|message| message.role == MessageRole::Tool)
                {
                    return Ok(vec![
                        AssistantEvent::TextDelta("failed".to_string()),
                        AssistantEvent::MessageStop,
                    ]);
                }
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "blocked".to_string(),
                        input: r#"{"path":"secret.txt"}"#.to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        // given
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            SingleCallApiClient,
            StaticToolExecutor::new().register("blocked", |_input| {
                panic!("tool should not execute when hook fails")
            }),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'broken hook'; exit 1")],
                Vec::new(),
                Vec::new(),
            )),
        );

        // when
        let summary = runtime
            .run_turn("use the tool", None)
            .expect("conversation should continue after hook failure");

        // then
        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "hook failure should produce an error result: {output}"
        );
        assert!(
            output.contains("exited with status 1") || output.contains("broken hook"),
            "unexpected hook failure output: {output:?}"
        );
    }

    #[test]
    fn appends_post_tool_hook_feedback_to_tool_result() {
        struct TwoCallApiClient {
            calls: usize,
        }

        impl ApiClient for TwoCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Ok(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "add".to_string(),
                            input: r#"{"lhs":2,"rhs":2}"#.to_string(),
                        },
                        AssistantEvent::MessageStop,
                    ]),
                    2 => {
                        assert!(request
                            .messages
                            .iter()
                            .any(|message| message.role == MessageRole::Tool));
                        Ok(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("extra API call"),
                }
            }
        }

        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            TwoCallApiClient { calls: 0 },
            StaticToolExecutor::new().register("add", |_input| Ok("4".to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                vec![shell_snippet("printf 'pre hook ran'")],
                vec![shell_snippet("printf 'post hook ran'")],
                Vec::new(),
            )),
        );

        let summary = runtime
            .run_turn("use add", None)
            .expect("tool loop succeeds");

        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            !*is_error,
            "post hook should preserve non-error result: {output:?}"
        );
        assert!(
            output.contains('4'),
            "tool output missing value: {output:?}"
        );
        assert!(
            output.contains("pre hook ran"),
            "tool output missing pre hook feedback: {output:?}"
        );
        assert!(
            output.contains("post hook ran"),
            "tool output missing post hook feedback: {output:?}"
        );
    }

    #[test]
    fn appends_post_tool_use_failure_hook_feedback_to_tool_result() {
        struct TwoCallApiClient {
            calls: usize,
        }

        impl ApiClient for TwoCallApiClient {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Ok(vec![
                        AssistantEvent::ToolUse {
                            id: "tool-1".to_string(),
                            name: "fail".to_string(),
                            input: r#"{"path":"README.md"}"#.to_string(),
                        },
                        AssistantEvent::MessageStop,
                    ]),
                    2 => {
                        assert!(request
                            .messages
                            .iter()
                            .any(|message| message.role == MessageRole::Tool));
                        Ok(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("extra API call"),
                }
            }
        }

        // given
        let mut runtime = ConversationRuntime::new_with_features(
            Session::new(),
            TwoCallApiClient { calls: 0 },
            StaticToolExecutor::new()
                .register("fail", |_input| Err(ToolError::new("tool exploded"))),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
            &RuntimeFeatureConfig::default().with_hooks(RuntimeHookConfig::new(
                Vec::new(),
                vec![shell_snippet("printf 'post hook should not run'")],
                vec![shell_snippet("printf 'failure hook ran'")],
            )),
        );

        // when
        let summary = runtime
            .run_turn("use fail", None)
            .expect("tool loop succeeds");

        // then
        assert_eq!(summary.tool_results.len(), 1);
        let ContentBlock::ToolResult {
            is_error, output, ..
        } = &summary.tool_results[0].blocks[0]
        else {
            panic!("expected tool result block");
        };
        assert!(
            *is_error,
            "failure hook path should preserve error result: {output:?}"
        );
        assert!(
            output.contains("tool exploded"),
            "tool output missing failure reason: {output:?}"
        );
        assert!(
            output.contains("failure hook ran"),
            "tool output missing failure hook feedback: {output:?}"
        );
        assert!(
            !output.contains("post hook should not run"),
            "normal post hook should not run on tool failure: {output:?}"
        );
    }

    #[test]
    fn reconstructs_usage_tracker_from_restored_session() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut session = Session::new();
        session
            .messages
            .push(crate::session::ConversationMessage::assistant_with_usage(
                vec![ContentBlock::Text {
                    text: "earlier".to_string(),
                }],
                Some(TokenUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    cache_creation_input_tokens: 2,
                    cache_read_input_tokens: 1,
                }),
            ));

        let runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        assert_eq!(runtime.usage().turns(), 1);
        assert_eq!(runtime.usage().cumulative_usage().total_tokens(), 21);
    }

    #[test]
    fn compacts_session_after_turns() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );
        runtime.run_turn("a", None).expect("turn a");
        runtime.run_turn("b", None).expect("turn b");
        runtime.run_turn("c", None).expect("turn c");

        let result = runtime.compact(CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        });
        assert!(result.summary.contains("Conversation summary"));
        assert_eq!(
            result.compacted_session.messages[0].role,
            MessageRole::System
        );
        assert_eq!(
            result.compacted_session.session_id,
            runtime.session().session_id
        );
        assert!(result.compacted_session.compaction.is_some());
    }

    #[test]
    fn compact_prefers_session_memory_sidecar_when_available() {
        struct UnusedApi;
        impl ApiClient for UnusedApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                panic!("manual compact should not call the API client");
            }
        }

        let path = temp_session_path("manual-session-memory");
        let mut session = Session::new().with_persistence_path(path.clone());
        session.messages = vec![
            crate::session::ConversationMessage::user_text(
                "Ship the compaction pipeline in phases",
            ),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "I will start with rust/crates/runtime/src/conversation.rs".to_string(),
            }]),
            crate::session::ConversationMessage::user_text("Then wire in session memory"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "Next I will update rust/crates/runtime/src/session.rs".to_string(),
            }]),
        ];
        let memory_path =
            refresh_session_memory(&session).expect("session memory sidecar should be written");

        let runtime = ConversationRuntime::new(
            session,
            UnusedApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let result = runtime.compact(CompactionConfig {
            preserve_recent_messages: 2,
            max_estimated_tokens: 1,
        });

        let ContentBlock::Text { text } = &result.compacted_session.messages[0].blocks[0] else {
            panic!("compacted system message should contain text");
        };
        assert!(result.summary.contains("# Goals"));
        assert!(text.contains("# Goals"));
        assert!(text.contains("Recent prompts"));

        fs::remove_file(&path).ok();
        fs::remove_file(&memory_path).ok();
    }

    #[test]
    fn persists_conversation_turn_messages_to_jsonl_session() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let path = temp_session_path("persisted-turn");
        let session = Session::new().with_persistence_path(path.clone());
        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        runtime
            .run_turn("persist this turn", None)
            .expect("turn should succeed");

        let restored = Session::load_from_path(&path).expect("persisted session should reload");
        fs::remove_file(&path).expect("temp session file should be removable");

        assert_eq!(restored.messages.len(), 2);
        assert_eq!(restored.messages[0].role, MessageRole::User);
        assert_eq!(restored.messages[1].role, MessageRole::Assistant);
        assert_eq!(restored.session_id, runtime.session().session_id);
    }

    #[test]
    fn refreshes_session_memory_sidecar_after_successful_turn() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let path = temp_session_path("memory-after-turn");
        let session = Session::new().with_persistence_path(path.clone());
        let mut runtime = ConversationRuntime::new(
            session,
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        runtime
            .run_turn("refresh memory", None)
            .expect("turn should succeed");

        let memory_path = session_memory_path(runtime.session())
            .expect("persisted session should derive a memory sidecar path");
        let contents =
            fs::read_to_string(&memory_path).expect("session memory sidecar should be readable");

        assert!(contents.contains("# Goals"));
        assert!(contents.contains("# Recent prompts"));
        assert!(contents.contains("refresh memory"));
        assert!(contents.contains("done"));

        fs::remove_file(&path).ok();
        fs::remove_file(&memory_path).ok();
    }

    #[test]
    fn forks_runtime_session_without_mutating_original() {
        let mut session = Session::new();
        session
            .push_user_text("branch me")
            .expect("message should append");

        let runtime = ConversationRuntime::new(
            session.clone(),
            ScriptedApiClient { call_count: 0 },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let forked = runtime.fork_session(Some("alt-path".to_string()));

        assert_eq!(forked.messages, session.messages);
        assert_ne!(forked.session_id, session.session_id);
        assert_eq!(
            forked
                .fork
                .as_ref()
                .map(|fork| (fork.parent_session_id.as_str(), fork.branch_name.as_deref())),
            Some((session.session_id.as_str(), Some("alt-path")))
        );
        assert!(runtime.session().fork.is_none());
    }

    fn temp_session_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("runtime-conversation-{label}-{nanos}.json"))
    }

    fn tool_use_message(
        id: &str,
        tool_name: &str,
        input: &str,
    ) -> crate::session::ConversationMessage {
        crate::session::ConversationMessage::assistant(vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: tool_name.to_string(),
            input: input.to_string(),
        }])
    }

    fn tool_result_outputs(session: &Session) -> Vec<String> {
        session
            .messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                ContentBlock::ToolResult { output, .. } => Some(output.clone()),
                ContentBlock::Text { .. } | ContentBlock::ToolUse { .. } => None,
            })
            .collect()
    }

    #[test]
    fn snip_compact_runs_before_micro_and_auto_compaction() {
        struct SnipAwareApi;

        impl ApiClient for SnipAwareApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                assert!(request.messages.iter().any(|message| {
                    message.compaction_meta.as_ref().map(|meta| meta.kind)
                        == Some(CompactionMarkerKind::SnipBoundary)
                }));
                assert!(
                    !request.messages.iter().any(|message| {
                        message.compaction_meta.as_ref().map(|meta| meta.kind)
                            == Some(CompactionMarkerKind::MicrocompactBoundary)
                    }),
                    "snip should run early enough to avoid redundant micro compaction for already-snipped tool results",
                );
                assert!(
                    !request.messages.iter().any(|message| {
                        message.compaction_meta.as_ref().map(|meta| meta.kind)
                            == Some(CompactionMarkerKind::CompactBoundary)
                    }),
                    "snip should reduce the request before auto compact escalates to a full summary",
                );

                let tool_outputs = request
                    .messages
                    .iter()
                    .flat_map(|message| message.blocks.iter())
                    .filter_map(|block| match block {
                        ContentBlock::ToolResult { output, .. } => Some(output.clone()),
                        ContentBlock::Text { .. } | ContentBlock::ToolUse { .. } => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    tool_outputs,
                    vec![
                        SNIP_CLEARED_TOOL_RESULT_SENTINEL.to_string(),
                        SNIP_CLEARED_TOOL_RESULT_SENTINEL.to_string(),
                        SNIP_CLEARED_TOOL_RESULT_SENTINEL.to_string(),
                        SNIP_CLEARED_TOOL_RESULT_SENTINEL.to_string(),
                        SNIP_CLEARED_TOOL_RESULT_SENTINEL.to_string(),
                        "tool output 6 ".repeat(30),
                        "tool output 7 ".repeat(30),
                    ]
                );
                assert!(request.messages.iter().any(|message| {
                    message.role == MessageRole::Assistant
                        && message.blocks.iter().any(|block| {
                            matches!(
                                block,
                                ContentBlock::Text { text }
                                    if text == SNIP_CLEARED_ASSISTANT_TEXT_SENTINEL
                            )
                        })
                }));

                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut session = Session::new();
        session.messages = vec![crate::session::ConversationMessage::assistant(vec![
            ContentBlock::Text {
                text: "old assistant context ".repeat(40),
            },
        ])];
        for index in 1..=7 {
            session.messages.push(tool_use_message(
                &format!("tool-{index}"),
                "bash",
                &index.to_string(),
            ));
            session
                .messages
                .push(crate::session::ConversationMessage::tool_result(
                    format!("tool-{index}"),
                    "bash",
                    format!("tool output {index} ").repeat(30),
                    false,
                ));
        }

        let mut runtime = ConversationRuntime::new(
            session,
            SnipAwareApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(400);

        let summary = runtime
            .run_turn("continue", None)
            .expect("snip-first request should succeed");

        assert_eq!(summary.auto_compaction, None);
    }

    #[test]
    fn micro_compact_does_not_clear_current_turn_tool_results_before_post_tool_request() {
        struct PostToolApi {
            calls: usize,
        }

        impl ApiClient for PostToolApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => {
                        let mut events = (1..=7)
                            .map(|index| AssistantEvent::ToolUse {
                                id: format!("tool-{index}"),
                                name: "bash".to_string(),
                                input: index.to_string(),
                            })
                            .collect::<Vec<_>>();
                        events.push(AssistantEvent::MessageStop);
                        Ok(events)
                    }
                    2 => {
                        let outputs = request
                            .messages
                            .iter()
                            .flat_map(|message| message.blocks.iter())
                            .filter_map(|block| match block {
                                ContentBlock::ToolResult { output, .. } => Some(output.clone()),
                                ContentBlock::Text { .. } | ContentBlock::ToolUse { .. } => None,
                            })
                            .collect::<Vec<_>>();
                        assert_eq!(
                            outputs,
                            (1..=7)
                                .map(|index| format!("tool output {index}"))
                                .collect::<Vec<_>>(),
                            "microcompact must not clear tool results produced in the current turn before the model consumes them",
                        );
                        assert!(
                            !request.messages.iter().any(|message| {
                                message.compaction_meta.as_ref().map(|meta| meta.kind)
                                    == Some(CompactionMarkerKind::MicrocompactBoundary)
                            }),
                            "current-turn post-tool follow-up should not receive a microcompact marker",
                        );
                        Ok(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("extra API call"),
                }
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            PostToolApi { calls: 0 },
            StaticToolExecutor::new().register("bash", |input| Ok(format!("tool output {input}"))),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        runtime
            .run_turn("continue", None)
            .expect("post-tool follow-up should succeed");
    }

    #[test]
    fn micro_compact_persists_rewritten_snapshot_even_when_request_fails() {
        struct FailingApi;

        impl ApiClient for FailingApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Err(RuntimeError::new("upstream failed"))
            }
        }

        let path = temp_session_path("microcompact-failed-turn");
        let mut session = Session::new().with_persistence_path(path.clone());
        session.messages = (1..=7)
            .flat_map(|index| {
                [
                    tool_use_message(&format!("tool-{index}"), "bash", &index.to_string()),
                    crate::session::ConversationMessage::tool_result(
                        format!("tool-{index}"),
                        "bash",
                        format!("tool output {index}"),
                        false,
                    ),
                ]
            })
            .collect();
        session
            .save_to_path(&path)
            .expect("seed session should persist");

        let mut runtime = ConversationRuntime::new(
            session,
            FailingApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        let error = runtime
            .run_turn("continue", None)
            .expect_err("API failure should propagate");
        assert_eq!(error.to_string(), "upstream failed");

        let persisted_session =
            Session::load_from_path(&path).expect("persisted session should reload");
        assert_eq!(
            persisted_session.messages,
            runtime.session().messages,
            "failed turns must still persist the rewritten compacted snapshot",
        );
        assert_eq!(
            persisted_session.updated_at_ms,
            runtime.session().updated_at_ms
        );
        assert_eq!(
            tool_result_outputs(&persisted_session),
            vec![
                MICROCOMPACT_CLEARED_SENTINEL.to_string(),
                MICROCOMPACT_CLEARED_SENTINEL.to_string(),
                MICROCOMPACT_CLEARED_SENTINEL.to_string(),
                MICROCOMPACT_CLEARED_SENTINEL.to_string(),
                MICROCOMPACT_CLEARED_SENTINEL.to_string(),
                "tool output 6".to_string(),
                "tool output 7".to_string(),
            ]
        );
        assert!(persisted_session.messages.iter().any(|message| {
            message.compaction_meta.as_ref().map(|meta| meta.kind)
                == Some(CompactionMarkerKind::MicrocompactBoundary)
        }));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn reactive_compact_retries_once_on_context_window_exceeded() {
        struct ReactiveSnipApi {
            calls: usize,
        }

        impl ApiClient for ReactiveSnipApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Err(RuntimeError::new(
                        "Request is too large for this model's context window.",
                    )),
                    2 => {
                        assert!(request.messages.iter().any(|message| {
                            message.compaction_meta.as_ref().map(|meta| meta.kind)
                                == Some(CompactionMarkerKind::SnipBoundary)
                        }));
                        assert!(!request.messages.iter().any(|message| {
                            message.compaction_meta.as_ref().map(|meta| meta.kind)
                                == Some(CompactionMarkerKind::CompactBoundary)
                        }));
                        assert!(request.messages.iter().any(|message| {
                            message.blocks.iter().any(|block| {
                                matches!(
                                    block,
                                    ContentBlock::Text { text }
                                        if text == SNIP_CLEARED_ASSISTANT_TEXT_SENTINEL
                                )
                            })
                        }));
                        let tool_outputs = request
                            .messages
                            .iter()
                            .flat_map(|message| message.blocks.iter())
                            .filter_map(|block| match block {
                                ContentBlock::ToolResult { output, .. } => Some(output.clone()),
                                ContentBlock::Text { .. } | ContentBlock::ToolUse { .. } => None,
                            })
                            .collect::<Vec<_>>();
                        assert_eq!(
                            tool_outputs,
                            vec![
                                SNIP_CLEARED_TOOL_RESULT_SENTINEL.to_string(),
                                SNIP_CLEARED_TOOL_RESULT_SENTINEL.to_string(),
                            ]
                        );
                        Ok(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("reactive retry should issue exactly one retry"),
                }
            }
        }

        let mut session = Session::new();
        session.messages = vec![
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "old assistant context ".repeat(40),
            }]),
            tool_use_message("tool-1", "bash", "printf 1"),
            crate::session::ConversationMessage::tool_result(
                "tool-1",
                "bash",
                "old tool output 1 ".repeat(40),
                false,
            ),
            tool_use_message("tool-2", "bash", "printf 2"),
            crate::session::ConversationMessage::tool_result(
                "tool-2",
                "bash",
                "old tool output 2 ".repeat(40),
                false,
            ),
        ];

        let mut runtime = ConversationRuntime::new(
            session,
            ReactiveSnipApi { calls: 0 },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let summary = runtime
            .run_turn("continue", None)
            .expect("reactive snip retry should succeed");

        assert_eq!(summary.auto_compaction, None);
        assert!(runtime.session().messages.iter().any(|message| {
            message.compaction_meta.as_ref().map(|meta| meta.kind)
                == Some(CompactionMarkerKind::SnipBoundary)
        }));
    }

    #[test]
    fn reactive_compact_falls_back_to_full_compact_when_light_recovery_cannot_help() {
        struct ReactiveFullCompactApi {
            calls: usize,
        }

        impl ApiClient for ReactiveFullCompactApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                match self.calls {
                    1 => Err(RuntimeError::new(
                        "maximum context length exceeded for this request",
                    )),
                    2 => {
                        assert_eq!(
                            request.messages.first().map(|message| message.role),
                            Some(MessageRole::System)
                        );
                        assert!(request.messages.iter().any(|message| {
                            message.compaction_meta.as_ref().map(|meta| meta.kind)
                                == Some(CompactionMarkerKind::CompactBoundary)
                        }));
                        let ContentBlock::Text { text } = &request.messages[0].blocks[0] else {
                            panic!("reactive full compact should synthesize a summary message");
                        };
                        assert!(text.contains("Conversation summary:"));
                        Ok(vec![
                            AssistantEvent::TextDelta("done".to_string()),
                            AssistantEvent::MessageStop,
                        ])
                    }
                    _ => unreachable!("reactive retry should issue exactly one retry"),
                }
            }
        }

        let mut session = Session::new();
        session.messages = vec![
            crate::session::ConversationMessage::user_text("one"),
            crate::session::ConversationMessage::user_text("two"),
            crate::session::ConversationMessage::user_text("three"),
            crate::session::ConversationMessage::user_text("four"),
            crate::session::ConversationMessage::user_text("five"),
        ];

        let mut runtime = ConversationRuntime::new(
            session,
            ReactiveFullCompactApi { calls: 0 },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let summary = runtime
            .run_turn("continue", None)
            .expect("reactive fallback compact should succeed");

        assert_eq!(
            summary.auto_compaction,
            Some(AutoCompactionEvent {
                removed_message_count: 2,
            })
        );
    }

    #[test]
    fn reactive_compact_stops_after_a_single_retry_when_context_window_error_persists() {
        struct PersistentContextWindowApi {
            calls: usize,
        }

        impl ApiClient for PersistentContextWindowApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                self.calls += 1;
                Err(RuntimeError::new(
                    "request is too large for this model's context window",
                ))
            }
        }

        let mut session = Session::new();
        session.messages = vec![
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "old assistant context ".repeat(40),
            }]),
            tool_use_message("tool-1", "bash", "printf 1"),
            crate::session::ConversationMessage::tool_result(
                "tool-1",
                "bash",
                "old tool output 1 ".repeat(40),
                false,
            ),
        ];

        let mut runtime = ConversationRuntime::new(
            session,
            PersistentContextWindowApi { calls: 0 },
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let error = runtime
            .run_turn("continue", None)
            .expect_err("persistent context-window failures should stop after one retry");

        assert_eq!(
            error.to_string(),
            "request is too large for this model's context window"
        );
        assert_eq!(runtime.api_client_mut().calls, 2);
    }

    #[cfg(windows)]
    fn shell_snippet(script: &str) -> String {
        script.replace('\'', "\"")
    }

    #[cfg(not(windows))]
    fn shell_snippet(script: &str) -> String {
        script.to_string()
    }

    #[test]
    fn auto_compacts_before_request_when_estimated_tokens_cross_threshold() {
        struct PreRequestCompactingApi;
        impl ApiClient for PreRequestCompactingApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                assert_eq!(
                    request.messages.first().map(|message| message.role),
                    Some(MessageRole::System),
                    "request should already be compacted before the API call"
                );
                assert!(
                    request.messages.iter().any(|message| {
                        message.role == MessageRole::User
                            && message.blocks.iter().any(|block| {
                                matches!(
                                    block,
                                    ContentBlock::Text { text } if text == "trigger"
                                )
                            })
                    }),
                    "current user prompt must remain in the request after compaction"
                );
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut session = Session::new();
        session.messages = vec![
            crate::session::ConversationMessage::user_text("one"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two".to_string(),
            }]),
            crate::session::ConversationMessage::user_text("three"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "four".to_string(),
            }]),
        ];

        let mut runtime = ConversationRuntime::new(
            session,
            PreRequestCompactingApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(1);

        let summary = runtime
            .run_turn("trigger", None)
            .expect("turn should succeed");

        assert_eq!(
            summary.auto_compaction,
            Some(AutoCompactionEvent {
                removed_message_count: 1,
            })
        );
        assert_eq!(runtime.session().messages[0].role, MessageRole::System);
    }

    #[test]
    fn auto_compaction_prefers_session_memory_sidecar_when_available() {
        struct MemoryAwareApi;
        impl ApiClient for MemoryAwareApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                let ContentBlock::Text { text } = &request.messages[0].blocks[0] else {
                    panic!("compacted request should start with a text system message");
                };
                assert_eq!(request.messages[0].role, MessageRole::System);
                assert!(text.contains("# Goals"));
                assert!(text.contains("Recent prompts"));
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let path = temp_session_path("auto-session-memory");
        let mut session = Session::new().with_persistence_path(path.clone());
        session.messages = vec![
            crate::session::ConversationMessage::user_text(
                "Ship the compaction pipeline in phases",
            ),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "I will start with rust/crates/runtime/src/conversation.rs".to_string(),
            }]),
            crate::session::ConversationMessage::user_text("Then wire in session memory"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "Next I will update rust/crates/runtime/src/session.rs".to_string(),
            }]),
        ];
        let memory_path =
            refresh_session_memory(&session).expect("session memory sidecar should be written");

        let mut runtime = ConversationRuntime::new(
            session,
            MemoryAwareApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(1);

        let summary = runtime
            .run_turn("trigger", None)
            .expect("turn should succeed");

        assert_eq!(
            summary.auto_compaction,
            Some(AutoCompactionEvent {
                removed_message_count: 1,
            })
        );

        fs::remove_file(&path).ok();
        fs::remove_file(&memory_path).ok();
    }

    #[test]
    fn auto_compaction_falls_back_to_full_compact_when_sidecar_is_empty() {
        struct FallbackApi;
        impl ApiClient for FallbackApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                let ContentBlock::Text { text } = &request.messages[0].blocks[0] else {
                    panic!("compacted request should start with a text system message");
                };
                assert_eq!(request.messages[0].role, MessageRole::System);
                assert!(text.contains("Conversation summary:"));
                assert!(!text.contains("# Goals"));
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let path = temp_session_path("auto-session-memory-fallback");
        let mut session = Session::new().with_persistence_path(path.clone());
        session.messages = vec![
            crate::session::ConversationMessage::user_text("one"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "two".to_string(),
            }]),
            crate::session::ConversationMessage::user_text("three"),
            crate::session::ConversationMessage::assistant(vec![ContentBlock::Text {
                text: "four".to_string(),
            }]),
        ];
        let memory_path =
            session_memory_path(&session).expect("persisted session should have a sidecar path");
        fs::write(&memory_path, " \n").expect("empty sidecar should write");

        let mut runtime = ConversationRuntime::new(
            session,
            FallbackApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(1);

        let summary = runtime
            .run_turn("trigger", None)
            .expect("turn should succeed");

        assert_eq!(
            summary.auto_compaction,
            Some(AutoCompactionEvent {
                removed_message_count: 1,
            })
        );

        fs::remove_file(&path).ok();
        fs::remove_file(&memory_path).ok();
    }

    #[test]
    fn skips_auto_compaction_below_threshold() {
        struct SimpleApi;
        impl ApiClient for SimpleApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::Usage(TokenUsage {
                        input_tokens: 99_999,
                        output_tokens: 4,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let mut runtime = ConversationRuntime::new(
            Session::new(),
            SimpleApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_auto_compaction_input_tokens_threshold(100_000);

        let summary = runtime
            .run_turn("trigger", None)
            .expect("turn should succeed");
        assert_eq!(summary.auto_compaction, None);
        assert_eq!(runtime.session().messages.len(), 2);
    }

    #[test]
    fn auto_compaction_threshold_defaults_and_parses_values() {
        assert_eq!(
            parse_auto_compaction_threshold(None),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
        assert_eq!(parse_auto_compaction_threshold(Some("4321")), 4321);
        assert_eq!(
            parse_auto_compaction_threshold(Some("0")),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
        assert_eq!(
            parse_auto_compaction_threshold(Some("not-a-number")),
            DEFAULT_AUTO_COMPACTION_INPUT_TOKENS_THRESHOLD
        );
    }

    #[test]
    fn micro_compact_clears_old_tool_results_but_preserves_recent_tail() {
        let mut session = Session::new();
        session.messages = vec![
            tool_use_message("tool-1", "bash", "printf 'one'"),
            crate::session::ConversationMessage::tool_result(
                "tool-1",
                "bash",
                "old bash output 1",
                false,
            ),
            tool_use_message("tool-2", "grep_search", r#"{"pattern":"needle"}"#),
            crate::session::ConversationMessage::tool_result(
                "tool-2",
                "grep_search",
                "old grep output 2",
                false,
            ),
            tool_use_message("tool-3", "read_file", r#"{"path":"src/main.rs"}"#),
            crate::session::ConversationMessage::tool_result(
                "tool-3",
                "read_file",
                "recent read output 3",
                false,
            ),
        ];

        let result = micro_compact_session(
            &session,
            MicroCompactionConfig {
                trigger_count: 2,
                keep_recent: 1,
                gap_threshold_minutes: u64::MAX,
            },
        );

        assert_eq!(result.cleared_tool_result_count, 2);
        assert!(result.estimated_tokens_freed > 0);
        assert_eq!(
            tool_result_outputs(&result.compacted_session),
            vec![
                MICROCOMPACT_CLEARED_SENTINEL.to_string(),
                MICROCOMPACT_CLEARED_SENTINEL.to_string(),
                "recent read output 3".to_string(),
            ]
        );
        assert!(
            result.compacted_session.messages.iter().any(|message| {
                message.compaction_meta.as_ref().map(|meta| meta.kind)
                    == Some(CompactionMarkerKind::MicrocompactBoundary)
            }),
            "microcompact should append a boundary marker",
        );
    }

    #[test]
    fn micro_compact_time_based_trigger_uses_last_assistant_timestamp() {
        struct TimeBasedMicrocompactApi;

        impl ApiClient for TimeBasedMicrocompactApi {
            fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
                let outputs = request
                    .messages
                    .iter()
                    .flat_map(|message| message.blocks.iter())
                    .filter_map(|block| match block {
                        ContentBlock::ToolResult { output, .. } => Some(output.clone()),
                        ContentBlock::Text { .. } | ContentBlock::ToolUse { .. } => None,
                    })
                    .collect::<Vec<_>>();

                assert_eq!(
                    outputs,
                    vec![
                        MICROCOMPACT_CLEARED_SENTINEL.to_string(),
                        "recent tool output 2".to_string(),
                        "recent tool output 3".to_string(),
                    ],
                    "time-based microcompact should use the last assistant timestamp, not the last tool result timestamp",
                );
                assert!(request.messages.iter().any(|message| {
                    message.compaction_meta.as_ref().map(|meta| meta.kind)
                        == Some(CompactionMarkerKind::MicrocompactBoundary)
                }));
                assert!(request.messages.iter().any(|message| {
                    message.role == MessageRole::User
                        && message.blocks.iter().any(|block| {
                            matches!(
                                block,
                                ContentBlock::Text { text } if text == "keep going"
                            )
                        })
                }));

                Ok(vec![
                    AssistantEvent::TextDelta("done".to_string()),
                    AssistantEvent::MessageStop,
                ])
            }
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_millis() as u64;
        let stale_ms = now_ms.saturating_sub(2 * 60 * 60 * 1000);

        let mut session = Session::new();
        session.messages = vec![
            crate::session::ConversationMessage::user_text("inspect tools"),
            tool_use_message("tool-1", "bash", "printf 'one'"),
            crate::session::ConversationMessage::tool_result(
                "tool-1",
                "bash",
                "old tool output 1",
                false,
            ),
            tool_use_message("tool-2", "bash", "printf 'two'"),
            crate::session::ConversationMessage::tool_result(
                "tool-2",
                "bash",
                "recent tool output 2",
                false,
            ),
            tool_use_message("tool-3", "bash", "printf 'three'"),
            crate::session::ConversationMessage::tool_result(
                "tool-3",
                "bash",
                "recent tool output 3",
                false,
            ),
        ];

        for message in &mut session.messages {
            match message.role {
                MessageRole::Assistant => {
                    message.timestamp_ms = stale_ms;
                }
                MessageRole::Tool
                    if matches!(
                        message.blocks.first(),
                        Some(ContentBlock::ToolResult { tool_use_id, .. }) if tool_use_id == "tool-1"
                    ) =>
                {
                    message.timestamp_ms = stale_ms;
                }
                MessageRole::Tool => {
                    message.timestamp_ms = now_ms;
                }
                MessageRole::System | MessageRole::User => {}
            }
        }

        let mut runtime = ConversationRuntime::new(
            session,
            TimeBasedMicrocompactApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        runtime
            .run_turn("keep going", None)
            .expect("turn should succeed");
    }

    #[test]
    fn build_assistant_message_requires_message_stop_event() {
        // given
        let events = vec![AssistantEvent::TextDelta("hello".to_string())];

        // when
        let error = build_assistant_message(events)
            .expect_err("assistant messages should require a stop event");

        // then
        assert!(error
            .to_string()
            .contains("assistant stream ended without a message stop event"));
    }

    #[test]
    fn build_assistant_message_requires_content() {
        // given
        let events = vec![AssistantEvent::MessageStop];

        // when
        let error =
            build_assistant_message(events).expect_err("assistant messages should require content");

        // then
        assert!(error
            .to_string()
            .contains("assistant stream produced no content"));
    }

    #[test]
    fn static_tool_executor_rejects_unknown_tools() {
        // given
        let mut executor = StaticToolExecutor::new();

        // when
        let error = executor
            .execute("missing", "{}")
            .expect_err("unregistered tools should fail");

        // then
        assert_eq!(error.to_string(), "unknown tool: missing");
    }

    #[test]
    fn run_turn_errors_when_max_iterations_is_exceeded() {
        struct LoopingApi;

        impl ApiClient for LoopingApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Ok(vec![
                    AssistantEvent::ToolUse {
                        id: "tool-1".to_string(),
                        name: "echo".to_string(),
                        input: "payload".to_string(),
                    },
                    AssistantEvent::MessageStop,
                ])
            }
        }

        // given
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            LoopingApi,
            StaticToolExecutor::new().register("echo", |input| Ok(input.to_string())),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        )
        .with_max_iterations(1);

        // when
        let error = runtime
            .run_turn("loop", None)
            .expect_err("conversation loop should stop after the configured limit");

        // then
        assert!(error
            .to_string()
            .contains("conversation loop exceeded the maximum number of iterations"));
    }

    #[test]
    fn run_turn_propagates_api_errors() {
        struct FailingApi;

        impl ApiClient for FailingApi {
            fn stream(
                &mut self,
                _request: ApiRequest,
            ) -> Result<Vec<AssistantEvent>, RuntimeError> {
                Err(RuntimeError::new("upstream failed"))
            }
        }

        // given
        let mut runtime = ConversationRuntime::new(
            Session::new(),
            FailingApi,
            StaticToolExecutor::new(),
            PermissionPolicy::new(PermissionMode::DangerFullAccess),
            vec!["system".to_string()],
        );

        // when
        let error = runtime
            .run_turn("hello", None)
            .expect_err("API failures should propagate");

        // then
        assert_eq!(error.to_string(), "upstream failed");
    }
}
