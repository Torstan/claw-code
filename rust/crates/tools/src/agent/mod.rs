use super::{
    active_tool_session_id, agent_debug_log, build_system_blocks_with_cache_controls,
    canonical_tool_token, compress_summary_text, dedupe_superseded_commit_events,
    detect_provider_kind, enqueue_session_notification, execute_tool_with_enforcer,
    global_task_registry, is_background_task_tool_name, load_system_prompt,
    log_prompt_cache_block_diagnostics, max_tokens_for_model, mvp_tool_specs, read_base_url,
    render_tool_result_for_model, resolve_model_alias, resolve_startup_auth_source,
    summarize_prompt_cache_controls, AgentInput, AgentJob, AgentOutput, AgentToolOutput,
    AnthropicClient, ApiClient, ApiError, ApiRequest, ApiStreamEvent, AssistantEvent,
    AsyncAgentLaunchOutput, AuthSource, BTreeMap, BTreeSet, CacheControl, Command, ConfigLoader,
    ContentBlock, ContentBlockDelta, ConversationMessage, ConversationRuntime, InputContentBlock,
    InputMessage, LaneCommitProvenance, LaneEvent, LaneEventBlocker, LaneFailureClass,
    MessageRequest, MessageResponse, MessageRole, OAuthConfig, OutputContentBlock, Path,
    PermissionEnforcer, PermissionMode, PermissionPolicy, PromptCache, PromptCacheEvent,
    ProviderClient, ProviderFallbackConfig, ProviderKind, RuntimeError, Session, TaskStatus,
    ToolChoice, ToolDefinition, ToolError, ToolExecutor, ToolResultContentBlock, ToolSpec,
    SYSTEM_PROMPT_DYNAMIC_BOUNDARY,
};
use std::time::Instant;

const DEFAULT_AGENT_MODEL: &str = "claude-opus-4-6";
const FALLBACK_AGENT_SYSTEM_DATE: &str = "2026-03-31";
const DEFAULT_AGENT_MAX_ITERATIONS: usize = 32;

pub(crate) fn execute_agent(input: AgentInput) -> Result<AgentToolOutput, String> {
    agent_debug_log(
        "agent.execute.begin",
        format!(
            "description={:?} subagent_type={:?} name={:?} background={} prompt_len={}",
            input.description,
            input.subagent_type,
            input.name,
            input.run_in_background.unwrap_or(false),
            input.prompt.len()
        ),
    );
    if input.run_in_background.unwrap_or(false) {
        execute_agent_with_mode(input, spawn_agent_job)
    } else {
        execute_agent_with_mode(input, |job| run_agent_job(&job))
    }
}

pub(crate) fn execute_agent_with_mode<F>(
    input: AgentInput,
    spawn_fn: F,
) -> Result<AgentToolOutput, String>
where
    F: FnOnce(AgentJob) -> Result<(), String>,
{
    let started_at = Instant::now();
    let background = input.run_in_background.unwrap_or(false);
    let prompt = input.prompt.clone();
    let manifest = execute_agent_with_spawn(input, spawn_fn)?;
    agent_debug_log(
        "agent.execute.spawned",
        format!(
            "agent_id={} description={:?} background={} elapsed_ms={}",
            manifest.agent_id,
            manifest.description,
            background,
            started_at.elapsed().as_millis()
        ),
    );
    if background {
        Ok(AgentToolOutput::AsyncLaunched(
            build_async_agent_launch_output(&manifest, &prompt),
        ))
    } else {
        Ok(AgentToolOutput::Completed(manifest))
    }
}

pub(crate) fn execute_agent_with_spawn<F>(
    input: AgentInput,
    spawn_fn: F,
) -> Result<AgentOutput, String>
where
    F: FnOnce(AgentJob) -> Result<(), String>,
{
    if input.description.trim().is_empty() {
        return Err(String::from("description must not be empty"));
    }
    if input.prompt.trim().is_empty() {
        return Err(String::from("prompt must not be empty"));
    }

    let agent_id = make_agent_id();
    let output_dir = agent_store_dir()?;
    std::fs::create_dir_all(&output_dir).map_err(|error| error.to_string())?;
    let output_file = output_dir.join(format!("{agent_id}.md"));
    let manifest_file = output_dir.join(format!("{agent_id}.json"));
    let normalized_subagent_type = normalize_subagent_type(input.subagent_type.as_deref());
    let model = resolve_agent_model(input.model.as_deref());
    let agent_name = input
        .name
        .as_deref()
        .map(slugify_agent_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| slugify_agent_name(&input.description));
    let created_at = iso8601_now();
    let system_prompt = build_agent_system_prompt(&normalized_subagent_type)?;
    let allowed_tools = allowed_tools_for_subagent(&normalized_subagent_type);
    agent_debug_log(
        "agent.spawn.prepare",
        format!(
            "agent_id={} description={:?} subagent_type={} model={} allowed_tools={} prompt_len={}",
            agent_id,
            input.description,
            normalized_subagent_type,
            model,
            allowed_tools.len(),
            input.prompt.len()
        ),
    );

    let output_contents = format!(
        "# Agent Task

- id: {}
- name: {}
- description: {}
- subagent_type: {}
- created_at: {}

## Prompt

{}
",
        agent_id, agent_name, input.description, normalized_subagent_type, created_at, input.prompt
    );
    std::fs::write(&output_file, output_contents).map_err(|error| error.to_string())?;

    let manifest = AgentOutput {
        agent_id,
        name: agent_name,
        description: input.description,
        subagent_type: Some(normalized_subagent_type),
        model: Some(model),
        status: String::from("running"),
        output_file: output_file.display().to_string(),
        manifest_file: manifest_file.display().to_string(),
        created_at: created_at.clone(),
        started_at: Some(created_at),
        completed_at: None,
        lane_events: vec![LaneEvent::started(iso8601_now())],
        current_blocker: None,
        derived_state: String::from("working"),
        error: None,
        result: None,
    };
    write_agent_manifest(&manifest)?;
    agent_debug_log(
        "agent.spawn.manifest_written",
        format!(
            "agent_id={} manifest_file={} output_file={}",
            manifest.agent_id, manifest.manifest_file, manifest.output_file
        ),
    );

    let manifest_for_spawn = manifest.clone();
    let job = AgentJob {
        manifest: manifest_for_spawn,
        prompt: input.prompt,
        system_prompt,
        allowed_tools,
        parent_session_id: active_tool_session_id(),
    };
    if let Err(error) = spawn_fn(job) {
        let error = format!("failed to spawn sub-agent: {error}");
        agent_debug_log(
            "agent.spawn.error",
            format!("agent_id={} error={error}", manifest.agent_id),
        );
        persist_agent_terminal_state(&manifest, "failed", None, Some(error.clone()))?;
        return Err(error);
    }
    agent_debug_log(
        "agent.spawn.dispatched",
        format!(
            "agent_id={} background_status={}",
            manifest.agent_id, manifest.status
        ),
    );

    // If spawn_fn ran synchronously (e.g. run_agent_job inline), the manifest
    // on disk has been updated to its terminal state by persist_agent_terminal_state.
    // Read it back so the caller (and the model) sees the completed result.
    // Falls back to the initial "running" manifest if the file can't be read
    // (e.g. spawn_fn launched a background thread that hasn't finished yet).
    let final_manifest = read_agent_manifest(&manifest.manifest_file).unwrap_or(manifest);
    Ok(final_manifest)
}

