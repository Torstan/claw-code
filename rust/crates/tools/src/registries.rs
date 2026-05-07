use std::sync::OnceLock;

use runtime::{
    lsp_client::LspRegistry,
    mcp_tool_bridge::McpToolRegistry,
    task_registry::TaskRegistry,
    team_cron_registry::{CronRegistry, TeamRegistry},
    worker_boot::WorkerRegistry,
};

pub(crate) fn global_lsp_registry() -> &'static LspRegistry {
    static REGISTRY: OnceLock<LspRegistry> = OnceLock::new();
    REGISTRY.get_or_init(LspRegistry::new)
}

pub(crate) fn global_mcp_registry() -> &'static McpToolRegistry {
    static REGISTRY: OnceLock<McpToolRegistry> = OnceLock::new();
    REGISTRY.get_or_init(McpToolRegistry::new)
}

pub(crate) fn global_team_registry() -> &'static TeamRegistry {
    static REGISTRY: OnceLock<TeamRegistry> = OnceLock::new();
    REGISTRY.get_or_init(TeamRegistry::new)
}

pub(crate) fn global_cron_registry() -> &'static CronRegistry {
    static REGISTRY: OnceLock<CronRegistry> = OnceLock::new();
    REGISTRY.get_or_init(CronRegistry::new)
}

/// Global task registry shared across tool invocations within a session.
pub(crate) fn global_task_registry() -> &'static TaskRegistry {
    static REGISTRY: OnceLock<TaskRegistry> = OnceLock::new();
    REGISTRY.get_or_init(TaskRegistry::new)
}

pub(crate) fn global_worker_registry() -> &'static WorkerRegistry {
    static REGISTRY: OnceLock<WorkerRegistry> = OnceLock::new();
    REGISTRY.get_or_init(WorkerRegistry::new)
}

#[track_caller]
pub(crate) fn agent_debug_log(event: &str, detail: impl AsRef<str>) {
    runtime::agent_debug_log(event, detail);
}
