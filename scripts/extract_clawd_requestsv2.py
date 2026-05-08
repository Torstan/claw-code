#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import re
import sys
from collections import defaultdict, deque
from pathlib import Path
from typing import Any


DEFAULT_LOG_PATH = "/tmp/clawd-debug/clawd-agent-debug.log"
ANTHROPIC_BREAKPOINT_LOOKBACK_BLOCKS = 20
CACHE_CREATION_COST_MULTIPLIER = 1.25
CACHE_READ_COST_MULTIPLIER = 0.1

ENTRY_RE = re.compile(
    r"(\[clawd-agent-debug ts=.*?)(?=\[clawd-agent-debug ts=|\Z)",
    re.S,
)

ENTRY_HEADER_RE = re.compile(
    r"^\[clawd-agent-debug ts=(?P<ts>.+?) "
    r"pid=(?P<pid>\d+) "
    r"thread=(?P<thread>.*?) "
    r"tid=(?P<tid>ThreadId\(\d+\)) "
    r"caller=(?P<caller>[^\]]+)\] "
    r"(?P<event>\S+)"
    r"(?: (?P<detail>.*))?$",
    re.S,
)

TS_RE = re.compile(r"\[clawd-agent-debug ts=(?P<ts>.+?) pid=")
THREAD_AGENT_RE = re.compile(r"thread=clawd-agent-(?P<agent_id>agent-\d+)")

REQUEST_RE = re.compile(
    r"\] llm\.request "
    r"session_id=(?P<session_id>\S+) "
    r"iteration=(?P<iteration>\d+)"
    r".*?request=(?P<llm_req>ApiRequest \{.*\})\s*$",
    re.S,
)

RESPONSE_RE = re.compile(
    r"\] llm\.response "
    r"session_id=(?P<session_id>\S+) "
    r"iteration=(?P<iteration>\d+)"
    r".*?response=(?P<llm_resp>\[.*\])\s*$",
    re.S,
)

USAGE_RE = re.compile(
    r"Usage\(TokenUsage \{ "
    r"input_tokens: (?P<input_tokens>\d+), "
    r"output_tokens: (?P<output_tokens>\d+), "
    r"cache_creation_input_tokens: (?P<cache_creation_input_tokens>\d+), "
    r"cache_read_input_tokens: (?P<cache_read_input_tokens>\d+) "
    r"\}\)"
)

PROMPT_CACHE_RE = re.compile(
    r"PromptCache\(PromptCacheEvent \{ (?P<body>.*?) \}\)"
)

PROMPT_CACHE_FIELD_RE = re.compile(
    r'(?P<key>[a-zA-Z_][a-zA-Z0-9_]*): '
    r'(?P<value>true|false|-?\d+|"(?:\\.|[^"\\])*")'
)

PROMPT_CACHE_SUMMARY_RE = re.compile(
    r"\] (?:cli|agent)\.provider\.stream\.prompt_cache (?P<body>.*)\s*$",
    re.S,
)

PROMPT_CACHE_USAGE_RE = re.compile(
    r"\] (?:cli|agent)\.provider\.stream\.usage (?P<body>.*)\s*$",
    re.S,
)

PROMPT_CACHE_FINGERPRINT_RE = re.compile(
    r"\] cli\.provider\.stream\.prompt_cache_fingerprint (?P<body>.*)\s*$",
    re.S,
)

PROMPT_CACHE_BREAK_RE = re.compile(
    r"\] cli\.provider\.stream\.prompt_cache_break (?P<body>.*)\s*$",
    re.S,
)

PROMPT_CACHE_BLOCKS_RE = re.compile(
    r"\] (?:cli|agent)\.provider\.stream\.prompt_cache_blocks (?P<body>.*)\s*$",
    re.S,
)

TOOL_RESULT_SIZES_RE = re.compile(
    r"\] (?:cli|agent)\.provider\.stream\.tool_result_sizes (?P<body>.*)\s*$",
    re.S,
)

TOOL_STREAM_EVENTS = {
    "cli.provider.stream.tool_start",
    "cli.provider.stream.tool_input_delta",
    "cli.provider.stream.tool_stop",
    "agent.provider.stream.tool_start",
    "agent.provider.stream.tool_input_delta",
    "agent.provider.stream.tool_stop",
}

TOOL_INPUT_PARSE_ERROR_EVENTS = {
    "tool.execute.input_json_parse_error",
    "subagent.tool.execute.input_json_parse_error",
}