pub(crate) fn build_async_agent_launch_output(
    manifest: &AgentOutput,
    prompt: &str,
) -> AsyncAgentLaunchOutput {
    AsyncAgentLaunchOutput {
        status: "async_launched",
        agent_id: manifest.agent_id.clone(),
        description: manifest.description.clone(),
        prompt: prompt.to_string(),
        output_file: manifest.output_file.clone(),
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn spawn_agent_job(job: AgentJob) -> Result<(), String> {
    let started_at = Instant::now();
    let spawned_agent_id = job.manifest.agent_id.clone();
    agent_debug_log(
        "agent.background.register.begin",
        format!(
            "agent_id={} description={:?} parent_session_id={:?} prompt_len={}",
            job.manifest.agent_id,
            job.manifest.description,
            job.parent_session_id,
            job.prompt.len()
        ),
    );
    // Register in the task registry so TaskGet/TaskOutput can find this agent.
    let registry = global_task_registry();
    let agent_id = job.manifest.agent_id.clone();
    registry.create_with_id(
        agent_id.clone(),
        &job.prompt,
        Some(&job.manifest.description),
    );
    registry
        .set_status(&agent_id, TaskStatus::Running)
        .map_err(|e| e.clone())?;
    agent_debug_log(
        "agent.background.register.done",
        format!(
            "agent_id={} elapsed_ms={}",
            agent_id,
            started_at.elapsed().as_millis()
        ),
    );

    let thread_name = format!("clawd-agent-{}", job.manifest.agent_id);
    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            agent_debug_log(
                "agent.background.thread.begin",
                format!(
                    "agent_id={} description={:?}",
                    job.manifest.agent_id, job.manifest.description
                ),
            );
            let registry = global_task_registry();
            let started_at = Instant::now();
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_agent_job(&job)));
            match result {
                Ok(Ok(())) => {
                    agent_debug_log(
                        "agent.background.thread.completed",
                        format!(
                            "agent_id={} elapsed_ms={}",
                            job.manifest.agent_id,
                            started_at.elapsed().as_millis()
                        ),
                    );
                    let output = read_agent_manifest(&job.manifest.manifest_file)
                        .ok()
                        .and_then(|m| m.result)
                        .unwrap_or_default();
                    let _ = registry.append_output(&job.manifest.agent_id, &output);
                    let _ = registry.set_status(&job.manifest.agent_id, TaskStatus::Completed);
                    enqueue_background_agent_notification(&job, "completed", &output);
                }
                Ok(Err(error)) => {
                    agent_debug_log(
                        "agent.background.thread.failed",
                        format!(
                            "agent_id={} elapsed_ms={} error={error}",
                            job.manifest.agent_id,
                            started_at.elapsed().as_millis()
                        ),
                    );
                    let _ = persist_agent_terminal_state(
                        &job.manifest,
                        "failed",
                        None,
                        Some(error.clone()),
                    );
                    let _ = registry.append_output(&job.manifest.agent_id, &error);
                    let _ = registry.set_status(&job.manifest.agent_id, TaskStatus::Failed);
                    enqueue_background_agent_notification(&job, "failed", &error);
                }
                Err(_) => {
                    let msg = String::from("sub-agent thread panicked");
                    agent_debug_log(
                        "agent.background.thread.panicked",
                        format!(
                            "agent_id={} elapsed_ms={}",
                            job.manifest.agent_id,
                            started_at.elapsed().as_millis()
                        ),
                    );
                    let _ = persist_agent_terminal_state(
                        &job.manifest,
                        "failed",
                        None,
                        Some(msg.clone()),
                    );
                    let _ = registry.append_output(&job.manifest.agent_id, &msg);
                    let _ = registry.set_status(&job.manifest.agent_id, TaskStatus::Failed);
                    enqueue_background_agent_notification(&job, "failed", &msg);
                }
            }
        })
        .map(|_| {
            agent_debug_log(
                "agent.background.thread.spawned",
                format!("agent_id={spawned_agent_id}"),
            );
        })
        .map_err(|error| {
            agent_debug_log(
                "agent.background.thread.spawn_error",
                format!("agent_id={spawned_agent_id} error={error}"),
            );
            error.to_string()
        })
}

