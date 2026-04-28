#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import re
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any


DEFAULT_LOG_PATH = "/mnt/d/ginobili/code/claw-code/claude.req_and_rsp"

REQUEST_LINE_RE = re.compile(
    r"^(?P<connection>\d+\.\d+\.\d+\.\d+:\d+): POST https://(?P<host>\S+?)(?P<path>/\S*)$"
)
RESPONSE_LINE_RE = re.compile(r"^ << (?P<status_code>\d{3}) (?P<status_text>.+)$")

TOKEN_KEYS = (
    "input_tokens",
    "output_tokens",
    "cache_creation_input_tokens",
    "cache_read_input_tokens",
)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Extract Claude Code request/response data with JSON-formatted llm_req."
    )
    parser.add_argument(
        "log_path",
        nargs="?",
        default=DEFAULT_LOG_PATH,
        help=f"Path to the Claude Code transcript. Default: {DEFAULT_LOG_PATH}",
    )
    parser.add_argument(
        "-o",
        "--output",
        help="Write JSON output to this file. Defaults to stdout.",
    )
    return parser.parse_args()


def read_text(path: str) -> str:
    if path == "-":
        return sys.stdin.read()
    return Path(path).read_text(encoding="utf-8", errors="replace")


def skip_blank_lines(lines: list[str], index: int) -> int:
    while index < len(lines) and not lines[index].strip():
        index += 1
    return index


def parse_headers(lines: list[str], index: int) -> tuple[dict[str, str], int]:
    headers: dict[str, str] = {}
    while index < len(lines) and lines[index].startswith("    "):
        line = lines[index][4:].rstrip("\n")
        if ":" in line:
            name, value = line.split(":", 1)
            headers[name.strip()] = value.strip()
        index += 1
    return headers, index


def parse_indented_block(lines: list[str], index: int) -> tuple[str, int]:
    block_lines: list[str] = []
    while index < len(lines) and lines[index].startswith("    "):
        block_lines.append(lines[index][4:].rstrip("\n"))
        index += 1
    return "\n".join(block_lines).strip(), index


def parse_response_body(lines: list[str], index: int) -> tuple[str, int]:
    body_lines: list[str] = []
    while index < len(lines):
        line = lines[index]
        if line.strip() and not line.startswith(" "):
            break
        if line.startswith("    "):
            body_lines.append(line[4:].rstrip("\n"))
        else:
            body_lines.append(line.rstrip("\n"))
        index += 1
    return "\n".join(body_lines).strip(), index


def is_messages_request(line: str) -> re.Match[str] | None:
    match = REQUEST_LINE_RE.match(line.rstrip("\n"))
    if not match:
        return None
    if "/api/v1/messages" not in match.group("path"):
        return None
    return match


def parse_json(text: str) -> Any | None:
    if not text:
        return None
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return None


def collect_cache_controls(obj: Any, found: list[dict[str, Any]]) -> None:
    if isinstance(obj, dict):
        cache_control = obj.get("cache_control")
        if isinstance(cache_control, dict):
            found.append(cache_control)
        for value in obj.values():
            collect_cache_controls(value, found)
        return
    if isinstance(obj, list):
        for item in obj:
            collect_cache_controls(item, found)


def parse_sse_events(response_text: str) -> list[tuple[str, Any]]:
    events: list[tuple[str, Any]] = []
    current_event: str | None = None
    current_data: list[str] = []

    def flush() -> None:
        nonlocal current_event, current_data
        if current_event is None:
            return
        data_text = "\n".join(current_data).strip()
        payload: Any = data_text
        if data_text:
            try:
                payload = json.loads(data_text)
            except json.JSONDecodeError:
                payload = data_text
        events.append((current_event, payload))
        current_event = None
        current_data = []

    for line in response_text.splitlines():
        if not line.strip():
            flush()
            continue
        if line.startswith("event:"):
            flush()
            current_event = line[len("event:") :].strip()
            continue
        if line.startswith("data:"):
            current_data.append(line[len("data:") :].lstrip())

    flush()
    return events


def merge_usage(target: dict[str, Any], new_usage: Any) -> None:
    if not isinstance(new_usage, dict):
        return
    for key in TOKEN_KEYS:
        value = new_usage.get(key)
        if isinstance(value, int):
            target[key] = value
    cache_creation = new_usage.get("cache_creation")
    if isinstance(cache_creation, dict):
        target.setdefault("cache_creation", {}).update(cache_creation)


def format_usage(usage: dict[str, Any]) -> str | None:
    values = []
    for key in TOKEN_KEYS:
        value = usage.get(key)
        if not isinstance(value, int):
            return None
        values.append(f"{key}: {value}")
    return "Usage(TokenUsage { " + ", ".join(values) + " })"


def build_core_metrics(usage: dict[str, Any]) -> dict[str, float | int | None] | None:
    input_tokens = usage.get("input_tokens")
    output_tokens = usage.get("output_tokens")
    cache_creation_input_tokens = usage.get("cache_creation_input_tokens")
    cache_read_input_tokens = usage.get("cache_read_input_tokens")
    if (
        not isinstance(input_tokens, int)
        or not isinstance(output_tokens, int)
        or not isinstance(cache_creation_input_tokens, int)
        or not isinstance(cache_read_input_tokens, int)
    ):
        return None

    cache_total = cache_read_input_tokens + cache_creation_input_tokens
    return {
        "cache_hit_rate": (
            cache_read_input_tokens / cache_total if cache_total else None
        ),
        "input_tokens": (
            input_tokens
            + cache_creation_input_tokens * 1.25
            + cache_read_input_tokens * 0.1
        ),
        "output_tokens": output_tokens,
    }


