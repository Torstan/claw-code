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

ENTRY_RE = re.compile(
    r"(\[clawd-agent-debug ts=.*?)(?=\[clawd-agent-debug ts=|\Z)",
    re.S,
)

TS_RE = re.compile(r"\[clawd-agent-debug ts=(?P<ts>.+?) pid=")

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
    r"\] cli\.provider\.stream\.prompt_cache (?P<body>.*)\s*$",
    re.S,
)

PROMPT_CACHE_USAGE_RE = re.compile(
    r"\] cli\.provider\.stream\.usage (?P<body>.*)\s*$",
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
    return parser.parse_args()


def read_log(path: str) -> str:
    if path == "-":
        return sys.stdin.read()
    return Path(path).read_text(encoding="utf-8", errors="replace")


def split_entries(text: str) -> list[str]:
    return [match.group(1).strip() for match in ENTRY_RE.finditer(text)]


def parse_timestamp(entry: str) -> str | None:
    match = TS_RE.search(entry)
    return match.group("ts") if match else None


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
        "input_tokens": (
            input_tokens
            + cache_creation_input_tokens * 1.25
            + cache_read_input_tokens * 0.1
        ),
        "output_tokens": output_tokens,
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
        "request_ts": None,
        "response_ts": None,
        "llm_req": None,
        "llm_resp": None,
        "input_tokens": None,
        "output_tokens": None,
        "cache_creation_input_tokens": None,
        "cache_read_input_tokens": None,
        "core_metrics": None,
        "PromptCache": None,
    }


def extract_records(text: str) -> list[dict[str, object]]:
    entries = split_entries(text)
    records: list[dict[str, object]] = []
    pending_requests: dict[tuple[str, int], deque[int]] = defaultdict(deque)
    active_request_by_session: dict[str, int] = {}

    for entry in entries:
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

    return records


def write_output(records: list[dict[str, object]], output_path: str | None) -> None:
    payload = json.dumps(records, ensure_ascii=False, indent=2)
    if output_path:
        Path(output_path).write_text(payload + "\n", encoding="utf-8")
        return
    print(payload)


def main() -> int:
    args = parse_args()
    records = extract_records(read_log(args.log_path))
    write_output(records, args.output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
