#!/usr/bin/env python3
"""Build a clean pre-GA Sandhi ledger copy without touching the source."""

import argparse
import hashlib
import ipaddress
import json
import math
import os
import shutil
from pathlib import Path
import sqlite3
import sys
import tempfile
from typing import Any
from urllib.parse import quote, urlsplit

try:
    import idna
except ImportError:
    idna = None

CONTEXT_FIELDS = (
    "version",
    "ingress",
    "operation",
    "delivery",
    "plane",
    "upstream_ref",
    "upstream_budget_scope",
    "requested_model",
    "actual_model",
    "provider",
    "endpoint_family",
    "transport",
    "transport_binding_sha256",
    "route",
    "backend",
    "cache",
    "accounting_policy",
    "content_policy_revision",
)
ATTEMPT_FIELDS = (
    "version",
    "attempt_id",
    "attempt_ordinal",
    "dispatch_observed_at_ms",
    "terminal",
    "dropped_observations",
    "invalid",
    "observed_provider",
)
TERMINAL_FIELDS = (
    "observed_at_ms",
    "outcome",
    "provider_status",
    "provider_request_id",
    "progress",
)
PROGRESS_FIELDS = (
    "response_headers_received",
    "stream_event_received",
    "first_stream_event_at_ms",
    "time_to_first_stream_event_ms",
    "timeout_phase",
    "error_class",
    "error_subtype",
)
FAMILIES = {
    "openai_chat_completions",
    "openai_responses",
    "anthropic_messages",
    "gemini_generate_content",
    "cohere_chat",
    "ollama_chat",
    "bedrock_converse",
    "host_callback",
}
OUTCOMES = {
    "success",
    "provider_rejected",
    "transport_error",
    "timeout",
    "cancelled",
    "incomplete_stream",
}
TIMEOUT_PHASES = {"setup", "idle", "body", "unknown"}
ERROR_CLASSES = {
    "none",
    "timeout",
    "transport",
    "provider",
    "cancelled",
    "incomplete_stream",
}
ERROR_SUBTYPES = {
    "setup",
    "idle",
    "body",
    "unknown_timeout",
    "network",
    "http_status",
    "caller",
    "incomplete",
}
CACHE_MODES = {
    "preserve_native",
    "auto",
    "auto_stable_prefix",
    "auto_conversation_prefix",
    "suppress_gateway",
}
CACHE_LIFETIMES = {"5m", "1h", "30m", "12h", "in_memory", "24h"}
CACHE_PROFILES = {
    "native_only",
    "implicit_only",
    "anthropic_breakpoints",
    "openai_legacy",
    "openai_explicit",
}
CACHE_PROFILE_LIFETIMES = {
    "native_only": set(),
    "implicit_only": set(),
    "anthropic_breakpoints": {"5m", "1h"},
    "openai_legacy": {"in_memory", "24h"},
    "openai_explicit": {"30m", "12h"},
}
CACHE_NAMESPACES = {"provider_default", "scoped_key"}
MAX_EPOCH_MS = 253_402_300_799_999
U64_MAX = 18_446_744_073_709_551_615
ENDPOINT_FAMILIES = {
    "openai_chat_completions",
    "openai_responses",
    "anthropic_messages",
    "gemini_generate_content",
    "cohere_chat",
    "ollama_chat",
    "bedrock_converse",
    "host_callback",
}
ENDPOINT_PROFILE_FAMILIES = {
    "standard": ENDPOINT_FAMILIES,
    "chatgpt_codex": {"openai_responses"},
    "anthropic_implicit_cache": {"anthropic_messages"},
    "openai_terminal_chunk_usage": {"openai_chat_completions"},
    "zai_final_choice_usage": {"openai_chat_completions"},
    "deepseek_final_choice_usage": {"openai_chat_completions"},
}
THINKING_ENCODING_FAMILIES = {
    "openai_chat": "openai_chat_completions",
    "openai_responses": "openai_responses",
    "anthropic_messages": "anthropic_messages",
    "gemini_generate_content": "gemini_generate_content",
    "qwen_openai_chat": "openai_chat_completions",
    "zai_openai_chat": "openai_chat_completions",
}
THINKING_EFFORT_ENCODINGS = {
    "openai_chat",
    "openai_responses",
    "qwen_openai_chat",
}
THINKING_BUDGET_ENCODINGS = {
    "qwen_openai_chat",
    "zai_openai_chat",
    "anthropic_messages",
    "gemini_generate_content",
}
BACKEND_FIELDS = (
    "id",
    "provider",
    "credential_ref",
    "endpoint",
    "base_url",
    "user_agent",
    "models",
    "cache_policy",
)
BACKEND_MODEL_FIELDS = (
    "id",
    "aliases",
    "max_input_tokens",
    "max_output_tokens",
    "default_temperature",
    "capabilities",
    "thinking",
    "endpoint_url",
    "extensions",
)
MODEL_CAPABILITY_FIELDS = (
    "streaming",
    "tools",
    "parallel_tool_calls",
    "vision",
    "audio_input",
    "file_input",
    "structured_output",
    "reasoning",
    "prompt_cache_usage",
)
THINKING_FIELDS = (
    "supported",
    "encoding",
    "can_disable",
    "effort",
    "budget_tokens",
    "allow_effort_and_budget",
)
THINKING_EFFORT_FIELDS = ("values", "default")
THINKING_BUDGET_FIELDS = ("min", "max")


class RebuildError(ValueError):
    pass


def compact_json(value: dict[str, Any]) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def ordered(value: dict[str, Any], fields: tuple[str, ...]) -> dict[str, Any]:
    unexpected = set(value) - set(fields)
    if unexpected:
        raise RebuildError(f"unexpected fields: {sorted(unexpected)}")
    return {name: value[name] for name in fields if name in value}


