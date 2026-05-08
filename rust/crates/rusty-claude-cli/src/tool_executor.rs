use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use runtime::{
    active_tool_session_id, with_active_tool_session, ToolError, ToolExecutor, ToolInvocation,
};
use serde::Deserialize;
use serde_json::Value;
use tools::GlobalToolRegistry;

use crate::mcp_runtime::RuntimeMcpState;
use crate::provider_client::{debug_json_input_summary, normalize_tool_input_json};
use crate::render::TerminalRenderer;
use crate::tool_display::format_tool_result;
use crate::AllowedToolSet;

const DEFAULT_PARALLEL_AGENT_LIMIT: usize = 4;
const PARALLEL_AGENT_LIMIT_ENV_VAR: &str = "CLAWD_AGENT_MAX_CONCURRENCY";

#[derive(Debug, Deserialize)]
struct ToolSearchRequest {
    query: String,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct McpToolRequest {
    #[serde(rename = "qualifiedName")]
    qualified_name: Option<String>,
    tool: Option<String>,
    arguments: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ListMcpResourcesRequest {
    server: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReadMcpResourceRequest {
    server: String,
    uri: String,
}

pub(crate) struct CliToolExecutor {
    renderer: TerminalRenderer,
    emit_output: bool,
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
}

impl CliToolExecutor {
    pub(crate) fn new(
        allowed_tools: Option<AllowedToolSet>,
        emit_output: bool,
        tool_registry: GlobalToolRegistry,
        mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    ) -> Self {
        Self {
            renderer: TerminalRenderer::new(),
            emit_output,
            allowed_tools,
            tool_registry,
            mcp_state,
        }
    }

    fn execute_search_tool(&self, value: serde_json::Value) -> Result<String, ToolError> {
        let input: ToolSearchRequest = serde_json::from_value(value)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        let (pending_mcp_servers, mcp_degraded) =
            self.mcp_state.as_ref().map_or((None, None), |state| {
                let state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (state.pending_servers(), state.degraded_report())
            });
        serde_json::to_string_pretty(&self.tool_registry.search(
            &input.query,
            input.max_results.unwrap_or(5),
            pending_mcp_servers,
            mcp_degraded,
        ))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    fn execute_runtime_tool(
        &self,
        tool_name: &str,
        value: serde_json::Value,
    ) -> Result<String, ToolError> {
        let Some(mcp_state) = &self.mcp_state else {
            return Err(ToolError::new(format!(
                "runtime tool `{tool_name}` is unavailable without configured MCP servers"
            )));
        };
        let mut mcp_state = mcp_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        match tool_name {
            "MCPTool" => {
                let input: McpToolRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                let qualified_name = input
                    .qualified_name
                    .or(input.tool)
                    .ok_or_else(|| ToolError::new("missing required field `qualifiedName`"))?;
                mcp_state.call_tool(&qualified_name, input.arguments)
            }
            "ListMcpResourcesTool" => {
                let input: ListMcpResourcesRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                match input.server {
                    Some(server_name) => mcp_state.list_resources_for_server(&server_name),
                    None => mcp_state.list_resources_for_all_servers(),
                }
            }
            "ReadMcpResourceTool" => {
                let input: ReadMcpResourceRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                mcp_state.read_resource(&input.server, &input.uri)
            }
            _ => mcp_state.call_tool(tool_name, Some(value)),
        }
    }

    pub(crate) fn execute_raw(&self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        let started_at = Instant::now();
        let normalized_input = normalize_tool_input_json(input);
        cli_agent_debug_log(
            "tool.execute.begin",
            format!("tool_name={tool_name}\ninput={normalized_input}"),
        );
        if self
            .allowed_tools
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(tool_name))
        {
            let error = ToolError::new(format!(
                "tool `{tool_name}` is not enabled by the current --allowedTools setting"
            ));
            cli_agent_debug_log(
                "tool.execute.done",
                format!(
                    "tool_name={tool_name}\nok=false\nelapsed_us={}\nerror={error}",
                    started_at.elapsed().as_micros()
                ),
            );
            return Err(error);
        }
        let value = match serde_json::from_str(normalized_input) {
            Ok(value) => value,
            Err(error) => {
                let error = ToolError::new(format!("invalid tool input JSON: {error}"));
                cli_agent_debug_log(
                    "tool.execute.input_json_parse_error",
                    format!(
                        "tool_name={tool_name}\n{}\nerror={error}",
                        debug_json_input_summary(normalized_input, 500)
                    ),
                );
                cli_agent_debug_log(
                    "tool.execute.done",
                    format!(
                        "tool_name={tool_name}\nok=false\nelapsed_us={}\nerror={error}",
                        started_at.elapsed().as_micros()
                    ),
                );
                return Err(error);
            }
        };
        let result = if tool_name == "ToolSearch" {
            self.execute_search_tool(value)
        } else if self.tool_registry.has_runtime_tool(tool_name) {
            self.execute_runtime_tool(tool_name, value)
        } else {
            self.tool_registry
                .execute(tool_name, &value)
                .map_err(ToolError::new)
        };
        match &result {
            Ok(output) => cli_agent_debug_log(
                "tool.execute.done",
                format!(
                    "tool_name={tool_name}\nok=true\nelapsed_us={}\noutput={output}",
                    started_at.elapsed().as_micros()
                ),
            ),
            Err(error) => cli_agent_debug_log(
                "tool.execute.done",
                format!(
                    "tool_name={tool_name}\nok=false\nelapsed_us={}\nerror={error}",
                    started_at.elapsed().as_micros()
                ),
            ),
        }
        result
    }

    fn render_result(
        &self,
        tool_name: &str,
        result: &Result<String, ToolError>,
    ) -> Result<(), ToolError> {
        if !self.emit_output {
            return Ok(());
        }
        let (markdown, is_error) = match result {
            Ok(output) => (format_tool_result(tool_name, output, false), false),
            Err(error) => (
                format_tool_result(tool_name, &error.to_string(), true),
                true,
            ),
        };
        self.renderer
            .stream_markdown(&markdown, &mut io::stdout())
            .map_err(|error| {
                let label = if is_error {
                    "failed to render tool error"
                } else {
                    "failed to render tool result"
                };
                ToolError::new(format!("{label}: {error}"))
            })
    }
}

#[track_caller]
fn cli_agent_debug_log(event: &str, detail: impl AsRef<str>) {
    runtime::agent_debug_log(event, detail);
}

fn parallel_agent_limit() -> usize {
    std::env::var(PARALLEL_AGENT_LIMIT_ENV_VAR)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_PARALLEL_AGENT_LIMIT)
}

fn describe_parallel_invocation(invocation: &ToolInvocation) -> String {
    if invocation.tool_name != "Agent" {
        return format!("tool={}", invocation.tool_name);
    }

    let Ok(value) = serde_json::from_str::<Value>(&invocation.input) else {
        return format!("tool={} input_parse_error", invocation.tool_name);
    };
    format!(
        "tool={} description={:?} name={:?} background={} prompt_len={}",
        invocation.tool_name,
        value.get("description").and_then(Value::as_str),
        value.get("name").and_then(Value::as_str),
        value
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        value
            .get("prompt")
            .and_then(Value::as_str)
            .map_or(0, str::len)
    )
}

#[cfg(test)]
type TestParallelChunkObserver = Arc<dyn Fn(&[ToolInvocation]) + Send + Sync + 'static>;

#[cfg(test)]
fn test_parallel_chunk_observer() -> &'static Mutex<Option<TestParallelChunkObserver>> {
    static OBSERVER: std::sync::OnceLock<Mutex<Option<TestParallelChunkObserver>>> =
        std::sync::OnceLock::new();
    OBSERVER.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
type TestParallelInvocationExecutor =
    Arc<dyn Fn(&ToolInvocation) -> Result<String, ToolError> + Send + Sync + 'static>;

#[cfg(test)]
fn test_parallel_invocation_executor() -> &'static Mutex<Option<TestParallelInvocationExecutor>> {
    static EXECUTOR: std::sync::OnceLock<Mutex<Option<TestParallelInvocationExecutor>>> =
        std::sync::OnceLock::new();
    EXECUTOR.get_or_init(|| Mutex::new(None))
}

fn execute_parallel_invocation(
    invocation: &ToolInvocation,
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    session_id: Option<&str>,
) -> Result<String, ToolError> {
    #[cfg(test)]
    if let Some(executor) = test_parallel_invocation_executor()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        return executor(invocation);
    }