class RustDebugParser:
    IDENT_RE = re.compile(
        r"[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*"
    )

    def __init__(self, text: str) -> None:
        self.text = text
        self.length = len(text)
        self.pos = 0

    def parse(self) -> Any:
        value = self.parse_value()
        self.skip_ws()
        if self.pos != self.length:
            raise ValueError(f"unexpected trailing input at position {self.pos}")
        return value

    def skip_ws(self) -> None:
        while self.pos < self.length and self.text[self.pos].isspace():
            self.pos += 1

    def peek(self) -> str | None:
        if self.pos >= self.length:
            return None
        return self.text[self.pos]

    def consume(self, expected: str) -> None:
        self.skip_ws()
        if not self.text.startswith(expected, self.pos):
            raise ValueError(f"expected {expected!r} at position {self.pos}")
        self.pos += len(expected)

    def parse_value(self) -> Any:
        self.skip_ws()
        ch = self.peek()
        if ch is None:
            raise ValueError("unexpected end of input")
        if ch == '"':
            return self.parse_string()
        if ch == "[":
            return self.parse_list()
        if ch == "-" or ch.isdigit():
            return self.parse_number()

        ident = self.parse_identifier()
        if ident == "None":
            return None
        if ident == "true":
            return True
        if ident == "false":
            return False

        self.skip_ws()
        if self.peek() == "{":
            return self.parse_struct(ident)
        if self.peek() == "(":
            return self.parse_tuple_variant(ident)
        return ident

    def parse_identifier(self) -> str:
        self.skip_ws()
        match = self.IDENT_RE.match(self.text, self.pos)
        if not match:
            raise ValueError(f"expected identifier at position {self.pos}")
        self.pos = match.end()
        return match.group(0)

    def parse_string(self) -> str:
        self.skip_ws()
        start = self.pos
        self.pos += 1
        escaped = False

        while self.pos < self.length:
            ch = self.text[self.pos]
            if ch == '"' and not escaped:
                self.pos += 1
                return json.loads(self.text[start:self.pos])
            if ch == "\\" and not escaped:
                escaped = True
                self.pos += 1
                continue
            escaped = False
            self.pos += 1

        raise ValueError("unterminated string")

    def parse_number(self) -> int:
        self.skip_ws()
        start = self.pos
        if self.text[self.pos] == "-":
            self.pos += 1
        while self.pos < self.length and self.text[self.pos].isdigit():
            self.pos += 1
        return int(self.text[start:self.pos])

    def parse_list(self) -> list[Any]:
        items: list[Any] = []
        self.consume("[")
        while True:
            self.skip_ws()
            if self.peek() == "]":
                self.pos += 1
                return items
            items.append(self.parse_value())
            self.skip_ws()
            if self.peek() == ",":
                self.pos += 1
                continue
            if self.peek() == "]":
                self.pos += 1
                return items
            raise ValueError(f"expected ',' or ']' at position {self.pos}")

    def parse_struct(self, name: str) -> dict[str, Any]:
        fields: dict[str, Any] = {}
        self.consume("{")
        while True:
            self.skip_ws()
            if self.peek() == "}":
                self.pos += 1
                break
            key = self.parse_identifier()
            self.consume(":")
            fields[key] = self.parse_value()
            self.skip_ws()
            if self.peek() == ",":
                self.pos += 1
                continue
            if self.peek() == "}":
                self.pos += 1
                break
            raise ValueError(f"expected ',' or '}}' at position {self.pos}")

        if name == "ApiRequest":
            return fields
        if "type" not in fields:
            return {"type": name, **fields}
        return {"debug_type": name, **fields}

    def parse_tuple_variant(self, name: str) -> Any:
        values: list[Any] = []
        self.consume("(")
        while True:
            self.skip_ws()
            if self.peek() == ")":
                self.pos += 1
                break
            values.append(self.parse_value())
            self.skip_ws()
            if self.peek() == ",":
                self.pos += 1
                continue
            if self.peek() == ")":
                self.pos += 1
                break
            raise ValueError(f"expected ',' or ')' at position {self.pos}")

        if name == "Some":
            return values[0] if values else None
        if name == "Ok":
            return values[0] if values else None
        if name == "Err":
            return {"type": "Err", "value": values[0] if len(values) == 1 else values}
        if len(values) == 1:
            return {"type": name, "value": values[0]}
        return {"type": name, "values": values}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Extract clawd request/response data with JSON-formatted llm_req."
    )
    parser.add_argument(
        "log_path",
        nargs="?",
        default=DEFAULT_LOG_PATH,
        help=f"Path to the clawd debug log. Default: {DEFAULT_LOG_PATH}",
    )
    parser.add_argument(
        "-o",
        "--output",
        help="Write JSON output to this file. Defaults to stdout.",
    )
    parser.add_argument(
        "--summary-output",
        help=(
            "Write a compact JSON summary with cache marker coverage and "
            "20-block lookback risks to this file."
        ),
    )
    return parser.parse_args()


def read_log(path: str) -> str:
    if path == "-":
        return sys.stdin.read()
    return Path(path).read_text(encoding="utf-8", errors="replace")


def split_entries(text: str) -> list[str]:
    return [match.group(1).strip() for match in ENTRY_RE.finditer(text)]


def parse_entry_header(entry: str) -> dict[str, str] | None:
    match = ENTRY_HEADER_RE.match(entry)
    if not match:
        return None
    return {
        key: value
        for key, value in match.groupdict().items()
        if value is not None
    }


def group_entries(entries: list[str]) -> list[str]:
    grouped: list[str] = []
    current_header: dict[str, str] | None = None
    current_details: list[str] = []

    def flush() -> None:
        nonlocal current_header, current_details
        if current_header is None:
            return
        prefix = (
            f"[clawd-agent-debug ts={current_header['ts']} "
            f"pid={current_header['pid']} "
            f"thread={current_header['thread']} "
            f"tid={current_header['tid']} "
            f"caller={current_header['caller']}] "
            f"{current_header['event']}"
        )
        if current_details:
            grouped.append(f"{prefix} " + "\n".join(current_details))
        else:
            grouped.append(prefix)
        current_header = None
        current_details = []

    for entry in entries:
        header = parse_entry_header(entry)
        if header is None:
            flush()
            grouped.append(entry)
            continue

        signature = (
            header["ts"],
            header["pid"],
            header["thread"],
            header["tid"],
            header["caller"],
            header["event"],
        )
        current_signature = None
        if current_header is not None:
            current_signature = (
                current_header["ts"],
                current_header["pid"],
                current_header["thread"],
                current_header["tid"],
                current_header["caller"],
                current_header["event"],
            )
        if current_signature != signature:
            flush()
            current_header = header

        current_details.append(header.get("detail", ""))

    flush()
    return grouped


def parse_timestamp(entry: str) -> str | None:
    match = TS_RE.search(entry)
    return match.group("ts") if match else None


def parse_thread_agent_id(entry: str) -> str | None:
    match = THREAD_AGENT_RE.search(entry)
    return match.group("agent_id") if match else None


def parse_json_value(raw_value: Any) -> Any | None:
    if isinstance(raw_value, (dict, list)):
        return raw_value
    if not isinstance(raw_value, str) or not raw_value:
        return None
    try:
        return json.loads(raw_value)
    except json.JSONDecodeError:
        return None


def parse_rust_debug_value(raw_value: Any) -> Any | None:
    if not isinstance(raw_value, str) or not raw_value:
        return None
    try:
        return RustDebugParser(raw_value).parse()
    except ValueError:
        return None


def slugify(value: str) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", value.lower()).strip("-")
    return slug or "agent"


def normalize_agent_name(raw_name: str | None, prompt: str | None = None) -> str:
    text = " ".join(part for part in (raw_name, prompt) if part)
    lower = text.lower()
    if "quality" in lower:
        return "Code Quality Review"
    if "efficiency" in lower or "efficient" in lower:
        return "Efficiency Review"
    if "reuse" in lower:
        return "Code Reuse Review"
    return raw_name or "Subagent"