pub(crate) fn enqueue_background_agent_notification(job: &AgentJob, status: &str, body: &str) {
    let Some(session_id) = job.parent_session_id.as_deref() else {
        return;
    };
    let detail_label = if status.eq_ignore_ascii_case("completed") {
        "result"
    } else {
        "error"
    };
    let mut message = format!(
        "Background agent finished.\nagentId: {}\ndescription: {}\nstatus: {}\noutput_file: {}",
        job.manifest.agent_id, job.manifest.description, status, job.manifest.output_file
    );
    if !body.trim().is_empty() {
        let _ = std::fmt::Write::write_fmt(
            &mut message,
            format_args!("\n{detail_label}:\n{}", body.trim()),
        );
    }
    enqueue_session_notification(session_id.to_string(), message);
}

pub(crate) fn run_agent_job(job: &AgentJob) -> Result<(), String> {
    agent_debug_log(
        "agent.job.begin",
        format!(
            "agent_id={} model={:?} description={:?} prompt_len={}",
            job.manifest.agent_id,
            job.manifest.model,
            job.manifest.description,
            job.prompt.len()
        ),
    );
    let total_started_at = Instant::now();
    let runtime_started_at = Instant::now();
    let mut runtime = build_agent_runtime(job)?.with_max_iterations(DEFAULT_AGENT_MAX_ITERATIONS);
    agent_debug_log(
        "agent.job.runtime_ready",
        format!(
            "agent_id={} elapsed_ms={}",
            job.manifest.agent_id,
            runtime_started_at.elapsed().as_millis()
        ),
    );
    let run_turn_started_at = Instant::now();
    let summary = runtime
        .run_turn(job.prompt.clone(), None)
        .map_err(|error| {
            let rendered = error.to_string();
            agent_debug_log(
                "agent.job.run_turn_error",
                format!(
                    "agent_id={} elapsed_ms={} error={rendered}",
                    job.manifest.agent_id,
                    run_turn_started_at.elapsed().as_millis()
                ),
            );
            rendered
        })?;
    agent_debug_log(
        "agent.job.run_turn_done",
        format!(
            "agent_id={} elapsed_ms={} total_elapsed_ms={} assistant_messages={} tool_results={}",
            job.manifest.agent_id,
            run_turn_started_at.elapsed().as_millis(),
            total_started_at.elapsed().as_millis(),
            summary.assistant_messages.len(),
            summary.tool_results.len()
        ),
    );
    let final_text = final_assistant_text(&summary);
    agent_debug_log(
        "agent.job.persist_terminal_state",
        format!(
            "agent_id={} final_text_len={} total_elapsed_ms={}",
            job.manifest.agent_id,
            final_text.len(),
            total_started_at.elapsed().as_millis()
        ),
    );
    persist_agent_terminal_state(&job.manifest, "completed", Some(final_text.as_str()), None)
}

pub(crate) fn build_agent_runtime(
    job: &AgentJob,
) -> Result<ConversationRuntime<ProviderRuntimeClient, SubagentToolExecutor>, String> {
    let model = job
        .manifest
        .model
        .clone()
        .unwrap_or_else(|| DEFAULT_AGENT_MODEL.to_string());
    let allowed_tools = job.allowed_tools.clone();
    agent_debug_log(
        "agent.runtime.build.begin",
        format!(
            "agent_id={} model={} allowed_tools={} system_prompt_parts={}",
            job.manifest.agent_id,
            model,
            allowed_tools.len(),
            job.system_prompt.len()
        ),
    );
    let api_client = ProviderRuntimeClient::new_for_session(
        model,
        allowed_tools.clone(),
        &job.manifest.agent_id,
    )?;
    let permission_policy = agent_permission_policy();
    let tool_executor = SubagentToolExecutor::new(allowed_tools)
        .with_enforcer(PermissionEnforcer::new(permission_policy.clone()));
    let session = new_agent_session(&job.manifest.agent_id);
    Ok(ConversationRuntime::new(
        session,
        api_client,
        tool_executor,
        permission_policy,
        job.system_prompt.clone(),
    ))
}

pub(crate) fn new_agent_session(agent_id: &str) -> Session {
    let mut session = Session::new();
    session.session_id = agent_id.to_string();
    session
}

pub(crate) fn build_agent_system_prompt(subagent_type: &str) -> Result<Vec<String>, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let mut prompt = load_system_prompt(cwd, agent_system_date(), std::env::consts::OS, "unknown")
        .map_err(|error| error.to_string())?;
    let instruction = format!(
        "You are a delegated sub-agent of type `{subagent_type}`. Work only on the delegated task, use only the tools available to you, do not ask the user questions, and finish with a concise result."
    );
    if let Some(boundary) = prompt
        .iter()
        .position(|part| part == SYSTEM_PROMPT_DYNAMIC_BOUNDARY)
    {
        prompt.insert(boundary, instruction);
    } else {
        prompt.push(instruction);
    }
    Ok(prompt)
}