def require_object(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise RebuildError(f"{label} must be an object")
    return value


def require_optional_string(value: Any, label: str, *, nullable: bool = True) -> None:
    if value is None:
        if not nullable:
            raise RebuildError(f"{label} must not be null")
    elif not isinstance(value, str) or not is_canonical_label(value):
        raise RebuildError(f"{label} must be a non-empty string")


def is_canonical_label(value: str) -> bool:
    return bool(
        value.strip()
        and len(value.encode("utf-8")) <= 512
        and not any(
            ord(character) < 0x20 or 0x7F <= ord(character) < 0xA0
            for character in value
        )
    )


def require_digest(value: Any, label: str) -> None:
    if (
        not isinstance(value, str)
        or len(value) != 64
        or any(character not in "0123456789abcdef" for character in value)
    ):
        raise RebuildError(f"{label} must be a SHA-256 digest")


def require_positive_u64(value: Any, label: str) -> int:
    if (
        not isinstance(value, int)
        or isinstance(value, bool)
        or value <= 0
        or value > U64_MAX
    ):
        raise RebuildError(f"{label} must be a positive integer")
    return value


def require_nonnegative_u64(value: Any, label: str, maximum: int | None = None) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise RebuildError(f"{label} must be a non-negative integer")
    if value > (maximum if maximum is not None else U64_MAX):
        raise RebuildError(f"{label} exceeds its supported maximum")
    return value


def require_epoch_ms(value: Any, label: str) -> None:
    require_nonnegative_u64(value, label, MAX_EPOCH_MS)


def require_hostname(value: Any, label: str) -> None:
    if value is None:
        return
    valid = (
        isinstance(value, str)
        and bool(value)
        and len(value) <= 255
    )
    if isinstance(value, str) and ":" in value:
        try:
            valid = valid and ipaddress.IPv6Address(value) is not None
        except ValueError:
            valid = False
    elif isinstance(value, str):
        valid = valid and all(
            character.isascii()
            and (character.isalnum() or character in ".-_")
            for character in value
        )
    if (
        not valid
    ):
        raise RebuildError(f"{label} must be a bounded hostname or null")


def canonical_policy(value: Any) -> Any:
    if isinstance(value, str):
        if value == "buffered":
            return "buffered_legacy_unavailable"
        if value not in ("buffered_legacy_unavailable", "streaming_legacy_unavailable"):
            raise RebuildError("unsupported transport policy")
        return value
    value = require_object(value, "transport policy")
    if "buffered" in value:
        if set(value) != {"buffered"}:
            raise RebuildError("buffered transport policy has unexpected fields")
        policy = ordered(
            require_object(value["buffered"], "buffered transport policy"),
            ("duration_ms",),
        )
        require_positive_u64(policy.get("duration_ms"), "buffered duration_ms")
        return {"buffered": policy}
    if "streaming" in value:
        if set(value) != {"streaming"}:
            raise RebuildError("streaming transport policy has unexpected fields")
        policy = ordered(
            require_object(value["streaming"], "streaming transport policy"),
            ("setup_ms", "idle_ms", "body_ms"),
        )
        for field in ("setup_ms", "idle_ms", "body_ms"):
            require_positive_u64(policy.get(field), f"streaming {field}")
        return {"streaming": policy}
    raise RebuildError("unsupported transport policy")


def canonical_cache_intent(value: Any) -> dict[str, Any]:
    intent = ordered(
        require_object(value, "cache intent"),
        ("mode", "lifetime"),
    )
    if intent.get("mode") not in CACHE_MODES:
        raise RebuildError("cache intent has an unsupported mode")
    if intent.get("lifetime") is not None:
        if intent["lifetime"] not in CACHE_LIFETIMES:
            raise RebuildError("cache intent has an unsupported lifetime")
        if intent["mode"] in ("preserve_native", "suppress_gateway"):
            raise RebuildError(
                "preserve/suppress cache intent cannot select a lifetime"
            )
    else:
        intent.pop("lifetime", None)
    return intent


def canonical_cache_capability(value: Any, family: str) -> str:
    if not isinstance(value, str) or value not in CACHE_PROFILES:
        raise RebuildError("cache capability has an unsupported profile")
    compatible = (
        value in ("native_only", "implicit_only")
        or (value == "anthropic_breakpoints" and family == "anthropic_messages")
        or (
            value == "openai_legacy"
            and family in ("openai_chat_completions", "openai_responses")
        )
        or (value == "openai_explicit" and family == "openai_responses")
    )
    if not compatible:
        raise RebuildError(
            "cache capability profile is incompatible with endpoint family"
        )
    return value


def canonical_context(value: dict[str, Any]) -> dict[str, Any]:
    context = ordered(value, CONTEXT_FIELDS)
    required = {
        "version",
        "ingress",
        "delivery",
        "plane",
        "upstream_ref",
        "upstream_budget_scope",
        "requested_model",
        "actual_model",
        "provider",
        "endpoint_family",
        "transport",
        "transport_binding_sha256",
        "accounting_policy",
    }
    missing = required - set(context)
    if missing:
        raise RebuildError(f"context is missing fields: {sorted(missing)}")
    if type(context["version"]) is not int or context["version"] != 1:
        raise RebuildError("context version must be 1")
    if context["ingress"] not in FAMILIES or context["endpoint_family"] not in FAMILIES:
        raise RebuildError("context contains an unsupported endpoint family")
    if context["delivery"] not in ("buffered", "streaming"):
        raise RebuildError("context contains an unsupported delivery")
    if context["plane"] not in ("transparent", "translation"):
        raise RebuildError("context contains an unsupported plane")
    for field in (
        "upstream_ref",
        "upstream_budget_scope",
        "requested_model",
        "actual_model",
        "provider",
    ):
        require_optional_string(context[field], f"context {field}", nullable=False)
    if len(context["provider"].encode("utf-8")) > 128:
        raise RebuildError("context provider exceeds its supported maximum")
    operation = context.get("operation")
    if operation is not None:
        if operation not in ("chat", "text_completion", "embedding"):
            raise RebuildError("context contains an unsupported operation")
        same_openai = (
            context["ingress"] == "openai_chat_completions"
            and context["endpoint_family"] == "openai_chat_completions"
            and context["plane"] == "transparent"
        )
        if operation == "text_completion" and not same_openai:
            raise RebuildError("context text-completion operation is invalid")
        if operation == "embedding" and (
            not same_openai or context["delivery"] != "buffered"
        ):
            raise RebuildError("context embedding operation is invalid")
    transport = require_object(context["transport"], "context transport")
    if set(transport) != {"upstream_host", "policy"}:
        raise RebuildError("context transport has unexpected fields")
    require_hostname(transport["upstream_host"], "context upstream_host")
    policy = canonical_policy(transport["policy"])
    policy_matches_delivery = (
        context["delivery"] == "buffered"
        and (
            (isinstance(policy, dict) and "buffered" in policy)
            or policy == "buffered_legacy_unavailable"
        )
    ) or (
        context["delivery"] == "streaming"
        and (
            (isinstance(policy, dict) and "streaming" in policy)
            or policy == "streaming_legacy_unavailable"
        )
    )
    if not policy_matches_delivery:
        raise RebuildError("context transport policy does not match delivery")
    if isinstance(policy, str):
        transport["upstream_host"] = None
    context["transport"] = {
        "upstream_host": transport["upstream_host"],
        "policy": policy,
    }
    require_digest(context["transport_binding_sha256"], "context transport binding")
    route = context.get("route")
    if route is not None:
        route = ordered(
            require_object(route, "context route"),
            ("id", "revision", "binding_sha256"),
        )
        require_optional_string(route.get("id"), "context route.id", nullable=False)
        if len(route["id"].encode("utf-8")) > 128:
            raise RebuildError("context route.id exceeds its supported maximum")
        require_positive_u64(route.get("revision"), "context route.revision")
        require_digest(route.get("binding_sha256"), "context route.binding_sha256")
        if (
            context["upstream_ref"] != f"route/{route['id']}"
            or context.get("backend") is None
        ):
            raise RebuildError("context route/backend linkage is invalid")
        context["route"] = route
    backend = context.get("backend")
    if backend is not None:
        backend = ordered(
            require_object(backend, "context backend"),
            (
                "id",
                "revision",
                "credential_ref",
                "credential_generation",
                "endpoint_profile",
            ),
        )
        require_optional_string(backend.get("id"), "context backend.id", nullable=False)
        if len(backend["id"].encode("utf-8")) > 128:
            raise RebuildError("context backend.id exceeds its supported maximum")
        require_positive_u64(backend.get("revision"), "context backend.revision")
        require_optional_string(
            backend.get("credential_ref"),
            "context backend.credential_ref",
            nullable=False,
        )
        require_positive_u64(
            backend.get("credential_generation"),
            "context backend.credential_generation",
        )
        profile = backend.get("endpoint_profile")
        if profile not in (
            "standard",
            "chatgpt_codex",
            "anthropic_implicit_cache",
            "openai_terminal_chunk_usage",
            "zai_final_choice_usage",
            "deepseek_final_choice_usage",
        ):
            raise RebuildError("context backend endpoint profile is unsupported")
        if (
            profile == "anthropic_implicit_cache"
            and context["endpoint_family"] != "anthropic_messages"
        ):
            raise RebuildError("context backend endpoint profile is incompatible")
        if profile in (
            "openai_terminal_chunk_usage",
            "zai_final_choice_usage",
            "deepseek_final_choice_usage",
        ) and context["endpoint_family"] != "openai_chat_completions":
            raise RebuildError("context backend endpoint profile is incompatible")
        if (
            profile == "chatgpt_codex"
            and context["endpoint_family"] != "openai_responses"
        ):
            raise RebuildError("context backend endpoint profile is incompatible")
        context["backend"] = backend
    cache = context.get("cache")
    if cache is not None:
        cache = ordered(
            require_object(cache, "context cache"),
            (
                "plan_sha256",
                "binding_sha256",
                "intent",
                "capability",
                "native_preserved",
            ),
        )
        require_digest(cache.get("plan_sha256"), "context cache.plan_sha256")
        require_digest(cache.get("binding_sha256"), "context cache.binding_sha256")
        cache["intent"] = canonical_cache_intent(cache.get("intent"))
        cache["capability"] = canonical_cache_capability(
            cache.get("capability"), context["endpoint_family"]
        )
        if not isinstance(cache.get("native_preserved"), bool):
            raise RebuildError("context cache native_preserved must be boolean")
        context["cache"] = cache
    accounting = ordered(
        require_object(context.get("accounting_policy"), "context accounting_policy"),
        ("version", "cache_read_weight"),
    )
    if type(accounting.get("version")) is not int or accounting.get("version") != 1:
        raise RebuildError("context accounting policy is malformed")
    require_nonnegative_u64(
        accounting.get("cache_read_weight"), "cache_read_weight", 1_000_000
    )
    context["accounting_policy"] = accounting
    if context.get("content_policy_revision") is not None:
        require_positive_u64(
            context["content_policy_revision"], "context content_policy_revision"
        )
    elif "content_policy_revision" not in context:
        context["content_policy_revision"] = None
    for field in ("route", "backend", "cache"):
        if field not in context:
            context[field] = None
    return {
        name: context[name]
        for name in CONTEXT_FIELDS
        if name != "operation" or "operation" in context
    }


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def require_tables(connection: sqlite3.Connection) -> None:
    required = {
        "budget_execution_context",
        "budget_physical_attempt",
        "budget_dispatch_fence",
    }
    rows = connection.execute(
        "SELECT name FROM sqlite_master WHERE type='table'"
    ).fetchall()
    present = {row[0] for row in rows}
    missing = required - present
    if missing:
        raise RebuildError(
            f"not a tracked Sandhi ledger; missing tables: {sorted(missing)}"
        )


def quote_identifier(value: str) -> str:
    return '"' + value.replace('"', '""') + '"'


def drop_triggers(connection: sqlite3.Connection) -> None:
    names = [
        row[0]
        for row in connection.execute(
            "SELECT name FROM sqlite_master WHERE type='trigger' ORDER BY name"
        ).fetchall()
    ]
    for name in names:
        connection.execute(f"DROP TRIGGER {quote_identifier(name)}")


def transform_context(row: tuple[str, str]) -> tuple[str, str]:
    execution_id, encoded = row
    try:
        value = json.loads(encoded)
    except (TypeError, json.JSONDecodeError) as error:
        raise RebuildError(f"invalid context JSON for {execution_id}") from error
    if (
        not isinstance(value, dict)
        or type(value.get("version")) is not int
        or value.get("version") != 1
    ):
        raise RebuildError(f"context version must be 1 for {execution_id}")
    if "transport" not in value:
        delivery = value.get("delivery")
        if delivery == "buffered":
            policy: Any = "buffered_legacy_unavailable"
        elif delivery == "streaming":
            policy = "streaming_legacy_unavailable"
        else:
            raise RebuildError(f"invalid delivery for context {execution_id}")
        value["transport"] = {"upstream_host": None, "policy": policy}
    result = canonical_context(value)
    encoded_result = compact_json(result)
    if len(encoded_result.encode("utf-8")) > 16_384:
        raise RebuildError(f"context exceeds the supported size for {execution_id}")
    return execution_id, encoded_result


def legacy_progress(outcome: str) -> dict[str, Any]:
    classes = {
        "success": "none",
        "provider_rejected": "provider",
        "transport_error": "transport",
        "timeout": "timeout",
        "cancelled": "cancelled",
        "incomplete_stream": "incomplete_stream",
    }
    if outcome not in classes:
        raise RebuildError(f"invalid terminal outcome {outcome!r}")
    progress: dict[str, Any] = {}
    if outcome == "timeout":
        progress["timeout_phase"] = "unknown"
        progress["error_class"] = classes[outcome]
        progress["error_subtype"] = "unknown_timeout"
    elif outcome == "incomplete_stream":
        progress["error_class"] = classes[outcome]
        progress["error_subtype"] = "incomplete"
    else:
        progress["error_class"] = classes[outcome]
    return progress


def canonical_attempt(value: dict[str, Any], execution_id: str) -> dict[str, Any]:
    result = ordered(value, ATTEMPT_FIELDS)
    attempt_id = result.get("attempt_id")
    if (
        not isinstance(attempt_id, str)
        or len(attempt_id) != 64
        or any(character not in "0123456789abcdef" for character in attempt_id)
    ):
        raise RebuildError(f"invalid attempt identity for {execution_id}")
    if (
        type(result.get("version")) is not int
        or result.get("version") != 1
        or type(result.get("attempt_ordinal")) is not int
        or result.get("attempt_ordinal") != 1
    ):
        raise RebuildError(f"invalid attempt version or ordinal for {execution_id}")
    if result.get("invalid") not in (True, False):
        raise RebuildError(f"invalid attempt invalid flag for {execution_id}")
    dropped = result.get("dropped_observations")
    if (
        not isinstance(dropped, int)
        or isinstance(dropped, bool)
        or not 0 <= dropped <= 2
    ):
        raise RebuildError(f"invalid dropped observation count for {execution_id}")
    dispatched = result.get("dispatch_observed_at_ms")
    if dispatched is not None:
        require_epoch_ms(dispatched, f"dispatch timestamp for {execution_id}")
    observed = result.get("observed_provider")
    if observed is not None:
        observed = ordered(
            require_object(observed, f"observed provider for {execution_id}"),
            ("provider", "model", "upstream_host"),
        )
        provider = observed.get("provider")
        require_optional_string(
            provider, f"provider for {execution_id}", nullable=False
        )
        if len(provider.encode("utf-8")) > 128:
            raise RebuildError(
                f"provider exceeds its supported maximum for {execution_id}"
            )
        if "model" in observed:
            model = observed.get("model")
            require_optional_string(model, f"model for {execution_id}")
            if model is not None and len(model.encode("utf-8")) > 512:
                raise RebuildError(
                    f"model exceeds its supported maximum for {execution_id}"
                )
        if "upstream_host" in observed:
            require_hostname(
                observed.get("upstream_host"), f"upstream host for {execution_id}"
            )
        observed = {
            name: observed[name]
            for name in ("provider", "model", "upstream_host")
            if name in observed
        }
        result["observed_provider"] = observed
    terminal = result.get("terminal")
    if terminal is not None:
        terminal = ordered(
            require_object(terminal, f"terminal for {execution_id}"),
            TERMINAL_FIELDS,
        )
        if terminal.get("outcome") not in OUTCOMES:
            raise RebuildError(f"invalid terminal outcome for {execution_id}")
        observed_at = terminal.get("observed_at_ms")
        if observed_at is None:
            raise RebuildError(f"invalid terminal timestamp for {execution_id}")
        require_epoch_ms(observed_at, f"terminal timestamp for {execution_id}")
        status = terminal.get("provider_status")
        if status is not None and (
            not isinstance(status, int)
            or isinstance(status, bool)
            or not 100 <= status <= 999
        ):
            raise RebuildError(f"invalid provider status for {execution_id}")
        if "provider_request_id" in terminal:
            request_id = terminal.get("provider_request_id")
            require_optional_string(
                request_id, f"provider request id for {execution_id}"
            )
            if request_id is not None and len(request_id.encode("utf-8")) > 256:
                raise RebuildError(
                    f"provider request id exceeds its supported maximum for {execution_id}"
                )
        progress = ordered(
            require_object(terminal.get("progress"), f"progress for {execution_id}"),
            PROGRESS_FIELDS,
        )
        if progress.get("error_class") not in ERROR_CLASSES:
            raise RebuildError(f"invalid transport progress for {execution_id}")
        for field in ("response_headers_received", "stream_event_received"):
            if field in progress and not isinstance(progress[field], bool):
                raise RebuildError(
                    f"progress {field} must be boolean for {execution_id}"
                )
        for field in ("first_stream_event_at_ms", "time_to_first_stream_event_ms"):
            if field in progress:
                timestamp = progress[field]
                if timestamp is None:
                    continue
                if field == "first_stream_event_at_ms":
                    require_epoch_ms(timestamp, f"progress {field} for {execution_id}")
                else:
                    require_nonnegative_u64(
                        timestamp,
                        f"progress {field} for {execution_id}",
                        4_294_967_295,
                    )
        timeout_phase = progress.get("timeout_phase")
        if timeout_phase is not None:
            if timeout_phase not in TIMEOUT_PHASES:
                raise RebuildError(f"invalid timeout phase for {execution_id}")
            if progress.get("error_class") != "timeout":
                raise RebuildError(
                    f"timeout phase without timeout class for {execution_id}"
                )
        error_subtype = progress.get("error_subtype")
        if error_subtype is not None:
            if error_subtype not in ERROR_SUBTYPES:
                raise RebuildError(f"invalid error subtype for {execution_id}")
            if progress.get("error_class") == "none":
                raise RebuildError(
                    f"error subtype without error class for {execution_id}"
                )
        progress = {
            name: progress[name]
            for name in PROGRESS_FIELDS
            if progress.get(name) is not None
        }
        terminal["progress"] = progress
        result["terminal"] = terminal
    for field in ("dispatch_observed_at_ms", "terminal"):
        if field not in result:
            result[field] = None
    return {
        name: result[name]
        for name in ATTEMPT_FIELDS
        if name != "observed_provider" or "observed_provider" in result
    }


def transform_attempt(row: tuple[str, str, str]) -> tuple[str, str, str, str]:
    execution_id, persisted_attempt_id, encoded = row
    try:
        value = json.loads(encoded)
    except (TypeError, json.JSONDecodeError) as error:
        raise RebuildError(f"invalid attempt JSON for {execution_id}") from error
    if not isinstance(value, dict) or value.get("version") not in (1, 2):
        raise RebuildError(f"unsupported attempt version for {execution_id}")

    result = ordered(value, ATTEMPT_FIELDS)
    terminal = result.get("terminal")
    if terminal is not None:
        if not isinstance(terminal, dict):
            raise RebuildError(f"invalid terminal for attempt {execution_id}")
        terminal = ordered(terminal, TERMINAL_FIELDS)
        progress = terminal.get("progress")
        if not isinstance(progress, dict):
            progress = legacy_progress(terminal["outcome"])
        else:
            progress = ordered(progress, PROGRESS_FIELDS)
        terminal["progress"] = progress
        result["terminal"] = terminal
    result["version"] = 1
    result = canonical_attempt(result, execution_id)
    encoded_result = compact_json(result)
    if len(encoded_result.encode("utf-8")) > 4_096:
        raise RebuildError(f"attempt exceeds the supported size for {execution_id}")
    return execution_id, persisted_attempt_id, result["attempt_id"], encoded_result


def read_only_connection(path: Path) -> sqlite3.Connection:
    connection = sqlite3.connect(
        f"file:{quote(str(path))}?mode=ro", uri=True, timeout=60.0
    )
    connection.execute("PRAGMA busy_timeout=60000")
    return connection


def source_identity(connection: sqlite3.Connection) -> dict[str, Any]:
    tables = [
        row[0]
        for row in connection.execute(
            "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
        )
    ]
    counts = {
        table: connection.execute(f'SELECT count(*) FROM "{table}"').fetchone()[0]
        for table in tables
    }
    reservations = connection.execute(
        "SELECT scope, count(*), sum(ceiling), sum(actual), sum(settled) "
        "FROM budget_reservation GROUP BY scope ORDER BY scope"
    ).fetchall()
    unresolved_reservations = connection.execute(
        "SELECT count(*) FROM budget_reservation WHERE settled=0"
    ).fetchone()[0]
    pragmas = {
        name: connection.execute(f"PRAGMA {name}").fetchone()[0]
        for name in ("data_version", "schema_version", "page_count", "freelist_count")
    }
    return {
        "tables": counts,
        "reservations": reservations,
        "unresolved_reservations": unresolved_reservations,
        "pragmas": pragmas,
    }


def validate_rebuilt(
    connection: sqlite3.Connection,
    context_rows: list[tuple[str, str]],
    attempt_rows: list[tuple[str, str, str]],
    expected_reservations: list[Any],
) -> None:
    missing_contexts = connection.execute(
        "SELECT count(*) FROM budget_dispatch_fence f "
        "LEFT JOIN budget_execution_context c USING(execution_id) "
        "WHERE (f.context_version=1 AND c.execution_id IS NULL) "
        "OR (f.context_version!=1 AND c.execution_id IS NOT NULL) "
        "OR (f.context_version!=0 AND c.execution_id IS NULL)"
    ).fetchone()[0]
    orphan_contexts = connection.execute(
        "SELECT count(*) FROM budget_execution_context c "
        "LEFT JOIN budget_dispatch_fence f USING(execution_id) "
        "WHERE f.execution_id IS NULL OR f.context_version != 1"
    ).fetchone()[0]
    orphan_attempts = connection.execute(
        "SELECT count(*) FROM budget_physical_attempt a "
        "LEFT JOIN budget_dispatch_fence f USING(execution_id) "
        "WHERE f.execution_id IS NULL OR f.attempt_version != 1 "
        "OR f.attempt_id IS NULL OR f.attempt_id IS NOT a.attempt_id"
    ).fetchone()[0]
    missing_attempts = connection.execute(
        "SELECT count(*) FROM budget_dispatch_fence f "
        "LEFT JOIN budget_physical_attempt a USING(execution_id) "
        "WHERE (f.attempt_version=1 AND a.execution_id IS NULL) "
        "OR (f.attempt_version!=1 AND a.execution_id IS NOT NULL) "
        "OR (f.attempt_version!=0 AND a.execution_id IS NULL)"
    ).fetchone()[0]
    invalid_attempts = connection.execute(
        "SELECT count(*) FROM budget_dispatch_fence f "
        "JOIN budget_physical_attempt a USING(execution_id) "
        "WHERE f.attempt_version=1 "
        "AND (f.phase!=1 OR f.transitioned_at IS NULL "
        "OR f.transitioned_at!=a.authorized_at)"
    ).fetchone()[0]
    mismatched_attempts = connection.execute(
        "SELECT count(*) FROM budget_physical_attempt a "
        "JOIN budget_dispatch_fence f USING(execution_id) "
        "JOIN budget_execution_context c USING(execution_id) "
        "WHERE f.attempt_id IS NULL "
        "OR json_extract(a.evidence_json, '$.attempt_id') IS NOT a.attempt_id "
        "OR json_extract(a.evidence_json, '$.attempt_id') IS NOT f.attempt_id"
    ).fetchone()[0]
    if (
        missing_contexts
        or orphan_contexts
        or missing_attempts
        or orphan_attempts
        or invalid_attempts
        or mismatched_attempts
    ):
        raise RebuildError("rebuilt execution context and attempt linkage is invalid")
    for source_execution_id, source_encoded in context_rows:
        expected_execution_id, expected_encoded = transform_context(
            (source_execution_id, source_encoded)
        )
        actual = connection.execute(
            "SELECT context_json FROM budget_execution_context WHERE execution_id=?",
            (expected_execution_id,),
        ).fetchone()
        if actual is None or actual[0] != expected_encoded:
            raise RebuildError(
                f"rebuilt context differs from canonical evidence: {expected_execution_id}"
            )
    for source_attempt in attempt_rows:
        expected_execution_id, persisted_attempt_id, embedded_attempt_id, expected_encoded = (
            transform_attempt(source_attempt)
        )
        if embedded_attempt_id != persisted_attempt_id:
            raise RebuildError(f"attempt identity mismatch for {expected_execution_id}")
        actual = connection.execute(
            "SELECT evidence_json FROM budget_physical_attempt WHERE execution_id=?",
            (expected_execution_id,),
        ).fetchone()
        if actual is None or actual[0] != expected_encoded:
            raise RebuildError(
                f"rebuilt attempt differs from canonical evidence: {expected_execution_id}"
            )
    actual_reservations = connection.execute(
        "SELECT scope, count(*), sum(ceiling), sum(actual), sum(settled) "
        "FROM budget_reservation GROUP BY scope ORDER BY scope"
    ).fetchall()
    if actual_reservations != expected_reservations:
        raise RebuildError("rebuilt ledger does not conserve reservations")
    if connection.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
        raise RebuildError("output integrity_check failed")
    validate_backends(connection)


def validate_backends(connection: sqlite3.Connection) -> None:
    exists = connection.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='backends'"
    ).fetchone()
    if not exists:
        return
    rows = connection.execute(
        "SELECT id, definition_json FROM backends ORDER BY id"
    ).fetchall()
    for backend_id, encoded in rows:
        try:
            value = json.loads(encoded)
        except (TypeError, json.JSONDecodeError) as error:
            raise RebuildError(
                f"invalid rebuilt backend definition JSON for {backend_id}"
            ) from error
        if value.get("id") != backend_id:
            raise RebuildError(f"rebuilt backend identity mismatch for {backend_id}")
        if compact_json(canonical_backend_definition(value)) != encoded:
            raise RebuildError(
                f"rebuilt backend definition is noncanonical for {backend_id}"
            )