def iter_request_blocks(llm_req: Any) -> list[dict[str, Any]]:
    if not isinstance(llm_req, dict):
        return []

    blocks: list[dict[str, Any]] = []
    messages = llm_req.get("messages")
    if not isinstance(messages, list):
        return blocks

    for message in messages:
        if not isinstance(message, dict):
            continue
        message_blocks = message.get("blocks")
        if not isinstance(message_blocks, list):
            continue
        for block in message_blocks:
            if isinstance(block, dict):
                blocks.append(block)
    return blocks


def iter_response_events(llm_resp: Any) -> list[dict[str, Any]]:
    parsed = parse_rust_debug_value(llm_resp)
    if not isinstance(parsed, list):
        return []
    return [event for event in parsed if isinstance(event, dict)]


def request_system_text(llm_req: Any) -> str:
    if not isinstance(llm_req, dict):
        return ""
    system_prompt = llm_req.get("system_prompt")
    if not isinstance(system_prompt, list):
        return ""
    return "\n".join(part for part in system_prompt if isinstance(part, str))


def request_user_text(llm_req: Any) -> str:
    if not isinstance(llm_req, dict):
        return ""
    messages = llm_req.get("messages")
    if not isinstance(messages, list):
        return ""

    parts: list[str] = []
    for message in messages:
        if not isinstance(message, dict) or message.get("role") != "User":
            continue
        blocks = message.get("blocks")
        if not isinstance(blocks, list):
            continue
        for block in blocks:
            if isinstance(block, dict) and block.get("type") == "Text":
                text = block.get("text")
                if isinstance(text, str):
                    parts.append(text)
    return "\n".join(parts)


def is_subagent_request(llm_req: Any) -> bool:
    system_text = request_system_text(llm_req)
    return "You are a background sub-agent" in system_text or (
        "Work only on the delegated task" in system_text
    )


def collect_agent_launch_intents(
    records: list[dict[str, object]],
) -> list[dict[str, object]]:
    launches: list[dict[str, object]] = []

    for record in records:
        for event in iter_response_events(record.get("llm_resp")):
            if event.get("type") != "ToolUse" or event.get("name") != "Agent":
                continue
            tool_use_id = event.get("id")
            tool_input = parse_json_value(event.get("input"))
            if not isinstance(tool_use_id, str) or not isinstance(tool_input, dict):
                continue

            description = tool_input.get("description")
            if not isinstance(description, str):
                description = None
            prompt = tool_input.get("prompt")
            if not isinstance(prompt, str):
                prompt = None
            raw_name = tool_input.get("name")
            if not isinstance(raw_name, str):
                raw_name = description

            agent_name = normalize_agent_name(raw_name, prompt)
            launches.append(
                {
                    "agent_name": agent_name,
                    "agent_launcher_tool_use_id": tool_use_id,
                    "agent_description": description,
                    "agent_prompt": prompt,
                }
            )

    return launches


def find_matching_launch(
    user_text: str, launches: list[dict[str, object]]
) -> dict[str, object] | None:
    for launch in reversed(launches):
        prompt = launch.get("agent_prompt")
        if not isinstance(prompt, str) or not prompt:
            continue
        prompt_head = prompt[:200]
        user_head = user_text[:200]
        if (prompt_head and prompt_head in user_text) or (
            user_head and user_head in prompt
        ):
            return launch
    return None


def collect_agent_launches(records: list[dict[str, object]]) -> dict[str, dict[str, object]]:
    tool_inputs: dict[str, dict[str, object]] = {}
    launches: dict[str, dict[str, object]] = {}

    for record in records:
        for block in iter_request_blocks(record.get("llm_req")):
            block_type = block.get("type")

            if block_type == "ToolUse" and block.get("name") == "Agent":
                tool_use_id = block.get("id")
                tool_input = parse_json_value(block.get("input"))
                if isinstance(tool_use_id, str) and isinstance(tool_input, dict):
                    tool_inputs[tool_use_id] = tool_input
                continue

            if block_type != "ToolResult" or block.get("tool_name") != "Agent":
                continue

            tool_use_id = block.get("tool_use_id")
            output = parse_json_value(block.get("output"))
            if not isinstance(tool_use_id, str) or not isinstance(output, dict):
                continue

            agent_id = output.get("agentId") or output.get("agent_id")
            if not isinstance(agent_id, str):
                continue

            launch_input = tool_inputs.get(tool_use_id, {})
            raw_name = launch_input.get("name")
            if not isinstance(raw_name, str):
                raw_name = output.get("name") if isinstance(output.get("name"), str) else None
            prompt = launch_input.get("prompt")
            if not isinstance(prompt, str):
                prompt = output.get("prompt") if isinstance(output.get("prompt"), str) else None
            description = launch_input.get("description")
            if not isinstance(description, str):
                description = (
                    output.get("description")
                    if isinstance(output.get("description"), str)
                    else None
                )

            launches[agent_id] = {
                "agent_id": agent_id,
                "agent_name": normalize_agent_name(raw_name or description, prompt),
                "agent_launcher_tool_use_id": tool_use_id,
                "agent_description": description,
                "agent_prompt": prompt,
                "agent_output_file": output.get("outputFile"),
            }

    return launches


def annotate_agent_metadata(records: list[dict[str, object]]) -> None:
    launches = collect_agent_launches(records)
    launch_intents = collect_agent_launch_intents(records)
    rounds_by_agent: dict[str, int] = defaultdict(int)

    for record in records:
        agent_id = record.get("agent_id")
        if not isinstance(agent_id, str) or not agent_id:
            agent_id = "main"
            record["agent_id"] = agent_id

        if agent_id == "main" and is_subagent_request(record.get("llm_req")):
            user_text = request_user_text(record.get("llm_req"))
            launch = find_matching_launch(user_text, launch_intents)
            if launch:
                for key, value in launch.items():
                    record[key] = value
                agent_name = str(launch["agent_name"])
            else:
                agent_name = normalize_agent_name(None, user_text)
                record["agent_name"] = agent_name
                record["agent_launcher_tool_use_id"] = None

            session_id = record.get("session_id")
            agent_id = session_id if isinstance(session_id, str) else f"agent-{slugify(agent_name)}"
            record["agent_id"] = agent_id
            record["agent_role"] = "subagent"
        elif agent_id == "main":
            record["agent_role"] = "main"
            record["agent_name"] = "Main"
        else:
            record["agent_role"] = "subagent"
            metadata = launches.get(agent_id)
            if metadata:
                for key, value in metadata.items():
                    record[key] = value
            else:
                record["agent_name"] = "Subagent"

        rounds_by_agent[agent_id] += 1
        record["agent_round"] = rounds_by_agent[agent_id]


