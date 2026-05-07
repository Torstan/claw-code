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

        let handles: Vec<std::thread::JoinHandle<Result<String, ToolError>>> = invocations
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
                    let worker = CliToolExecutor {
                        renderer: TerminalRenderer::new(),
                        emit_output: false,
                        allowed_tools,
                        tool_registry,
                        mcp_state,
                    };
                    let result = with_active_tool_session(session_id.as_deref(), || {
                        worker.execute_raw(&invocation.tool_name, &invocation.input)
                    });
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

        let results = handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(result) => result,
                Err(_) => Err(ToolError::new("tool execution thread panicked")),
            })
            .collect::<Vec<_>>();

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