def source_snapshot(source: Path) -> dict[str, str]:
    snapshot: dict[str, str] = {}
    for suffix in ("", "-wal", "-shm"):
        candidate = source.with_name(source.name + suffix)
        if candidate.exists():
            snapshot[str(candidate)] = sha256_file(candidate)
    return snapshot


def backend_definition_is_canonical(value: Any) -> bool:
    try:
        transformed = canonical_backend_definition(value)
    except (KeyError, TypeError, RebuildError):
        return False
    return value == transformed


def require_backend_label(value: Any, label: str, maximum: int) -> None:
    if not isinstance(value, str) or not is_canonical_label(value):
        raise RebuildError(f"{label} must be a non-empty canonical label")
    if len(value.encode("utf-8")) > maximum:
        raise RebuildError(f"{label} exceeds its supported maximum")


def require_vault_reference(value: Any, label: str, maximum: int = 128) -> None:
    if (
        not isinstance(value, str)
        or not value
        or value.endswith(".")
        or len(value) > maximum
        or any(
            not (
                character.isascii()
                and (character.islower() or character.isdigit() or character in "._-")
            )
            for character in value
        )
    ):
        raise RebuildError(f"{label} is not a valid backend reference")


def require_runtime_url_host(host: str) -> None:
    if ":" in host:
        try:
            ipaddress.IPv6Address(host)
        except ValueError as error:
            raise RebuildError("backend base URL host is invalid") from error
        return
    forbidden = set("\0\t\n\r #/:<>?@[\\]^|%" + chr(0x7F))
    if any(character in forbidden or ord(character) < 0x20 for character in host):
        raise RebuildError("backend base URL host is invalid")
    if idna is not None:
        try:
            idna.encode(
                host,
                uts46=True,
                transitional=False,
                std3_rules=True,
            )
        except idna.IDNAError as error:
            raise RebuildError("backend base URL host is invalid") from error
    elif not host.isascii() or not all(
        character.isascii() and (character.isalnum() or character in ".-")
        for character in host
    ):
        raise RebuildError("backend base URL host is invalid")