pub(crate) fn agent_system_date() -> String {
    if let Ok(date) = std::env::var("CLAWD_AGENT_SYSTEM_DATE") {
        let trimmed = date.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(output) = Command::new("date").args(["+%Y-%m-%d"]).output() {
        if output.status.success() {
            let date = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !date.is_empty() {
                return date;
            }
        }
    }
    FALLBACK_AGENT_SYSTEM_DATE.to_string()
}

pub(crate) fn resolve_agent_model(model: Option<&str>) -> String {
    model
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or(DEFAULT_AGENT_MODEL)
        .to_string()
}

pub(crate) fn allowed_tools_for_subagent(subagent_type: &str) -> BTreeSet<String> {
    let tools = match subagent_type {
        "Explore" => vec![
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "Skill",
            "StructuredOutput",
        ],
        "Plan" => vec![
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "Skill",
            "TodoWrite",
            "StructuredOutput",
            "SendUserMessage",
        ],
        "Verification" => vec![
            "bash",
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "TodoWrite",
            "StructuredOutput",
            "SendUserMessage",
            "PowerShell",
        ],
        "claw-guide" => vec![
            "read_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "ToolSearch",
            "Skill",
            "StructuredOutput",
            "SendUserMessage",
        ],
        "statusline-setup" => vec![
            "bash",
            "read_file",
            "write_file",
            "edit_file",
            "glob_search",
            "grep_search",
            "ToolSearch",
        ],
        _ => vec![
            "bash",
            "read_file",
            "write_file",
            "edit_file",
            "glob_search",
            "grep_search",
            "WebFetch",
            "WebSearch",
            "TodoWrite",
            "Skill",
            "ToolSearch",
            "NotebookEdit",
            "Sleep",
            "SendUserMessage",
            "Config",
            "StructuredOutput",
            "REPL",
            "PowerShell",
        ],
    };
    tools.into_iter().map(str::to_string).collect()
}

pub(crate) fn agent_permission_policy() -> PermissionPolicy {
    mvp_tool_specs().into_iter().fold(
        PermissionPolicy::new(PermissionMode::DangerFullAccess),
        |policy, spec| policy.with_tool_requirement(spec.name, spec.required_permission),
    )
}

pub(crate) fn write_agent_manifest(manifest: &AgentOutput) -> Result<(), String> {
    let mut normalized = manifest.clone();
    normalized.lane_events = dedupe_superseded_commit_events(&normalized.lane_events);
    std::fs::write(
        &normalized.manifest_file,
        serde_json::to_string_pretty(&normalized).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

pub(crate) fn read_agent_manifest(path: &str) -> Result<AgentOutput, String> {
    let content = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    serde_json::from_str(&content).map_err(|error| error.to_string())
}

pub(crate) fn persist_agent_terminal_state(
    manifest: &AgentOutput,
    status: &str,
    result: Option<&str>,
    error: Option<String>,
) -> Result<(), String> {
    let blocker = error.as_deref().map(classify_lane_blocker);
    append_agent_output(
        &manifest.output_file,
        &format_agent_terminal_output(status, result, blocker.as_ref(), error.as_deref()),
    )?;
    let mut next_manifest = manifest.clone();
    next_manifest.status = status.to_string();
    next_manifest.completed_at = Some(iso8601_now());
    next_manifest.current_blocker.clone_from(&blocker);
    next_manifest.derived_state =
        derive_agent_state(status, result, error.as_deref(), blocker.as_ref()).to_string();
    next_manifest.error = error;
    next_manifest.result = result.map(str::to_string);
    if let Some(blocker) = blocker {
        next_manifest
            .lane_events
            .push(LaneEvent::blocked(iso8601_now(), &blocker));
        next_manifest
            .lane_events
            .push(LaneEvent::failed(iso8601_now(), &blocker));
    } else {
        next_manifest.current_blocker = None;
        let compressed_detail = result
            .filter(|value| !value.trim().is_empty())
            .map(|value| compress_summary_text(value.trim()));
        next_manifest
            .lane_events
            .push(LaneEvent::finished(iso8601_now(), compressed_detail));
        if let Some(provenance) = maybe_commit_provenance(result) {
            next_manifest.lane_events.push(LaneEvent::commit_created(
                iso8601_now(),
                Some(format!("commit {}", provenance.commit)),
                provenance,
            ));
        }
    }
    write_agent_manifest(&next_manifest)
}

pub(crate) fn derive_agent_state(
    status: &str,
    result: Option<&str>,
    error: Option<&str>,
    blocker: Option<&LaneEventBlocker>,
) -> &'static str {
    let normalized_status = status.trim().to_ascii_lowercase();
    let normalized_error = error.unwrap_or_default().to_ascii_lowercase();

    if normalized_status == "running" {
        return "working";
    }
    if normalized_status == "completed" {
        return if result.is_some_and(|value| !value.trim().is_empty()) {
            "finished_cleanable"
        } else {
            "finished_pending_report"
        };
    }
    if normalized_error.contains("background") {
        return "blocked_background_job";
    }
    if normalized_error.contains("merge conflict") || normalized_error.contains("cherry-pick") {
        return "blocked_merge_conflict";
    }
    if normalized_error.contains("mcp") {
        return "degraded_mcp";
    }
    if normalized_error.contains("transport")
        || normalized_error.contains("broken pipe")
        || normalized_error.contains("connection")
        || normalized_error.contains("interrupted")
    {
        return "interrupted_transport";
    }
    if blocker.is_some() {
        return "truly_idle";
    }
    "truly_idle"
}

pub(crate) fn maybe_commit_provenance(result: Option<&str>) -> Option<LaneCommitProvenance> {
    let commit = extract_commit_sha(result?)?;
    let branch = current_git_branch().unwrap_or_else(|| "unknown".to_string());
    let worktree = std::env::current_dir()
        .ok()
        .map(|path| path.display().to_string());
    Some(LaneCommitProvenance {
        commit: commit.clone(),
        branch,
        worktree,
        canonical_commit: Some(commit.clone()),
        superseded_by: None,
        lineage: vec![commit],
    })
}

pub(crate) fn extract_commit_sha(result: &str) -> Option<String> {
    result
        .split(|c: char| !c.is_ascii_hexdigit())
        .find(|token| token.len() >= 7 && token.len() <= 40)
        .map(str::to_string)
}

pub(crate) fn current_git_branch() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(crate) fn append_agent_output(path: &str, suffix: &str) -> Result<(), String> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    file.write_all(suffix.as_bytes())
        .map_err(|error| error.to_string())
}

pub(crate) fn format_agent_terminal_output(
    status: &str,
    result: Option<&str>,
    blocker: Option<&LaneEventBlocker>,
    error: Option<&str>,
) -> String {
    let mut sections = vec![format!("\n## Result\n\n- status: {status}\n")];
    if let Some(blocker) = blocker {
        sections.push(format!(
            "\n### Blocker\n\n- failure_class: {}\n- detail: {}\n",
            serde_json::to_string(&blocker.failure_class)
                .unwrap_or_else(|_| "\"infra\"".to_string())
                .trim_matches('"'),
            blocker.detail.trim()
        ));
    }
    if let Some(result) = result.filter(|value| !value.trim().is_empty()) {
        sections.push(format!("\n### Final response\n\n{}\n", result.trim()));
    }
    if let Some(error) = error.filter(|value| !value.trim().is_empty()) {
        sections.push(format!("\n### Error\n\n{}\n", error.trim()));
    }
    sections.join("")
}

pub(crate) fn classify_lane_blocker(error: &str) -> LaneEventBlocker {
    let detail = error.trim().to_string();
    LaneEventBlocker {
        failure_class: classify_lane_failure(error),
        detail,
    }
}

pub(crate) fn classify_lane_failure(error: &str) -> LaneFailureClass {
    let normalized = error.to_ascii_lowercase();

    if normalized.contains("prompt") && normalized.contains("deliver") {
        LaneFailureClass::PromptDelivery
    } else if normalized.contains("trust") {
        LaneFailureClass::TrustGate
    } else if normalized.contains("branch")
        && (normalized.contains("stale") || normalized.contains("diverg"))
    {
        LaneFailureClass::BranchDivergence
    } else if normalized.contains("gateway") || normalized.contains("routing") {
        LaneFailureClass::GatewayRouting
    } else if normalized.contains("compile")
        || normalized.contains("build failed")
        || normalized.contains("cargo check")
    {
        LaneFailureClass::Compile
    } else if normalized.contains("test") {
        LaneFailureClass::Test
    } else if normalized.contains("tool failed")
        || normalized.contains("runtime tool")
        || normalized.contains("tool runtime")
    {
        LaneFailureClass::ToolRuntime
    } else if normalized.contains("plugin") {
        LaneFailureClass::PluginStartup
    } else if normalized.contains("mcp") && normalized.contains("handshake") {
        LaneFailureClass::McpHandshake
    } else if normalized.contains("mcp") {
        LaneFailureClass::McpStartup
    } else {
        LaneFailureClass::Infra
    }
}

pub(crate) struct ProviderEntry {
    pub(crate) model: String,
    pub(crate) client: ProviderClient,
}

pub(crate) struct ProviderRuntimeClient {
    runtime: tokio::runtime::Runtime,
    pub(crate) chain: Vec<ProviderEntry>,
    allowed_tools: BTreeSet<String>,
    session_id: String,
}

impl ProviderRuntimeClient {
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn new_for_session(
        model: String,
        allowed_tools: BTreeSet<String>,
        session_id: &str,
    ) -> Result<Self, String> {
        let fallback_config = load_provider_fallback_config();
        Self::new_with_fallback_config_for_session(
            model,
            allowed_tools,
            &fallback_config,
            session_id,
        )
    }

    #[cfg_attr(not(test), allow(dead_code))]
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn new_with_fallback_config(
        model: String,
        allowed_tools: BTreeSet<String>,
        fallback_config: &ProviderFallbackConfig,
    ) -> Result<Self, String> {
        Self::new_with_fallback_config_for_session(
            model,
            allowed_tools,
            fallback_config,
            "subagent-runtime",
        )
    }

    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn new_with_fallback_config_for_session(
        model: String,
        allowed_tools: BTreeSet<String>,
        fallback_config: &ProviderFallbackConfig,
        session_id: &str,
    ) -> Result<Self, String> {
        let primary_model = fallback_config.primary().map_or(model, str::to_string);
        let primary = build_provider_entry(&primary_model, session_id)?;
        let mut chain = vec![primary];
        for fallback_model in fallback_config.fallbacks() {
            match build_provider_entry(fallback_model, session_id) {
                Ok(entry) => chain.push(entry),
                Err(error) => {
                    eprintln!(
                        "warning: skipping unavailable fallback provider {fallback_model}: {error}"
                    );
                }
            }
        }
        Ok(Self {
            runtime: tokio::runtime::Runtime::new().map_err(|error| error.to_string())?,
            chain,
            allowed_tools,
            session_id: session_id.to_string(),
        })
    }
}

pub(crate) fn build_provider_entry(model: &str, session_id: &str) -> Result<ProviderEntry, String> {
    let resolved = resolve_model_alias(model).clone();
    let client = match detect_provider_kind(&resolved) {
        ProviderKind::Anthropic => {
            let auth = resolve_subagent_auth_source()?;
            let client = AnthropicClient::from_auth(auth)
                .with_base_url(read_base_url())
                .with_extra_header("X-Claude-Code-Session-Id", session_id.to_string())
                .with_extra_header("x-app", "cli")
                .with_extra_header("anthropic-dangerous-direct-browser-access", "true")
                .with_prompt_cache(PromptCache::new(session_id));
            ProviderClient::Anthropic(client)
        }
        ProviderKind::Xai | ProviderKind::OpenAi => {
            ProviderClient::from_model(&resolved).map_err(|error| error.to_string())?
        }
    };
    Ok(ProviderEntry {
        model: resolved,
        client,
    })
}

pub(crate) fn resolve_subagent_auth_source() -> Result<AuthSource, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    resolve_startup_auth_source(|| {
        Ok(Some(
            load_subagent_oauth_config_for(&cwd)?.unwrap_or_else(default_subagent_oauth_config),
        ))
    })
    .map_err(|error| error.to_string())
}