def parse_usage(response_text: str) -> dict[str, int | None]:
    match = USAGE_RE.search(response_text)
    if not match:
        return {
            "input_tokens": None,
            "output_tokens": None,
            "cache_creation_input_tokens": None,
            "cache_read_input_tokens": None,
        }

    return {
        "input_tokens": int(match.group("input_tokens")),
        "output_tokens": int(match.group("output_tokens")),
        "cache_creation_input_tokens": int(
            match.group("cache_creation_input_tokens")
        ),
        "cache_read_input_tokens": int(
            match.group("cache_read_input_tokens")
        ),
    }


def _int_field(record: dict[str, object], key: str) -> int:
    """Safely extract an integer field from a record dict."""
    return int(record.get(key) or 0)


def _weighted_input(input_tokens: int, cache_creation: int, cache_read: int) -> float:
    """Core cost-weighted input formula shared across metrics."""
    return (
        input_tokens
        + cache_creation * CACHE_CREATION_COST_MULTIPLIER
        + cache_read * CACHE_READ_COST_MULTIPLIER
    )


def build_core_metrics(usage: dict[str, int | None]) -> dict[str, float | int | None] | None:
    input_tokens = usage["input_tokens"]
    output_tokens = usage["output_tokens"]
    cache_creation_input_tokens = usage["cache_creation_input_tokens"]
    cache_read_input_tokens = usage["cache_read_input_tokens"]
    if (
        input_tokens is None
        or output_tokens is None
        or cache_creation_input_tokens is None
        or cache_read_input_tokens is None
    ):
        return None

    cache_total = cache_read_input_tokens + cache_creation_input_tokens
    return {
        "cache_hit_rate": (
            cache_read_input_tokens / cache_total if cache_total else None
        ),
        "input_tokens": _weighted_input(
            input_tokens, cache_creation_input_tokens, cache_read_input_tokens
        ),
        "output_tokens": output_tokens,
    }


def core_input_for_record(record: dict[str, object]) -> float:
    return _weighted_input(
        _int_field(record, "input_tokens"),
        _int_field(record, "cache_creation_input_tokens"),
        _int_field(record, "cache_read_input_tokens"),
    )


def is_zero_cache_record(record: dict[str, object]) -> bool:
    return (
        _int_field(record, "cache_creation_input_tokens") == 0
        and _int_field(record, "cache_read_input_tokens") == 0
    )