def require_backend_base_url(value: Any) -> None:
    if (
        not isinstance(value, str)
        or len(value.encode("utf-8")) > 2048
        or any(ord(character) < 32 or ord(character) == 127 for character in value)
    ):
        raise RebuildError("backend base URL is invalid")
    try:
        parsed = urlsplit(value)
        parsed.port
    except ValueError as error:
        raise RebuildError("backend base URL is invalid") from error
    if (
        parsed.scheme not in ("http", "https")
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
    ):
        raise RebuildError(
            "backend base URL must be HTTP(S) without user info, query or fragment"
        )
    require_runtime_url_host(parsed.hostname)


def require_optional_nonnegative_u64(
    value: Any, label: str, *, maximum: int | None = None
) -> None:
    if value is not None:
        require_nonnegative_u64(value, label, maximum)


def canonical_thinking_capability(
    value: Any, endpoint_family: str
) -> dict[str, Any]:
    thinking = ordered(
        require_object(value, "backend model thinking"), tuple(THINKING_FIELDS)
    )
    for field in ("can_disable", "allow_effort_and_budget"):
        if field in thinking and not isinstance(thinking[field], bool):
            raise RebuildError(f"backend thinking {field} must be boolean")
    supported = thinking.get("supported")
    if not isinstance(supported, bool):
        raise RebuildError("backend model thinking.supported must be boolean")
    controls = tuple(
        thinking[name]
        for name in ("encoding", "effort", "budget_tokens")
        if name in thinking
    )
    if not supported:
        if any(value is not None for value in controls):
            raise RebuildError("unsupported backend thinking carries controls")
        if thinking.get("can_disable") or thinking.get("allow_effort_and_budget"):
            raise RebuildError("unsupported backend thinking carries controls")
        return {"supported": False}

    encoding = thinking.get("encoding")
    if encoding not in THINKING_ENCODING_FAMILIES:
        raise RebuildError("supported backend thinking requires a known encoding")
    if THINKING_ENCODING_FAMILIES[encoding] != endpoint_family:
        raise RebuildError("backend thinking encoding is incompatible with endpoint")
    effort = thinking.get("effort")
    if effort is not None:
        effort = ordered(
            require_object(effort, "backend thinking effort"),
            tuple(THINKING_EFFORT_FIELDS),
        )
        values = effort.get("values")
        if (
            not isinstance(values, list)
            or not 1 <= len(values) <= 16
            or any(
                not isinstance(value, str)
                or not value
                or len(value.encode("utf-8")) > 16
                or any(
                    character not in "abcdefghijklmnopqrstuvwxyz0123456789_"
                    for character in value
                )
                for value in values
            )
            or len(set(values)) != len(values)
        ):
            raise RebuildError("backend thinking effort values are invalid")
        default = effort.get("default")
        if default is not None and default not in values:
            raise RebuildError("backend thinking effort default is not advertised")
        if encoding not in THINKING_EFFORT_ENCODINGS:
            raise RebuildError("backend thinking encoding cannot express effort")
        thinking["effort"] = effort
    budget = thinking.get("budget_tokens")
    if budget is not None:
        budget = ordered(
            require_object(budget, "backend thinking budget"),
            tuple(THINKING_BUDGET_FIELDS),
        )
        minimum = budget.get("min")
        maximum = budget.get("max")
        if (
            not isinstance(minimum, int)
            or isinstance(minimum, bool)
            or not isinstance(maximum, int)
            or isinstance(maximum, bool)
            or minimum <= 0
            or maximum < minimum
            or maximum > 262_144
        ):
            raise RebuildError("backend thinking budget bounds are invalid")
        if encoding not in THINKING_BUDGET_ENCODINGS:
            raise RebuildError("backend thinking encoding cannot express a budget")
        thinking["budget_tokens"] = budget
    if effort is None and budget is None:
        raise RebuildError("supported backend thinking requires a control")
    if thinking.get("can_disable") and encoding not in THINKING_BUDGET_ENCODINGS:
        raise RebuildError("backend thinking encoding cannot express disable")
    if thinking.get("allow_effort_and_budget") and (effort is None or budget is None):
        raise RebuildError("combined backend thinking requires effort and budget")
    return thinking