pub(crate) fn load_subagent_oauth_config_for(cwd: &Path) -> Result<Option<OAuthConfig>, ApiError> {
    let config = ConfigLoader::default_for(cwd)
        .load()
        .map_err(|error| ApiError::Auth(format!("failed to load runtime OAuth config: {error}")))?;
    Ok(config.oauth().cloned())
}

pub(crate) fn default_subagent_oauth_config() -> OAuthConfig {
    OAuthConfig {
        client_id: String::from("9d1c250a-e61b-44d9-88ed-5944d1962f5e"),
        authorize_url: String::from("https://platform.claude.com/oauth/authorize"),
        token_url: String::from("https://platform.claude.com/v1/oauth/token"),
        callback_port: None,
        manual_redirect_url: None,
        scopes: vec![
            String::from("user:profile"),
            String::from("user:inference"),
            String::from("user:sessions:claude_code"),
        ],
    }
}

pub(crate) fn load_provider_fallback_config() -> ProviderFallbackConfig {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| ConfigLoader::default_for(cwd).load().ok())
        .map_or_else(ProviderFallbackConfig::default, |config| {
            config.provider_fallbacks().clone()
        })
}

impl ApiClient for ProviderRuntimeClient {
    #[allow(clippy::too_many_lines)]
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let mut tools = tool_definitions_for_request_tools(
            &self.allowed_tools,
            request.suppress_background_task_tools,
        );
        apply_tool_cache_controls(&mut tools);
        let messages = convert_messages(&request.messages);
        let system = build_system_blocks_with_cache_controls(&request.system_prompt);
        let tool_choice = (!tools.is_empty()).then_some(ToolChoice::Auto);