def summarize_records(records: list[dict[str, object]]) -> dict[str, object]:
    def aggregate(selected: list[dict[str, object]]) -> dict[str, object]:
        cache_creation = sum(
            _int_field(record, "cache_creation_input_tokens")
            for record in selected
        )
        cache_read = sum(
            _int_field(record, "cache_read_input_tokens") for record in selected
        )
        cache_total = cache_creation + cache_read
        output_tokens = sum(
            _int_field(record, "output_tokens") for record in selected
        )
        core_input = sum(core_input_for_record(record) for record in selected)
        input_tokens = sum(_int_field(record, "input_tokens") for record in selected)
        zero_cache = [record for record in selected if is_zero_cache_record(record)]
        zero_cache_input = sum(_int_field(record, "input_tokens") for record in zero_cache)
        zero_cache_core_input = sum(core_input_for_record(record) for record in zero_cache)
        raw_inclusive_cache_denominator = input_tokens + cache_creation + cache_read
        return {
            "record_count": len(selected),
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "cache_creation_input_tokens": cache_creation,
            "cache_read_input_tokens": cache_read,
            "core_input_tokens": core_input,
            "core_total_tokens": core_input + output_tokens,
            "core_cache_hit": cache_read / cache_total if cache_total else None,
            "raw_inclusive_cache_coverage": (
                cache_read / raw_inclusive_cache_denominator
                if raw_inclusive_cache_denominator
                else None
            ),
            "zero_cache_record_count": len(zero_cache),
            "zero_cache_input_tokens": zero_cache_input,
            "zero_cache_core_input_tokens": zero_cache_core_input,
            "zero_cache_core_input_share": (
                zero_cache_core_input / core_input if core_input else None
            ),
        }

    def record_ref(record: dict[str, object]) -> dict[str, object]:
        return {
            "request_index": record.get("request_index"),
            "session_id": record.get("session_id"),
            "iteration": record.get("iteration"),
            "agent_role": record.get("agent_role"),
            "agent_name": record.get("agent_name"),
            "agent_round": record.get("agent_round"),
            "input_tokens": record.get("input_tokens"),
            "cache_creation_input_tokens": record.get(
                "cache_creation_input_tokens"
            ),
            "cache_read_input_tokens": record.get("cache_read_input_tokens"),
            "core_input_tokens": core_input_for_record(record),
        }

    marker_distribution: dict[
        tuple[object, object, object, object, object, object], int
    ] = {}
    lookback_risks: list[dict[str, object]] = []
    cache_read_zero_records: list[dict[str, object]] = []
    zero_cache_records: list[dict[str, object]] = []

    for record in records:
        prompt_cache_raw = record.get("PromptCache")
        prompt_cache = prompt_cache_raw if isinstance(prompt_cache_raw, dict) else {}
        marker_key = (
            record.get("agent_role"),
            prompt_cache.get("automatic_cache_control_count", 0),
            prompt_cache.get("system_cache_control_count"),
            prompt_cache.get("tool_cache_control_count"),
            prompt_cache.get("message_cache_control_count"),
            prompt_cache.get("cache_control_count"),
        )
        marker_distribution[marker_key] = marker_distribution.get(marker_key, 0) + 1

        cache_read_zero = _int_field(record, "cache_read_input_tokens") == 0
        is_zero_cache = is_zero_cache_record(record)
        if cache_read_zero or is_zero_cache:
            ref = record_ref(record)
            if cache_read_zero:
                cache_read_zero_records.append(ref)
            if is_zero_cache:
                zero_cache_records.append(ref)

        block_diagnostics = prompt_cache.get("block_diagnostics")
        if not isinstance(block_diagnostics, dict):
            continue
        cache_breakpoints = block_diagnostics.get("cache_breakpoints")
        if not isinstance(cache_breakpoints, list):
            continue
        for bp in cache_breakpoints:
            if not isinstance(bp, dict):
                continue
            distance = bp.get("distance_from_previous_breakpoint")
            exceeds_from_distance = (
                isinstance(distance, int)
                and distance > ANTHROPIC_BREAKPOINT_LOOKBACK_BLOCKS
            )
            exceeds_from_log = (
                bp.get("exceeds_anthropic_20_block_lookback") is True
            )
            if not exceeds_from_distance and not exceeds_from_log:
                continue
            risk = record_ref(record)
            risk.update(
                {
                    "breakpoint_section": bp.get("section"),
                    "breakpoint_block_type": bp.get("block_type"),
                    "breakpoint_cache_control_source": bp.get(
                        "cache_control_source"
                    ),
                    "breakpoint_block_index": bp.get("block_index"),
                    "distance_from_previous_breakpoint": distance,
                    "estimated_prefix_tokens": bp.get(
                        "estimated_prefix_tokens"
                    ),
                    "below_opus_4_6_min_cache_tokens": bp.get(
                        "below_opus_4_6_min_cache_tokens"
                    ),
                    "exceeds_anthropic_20_block_lookback": exceeds_from_distance
                    or exceeds_from_log,
                    "exceeds_anthropic_20_block_lookback_from_log": (
                        bp.get("exceeds_anthropic_20_block_lookback")
                    ),
                    "lookback_threshold_blocks": (
                        ANTHROPIC_BREAKPOINT_LOOKBACK_BLOCKS
                    ),
                }
            )
            lookback_risks.append(risk)

    marker_distribution_items = [
        {
            "agent_role": key[0],
            "automatic_cache_control_count": key[1],
            "system_cache_control_count": key[2],
            "tool_cache_control_count": key[3],
            "message_cache_control_count": key[4],
            "cache_control_count": key[5],
            "record_count": count,
        }
        for key, count in sorted(
            marker_distribution.items(), key=lambda item: repr(item[0])
        )
    ]

    return {
        "record_count": len(records),
        "aggregates": {
            "all": aggregate(records),
            "main": aggregate(
                [record for record in records if record.get("agent_role") == "main"]
            ),
            "subagent": aggregate(
                [
                    record
                    for record in records
                    if record.get("agent_role") == "subagent"
                ]
            ),
        },
        "aggregates_by_agent_name": {
            str(agent_name): aggregate(
                [record for record in records if record.get("agent_name") == agent_name]
            )
            for agent_name in sorted(
                {
                    record.get("agent_name")
                    for record in records
                    if record.get("agent_name") is not None
                },
                key=str,
            )
        },
        "marker_distribution": marker_distribution_items,
        "lookback_risks": lookback_risks,
        "cache_read_zero_records": cache_read_zero_records,
        "zero_cache_records": zero_cache_records,
        "counts": {
            "lookback_risk_breakpoints": len(lookback_risks),
            "cache_read_zero_records": len(cache_read_zero_records),
            "zero_cache_records": len(zero_cache_records),
        },
    }


def parse_prompt_cache(response_text: str) -> dict[str, object] | None:
    match = PROMPT_CACHE_RE.search(response_text)
    if not match:
        return None

    prompt_cache: dict[str, object] = {}
    for field_match in PROMPT_CACHE_FIELD_RE.finditer(match.group("body")):
        key = field_match.group("key")
        value = field_match.group("value")
        if value == "true":
            prompt_cache[key] = True
        elif value == "false":
            prompt_cache[key] = False
        elif re.fullmatch(r"-?\d+", value):
            prompt_cache[key] = int(value)
        else:
            prompt_cache[key] = json.loads(value)
    return prompt_cache


def parse_key_value_body(raw_body: str) -> dict[str, str]:
    pair_re = re.compile(
        r"(?P<key>[A-Za-z_][A-Za-z0-9_]*)="
        r"(?P<value>\{[^{}]*\}|\[[^\[\]]*\]|\"(?:\\.|[^\"\\])*\"|[^ \t\r\n]+)"
    )
    fields: dict[str, str] = {}
    for match in pair_re.finditer(raw_body):
        fields[match.group("key")] = match.group("value")
    return fields


def parse_scalar_fields(fields: dict[str, str]) -> dict[str, object]:
    parsed: dict[str, object] = {}
    for key, value in fields.items():
        if value == "true":
            parsed[key] = True
        elif value == "false":
            parsed[key] = False
        elif re.fullmatch(r"-?\d+", value):
            parsed[key] = int(value)
        elif value.startswith('"') and value.endswith('"'):
            parsed[key] = json.loads(value)
        else:
            parsed[key] = value
    return parsed


def parse_debug_field_value(value: str) -> object:
    if value == "true":
        return True
    if value == "false":
        return False
    if value in {"null", "None"}:
        return None
    if re.fullmatch(r"-?\d+", value):
        return int(value)
    if value.startswith('"') and value.endswith('"'):
        try:
            return json.loads(value)
        except json.JSONDecodeError:
            return value
    return value


def parse_debug_fields(body: str) -> dict[str, object]:
    fields: dict[str, object] = {}
    for line in body.splitlines():
        if "=" not in line:
            continue
        key, value = line.split("=", 1)
        key = key.strip()
        if not key:
            continue
        fields[key] = parse_debug_field_value(value.strip())
    return fields


def parse_event_and_body(entry: str) -> tuple[str | None, str]:
    header = parse_entry_header(entry)
    if header is None:
        return None, ""
    return header["event"], header.get("detail", "")