def normalize_delta(payload: Any) -> str | None:
    if not isinstance(payload, dict):
        return None
    if payload.get("type") != "content_block_delta":
        return None

    delta = payload.get("delta")
    if not isinstance(delta, dict):
        return None

    delta_type = delta.get("type")
    if delta_type == "text_delta":
        return f"TextDelta({json.dumps(delta.get('text', ''), ensure_ascii=False)})"
    if delta_type == "thinking_delta":
        return f"ThinkingDelta({json.dumps(delta.get('thinking', ''), ensure_ascii=False)})"
    if delta_type == "signature_delta":
        return f"SignatureDelta({json.dumps(delta.get('signature', ''), ensure_ascii=False)})"
    if delta_type == "input_json_delta":
        return f"InputJsonDelta({json.dumps(delta.get('partial_json', ''), ensure_ascii=False)})"
    return f"Delta({json.dumps(delta, ensure_ascii=False)})"


def normalize_response(response_text: str) -> tuple[str, dict[str, Any]]:
    if not response_text.startswith("event:"):
        return response_text, {}

    usage: dict[str, Any] = {}
    parts: list[str] = []
    events = parse_sse_events(response_text)

    for event_name, payload in events:
        if event_name == "message_start" and isinstance(payload, dict):
            merge_usage(usage, payload.get("message", {}).get("usage"))
            continue

        if event_name == "content_block_delta":
            delta = normalize_delta(payload)
            if delta:
                parts.append(delta)
            continue

        if event_name == "message_delta" and isinstance(payload, dict):
            merge_usage(usage, payload.get("usage"))
            usage_text = format_usage(usage)
            if usage_text:
                parts.append(usage_text)
            continue

        if event_name == "message_stop":
            parts.append("MessageStop")
            continue

        if event_name == "error":
            parts.append(f"Error({json.dumps(payload, ensure_ascii=False)})")

    if not parts:
        return response_text, usage
    return "[" + ", ".join(parts) + "]", usage


def build_prompt_cache(request_json: Any, usage: dict[str, Any]) -> dict[str, Any] | None:
    cache_controls: list[dict[str, Any]] = []
    collect_cache_controls(request_json, cache_controls)

    prompt_cache: dict[str, Any] = {}
    if cache_controls:
        prompt_cache["enabled"] = True
        prompt_cache["cache_control_count"] = len(cache_controls)
        cache_control_types = sorted(
            {
                cache_control["type"]
                for cache_control in cache_controls
                if isinstance(cache_control.get("type"), str)
            }
        )
        if cache_control_types:
            prompt_cache["cache_control_types"] = cache_control_types

    cache_creation = usage.get("cache_creation")
    if isinstance(cache_creation, dict) and cache_creation:
        prompt_cache["cache_creation"] = cache_creation

    return prompt_cache or None


def build_record(
    request_index: int,
    session_id: str | None,
    iteration: int,
    request_body: str,
    response_body: str,
    response_headers: dict[str, str],
) -> dict[str, Any]:
    request_json = parse_json(request_body)
    normalized_response, usage = normalize_response(response_body)
    prompt_cache = build_prompt_cache(request_json, usage)

    return {
        "request_index": request_index,
        "session_id": session_id,
        "iteration": iteration,
        "request_ts": None,
        "response_ts": response_headers.get("Date"),
        "llm_req": request_json if request_json is not None else request_body or None,
        "llm_resp": normalized_response or None,
        "input_tokens": usage.get("input_tokens"),
        "output_tokens": usage.get("output_tokens"),
        "cache_creation_input_tokens": usage.get("cache_creation_input_tokens"),
        "cache_read_input_tokens": usage.get("cache_read_input_tokens"),
        "core_metrics": build_core_metrics(usage),
        "PromptCache": prompt_cache,
    }


def extract_records(text: str) -> list[dict[str, Any]]:
    lines = text.splitlines(keepends=True)
    records: list[dict[str, Any]] = []
    iteration_by_session: dict[str | None, int] = defaultdict(int)
    index = 0

    while index < len(lines):
        request_match = is_messages_request(lines[index])
        if not request_match:
            index += 1
            continue

        index += 1
        request_headers, index = parse_headers(lines, index)
        index = skip_blank_lines(lines, index)
        request_body, index = parse_indented_block(lines, index)
        index = skip_blank_lines(lines, index)

        response_headers: dict[str, str] = {}
        response_body = ""

        if index < len(lines):
            response_match = RESPONSE_LINE_RE.match(lines[index].rstrip("\n"))
            if response_match:
                index += 1
                response_headers, index = parse_headers(lines, index)
                index = skip_blank_lines(lines, index)
                response_body, index = parse_response_body(lines, index)

        session_id = request_headers.get("X-Claude-Code-Session-Id")
        iteration_by_session[session_id] += 1
        record = build_record(
            request_index=len(records) + 1,
            session_id=session_id,
            iteration=iteration_by_session[session_id],
            request_body=request_body,
            response_body=response_body,
            response_headers=response_headers,
        )
        records.append(record)

    return records


def write_output(records: list[dict[str, Any]], output_path: str | None) -> None:
    payload = json.dumps(records, ensure_ascii=False, indent=2)
    if output_path:
        Path(output_path).write_text(payload + "\n", encoding="utf-8")
        return
    print(payload)


def main() -> int:
    args = parse_args()
    records = extract_records(read_text(args.log_path))
    write_output(records, args.output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