    let worker = CliToolExecutor {
        renderer: TerminalRenderer::new(),
        emit_output: false,
        allowed_tools,
        tool_registry,
        mcp_state,
    };
    with_active_tool_session(session_id, || {
        worker.execute_raw(&invocation.tool_name, &invocation.input)
    })
}

fn execute_parallel_chunk(
    invocations: &[ToolInvocation],
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    session_id: Option<String>,
) -> Vec<Result<String, ToolError>> {
    #[cfg(test)]
    if let Some(observer) = test_parallel_chunk_observer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
    {
        observer(invocations);
    }

    let handles = invocations
        .iter()
        .map(|invocation| {
            let invocation = invocation.clone();
            let allowed_tools = allowed_tools.clone();
            let tool_registry = tool_registry.clone();
            let mcp_state = mcp_state.clone();
            let session_id = session_id.clone();
            std::thread::spawn(move || {
                let invocation_started_at = Instant::now();
                cli_agent_debug_log(
                    "tool.execute_many.worker.begin",
                    format!(
                        "tool_name={} description={}",
                        invocation.tool_name,
                        describe_parallel_invocation(&invocation)
                    ),
                );
                let result = execute_parallel_invocation(
                    &invocation,
                    allowed_tools,
                    tool_registry,
                    mcp_state,
                    session_id.as_deref(),
                );
                cli_agent_debug_log(
                    "tool.execute_many.worker.done",
                    format!(
                        "tool_name={} description={} ok={} elapsed_us={}",
                        invocation.tool_name,
                        describe_parallel_invocation(&invocation),
                        result.is_ok(),
                        invocation_started_at.elapsed().as_micros()
                    ),
                );
                result
            })
        })
        .collect::<Vec<_>>();

    handles
        .into_iter()
        .map(|handle| match handle.join() {
            Ok(result) => result,
            Err(_) => Err(ToolError::new("tool execution thread panicked")),
        })
        .collect()
}