def json_parse_status(raw_input: object) -> dict[str, object]:
    if not isinstance(raw_input, str):
        return {
            "input_json_valid": False,
            "input_json_error": "tool input is not a string",
        }
    try:
        json.loads(raw_input)
    except json.JSONDecodeError as error:
        return {
            "input_json_valid": False,
            "input_json_error": str(error),
        }
    return {
        "input_json_valid": True,
        "input_json_error": None,
    }


def full_input_from_debug_summary(
    fields: dict[str, object], label: str = "input"
) -> str | None:
    prefix = fields.get(f"{label}_prefix")
    suffix = fields.get(f"{label}_suffix")
    input_bytes = fields.get(f"{label}_bytes")
    if not isinstance(prefix, str) or not isinstance(suffix, str):
        return None
    if prefix != suffix:
        return None
    if isinstance(input_bytes, int) and len(prefix.encode("utf-8")) != input_bytes:
        return None
    return prefix


def update_tool_stream_delta_summary(
    stream_tool: dict[str, object], fields: dict[str, object]
) -> None:
    summary = stream_tool.get("delta_summary")
    if not isinstance(summary, dict):
        summary = {
            "count": 0,
            "partial_bytes_total": 0,
            "partial_chars_total": 0,
        }
        stream_tool["delta_summary"] = summary

    summary["count"] = int(summary.get("count", 0)) + 1
    partial_bytes = fields.get("partial_bytes")
    if isinstance(partial_bytes, int):
        summary["partial_bytes_total"] = int(summary.get("partial_bytes_total", 0)) + partial_bytes
    partial_chars = fields.get("partial_chars")
    if isinstance(partial_chars, int):
        summary["partial_chars_total"] = int(summary.get("partial_chars_total", 0)) + partial_chars

    for key in (
        "accumulated_bytes",
        "accumulated_chars",
        "partial",
        "accumulated_suffix",
    ):
        if key in fields:
            summary[f"last_{key}"] = fields[key]


def collect_tool_stream_diagnostic(
    entry: str,
    stream_tools_by_id: dict[str, dict[str, object]],
    parse_errors: list[dict[str, object]],
) -> bool:
    event, body = parse_event_and_body(entry)
    if event is None:
        return False

    if event in TOOL_INPUT_PARSE_ERROR_EVENTS:
        fields = parse_debug_fields(body)
        fields["event"] = event
        fields["ts"] = parse_timestamp(entry)
        parse_errors.append(fields)
        return True

    if event not in TOOL_STREAM_EVENTS:
        return False

    fields = parse_debug_fields(body)
    tool_id = fields.get("tool_id")
    if not isinstance(tool_id, str):
        return True

    stream_scope = "cli" if event.startswith("cli.") else "agent"
    phase = event.rsplit(".", 1)[-1].removeprefix("tool_")
    stream_tool = stream_tools_by_id.setdefault(
        tool_id,
        {
            "tool_id": tool_id,
            "stream_scope": stream_scope,
        },
    )
    stream_tool["stream_scope"] = stream_scope
    if "tool_name" in fields:
        stream_tool["tool_name"] = fields["tool_name"]
    if "session_id" in fields:
        stream_tool["session_id"] = fields["session_id"]
    if "model" in fields:
        stream_tool["model"] = fields["model"]

    if phase == "input_delta":
        update_tool_stream_delta_summary(stream_tool, fields)
        return True

    fields["ts"] = parse_timestamp(entry)
    stream_tool[phase] = fields

    if phase == "stop":
        full_input = full_input_from_debug_summary(
            fields, "normalized_input"
        ) or full_input_from_debug_summary(fields)
        if full_input is not None:
            stream_tool["input_full_available"] = True
            stream_tool["input"] = full_input
            stream_tool.update(json_parse_status(full_input))
        else:
            stream_tool["input_full_available"] = False

    return True


def parse_error_matches_stream_tool(
    parse_error: dict[str, object], stream_tool: dict[str, object]
) -> bool:
    stop = stream_tool.get("stop")
    if not isinstance(stop, dict):
        return False
    if parse_error.get("tool_name") != stream_tool.get("tool_name"):
        return False
    for key in ("input_bytes", "input_chars", "input_prefix", "input_suffix"):
        if key in parse_error and key in stop and parse_error[key] != stop[key]:
            return False
    return True


def attach_parse_errors_to_stream_tools(
    stream_tools_by_id: dict[str, dict[str, object]],
    parse_errors: list[dict[str, object]],
) -> None:
    for parse_error in parse_errors:
        matches = [
            stream_tool
            for stream_tool in stream_tools_by_id.values()
            if parse_error_matches_stream_tool(parse_error, stream_tool)
        ]
        if not matches:
            continue
        matches[-1]["execution_parse_error"] = parse_error


def extract_response_tool_uses(
    response_text: object,
    stream_tools_by_id: dict[str, dict[str, object]],
) -> list[dict[str, object]]:
    if not isinstance(response_text, str):
        return []

    tool_uses: list[dict[str, object]] = []
    for event in iter_response_events(response_text):
        if event.get("type") != "ToolUse":
            continue
        tool_use_id = event.get("id")
        tool_name = event.get("name")
        tool_input = event.get("input")
        tool_use: dict[str, object] = {
            "id": tool_use_id,
            "name": tool_name,
            "input": tool_input,
        }
        tool_use.update(json_parse_status(tool_input))
        if isinstance(tool_use_id, str):
            stream_tool = stream_tools_by_id.get(tool_use_id)
            if stream_tool is not None:
                tool_use["stream"] = stream_tool
                if "execution_parse_error" in stream_tool:
                    tool_use["execution_parse_error"] = stream_tool[
                        "execution_parse_error"
                    ]
        tool_uses.append(tool_use)
    return tool_uses


def annotate_tool_uses(
    records: list[dict[str, object]],
    stream_tools_by_id: dict[str, dict[str, object]],
) -> None:
    for record in records:
        tool_uses = extract_response_tool_uses(record.get("llm_resp"), stream_tools_by_id)
        record["tool_uses"] = tool_uses
        record["invalid_tool_use_count"] = sum(
            1 for tool_use in tool_uses if tool_use.get("input_json_valid") is False
        )