def canonical_backend_definition(value: Any) -> dict[str, Any]:
    definition = require_object(value, "backend definition")
    definition = ordered(definition, tuple(BACKEND_FIELDS))
    for field in ("id", "provider", "credential_ref", "endpoint", "models"):
        if field not in definition:
            raise RebuildError(f"backend definition is missing {field}")
    require_vault_reference(definition["id"], "backend id")
    require_vault_reference(definition["provider"], "backend provider")
    credential_ref = definition["credential_ref"]
    if not isinstance(credential_ref, str) or credential_ref.count(":") != 1:
        raise RebuildError("backend credential_ref requires provider:label")
    credential_provider, credential_label = credential_ref.split(":", 1)
    require_vault_reference(credential_provider, "credential provider")
    require_vault_reference(credential_label, "credential label")

    endpoint = ordered(
        require_object(definition["endpoint"], "backend endpoint"),
        ("endpoint_family", "endpoint_profile"),
    )
    endpoint_family = endpoint.get("endpoint_family")
    if endpoint_family not in ENDPOINT_FAMILIES:
        raise RebuildError("backend endpoint family is unsupported")
    endpoint_profile = endpoint.get("endpoint_profile", "standard")
    if (
        endpoint_profile not in ENDPOINT_PROFILE_FAMILIES
        or endpoint_family not in ENDPOINT_PROFILE_FAMILIES[endpoint_profile]
    ):
        raise RebuildError("backend endpoint profile is incompatible")
    endpoint["endpoint_profile"] = endpoint_profile
    definition["endpoint"] = endpoint
    for field in ("base_url", "user_agent"):
        value = definition.get(field)
        if field == "base_url":
            if value is not None:
                require_backend_base_url(value)
        else:
            if value is not None and (
                not isinstance(value, str)
                or not value
                or not value.isascii()
                or len(value.encode("utf-8")) > 256
                or any(
                    ord(character) < 32 or ord(character) == 127
                    for character in value
                )
            ):
                raise RebuildError("backend user_agent is invalid")

    models = definition["models"]
    if not isinstance(models, list) or not 1 <= len(models) <= 128:
        raise RebuildError("backend requires between 1 and 128 models")
    ids: set[str] = set()
    canonical_models = []
    for raw_model in models:
        model = ordered(
            require_object(raw_model, "backend model"), tuple(BACKEND_MODEL_FIELDS)
        )
        if "id" not in model:
            raise RebuildError("backend model is missing id")
        require_backend_label(model["id"], "backend model id", 512)
        if model["id"].lower() in ids:
            raise RebuildError("backend model ids must be unique")
        ids.add(model["id"].lower())
        if "aliases" in model and (
            not isinstance(model["aliases"], list) or model["aliases"]
        ):
            raise RebuildError("backend model aliases are not supported")
        require_optional_nonnegative_u64(
            model.get("max_input_tokens"), "backend model max_input_tokens"
        )
        require_optional_nonnegative_u64(
            model.get("max_output_tokens"), "backend model max_output_tokens"
        )
        temperature = model.get("default_temperature")
        if temperature is not None and (
            isinstance(temperature, bool)
            or not isinstance(temperature, (int, float))
            or not math.isfinite(temperature)
        ):
            raise RebuildError("backend model default_temperature is invalid")
        capabilities = model.get("capabilities", {})
        capabilities = ordered(
            require_object(capabilities, "backend model capabilities"),
            tuple(MODEL_CAPABILITY_FIELDS),
        )
        if any(not isinstance(item, bool) for item in capabilities.values()):
            raise RebuildError("backend model capabilities must be boolean")
        model["capabilities"] = capabilities
        if model.get("endpoint_url") is not None:
            raise RebuildError("backend model endpoint overrides are not supported")
        extensions = model.get("extensions", {})
        if not isinstance(extensions, dict) or extensions:
            raise RebuildError("backend model extensions are not supported")
        model["extensions"] = extensions
        if "thinking" not in model:
            model["thinking"] = {"supported": False}
        model["thinking"] = canonical_thinking_capability(
            model["thinking"], endpoint_family
        )
        model = ordered(model, tuple(BACKEND_MODEL_FIELDS))
        canonical_models.append(model)
    definition["models"] = canonical_models

    cache_policy = definition.get("cache_policy")
    if cache_policy is not None:
        cache_policy = ordered(
            require_object(cache_policy, "backend cache policy"),
            ("default_intent", "allow_request_override", "namespace", "models"),
        )
        if not isinstance(cache_policy.get("allow_request_override", True), bool):
            raise RebuildError("backend cache override policy is invalid")
        namespace = cache_policy.get("namespace", "provider_default")
        if namespace not in CACHE_NAMESPACES:
            raise RebuildError("backend cache namespace is unsupported")
        policy_models = cache_policy.get("models")
        if not isinstance(policy_models, dict) or set(policy_models) != ids:
            raise RebuildError("backend cache policy model linkage is invalid")
        for model_capabilities in policy_models.values():
            capabilities = ordered(
                require_object(
                    model_capabilities, "backend cache capabilities"
                ),
                ("profile", "lifetimes", "max_breakpoints"),
            )
            profile = canonical_cache_capability(
                capabilities.get("profile"), endpoint_family
            )
            capabilities["profile"] = profile
            lifetimes = capabilities.get("lifetimes", [])
            capabilities["lifetimes"] = lifetimes
            allowed = CACHE_PROFILE_LIFETIMES[profile]
            if (
                not isinstance(lifetimes, list)
                or len(lifetimes) != len(set(lifetimes))
                or any(lifetime not in allowed for lifetime in lifetimes)
            ):
                raise RebuildError("backend cache lifetimes are invalid")
            breakpoints = capabilities.get("max_breakpoints")
            if profile in ("anthropic_breakpoints", "openai_explicit"):
                if not isinstance(breakpoints, int) or isinstance(breakpoints, bool) or not 1 <= breakpoints <= 4:
                    raise RebuildError("backend cache breakpoint limit is invalid")
            elif breakpoints is not None:
                raise RebuildError("backend cache breakpoint limit is unsupported")
            if namespace == "scoped_key" and profile not in (
                "openai_legacy",
                "openai_explicit",
            ):
                raise RebuildError("scoped cache keys require an OpenAI cache profile")
            default_intent = cache_policy.get("default_intent")
            if default_intent is not None:
                default_intent = canonical_cache_intent(default_intent)
                if default_intent["mode"] == "auto" and profile == "native_only":
                    raise RebuildError("automatic cache default requires capability")
                if (
                    default_intent.get("lifetime") is not None
                    and default_intent["lifetime"] not in lifetimes
                ):
                    raise RebuildError("default cache lifetime is undeclared")
                if default_intent["mode"] in (
                    "auto_stable_prefix",
                    "auto_conversation_prefix",
                ) and profile not in ("anthropic_breakpoints", "openai_explicit"):
                    raise RebuildError("automatic cache placement requires breakpoints")
                cache_policy["default_intent"] = default_intent
        if endpoint_profile == "chatgpt_codex" and any(
            value.get("profile") != "native_only" for value in policy_models.values()
        ):
            raise RebuildError("Codex cache capabilities must remain native-only")
        cache_policy["namespace"] = namespace
        cache_policy["models"] = policy_models
        definition["cache_policy"] = cache_policy
    encoded_definition = compact_json(definition)
    if len(encoded_definition.encode("utf-8")) > 65_536:
        raise RebuildError("backend definition exceeds size limit")
    return definition


