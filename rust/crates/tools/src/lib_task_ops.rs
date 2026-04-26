use crate::{
    global_task_registry, global_worker_registry, global_team_registry,
    agent_debug_log, to_pretty_json,
};
use runtime::task_registry::TaskStatus;
use runtime::worker_registry::WorkerReadySnapshot;
use runtime::ConfigLoader;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

// ============================================================================
// Input Types
// ============================================================================

#[derive(Debug, Deserialize)]
struct TaskCreateInput {
    prompt: String,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaskIdInput {
    task_id: String,
}

#[derive(Debug, Deserialize)]
struct TaskOutputInput {
    task_id: String,
    block: bool,
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
    #[serde(default)]
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
    prompt: Option<String>,
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

#[derive(Debug, Serialize)]
struct TaskOutputPayload {
    task_id: String,
    status: String,
    prompt: String,
    description: Option<String>,
    created_at: String,
    updated_at: String,
    messages: Vec<String>,
    output: String,
    has_output: bool,
    team_id: Option<String>,
}

// ============================================================================
// Task Functions
// ============================================================================

pub(crate) fn run_task_create(input: TaskCreateInput) -> Result<String, String> {
    let registry = global_task_registry();
    let task = registry.create(&input.prompt, input.description.as_deref());
    to_pretty_json(json!({
        "task_id": task.task_id,
        "status": task.status,
        "prompt": task.prompt,
        "description": task.description,
        "task_packet": task.task_packet,
        "created_at": task.created_at
    }))
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_task_packet(input: runtime::task_registry::TaskPacket) -> Result<String, String> {
    let registry = global_task_registry();
    let task = registry
        .create_from_packet(input)
        .map_err(|error| error.to_string())?;

    to_pretty_json(json!({
        "task_id": task.task_id,
        "status": task.status,
        "prompt": task.prompt,
        "description": task.description,
        "task_packet": task.task_packet,
        "created_at": task.created_at
    }))
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_task_get(input: TaskIdInput) -> Result<String, String> {
    let registry = global_task_registry();
    match registry.get(&input.task_id) {
        Some(task) => to_pretty_json(json!({
            "task_id": task.task_id,
            "status": task.status,
            "prompt": task.prompt,
            "description": task.description,
            "task_packet": task.task_packet,
            "created_at": task.created_at,
            "updated_at": task.updated_at,
            "messages": task.messages,
            "team_id": task.team_id
        })),
        None => Err(format!("task not found: {}", input.task_id)),
    }
}

pub(crate) fn run_task_list(_input: Value) -> Result<String, String> {
    let registry = global_task_registry();
    let tasks: Vec<_> = registry
        .list(None)
        .into_iter()
        .map(|t| {
            json!({
                "task_id": t.task_id,
                "status": t.status,
                "prompt": t.prompt,
                "description": t.description,
                "task_packet": t.task_packet,
                "created_at": t.created_at,
                "updated_at": t.updated_at,
                "team_id": t.team_id
            })
        })
        .collect();
    to_pretty_json(json!({
        "tasks": tasks,
        "count": tasks.len()
    }))
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_task_stop(input: TaskIdInput) -> Result<String, String> {
    let registry = global_task_registry();
    match registry.stop(&input.task_id) {
        Ok(task) => to_pretty_json(json!({
            "task_id": task.task_id,
            "status": task.status,
            "message": "Task stopped"
        })),
        Err(e) => Err(e),
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_task_update(input: TaskUpdateInput) -> Result<String, String> {
    let registry = global_task_registry();
    match registry.update(&input.task_id, &input.message) {
        Ok(task) => to_pretty_json(json!({
            "task_id": task.task_id,
            "status": task.status,
            "message_count": task.messages.len(),
            "last_message": input.message
        })),
        Err(e) => Err(e),
    }
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_task_output(input: TaskOutputInput) -> Result<String, String> {
    let registry = global_task_registry();
    let started_at = Instant::now();
    agent_debug_log(
        "task_output.begin",
        format!(
            "task_id={} block={} timeout_ms={}",
            input.task_id, input.block, input.timeout_ms
        ),
    );
    let task = if input.block {
        registry
            .wait_for_terminal(&input.task_id, Duration::from_millis(input.timeout_ms))
            .map_err(|error| error.to_string())?
    } else {
        registry
            .get(&input.task_id)
            .ok_or_else(|| format!("task not found: {}", input.task_id))?
    };

    let retrieval_status = if matches!(task.status, TaskStatus::Created | TaskStatus::Running) {
        if input.block {
            "timeout"
        } else {
            "not_ready"
        }
    } else {
        "success"
    };
    agent_debug_log(
        "task_output.done",
        format!(
            "task_id={} retrieval_status={} task_status={} output_len={} elapsed_ms={}",
            task.task_id,
            retrieval_status,
            task.status,
            task.output.len(),
            started_at.elapsed().as_millis()
        ),
    );
    let task_payload = task_output_payload(task);

    to_pretty_json(json!({
        "task_id": task_payload.task_id,
        "output": task_payload.output,
        "has_output": task_payload.has_output,
        "retrieval_status": retrieval_status,
        "task": task_payload
    }))
}

fn task_output_payload(task: runtime::task_registry::Task) -> TaskOutputPayload {
    let runtime::task_registry::Task {
        task_id,
        prompt,
        description,
        status,
        created_at,
        updated_at,
        messages,
        output,
        team_id,
        ..
    } = task;
    let has_output = !output.is_empty();
    TaskOutputPayload {
        task_id,
        status: status.to_string(),
        prompt,
        description,
        created_at,
        updated_at,
        messages,
        output,
        has_output,
        team_id,
    }
}

// ============================================================================
// Worker Functions
// ============================================================================

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_worker_create(input: WorkerCreateInput) -> Result<String, String> {
    // Merge config-level trusted_roots with per-call overrides.
    // Config provides the default allowlist; per-call roots add on top.
    let config_roots: Vec<String> = ConfigLoader::default_for(&input.cwd)
        .load()
        .ok()
        .map(|c| c.trusted_roots().to_vec())
        .unwrap_or_default();
    let merged_roots: Vec<String> = config_roots
        .into_iter()
        .chain(input.trusted_roots.iter().cloned())
        .collect();
    let worker = global_worker_registry().create(
        &input.cwd,
        &merged_roots,
        input.auto_recover_prompt_misdelivery,
    );
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_worker_get(input: WorkerIdInput) -> Result<String, String> {
    global_worker_registry().get(&input.worker_id).map_or_else(
        || Err(format!("worker not found: {}", input.worker_id)),
        to_pretty_json,
    )
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_worker_observe(input: WorkerObserveInput) -> Result<String, String> {
    let worker = global_worker_registry().observe(&input.worker_id, &input.screen_text)?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_worker_resolve_trust(input: WorkerIdInput) -> Result<String, String> {
    let worker = global_worker_registry().resolve_trust(&input.worker_id)?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_worker_await_ready(input: WorkerIdInput) -> Result<String, String> {
    let snapshot: WorkerReadySnapshot = global_worker_registry().await_ready(&input.worker_id)?;
    to_pretty_json(snapshot)
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_worker_send_prompt(input: WorkerSendPromptInput) -> Result<String, String> {
    let worker = global_worker_registry().send_prompt(&input.worker_id, input.prompt.as_deref())?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_worker_restart(input: WorkerIdInput) -> Result<String, String> {
    let worker = global_worker_registry().restart(&input.worker_id)?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_worker_terminate(input: WorkerIdInput) -> Result<String, String> {
    let worker = global_worker_registry().terminate(&input.worker_id)?;
    to_pretty_json(worker)
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_worker_observe_completion(input: WorkerObserveCompletionInput) -> Result<String, String> {
    let worker = global_worker_registry().observe_completion(
        &input.worker_id,
        &input.finish_reason,
        input.tokens_output,
    )?;
    to_pretty_json(worker)
}

// ============================================================================
// Team Functions
// ============================================================================

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_team_create(input: TeamCreateInput) -> Result<String, String> {
    let task_ids: Vec<String> = input
        .tasks
        .iter()
        .filter_map(|t| t.get("task_id").and_then(|v| v.as_str()).map(str::to_owned))
        .collect();
    let team = global_team_registry().create(&input.name, task_ids);
    // Register team assignment on each task
    for task_id in &team.task_ids {
        let _ = global_task_registry().assign_team(task_id, &team.team_id);
    }
    to_pretty_json(json!({
        "team_id": team.team_id,
        "name": team.name,
        "task_count": team.task_ids.len(),
        "task_ids": team.task_ids,
        "status": team.status,
        "created_at": team.created_at
    }))
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn run_team_delete(input: TeamDeleteInput) -> Result<String, String> {
    match global_team_registry().delete(&input.team_id) {
        Ok(team) => to_pretty_json(json!({
            "team_id": team.team_id,
            "name": team.name,
            "status": team.status,
            "message": "Team deleted"
        })),
        Err(e) => Err(e),
    }
}