        let runtime = &self.runtime;
        let chain = &self.chain;
        let mut last_error: Option<ApiError> = None;
        for (index, entry) in chain.iter().enumerate() {
            let attempt_started_at = Instant::now();
            agent_debug_log(
                "agent.provider.stream.begin",
                format!(
                    "model={} attempt={} chain_len={} messages={} tools={} system_prompt_parts={}",
                    entry.model,
                    index + 1,
                    chain.len(),
                    request.messages.len(),
                    tools.len(),
                    request.system_prompt.len()
                ),
            );
            let message_request = MessageRequest {
                model: entry.model.clone(),
                max_tokens: max_tokens_for_model(&entry.model),
                messages: messages.clone(),
                cache_control: matches!(entry.client, ProviderClient::Anthropic(_))
                    .then(CacheControl::ephemeral),
                system: system.clone(),
                tools: (!tools.is_empty()).then(|| tools.clone()),
                tool_choice: tool_choice.clone(),
                stream: true,
                ..Default::default()
            };
            let prompt_cache_summary = summarize_prompt_cache_controls(&message_request);
            let cache_control_types_json =
                serde_json::to_string(&prompt_cache_summary.cache_control_types)
                    .unwrap_or_else(|_| "[]".to_string());
            agent_debug_log(
                "agent.provider.stream.prompt_cache",
                format!(
                    "session_id={} model={} message_count={} cache_enabled={} cache_control_count={} cache_control_types={} automatic_cache_control_count={} system_cache_control_count={} tool_cache_control_count={} message_cache_control_count={}",
                    self.session_id,
                    entry.model,
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
            log_prompt_cache_block_diagnostics(
                "agent",
                &self.session_id,
                &entry.model,
                &message_request,
            );

            let attempt = runtime.block_on(stream_with_provider(
                &entry.client,
                &message_request,
                &self.session_id,
                &entry.model,
            ));
            match attempt {
                Ok(events) => {
                    agent_debug_log(
                        "agent.provider.stream.success",
                        format!(
                            "model={} attempt={} elapsed_ms={} events={}",
                            entry.model,
                            index + 1,
                            attempt_started_at.elapsed().as_millis(),
                            events.len()
                        ),
                    );
                    return Ok(events);
                }
                Err(error) if error.is_retryable() && index + 1 < chain.len() => {
                    agent_debug_log(
                        "agent.provider.stream.retryable_error",
                        format!(
                            "model={} attempt={} elapsed_ms={} error={error}",
                            entry.model,
                            index + 1,
                            attempt_started_at.elapsed().as_millis()
                        ),
                    );
                    eprintln!(
                        "provider {} failed with retryable error, falling back: {error}",
                        entry.model
                    );
                    last_error = Some(error);
                }
                Err(error) => {
                    agent_debug_log(
                        "agent.provider.stream.error",
                        format!(
                            "model={} attempt={} elapsed_ms={} error={error}",
                            entry.model,
                            index + 1,
                            attempt_started_at.elapsed().as_millis()
                        ),
                    );
                    return Err(RuntimeError::new(error.to_string()));
                }
            }
        }

        Err(RuntimeError::new(last_error.map_or_else(
            || String::from("provider chain exhausted with no attempts"),
            |error| error.to_string(),
        )))
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn stream_with_provider(
    client: &ProviderClient,
    message_request: &MessageRequest,
    session_id: &str,
    model: &str,
) -> Result<Vec<AssistantEvent>, ApiError> {
    let mut stream = client.stream_message(message_request).await?;
    let mut events = Vec::new();
    let mut pending_tools: BTreeMap<u32, (String, String, String)> = BTreeMap::new();
    let mut tool_block_indices: BTreeSet<u32> = BTreeSet::new();
    let mut saw_stop = false;
    let mut stream_event_seq = 0_u64;

    while let Some(event) = stream.next_event().await? {
        stream_event_seq += 1;
        if should_log_stream_event_for_tool_diagnostics(&event) {
            agent_debug_log(
                "agent.provider.stream.event",
                format!(
                    "session_id={session_id}\nmodel={model}\nevent_seq={stream_event_seq}\n{}\n{}",
                    stream_event_debug_summary(&event, 4000),
                    pending_tools_debug_summary(&pending_tools, 240)
                ),
            );
        }
        match event {
            ApiStreamEvent::MessageStart(start) => {
                for (index, block) in start.message.content.into_iter().enumerate() {
                    let index = u32::try_from(index).expect("stream message block index overflow");
                    track_tool_block_index(&block, index, &mut tool_block_indices);
                    push_output_block(block, index, &mut events, &mut pending_tools, true);
                }
            }
            ApiStreamEvent::ContentBlockStart(start) => {
                track_tool_block_index(&start.content_block, start.index, &mut tool_block_indices);
                if let OutputContentBlock::ToolUse { id, name, input } = &start.content_block {
                    agent_debug_log(
                        "agent.provider.stream.tool_start",
                        format!(
                            "session_id={session_id}\nmodel={model}\nindex={}\ntool_id={id}\ntool_name={name}\n{}",
                            start.index,
                            debug_json_value_summary(input, 160)
                        ),
                    );
                }
                push_output_block(
                    start.content_block,
                    start.index,
                    &mut events,
                    &mut pending_tools,
                    true,
                );
            }
            ApiStreamEvent::ContentBlockDelta(delta) => match delta.delta {
                ContentBlockDelta::TextDelta { text } => {
                    if !text.is_empty() {
                        events.push(AssistantEvent::TextDelta(text));
                    }
                }
                ContentBlockDelta::InputJsonDelta { partial_json } => {
                    if let Some((id, name, input)) = pending_tools.get_mut(&delta.index) {
                        input.push_str(&partial_json);
                        agent_debug_log(
                            "agent.provider.stream.tool_input_delta",
                            format!(
                                "session_id={session_id}\nmodel={model}\nindex={}\ntool_id={id}\ntool_name={name}\npartial_bytes={}\npartial_chars={}\naccumulated_bytes={}\naccumulated_chars={}\npartial={}\naccumulated_suffix={}",
                                delta.index,
                                partial_json.len(),
                                partial_json.chars().count(),
                                input.len(),
                                input.chars().count(),
                                json_debug_string(&partial_json, 160),
                                json_debug_suffix(input, 160)
                            ),
                        );
                    } else {
                        agent_debug_log(
                            "agent.provider.stream.tool_input_delta_without_start",
                            format!(
                                "session_id={session_id}\nmodel={model}\nindex={}\npartial_bytes={}\npartial_chars={}\npartial={}",
                                delta.index,
                                partial_json.len(),
                                partial_json.chars().count(),
                                json_debug_string(&partial_json, 160)
                            ),
                        );
                    }
                }
                ContentBlockDelta::ThinkingDelta { .. }
                | ContentBlockDelta::SignatureDelta { .. } => {}
            },
            ApiStreamEvent::ContentBlockStop(stop) => {
                if let Some((id, name, input)) = pending_tools.remove(&stop.index) {
                    tool_block_indices.remove(&stop.index);
                    let normalized_empty_to_object = input.trim().is_empty();
                    let raw_summary = debug_labeled_json_input_summary("raw_input", &input, 240);
                    let input = normalize_tool_input_string(input);
                    agent_debug_log(
                        "agent.provider.stream.tool_stop",
                        format!(
                            "session_id={session_id}\nmodel={model}\nindex={}\ntool_id={id}\ntool_name={name}\nnormalized_empty_to_object={normalized_empty_to_object}\n{}\n{}",
                            stop.index,
                            raw_summary,
                            debug_labeled_json_input_summary("normalized_input", &input, 240)
                        ),
                    );
                    events.push(AssistantEvent::ToolUse { id, name, input });
                } else if should_log_tool_stop_without_start(&mut tool_block_indices, stop.index) {
                    agent_debug_log(
                        "agent.provider.stream.tool_stop_without_start",
                        format!(
                            "session_id={session_id}\nmodel={model}\nindex={}\n{}",
                            stop.index,
                            pending_tools_debug_summary(&pending_tools, 240)
                        ),
                    );
                }
            }
            ApiStreamEvent::MessageDelta(delta) => {
                events.push(AssistantEvent::Usage(delta.usage.token_usage()));
            }
            ApiStreamEvent::MessageStop(_) => {
                saw_stop = true;
                if !pending_tools.is_empty() {
                    agent_debug_log(
                        "agent.provider.stream.message_stop_with_pending_tools",
                        format!(
                            "session_id={session_id}\nmodel={model}\n{}",
                            pending_tools_debug_summary(&pending_tools, 240)
                        ),
                    );
                }
                events.push(AssistantEvent::MessageStop);
            }
        }
    }

    if !pending_tools.is_empty() {
        agent_debug_log(
            "agent.provider.stream.ended_with_pending_tools",
            format!(
                "session_id={session_id}\nmodel={model}\nsaw_stop={saw_stop}\n{}",
                pending_tools_debug_summary(&pending_tools, 240)
            ),
        );
    }

    push_prompt_cache_record(client, &mut events);

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

    let response = client
        .send_message(&MessageRequest {
            stream: false,
            ..message_request.clone()
        })
        .await?;
    let mut events = response_to_events(response);
    push_prompt_cache_record(client, &mut events);
    Ok(events)
}

pub(crate) struct SubagentToolExecutor {
    allowed_tools: BTreeSet<String>,
    enforcer: Option<PermissionEnforcer>,
}

impl SubagentToolExecutor {
    pub(crate) fn new(allowed_tools: BTreeSet<String>) -> Self {
        Self {
            allowed_tools,
            enforcer: None,
        }
    }

    pub(crate) fn with_enforcer(mut self, enforcer: PermissionEnforcer) -> Self {
        self.enforcer = Some(enforcer);
        self
    }
}

impl ToolExecutor for SubagentToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        if !self.allowed_tools.contains(tool_name) {
            return Err(ToolError::new(format!(
                "tool `{tool_name}` is not enabled for this sub-agent"
            )));
        }
        let normalized_input = normalize_tool_input_json(input);
        let value = match serde_json::from_str(normalized_input) {
            Ok(value) => value,
            Err(error) => {
                let error = ToolError::new(format!("invalid tool input JSON: {error}"));
                agent_debug_log(
                    "subagent.tool.execute.input_json_parse_error",
                    format!(
                        "tool_name={tool_name}\n{}\nerror={error}",
                        debug_json_input_summary(normalized_input, 500)
                    ),
                );
                return Err(error);
            }
        };
        execute_tool_with_enforcer(self.enforcer.as_ref(), tool_name, &value)
            .map_err(ToolError::new)
    }
}

pub(crate) fn tool_specs_for_allowed_tools(
    allowed_tools: Option<&BTreeSet<String>>,
    suppress_background_task_tools: bool,
) -> Vec<ToolSpec> {
    mvp_tool_specs()
        .into_iter()
        .filter(|spec| {
            allowed_tools.is_none_or(|allowed| allowed.contains(spec.name))
                && !(suppress_background_task_tools && is_background_task_tool_name(spec.name))
        })
        .collect()
}

pub(crate) fn tool_definitions_for_request_tools(
    allowed_tools: &BTreeSet<String>,
    suppress_background_task_tools: bool,
) -> Vec<ToolDefinition> {
    tool_specs_for_allowed_tools(Some(allowed_tools), suppress_background_task_tools)
        .into_iter()
        .map(|spec| ToolDefinition {
            name: spec.name.to_string(),
            description: Some(spec.description.to_string()),
            input_schema: spec.input_schema,
            cache_control: None,
        })
        .collect()
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

pub(crate) fn debug_json_value_summary(value: &serde_json::Value, limit: usize) -> String {
    let rendered = serde_json::to_string(value).unwrap_or_else(|_| value.to_string());
    debug_json_input_summary(&rendered, limit)
}

pub(crate) fn debug_json_input_summary(input: &str, limit: usize) -> String {
    debug_labeled_json_input_summary("input", input, limit)
}

pub(crate) fn debug_labeled_json_input_summary(label: &str, input: &str, limit: usize) -> String {
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
            delta: ContentBlockDelta::TextDelta { .. },
            ..
        })
    )
}