def parse_prompt_cache_summary(entry: str) -> dict[str, object] | None:
    match = PROMPT_CACHE_SUMMARY_RE.search(entry)
    if not match:
        return None
    fields = parse_key_value_body(match.group("body"))

    enabled_raw = fields.get("cache_enabled")
    count_raw = fields.get("cache_control_count")
    session_id = fields.get("session_id")
    if enabled_raw not in {"true", "false"} or count_raw is None or session_id is None:
        return None

    types_raw = fields.get("cache_control_types", "[]")
    try:
        cache_control_types = json.loads(types_raw)
    except json.JSONDecodeError:
        cache_control_types = []

    summary: dict[str, object] = {
        "session_id": session_id,
        "enabled": enabled_raw == "true",
        "cache_control_count": int(count_raw),
        "cache_control_types": cache_control_types
        if isinstance(cache_control_types, list)
        else [],
    }
    for key in (
        "automatic_cache_control_count",
        "system_cache_control_count",
        "tool_cache_control_count",
        "message_cache_control_count",
    ):
        value = fields.get(key)
        if value is not None and re.fullmatch(r"-?\d+", value):
            summary[key] = int(value)
    return summary


def parse_prompt_cache_usage(entry: str) -> dict[str, object] | None:
    match = PROMPT_CACHE_USAGE_RE.search(entry)
    if not match:
        return None
    fields = parse_key_value_body(match.group("body"))
    session_id = fields.get("session_id")
    if session_id is None:
        return None

    usage: dict[str, object] = {"session_id": session_id}
    cache_creation_raw = fields.get("cache_creation")
    if cache_creation_raw:
        try:
            cache_creation = json.loads(cache_creation_raw)
            if isinstance(cache_creation, dict):
                usage["cache_creation"] = cache_creation
        except json.JSONDecodeError:
            pass
    return usage


def parse_prompt_cache_fingerprint(entry: str) -> dict[str, object] | None:
    match = PROMPT_CACHE_FINGERPRINT_RE.search(entry)
    if not match:
        return None
    parsed = parse_scalar_fields(parse_key_value_body(match.group("body")))
    return parsed if isinstance(parsed.get("session_id"), str) else None


def parse_prompt_cache_break(entry: str) -> dict[str, object] | None:
    match = PROMPT_CACHE_BREAK_RE.search(entry)
    if not match:
        return None
    parsed = parse_scalar_fields(parse_key_value_body(match.group("body")))
    return parsed if isinstance(parsed.get("session_id"), str) else None


def parse_json_array_field(body: str, field_name: str) -> list[object] | None:
    marker = f"{field_name}="
    start = body.find(marker)
    if start < 0:
        return None
    start += len(marker)
    while start < len(body) and body[start].isspace():
        start += 1
    try:
        value, _ = json.JSONDecoder().raw_decode(body[start:])
    except json.JSONDecodeError:
        return None
    return value if isinstance(value, list) else None


def parse_prompt_cache_blocks(entry: str) -> dict[str, object] | None:
    match = PROMPT_CACHE_BLOCKS_RE.search(entry)
    if not match:
        return None
    body = match.group("body")
    parsed = parse_scalar_fields(parse_key_value_body(body))
    if not isinstance(parsed.get("session_id"), str):
        return None

    breakpoints = parse_json_array_field(body, "cache_breakpoints")
    parsed["cache_breakpoints"] = breakpoints if breakpoints is not None else []
    return parsed


def parse_tool_result_sizes(entry: str) -> dict[str, object] | None:
    match = TOOL_RESULT_SIZES_RE.search(entry)
    if not match:
        return None
    body = match.group("body")
    parsed = parse_scalar_fields(parse_key_value_body(body))
    if not isinstance(parsed.get("session_id"), str):
        return None

    tool_results = parse_json_array_field(body, "tool_results")
    parsed["tool_results"] = tool_results if tool_results is not None else []
    return parsed


def parse_api_request(raw_request: str) -> Any:
    try:
        return RustDebugParser(raw_request).parse()
    except ValueError:
        return raw_request


def make_empty_record(record_index: int, session_id: str, iteration: int) -> dict[str, object]:
    return {
        "request_index": record_index,
        "session_id": session_id,
        "iteration": iteration,
        "agent_role": None,
        "agent_id": None,
        "agent_name": None,
        "agent_round": None,
        "agent_launcher_tool_use_id": None,
        "request_ts": None,
        "response_ts": None,
        "llm_req": None,
        "llm_resp": None,
        "tool_uses": [],
        "invalid_tool_use_count": 0,
        "input_tokens": None,
        "output_tokens": None,
        "cache_creation_input_tokens": None,
        "cache_read_input_tokens": None,
        "core_metrics": None,
        "PromptCache": None,
    }