impl ToolExecutor for CliToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        let result = self.execute_raw(tool_name, input);
        self.render_result(tool_name, &result)?;
        result
    }

    #[allow(clippy::too_many_lines)]
    fn execute_many(&mut self, invocations: &[ToolInvocation]) -> Vec<Result<String, ToolError>> {
        if invocations.len() < 2
            || !invocations
                .iter()
                .all(|invocation| self.supports_parallel_execution(&invocation.tool_name))
        {
            return invocations
                .iter()
                .map(|invocation| self.execute(&invocation.tool_name, &invocation.input))
                .collect();
        }

        let batch_started_at = Instant::now();
        cli_agent_debug_log(
            "tool.execute_many.parallel.begin",
            format!(
                "invocations={} session_id={:?} summary={}",
                invocations.len(),
                active_tool_session_id(),
                invocations
                    .iter()
                    .map(describe_parallel_invocation)
                    .collect::<Vec<_>>()
                    .join(" | ")
            ),
        );
        let session_id = active_tool_session_id();
        let allowed_tools = self.allowed_tools.clone();
        let tool_registry = self.tool_registry.clone();
        let mcp_state = self.mcp_state.clone();
        let emit_output = self.emit_output;

        let parallel_limit = parallel_agent_limit();
        let mut results = Vec::with_capacity(invocations.len());
        for chunk in invocations.chunks(parallel_limit) {
            results.extend(execute_parallel_chunk(
                chunk,
                allowed_tools.clone(),
                tool_registry.clone(),
                mcp_state.clone(),
                session_id.clone(),
            ));
        }

        cli_agent_debug_log(
            "tool.execute_many.parallel.done",
            format!(
                "invocations={} ok_count={} err_count={} elapsed_us={}",
                invocations.len(),
                results.iter().filter(|result| result.is_ok()).count(),
                results.iter().filter(|result| result.is_err()).count(),
                batch_started_at.elapsed().as_micros()
            ),
        );

        if emit_output {
            for (index, (invocation, result)) in invocations.iter().zip(results.iter()).enumerate()
            {
                if let Err(error) = self.render_result(&invocation.tool_name, result) {
                    return invocations
                        .iter()
                        .zip(results.into_iter())
                        .enumerate()
                        .map(|(result_index, (_invocation, result))| {
                            if result_index == index {
                                Err(error.clone())
                            } else {
                                result
                            }
                        })
                        .collect();
                }
            }
        }

        results
    }

    fn supports_parallel_execution(&self, tool_name: &str) -> bool {
        tool_name == "Agent"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    struct TestParallelChunkObserverGuard;

    impl TestParallelChunkObserverGuard {
        fn set(observer: impl Fn(&[ToolInvocation]) + Send + Sync + 'static) -> Self {
            *test_parallel_chunk_observer()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(observer));
            Self
        }
    }

    impl Drop for TestParallelChunkObserverGuard {
        fn drop(&mut self) {
            *test_parallel_chunk_observer()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }

    struct TestParallelInvocationExecutorGuard;

    impl TestParallelInvocationExecutorGuard {
        fn set(
            executor: impl Fn(&ToolInvocation) -> Result<String, ToolError> + Send + Sync + 'static,
        ) -> Self {
            *test_parallel_invocation_executor()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(executor));
            Self
        }
    }

    impl Drop for TestParallelInvocationExecutorGuard {
        fn drop(&mut self) {
            *test_parallel_invocation_executor()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }

    fn debug_temp_dir(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        std::env::temp_dir().join(format!("clawd-cli-tool-executor-{name}-{unique}"))
    }

    #[test]
    fn confirms_issue_16_tool_debug_log_redacts_secret_shaped_values() {
        let _lock = env_lock();
        let debug_dir = debug_temp_dir("issue-16");
        std::fs::create_dir_all(&debug_dir).expect("debug dir should create");
        let _debug_env = EnvVarGuard::set("CLAWD_AGENT_DEBUG", &debug_dir);
        cli_agent_debug_log(
            "tool.execute.done",
            "tool_name=bash\nok=true\noutput=ANTHROPIC_API_KEY=sk-ant-secret-value",
        );

        let log_path = debug_dir.join("clawd-agent-debug.log");
        let log = std::fs::read_to_string(&log_path).expect("debug log should exist");
        let leaked_secret = log.contains("sk-ant-secret-value");
        let _ = std::fs::remove_dir_all(&debug_dir);

        assert!(
            !leaked_secret,
            "debug logs must redact secret-shaped values before writing to disk"
        );
        assert!(log.contains("[REDACTED_SECRET]"));
    }

    #[test]
    fn parallel_agent_limit_uses_env_override() {
        let _lock = env_lock();
        let _limit_env = EnvVarGuard::set(PARALLEL_AGENT_LIMIT_ENV_VAR, "2");
        assert_eq!(parallel_agent_limit(), 2);
    }

    #[test]
    fn parallel_agent_limit_chunks_agent_invocations_and_preserves_result_order() {
        let _lock = env_lock();
        let _limit_env = EnvVarGuard::set(PARALLEL_AGENT_LIMIT_ENV_VAR, "2");
        let observed_chunks = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let _observer_guard = TestParallelChunkObserverGuard::set({
            let observed_chunks = Arc::clone(&observed_chunks);
            move |chunk| {
                observed_chunks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(
                        chunk
                            .iter()
                            .map(|invocation| invocation.input.clone())
                            .collect(),
                    );
            }
        });
        let _executor_guard = TestParallelInvocationExecutorGuard::set(|invocation| {
            let value = serde_json::from_str::<Value>(&invocation.input)
                .expect("test invocations should be valid JSON");
            let description = value
                .get("description")
                .and_then(Value::as_str)
                .expect("test invocation should include a description");
            Ok(format!("result:{description}"))
        });

        let invocations = [
            ToolInvocation {
                tool_name: "Agent".to_string(),
                input: r#"{"description":"first","prompt":"first prompt"}"#.to_string(),
            },
            ToolInvocation {
                tool_name: "Agent".to_string(),
                input: r#"{"description":"second","prompt":"second prompt"}"#.to_string(),
            },
            ToolInvocation {
                tool_name: "Agent".to_string(),
                input: r#"{"description":"third","prompt":"third prompt"}"#.to_string(),
            },
        ];
        let mut executor = CliToolExecutor::new(None, false, GlobalToolRegistry::builtin(), None);

        let results = executor.execute_many(&invocations);

        assert_eq!(parallel_agent_limit(), 2);
        assert_eq!(
            *observed_chunks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![
                vec![invocations[0].input.clone(), invocations[1].input.clone()],
                vec![invocations[2].input.clone()]
            ]
        );
        assert_eq!(
            results
                .into_iter()
                .map(|result| result.expect("test executor should succeed"))
                .collect::<Vec<_>>(),
            vec!["result:first", "result:second", "result:third"]
        );
    }
}