def migrate_backends(connection: sqlite3.Connection) -> int:
    exists = connection.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='backends'"
    ).fetchone()
    if not exists:
        return 0
    rows = connection.execute(
        "SELECT id, definition_json FROM backends ORDER BY id"
    ).fetchall()
    if len(rows) > 256:
        raise RebuildError("backend registry exceeds capacity")
    changed = 0
    for backend_id, encoded in rows:
        try:
            value = json.loads(encoded)
        except (TypeError, json.JSONDecodeError) as error:
            raise RebuildError(
                f"invalid backend definition JSON for {backend_id}"
            ) from error
        if value.get("id") != backend_id:
            raise RebuildError(f"backend definition identity mismatch for {backend_id}")
        transformed = canonical_backend_definition(value)
        encoded_transformed = compact_json(transformed)
        if encoded_transformed != encoded:
            connection.execute(
                "UPDATE backends SET definition_json=? WHERE id=?",
                (encoded_transformed, backend_id),
            )
            actual = connection.execute(
                "SELECT definition_json FROM backends WHERE id=?", (backend_id,)
            ).fetchone()[0]
            if actual != encoded_transformed:
                raise RebuildError(
                    f"rebuilt backend definition changed during update: {backend_id}"
                )
            changed += 1
    return changed


def rebuild(source: Path, output: Path, manifest: Path) -> dict[str, Any]:
    source = source.resolve()
    output = output.resolve()
    manifest = manifest.resolve()
    if len({source, output, manifest}) != 3:
        raise RebuildError("source, output, and manifest must be distinct")
    output_sidecars = (
        output.with_name(output.name + "-wal"),
        output.with_name(output.name + "-shm"),
        output.with_name(output.name + "-journal"),
    )
    if source in output_sidecars or manifest in output_sidecars:
        raise RebuildError("source and manifest must not alias output SQLite sidecars")
    for candidate in (output, manifest):
        if candidate in (
            source.with_name(source.name + "-wal"),
            source.with_name(source.name + "-shm"),
        ):
            raise RebuildError("output and manifest must not alias SQLite sidecars")
    if not source.is_file():
        raise RebuildError(f"source ledger does not exist: {source}")
    if output.exists() or output.is_symlink():
        raise RebuildError(f"output already exists: {output}")
    if any(path.exists() or path.is_symlink() for path in output_sidecars):
        raise RebuildError("output SQLite sidecars already exist")
    if manifest.exists():
        raise RebuildError(f"manifest already exists: {manifest}")
    output.parent.mkdir(parents=True, exist_ok=True)
    manifest.parent.mkdir(parents=True, exist_ok=True)

    source_snapshot_before = source_snapshot(source)
    source_hash_before = source_snapshot_before[str(source)]
    source_stat = source.stat()
    snapshot_directory = tempfile.TemporaryDirectory(prefix="sandhi-ledger-rebuild-")
    staging_source = Path(snapshot_directory.name) / source.name
    for suffix in ("", "-wal", "-shm"):
        candidate = source.with_name(source.name + suffix)
        if candidate.exists():
            shutil.copy2(
                candidate,
                staging_source.with_name(staging_source.name + suffix),
            )
    source_connection = read_only_connection(staging_source)
    destination_connection = None
    final_connection = None
    completed = False
    try:
        if source_connection.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
            raise RebuildError("source integrity_check failed")
        require_tables(source_connection)
        before_identity = source_identity(source_connection)
        source_connection.close()

        with output.open("xb") as claimed_output:
            claimed_output.close()
        os.chmod(output, source_stat.st_mode & 0o7777)
        try:
            os.chown(output, source_stat.st_uid, source_stat.st_gid)
        except OSError as error:
            raise RebuildError(
                "output ownership cannot match the source ledger"
            ) from error
        destination_connection = sqlite3.connect(output)
        backup_connection = read_only_connection(staging_source)
        try:
            backup_connection.backup(destination_connection)
        finally:
            backup_connection.close()
        drop_triggers(destination_connection)

        source_snapshot_after = source_snapshot(source)
        source_connection = read_only_connection(staging_source)
        after_identity = source_identity(source_connection)
        source_connection.close()
        snapshot_directory.cleanup()
        if (
            source_snapshot_after != source_snapshot_before
            or after_identity != before_identity
        ):
            raise RebuildError("source ledger changed while its backup was created")

        require_tables(destination_connection)
        context_rows = destination_connection.execute(
            "SELECT execution_id, context_json FROM budget_execution_context "
            "ORDER BY execution_id"
        ).fetchall()
        attempt_rows = destination_connection.execute(
            "SELECT a.execution_id,a.attempt_id,a.evidence_json "
            "FROM budget_physical_attempt a LEFT JOIN budget_dispatch_fence f "
            "USING(execution_id) WHERE a.evidence_json IS NOT NULL "
            "AND f.attempt_id=a.attempt_id "
            "ORDER BY a.execution_id"
        ).fetchall()
        if (
            len(attempt_rows)
            != destination_connection.execute(
                "SELECT count(*) FROM budget_physical_attempt WHERE evidence_json IS NOT NULL"
            ).fetchone()[0]
        ):
            raise RebuildError("source attempt identity linkage is invalid")
        if destination_connection.execute(
            "SELECT count(*) FROM budget_dispatch_fence f "
            "LEFT JOIN budget_execution_context c USING(execution_id) "
            "WHERE c.execution_id IS NULL AND coalesce(f.context_version,0)!=0"
        ).fetchone()[0]:
            raise RebuildError("source context dispatch marker has no context")
        if destination_connection.execute(
            "SELECT count(*) FROM budget_dispatch_fence f "
            "LEFT JOIN budget_physical_attempt a USING(execution_id) "
            "WHERE a.execution_id IS NULL AND coalesce(f.attempt_version,0)!=0"
        ).fetchone()[0]:
            raise RebuildError("source attempt dispatch marker has no attempt")
        transformed_contexts = [transform_context(row) for row in context_rows]
        transformed_attempts = [transform_attempt(row) for row in attempt_rows]
        destination_connection.execute("PRAGMA busy_timeout=60000")
        destination_connection.execute("BEGIN IMMEDIATE")
        for execution_id, encoded in transformed_contexts:
            changed = destination_connection.execute(
                "UPDATE budget_execution_context SET context_json=:encoded "
                "WHERE execution_id=:execution_id",
                {"execution_id": execution_id, "encoded": encoded},
            ).rowcount
            if changed != 1:
                raise RebuildError(
                    f"context disappeared during rebuild: {execution_id}"
                )
        for (
            execution_id,
            persisted_attempt_id,
            embedded_attempt_id,
            encoded,
        ) in transformed_attempts:
            if persisted_attempt_id != embedded_attempt_id:
                raise RebuildError(
                    f"embedded attempt identity mismatch: {execution_id}"
                )
            changed = destination_connection.execute(
                "UPDATE budget_physical_attempt SET evidence_json=:encoded "
                "WHERE execution_id=:execution_id",
                {"execution_id": execution_id, "encoded": encoded},
            ).rowcount
            if changed != 1:
                raise RebuildError(
                    f"attempt disappeared during rebuild: {execution_id}"
                )
        changed = destination_connection.execute(
            "UPDATE budget_dispatch_fence SET context_version=1 "
            "WHERE execution_id IN (SELECT execution_id FROM budget_execution_context)"
        ).rowcount
        if changed != len(context_rows):
            raise RebuildError("context dispatch markers disappeared during rebuild")
        changed = destination_connection.execute(
            "UPDATE budget_dispatch_fence SET context_version=0 "
            "WHERE execution_id NOT IN (SELECT execution_id FROM budget_execution_context)"
        ).rowcount
        if changed < 0:
            raise RebuildError("context dispatch markers could not be normalized")
        changed = destination_connection.execute(
            "UPDATE budget_dispatch_fence SET attempt_version=1 "
            "WHERE execution_id IN (SELECT execution_id FROM budget_physical_attempt)"
        ).rowcount
        if changed != len(
            destination_connection.execute(
                "SELECT execution_id FROM budget_physical_attempt"
            ).fetchall()
        ):
            raise RebuildError("attempt dispatch markers disappeared during rebuild")
        changed = destination_connection.execute(
            "UPDATE budget_dispatch_fence SET attempt_version=0 "
            "WHERE execution_id NOT IN (SELECT execution_id FROM budget_physical_attempt)"
        ).rowcount
        if changed < 0:
            raise RebuildError("attempt dispatch markers could not be normalized")
        destination_connection.commit()
        migrated_backends = migrate_backends(destination_connection)
        destination_connection.commit()
        validate_rebuilt(
            destination_connection,
            context_rows,
            attempt_rows,
            before_identity["reservations"],
        )
        if destination_connection.execute("PRAGMA journal_mode=DELETE").fetchone()[0] != "delete":
            raise RebuildError("output could not be finalized as a standalone database")
        destination_connection.commit()
        try:
            destination_connection.close()
        finally:
            destination_connection = None
        final_connection = read_only_connection(output)
        try:
            validate_rebuilt(
                final_connection,
                context_rows,
                attempt_rows,
                before_identity["reservations"],
            )
            if final_connection.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
                raise RebuildError("final output integrity_check failed")
        finally:
            final_connection.close()
            final_connection = None
        if any(path.exists() or path.is_symlink() for path in output_sidecars):
            raise RebuildError("final output unexpectedly has SQLite sidecars")
        final_stat = output.stat()
        if (
            final_stat.st_mode & 0o7777 != source_stat.st_mode & 0o7777
            or final_stat.st_uid != source_stat.st_uid
            or final_stat.st_gid != source_stat.st_gid
        ):
            raise RebuildError("final output ownership or mode differs from source")
        receipt = {
            "schema": "sandhi_prega_ledger_rebuild_v1",
            "source": str(source),
            "output": str(output),
            "source_bytes": source.stat().st_size,
            "output_bytes": output.stat().st_size,
            "output_mode": final_stat.st_mode & 0o7777,
            "output_uid": final_stat.st_uid,
            "output_gid": final_stat.st_gid,
            "source_sha256": source_hash_before,
            "source_snapshot_sha256": {
                Path(path).name[len(source.name) :]: digest
                for path, digest in source_snapshot_before.items()
            },
            "output_sha256": sha256_file(output),
            "execution_contexts": len(context_rows),
            "physical_attempts_with_evidence": len(attempt_rows),
            "legacy_backends_migrated_to_unsupported_thinking": migrated_backends,
            "reservation_inventory": before_identity["reservations"],
            "unresolved_reservations": before_identity["unresolved_reservations"],
            "clean_evidence_version": 1,
        }
        write_manifest(manifest, receipt)
        completed = True
    except Exception:
        if destination_connection is not None:
            destination_connection.rollback()
        raise
    finally:
        source_connection.close()
        if final_connection is not None:
            final_connection.close()
        if destination_connection is not None:
            destination_connection.close()
        if not completed:
            output.unlink(missing_ok=True)
            for path in output_sidecars:
                path.unlink(missing_ok=True)
            manifest.unlink(missing_ok=True)

    return receipt


def write_manifest(path: Path, value: dict[str, Any]) -> None:
    encoded = json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True) + "\n"
    descriptor, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            output.write(encoded)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    except Exception:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
        raise


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--manifest", required=True, type=Path)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        receipt = rebuild(args.source, args.output, args.manifest)
    except (OSError, sqlite3.Error, RebuildError) as error:
        print(f"rebuild failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(receipt, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