def extract_records(text: str) -> list[dict[str, object]]:
    entries = group_entries(split_entries(text))
    records: list[dict[str, object]] = []
    pending_requests: dict[tuple[str, int], deque[int]] = defaultdict(deque)
    active_request_by_session: dict[str, int] = {}
    stream_tools_by_id: dict[str, dict[str, object]] = {}
    tool_input_parse_errors: list[dict[str, object]] = []

    for entry in entries:
        if collect_tool_stream_diagnostic(
            entry, stream_tools_by_id, tool_input_parse_errors
        ):
            continue

        fingerprint = parse_prompt_cache_fingerprint(entry)
        if fingerprint:
            session_id = fingerprint.get("session_id")
            if isinstance(session_id, str):
                active_idx = active_request_by_session.get(session_id)
                if active_idx is not None:
                    record = records[active_idx]
                    prompt_cache = (
                        record["PromptCache"] if isinstance(record["PromptCache"], dict) else {}
                    )
                    prompt_cache["request_fingerprint"] = {
                        key: value
                        for key, value in fingerprint.items()
                        if key != "session_id"
                    }
                    record["PromptCache"] = prompt_cache
            continue

        break_diagnostics = parse_prompt_cache_break(entry)
        if break_diagnostics:
            session_id = break_diagnostics.get("session_id")
            if isinstance(session_id, str):
                active_idx = active_request_by_session.get(session_id)
                if active_idx is not None:
                    record = records[active_idx]
                    prompt_cache = (
                        record["PromptCache"] if isinstance(record["PromptCache"], dict) else {}
                    )
                    prompt_cache["break_diagnostics"] = {
                        key: value
                        for key, value in break_diagnostics.items()
                        if key != "session_id"
                    }
                    record["PromptCache"] = prompt_cache
            continue

        block_diagnostics = parse_prompt_cache_blocks(entry)
        if block_diagnostics:
            session_id = block_diagnostics.get("session_id")
            if isinstance(session_id, str):
                active_idx = active_request_by_session.get(session_id)
                if active_idx is not None:
                    record = records[active_idx]
                    prompt_cache = (
                        record["PromptCache"] if isinstance(record["PromptCache"], dict) else {}
                    )
                    prompt_cache["block_diagnostics"] = {
                        key: value
                        for key, value in block_diagnostics.items()
                        if key != "session_id"
                    }
                    record["PromptCache"] = prompt_cache
            continue

        tool_result_sizes = parse_tool_result_sizes(entry)
        if tool_result_sizes:
            session_id = tool_result_sizes.get("session_id")
            if isinstance(session_id, str):
                active_idx = active_request_by_session.get(session_id)
                if active_idx is not None:
                    record = records[active_idx]
                    prompt_cache = (
                        record["PromptCache"] if isinstance(record["PromptCache"], dict) else {}
                    )
                    prompt_cache["tool_result_sizes"] = {
                        key: value
                        for key, value in tool_result_sizes.items()
                        if key != "session_id"
                    }
                    record["PromptCache"] = prompt_cache
            continue

        summary = parse_prompt_cache_summary(entry)
        if summary:
            session_id = summary.get("session_id")
            if isinstance(session_id, str):
                active_idx = active_request_by_session.get(session_id)
                if active_idx is not None:
                    record = records[active_idx]
                    prompt_cache = (
                        record["PromptCache"] if isinstance(record["PromptCache"], dict) else {}
                    )
                    prompt_cache["enabled"] = summary["enabled"]
                    prompt_cache["cache_control_count"] = summary["cache_control_count"]
                    prompt_cache["cache_control_types"] = summary["cache_control_types"]
                    for key in (
                        "automatic_cache_control_count",
                        "system_cache_control_count",
                        "tool_cache_control_count",
                        "message_cache_control_count",
                    ):
                        if key in summary:
                            prompt_cache[key] = summary[key]
                    record["PromptCache"] = prompt_cache
            continue

        cache_usage = parse_prompt_cache_usage(entry)
        if cache_usage:
            session_id = cache_usage.get("session_id")
            if isinstance(session_id, str):
                active_idx = active_request_by_session.get(session_id)
                if active_idx is not None:
                    record = records[active_idx]
                    prompt_cache = (
                        record["PromptCache"] if isinstance(record["PromptCache"], dict) else {}
                    )
                    cache_creation = cache_usage.get("cache_creation")
                    if isinstance(cache_creation, dict):
                        prompt_cache["cache_creation"] = cache_creation
                    if prompt_cache:
                        record["PromptCache"] = prompt_cache
            continue

        request_match = REQUEST_RE.search(entry)
        if request_match:
            session_id = request_match.group("session_id")
            iteration = int(request_match.group("iteration"))
            record = make_empty_record(len(records) + 1, session_id, iteration)
            record["agent_id"] = parse_thread_agent_id(entry) or "main"
            record["request_ts"] = parse_timestamp(entry)
            record["llm_req"] = parse_api_request(request_match.group("llm_req").strip())
            records.append(record)
            record_idx = len(records) - 1
            pending_requests[(session_id, iteration)].append(record_idx)
            active_request_by_session[session_id] = record_idx
            continue

        response_match = RESPONSE_RE.search(entry)
        if not response_match:
            continue

        session_id = response_match.group("session_id")
        iteration = int(response_match.group("iteration"))
        key = (session_id, iteration)

        if pending_requests[key]:
            record_idx = pending_requests[key].popleft()
            record = records[record_idx]
        else:
            record = make_empty_record(len(records) + 1, session_id, iteration)
            record["agent_id"] = parse_thread_agent_id(entry) or "main"
            records.append(record)
            record_idx = len(records) - 1

        response_text = response_match.group("llm_resp").strip()
        usage = parse_usage(response_text)

        record["response_ts"] = parse_timestamp(entry)
        record["llm_resp"] = response_text
        record["input_tokens"] = usage["input_tokens"]
        record["output_tokens"] = usage["output_tokens"]
        record["cache_creation_input_tokens"] = usage[
            "cache_creation_input_tokens"
        ]
        record["cache_read_input_tokens"] = usage[
            "cache_read_input_tokens"
        ]
        record["core_metrics"] = build_core_metrics(usage)
        parsed_prompt_cache = parse_prompt_cache(response_text)
        if isinstance(parsed_prompt_cache, dict):
            existing_prompt_cache = (
                record["PromptCache"] if isinstance(record["PromptCache"], dict) else {}
            )
            existing_prompt_cache.update(parsed_prompt_cache)
            record["PromptCache"] = existing_prompt_cache

        active_idx = active_request_by_session.get(session_id)
        if active_idx is not None and active_idx == record_idx:
            active_request_by_session.pop(session_id, None)

    attach_parse_errors_to_stream_tools(stream_tools_by_id, tool_input_parse_errors)
    annotate_agent_metadata(records)
    annotate_tool_uses(records, stream_tools_by_id)
    return records


def write_output(records: list[dict[str, object]], output_path: str | None) -> None:
    payload = json.dumps(records, ensure_ascii=False, indent=2)
    if output_path:
        Path(output_path).write_text(payload + "\n", encoding="utf-8")
        return
    print(payload)


def write_summary(records: list[dict[str, object]], output_path: str | None) -> None:
    if not output_path:
        return
    payload = json.dumps(summarize_records(records), ensure_ascii=False, indent=2)
    Path(output_path).write_text(payload + "\n", encoding="utf-8")


def main() -> int:
    args = parse_args()
    records = extract_records(read_log(args.log_path))
    write_output(records, args.output)
    write_summary(records, args.summary_output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