pub(crate) fn stream_event_debug_summary(event: &ApiStreamEvent, limit: usize) -> String {
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

pub(crate) fn pending_tools_debug_summary(
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

pub(crate) fn json_debug_string(input: &str, limit: usize) -> String {
    let value = input.chars().take(limit).collect::<String>();
    serde_json::to_string(&value).unwrap_or_else(|_| "\"<unprintable>\"".to_string())
}

pub(crate) fn json_debug_suffix(input: &str, limit: usize) -> String {
    let mut suffix = input.chars().rev().take(limit).collect::<Vec<_>>();
    suffix.reverse();
    let value = suffix.into_iter().collect::<String>();
    serde_json::to_string(&value).unwrap_or_else(|_| "\"<unprintable>\"".to_string())
}

pub(crate) fn convert_messages(messages: &[ConversationMessage]) -> Vec<InputMessage> {
    messages
        .iter()
        .filter_map(|message| {
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
            (!content.is_empty()).then(|| InputMessage {
                role: role.to_string(),
                content,
            })
        })
        .collect()
}

pub(crate) fn apply_tool_cache_controls(tools: &mut [ToolDefinition]) {
    if let Some(last_tool) = tools.last_mut() {
        last_tool.cache_control = Some(CacheControl::ephemeral());
    }
}

pub(crate) fn push_output_block(
    block: OutputContentBlock,
    block_index: u32,
    events: &mut Vec<AssistantEvent>,
    pending_tools: &mut BTreeMap<u32, (String, String, String)>,
    streaming_tool_input: bool,
) {
    match block {
        OutputContentBlock::Text { text } => {
            if !text.is_empty() {
                events.push(AssistantEvent::TextDelta(text));
            }
        }
        OutputContentBlock::ToolUse { id, name, input } => {
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
        OutputContentBlock::Thinking { .. } | OutputContentBlock::RedactedThinking { .. } => {}
    }
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

pub(crate) fn response_to_events(response: MessageResponse) -> Vec<AssistantEvent> {
    let mut events = Vec::new();
    let mut pending_tools = BTreeMap::new();

    for (index, block) in response.content.into_iter().enumerate() {
        let index = u32::try_from(index).expect("response block index overflow");
        push_output_block(block, index, &mut events, &mut pending_tools, false);
        if let Some((id, name, input)) = pending_tools.remove(&index) {
            events.push(AssistantEvent::ToolUse { id, name, input });
        }
    }

    events.push(AssistantEvent::Usage(response.usage.token_usage()));
    events.push(AssistantEvent::MessageStop);
    events
}

pub(crate) fn push_prompt_cache_record(client: &ProviderClient, events: &mut Vec<AssistantEvent>) {
    if let Some(record) = client.take_last_prompt_cache_record() {
        if let Some(event) = prompt_cache_record_to_runtime_event(record) {
            events.push(AssistantEvent::PromptCache(event));
        }
    }
}

pub(crate) fn prompt_cache_record_to_runtime_event(
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

pub(crate) fn final_assistant_text(summary: &runtime::TurnSummary) -> String {
    summary
        .assistant_messages
        .last()
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

pub(crate) fn agent_store_dir() -> Result<std::path::PathBuf, String> {
    if let Ok(path) = std::env::var("CLAWD_AGENT_STORE") {
        return Ok(std::path::PathBuf::from(path));
    }
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    if let Some(workspace_root) = cwd.ancestors().nth(2) {
        return Ok(workspace_root.join(".clawd-agents"));
    }
    Ok(cwd.join(".clawd-agents"))
}

pub(crate) fn make_agent_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("agent-{nanos}")
}

pub(crate) fn slugify_agent_name(description: &str) -> String {
    let mut out = description
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').chars().take(32).collect()
}

pub(crate) fn normalize_subagent_type(subagent_type: Option<&str>) -> String {
    let trimmed = subagent_type.map(str::trim).unwrap_or_default();
    if trimmed.is_empty() {
        return String::from("general-purpose");
    }

    match canonical_tool_token(trimmed).as_str() {
        "general" | "generalpurpose" | "generalpurposeagent" => String::from("general-purpose"),
        "explore" | "explorer" | "exploreagent" => String::from("Explore"),
        "plan" | "planagent" => String::from("Plan"),
        "verification" | "verificationagent" | "verify" | "verifier" => {
            String::from("Verification")
        }
        "clawguide" | "clawguideagent" | "guide" => String::from("claw-guide"),
        "statusline" | "statuslinesetup" => String::from("statusline-setup"),
        _ => trimmed.to_string(),
    }
}

pub(crate) fn iso8601_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}
