#!/usr/bin/env python3
"""Opt-in live SDK matrix. Uses a disposable gateway; never edits user settings.

Requires openai, anthropic, google-genai and httpx. Build sandhi-proxy first.
Every cell issues one inference, without SDK retries. Artifacts contain synthetic
prompts/results and usage evidence, never credentials. --live is mandatory.
"""

import argparse
from datetime import datetime, timezone
import hashlib
from importlib.metadata import version
import json
import os
import re
from pathlib import Path
import secrets
import socket
import sqlite3
import subprocess
import tempfile
import time

import anthropic
from google import genai
from google.genai import types
import httpx
import openai


ROOT = Path(__file__).resolve().parents[1]
ENDPOINTS = {
    "chat": ("openai_chat_completions", "https://api.z.ai/api/coding/paas/v4"),
    "messages": ("anthropic_messages", "https://api.z.ai/api/anthropic"),
    "responses": ("openai_responses", "https://api.z.ai/api/v1"),
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--live", action="store_true")
    parser.add_argument("--settings", type=Path, default=Path.home() / ".claude/settings.json.zai")
    parser.add_argument("--model", default="glm-5.3")
    parser.add_argument("--backend", choices=list(ENDPOINTS), action="append")
    parser.add_argument("--client", choices=["chat", "messages", "gemini", "responses"], action="append")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--verify", type=Path, help="Validate an existing sanitized report without live calls")
    parser.add_argument("--cache-pairs", action="store_true", help="Two identical long-prefix calls per cell")
    parser.add_argument("--cache-profile", choices=["native_only", "implicit_only"], default="native_only")
    parser.add_argument("--accounting", choices=["legacy", "tracked"], default="legacy")
    parser.add_argument("--delivery", choices=["buffered", "streaming", "both"], default="both")
    parser.add_argument("--logical-route", action="store_true", help="Resolve an authorized alias before dispatch")
    args = parser.parse_args()
    if args.verify:
        report = json.loads(args.verify.read_text())
        report["verification"] = verify_report(report)
        args.verify.write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report["verification"], indent=2))
        raise SystemExit(bool(report["verification"]["errors"]))
    if not args.live:
        parser.error("--live is required: this consumes provider inference quota")
    if args.output is None:
        parser.error("--output is required for live evidence")
    if args.accounting == "tracked" and args.delivery != "buffered" and any(
        (backend, client) not in (("chat", "chat"), ("responses", "responses"))
        for backend in args.backend or ENDPOINTS
        for client in args.client or ["chat", "messages", "gemini", "responses"]
    ):
        parser.error("tracked streaming currently requires same-family chat/chat or responses/responses selections")
    if any(values and len(set(values)) != len(values) for values in (args.backend, args.client)):
        parser.error("duplicate backend/client selections would repeat inference cells")
    credential = json.loads(args.settings.read_text())["env"]["ANTHROPIC_AUTH_TOKEN"]
    private = [credential, secrets.token_urlsafe(32)]

    def sanitize(value):
        if isinstance(value, str):
            for secret in private:
                value = value.replace(secret, "[redacted]")
            return value
        if isinstance(value, dict):
            return {key: sanitize(item) for key, item in value.items()}
        if isinstance(value, list):
            return [sanitize(item) for item in value]
        return value

    report = {"started_at": datetime.now(timezone.utc).isoformat(), "model": args.model,
              "endpoints": {key: value[1] for key, value in ENDPOINTS.items()},
              "cache_policy": args.cache_profile + (" / auto" if args.cache_profile == "implicit_only" else " / preserve_native"),
              "sdk_versions": {package: version(package) for package in ("openai", "anthropic", "google-genai")},
              "sdk_retries": 0,
              "accounting": args.accounting, "delivery": args.delivery, "logical_route": args.logical_route,
              "endpoint_profiles": {backend: "anthropic_implicit_cache" if backend == "messages" else "standard"
                                    for backend in args.backend or ENDPOINTS},
              "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "cache_pairs": args.cache_pairs, "cells": []}
    with (ROOT / "target/debug/sandhi-proxy").open("rb") as binary:
        report["gateway_binary_sha256"] = hashlib.file_digest(binary, "sha256").hexdigest()
    args.output.parent.mkdir(parents=True, exist_ok=True)

    def save():
        args.output.write_text(json.dumps(sanitize(report), indent=2) + "\n")

    with tempfile.TemporaryDirectory(prefix="sandhi-zai-") as temporary:
        directory = Path(temporary)
        credentials = directory / "credentials"
        credentials.mkdir(mode=0o700)
        secret_file = credentials / "zai__live"
        secret_file.touch(mode=0o600)
        secret_file.write_text(credential)
        config = directory / "config.json"
        config.write_text(json.dumps({"streaming_deadlines": {"ceiling_ms": 180000,
            "default": {"setup_ms": 30000, "idle_ms": 30000, "body_ms": 120000}}})
            if args.accounting == "tracked" and args.delivery != "buffered" else "{}")
        database = directory / "gateway.db"
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        base = f"http://127.0.0.1:{port}"
        environment = {key: value for key, value in os.environ.items()
                       if not key.startswith(("SANDHI_", "SENTINELPASS_"))}
        environment.update(SANDHI_AUTH_MODE="tokens", SANDHI_BIND=f"127.0.0.1:{port}",
                           SANDHI_ADMIN_TOKEN=private[1], SANDHI_STORE=str(database),
                           SANDHI_CONFIG=str(config), SANDHI_VAULT_BACKEND="credstore",
                           SANDHI_CREDENTIALS_DIR=str(credentials), SANDHI_SHUTDOWN_QUIESCE_MS="0",
                           RUST_LOG="sandhi_proxy=warn")
        if args.accounting == "tracked":
            environment.update(SANDHI_BUFFERED_ACCOUNTING="tracked", SANDHI_SHUTDOWN_GRACE_SECS="3")
        log_path = directory / "gateway.log"
        log_path.touch(mode=0o600)
        with log_path.open("w") as log:
            process = subprocess.Popen([str(ROOT / "target/debug/sandhi-proxy")],
                                       cwd=directory, env=environment, stdout=log, stderr=log)
            try:
                for _ in range(200):
                    if process.poll() is not None:
                        raise RuntimeError("gateway exited: " + log_path.read_text())
                    try:
                        if httpx.get(base + "/readyz", timeout=1).status_code == 200:
                            break
                    except httpx.TransportError:
                        pass
                    time.sleep(0.05)
                else:
                    raise RuntimeError("gateway readiness timeout")

                def admin(route, body):
                    response = httpx.post(base + route, json=body, timeout=15,
                                          headers={"authorization": "Bearer " + private[1]})
                    if response.is_error:
                        raise RuntimeError(f"{route}: {response.status_code} {response.text}")
                    return response.json()

                admin("/admin/keys/reference", {"provider": "zai", "label": "live", "scheme": "bearer",
                                               "base_url": ENDPOINTS["chat"][1]})
                for backend in args.backend or ENDPOINTS:
                    family, endpoint = ENDPOINTS[backend]
                    cache_policy = {"models": {args.model: {"profile": args.cache_profile}}}
                    if args.cache_profile == "implicit_only":
                        cache_policy["default_intent"] = {"mode": "auto"}
                    admin("/admin/backends", {"definition": {"id": "zai-" + backend, "provider": "zai",
                        "credential_ref": "zai:live", "endpoint": {"endpoint_family": family,
                            "endpoint_profile": "anthropic_implicit_cache" if backend == "messages" else "standard"},
                        "base_url": endpoint, "models": [{"id": args.model}],
                        "cache_policy": cache_policy}})
                    requested_model = args.model
                    upstream = "backend/zai-" + backend
                    if args.logical_route:
                        requested_model = "zai-code"
                        route_id = "qualify-" + backend
                        admin("/admin/routes", {"definition": {"id": route_id, "models": [{
                            "id": requested_model, "targets": [{"backend": "zai-" + backend, "model": args.model}]}]}})
                        upstream = "route/" + route_id
                    key = admin("/admin/keys/share", {"upstream": upstream,
                        "models": [requested_model], "group": "zai-qualification"})["virtual_key"]
                    private.append(key)
                    for client in args.client or ["chat", "messages", "gemini", "responses"]:
                        for stream in {"buffered": (False,), "streaming": (True,), "both": (False, True)}[args.delivery]:
                            for repeat in range(2 if args.cache_pairs else 1):
                                cell = {"backend": backend, "client": client, "stream": stream,
                                        "repeat": repeat, "status": "pending",
                                        "step_id": f"{backend}-{client}-{int(stream)}-{repeat}"}
                                report["cells"].append(cell)
                                prompt = "Write a Python identity function named identity. Return only the code."
                                if args.cache_pairs:
                                    prefix = "\n".join(f"def fixture_{i}(value): return value" for i in range(400))
                                    prompt = "Synthetic Python reference module:\n" + prefix + "\n\n" + prompt
                                started = time.monotonic()
                                try:
                                    cell.update(infer(client, base, key, requested_model, prompt, stream, cell["step_id"]))
                                    cell["status"] = "ok" if cell["text"].strip() else "empty_text"
                                except Exception as error:
                                    cell.update(status="error", error_type=type(error).__name__, error=str(error))
                                cell["elapsed_seconds"] = round(time.monotonic() - started, 3)
                                # One call at a time allows durable rows to be correlated without exporting keys.
                                with sqlite3.connect(database) as connection:
                                    connection.row_factory = sqlite3.Row
                                    rows = connection.execute("SELECT * FROM usage_events ORDER BY rowid").fetchall()
                                    cell["usage_rows_observed"] = len(rows)
                                save()
                                print(json.dumps(sanitize({key: value for key, value in cell.items()
                                      if key not in ("text", "reasoning", "usage", "events")})), flush=True)
            except Exception as error:
                report["harness_error"] = sanitize(str(error))
                raise RuntimeError(sanitize(str(error))) from None
            finally:
                process.terminate()
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
                    report["forced_shutdown"] = True
                if database.exists():
                    with sqlite3.connect(database) as connection:
                        connection.row_factory = sqlite3.Row
                        columns = {row[1] for row in connection.execute("PRAGMA table_info(usage_events)")}
                        selected = sorted(columns & {"request_id", "step_id", "occurred_at", "provider", "model", "backend",
                            "tokens_in", "tokens_out", "cache_creation_tokens", "cache_read_tokens",
                            "reasoning_tokens", "reasoning_included", "cache_read_status", "cache_read_source",
                            "duration_ms", "duration_source", "time_to_first_token_ms", "time_to_first_token_source"})
                        report["usage_events"] = [dict(row) for row in connection.execute(
                            "SELECT " + ",".join(selected) + " FROM usage_events ORDER BY rowid")]
                        if args.accounting == "tracked":
                            report["executions"], report["accounting_inventory"] = tracked_evidence(connection)
                report["gateway_exit_code"] = process.returncode
                # Static reason enums plus generated correlation IDs only. Never
                # export arbitrary log lines, provider bodies or credentials.
                report["accounting_diagnostics"] = [
                    {"request_id": request_id, "reason": reason}
                    for request_id, reason in re.findall(r'request_id=(req_[0-9_]{1,64}).*?reason="([a-z_]+)"',
                        re.sub(r'\x1b\[[0-9;]*m', '', log_path.read_text()))]
                report["finished_at"] = datetime.now(timezone.utc).isoformat()
                report["verification"] = verify_report(report)
                save()
    if report["verification"]["errors"]:
        raise SystemExit(1)


def infer(flavor, base, key, model, prompt, stream, step_id):
    headers = {"x-sandhi-session": "zai-router-qualification", "x-sandhi-run-id": "zai-router-qualification",
               "x-sandhi-step-id": step_id}
    text, reasoning, usage, events = [], [], [], []
    if flavor in ("chat", "responses"):
        with openai.OpenAI(base_url=base + "/v1", api_key=key, max_retries=0,
                           timeout=150, default_headers=headers) as sdk:
            if flavor == "chat":
                result = sdk.chat.completions.create(model=model, max_tokens=1024,
                    messages=[{"role": "user", "content": prompt}], stream=stream,
                    **({"stream_options": {"include_usage": True}} if stream else {}))
                if stream:
                    for chunk in result:
                        if chunk.usage:
                            usage.append(chunk.usage.model_dump(exclude_none=True))
                        for choice in chunk.choices:
                            text.append(choice.delta.content or "")
                            reasoning.append(getattr(choice.delta, "reasoning_content", "") or "")
                            if choice.finish_reason:
                                events.append(choice.finish_reason)
                else:
                    text.append(result.choices[0].message.content or "")
                    reasoning.append(getattr(result.choices[0].message, "reasoning_content", "") or "")
                    usage.append(result.usage.model_dump(exclude_none=True) if result.usage else None)
                    events.append(result.choices[0].finish_reason)
            else:
                result = sdk.responses.create(model=model, input=prompt, max_output_tokens=1024, stream=stream)
                if stream:
                    for event in result:
                        if event.type == "response.output_text.delta":
                            text.append(event.delta)
                        if event.type in ("response.completed", "response.incomplete", "response.failed"):
                            events.append(event.type)
                            usage.append(event.response.usage.model_dump(exclude_none=True) if event.response.usage else None)
                else:
                    text.append(result.output_text)
                    events.append(result.status)
                    usage.append(result.usage.model_dump(exclude_none=True) if result.usage else None)
    elif flavor == "messages":
        with anthropic.Anthropic(base_url=base, api_key=key, max_retries=0,
                                  timeout=150, default_headers=headers) as sdk:
            result = sdk.messages.create(model=model, max_tokens=1024,
                messages=[{"role": "user", "content": prompt}], stream=stream)
            if stream:
                for event in result:
                    if event.type == "content_block_delta":
                        text.append(getattr(event.delta, "text", "") or "")
                        reasoning.append(getattr(event.delta, "thinking", "") or "")
                    if event.type == "message_start":
                        usage.append(event.message.usage.model_dump(exclude_none=True))
                    if event.type == "message_delta":
                        if event.usage:
                            usage.append(event.usage.model_dump(exclude_none=True))
                        events.append(event.delta.stop_reason)
            else:
                text.extend(getattr(block, "text", "") or "" for block in result.content)
                reasoning.extend(getattr(block, "thinking", "") or "" for block in result.content)
                usage.append(result.usage.model_dump(exclude_none=True))
                events.append(result.stop_reason)
    else:
        with genai.Client(api_key=key, http_options=types.HttpOptions(base_url=base,
                timeout=150000, headers=headers, retry_options=types.HttpRetryOptions(attempts=1))) as sdk:
            result = (sdk.models.generate_content_stream if stream else sdk.models.generate_content)(
                model=model, contents=prompt, config=types.GenerateContentConfig(max_output_tokens=1024))
            for chunk in result if stream else [result]:
                for candidate in chunk.candidates or []:
                    for part in candidate.content.parts if candidate.content else []:
                        (reasoning if part.thought else text).append(part.text or "")
                    if candidate.finish_reason:
                        events.append(str(candidate.finish_reason))
                if chunk.usage_metadata:
                    usage.append(chunk.usage_metadata.model_dump(mode="json", exclude_none=True))
    return {"text": "".join(text), "reasoning": "".join(reasoning), "usage": usage, "events": events}


def tracked_evidence(connection):
    """Export a bounded, non-secret join from this disposable qualification store."""
    rows = connection.execute("""SELECT c.request_id,i.execution_id,x.context_json,
        a.attempt_id,a.authorized_at,a.evidence_json,t.usage_json,t.frozen_charge,
        p.scope AS primary_scope,p.actual AS primary_actual,p.settled AS primary_settled,p.settled_at AS primary_settled_at,
        b.scope AS upstream_scope,b.actual AS upstream_actual,b.settled AS upstream_settled,b.settled_at AS upstream_settled_at,
        o.receipt_id,o.charged_tokens,o.settled_at AS receipt_settled_at
        FROM budget_execution_intent i JOIN budget_request_correlation c USING(execution_id)
        LEFT JOIN budget_execution_context x USING(execution_id)
        LEFT JOIN budget_physical_attempt a USING(execution_id)
        LEFT JOIN budget_terminal_observation t USING(execution_id)
        LEFT JOIN budget_reservation p ON p.id=i.reservation_id
        LEFT JOIN budget_execution_back_leg l USING(execution_id)
        LEFT JOIN budget_reservation b ON b.id=l.reservation_id
        LEFT JOIN budget_settlement_outbox o ON o.reservation_id=i.reservation_id ORDER BY i.rowid""").fetchall()
    records = []
    for row in rows:
        record = dict(row)
        encoded_context = record.pop("context_json")
        context = json.loads(encoded_context) if encoded_context else None
        if context is not None:
            for key in ("transport_binding_sha256", "content_policy_revision"):
                context.pop(key, None)
            for key, fields in {"route": ("id", "revision"),
                                "backend": ("id", "revision", "endpoint_profile", "credential_generation"),
                                "cache": ("intent", "capability", "native_preserved")}.items():
                if context.get(key) is not None:
                    context[key] = {field: context[key][field] for field in fields}
        record["context"] = context
        encoded_attempt, encoded_usage = record.pop("evidence_json"), record.pop("usage_json")
        record["attempt"] = json.loads(encoded_attempt) if encoded_attempt else None
        record["usage"] = json.loads(encoded_usage) if encoded_usage else None
        records.append(record)
    inventory = {table: connection.execute(f"SELECT count(*) FROM {table}").fetchone()[0] for table in (
        "budget_execution_intent", "budget_execution_context", "budget_physical_attempt", "budget_terminal_observation",
        "budget_execution_back_leg", "budget_reservation", "budget_settlement_outbox")}
    return records, inventory


def verify_tracked(report, by_step):
    errors, matched = [], 0
    records = report.get("executions", [])
    by_request = {record["request_id"]: record for record in records}
    expected = len(report["cells"])
    if len(records) != expected or len(by_request) != expected:
        errors.append("tracked execution/request identity count mismatch")
    inventory = {table: expected * (2 if table == "budget_reservation" else 1) for table in (
        "budget_execution_intent", "budget_execution_context", "budget_physical_attempt", "budget_terminal_observation",
        "budget_execution_back_leg", "budget_reservation", "budget_settlement_outbox")}
    if report.get("accounting_inventory") != inventory:
        errors.append("tracked accounting inventory is incomplete or inconsistent")
    if report.get("gateway_exit_code") != 0:
        errors.append("tracked gateway did not shut down with complete accounting")
    identities, receipts = set(), set()
    for cell in report["cells"]:
        label = cell["step_id"]
        try:
            event = by_step[label]
            record = by_request[event["request_id"]]
            context, attempt, usage = record["context"], record["attempt"], record["usage"]
            identity, receipt = record["attempt_id"], record["receipt_id"]
            if identity in identities or receipt in receipts or not identity or not receipt:
                raise ValueError("missing or duplicate attempt/receipt identity")
            identities.add(identity)
            receipts.add(receipt)
            if context["endpoint_family"] != ENDPOINTS[cell["backend"]][0] or context["actual_model"] != report["model"]:
                raise ValueError("selected endpoint/model mismatch")
            if report.get("endpoint_profiles") and context["backend"]["endpoint_profile"] != report["endpoint_profiles"][cell["backend"]]:
                raise ValueError("endpoint usage/profile mismatch")
            ingress = {"chat": "openai_chat_completions", "messages": "anthropic_messages",
                       "responses": "openai_responses", "gemini": "gemini_generate_content"}[cell["client"]]
            if context["ingress"] != ingress or context["delivery"] != ("streaming" if cell.get("stream") else "buffered"):
                raise ValueError("ingress/delivery mismatch")
            if context["accounting_policy"]["version"] != 1:
                raise ValueError("unsupported accounting policy")
            if context["cache"]["capability"] != report["cache_policy"].split(" /", 1)[0]:
                raise ValueError("cache capability differs from configured policy")
            if report.get("logical_route") and (context["requested_model"] != "zai-code"
                    or context["route"]["id"] != "qualify-" + cell["backend"]
                    or context["backend"]["id"] != "zai-" + cell["backend"]):
                raise ValueError("logical route/backend attribution mismatch")
            if (attempt["version"] != 1 or attempt["attempt_id"] != identity or attempt["attempt_ordinal"] != 1
                    or attempt["dispatch_observed_at_ms"] is None or attempt["terminal"] is None
                    or attempt["invalid"] or attempt["dropped_observations"] != 0):
                raise ValueError("incomplete physical-attempt evidence")
            if attempt["terminal"]["outcome"] != "success" or not 200 <= attempt["terminal"]["provider_status"] < 300:
                raise ValueError("successful SDK result lacks successful transport evidence")
            if usage["completeness"] != "final" or usage["basis"] != "provider_reported":
                raise ValueError("unqualified terminal usage")
            fields = ("tokens_in", "cache_read_tokens", "cache_creation_tokens", "tokens_out", "reasoning_tokens", "reasoning_included")
            if any(usage.get(field) != event.get(field) for field in fields):
                raise ValueError("terminal usage differs from correlated observation")
            output = usage["tokens_out"] + ((usage.get("reasoning_tokens") or 0) if usage.get("reasoning_included") is False else 0)
            weight = context["accounting_policy"]["cache_read_weight"]
            charge = usage["tokens_in"] + usage["cache_creation_tokens"] + output + usage["cache_read_tokens"] * weight // 10000
            if any(record[field] != charge for field in ("frozen_charge", "charged_tokens", "primary_actual", "upstream_actual")):
                raise ValueError("paired/original charge mismatch")
            if record["primary_settled"] != 1 or record["upstream_settled"] != 1 or any(
                    record[field] != record["receipt_settled_at"] for field in ("primary_settled_at", "upstream_settled_at")):
                raise ValueError("paired settlement timestamp/state mismatch")
            if record["upstream_scope"] != context["upstream_budget_scope"] or record["upstream_scope"] == record["primary_scope"]:
                raise ValueError("paired scope mismatch")
            matched += 1
        except (KeyError, TypeError, ValueError) as error:
            errors.append(f"{label}: tracked evidence invalid ({error})")
    return matched, errors


def verify_report(report):
    """Compare client-visible accounting to drained, correlated storage evidence."""
    cells, rows = report["cells"], report.get("usage_events", [])
    errors, matched = [], 0
    correlated = bool(cells) and all(cell.get("step_id") for cell in cells)
    by_step = {row.get("step_id"): row for row in rows}
    if len(rows) != len(cells):
        errors.append(f"{len(cells)} cells but {len(rows)} durable observations")
    if correlated and len(by_step) != len(cells):
        errors.append("missing or duplicate durable step identifiers")
    hits = {backend: 0 for backend in ENDPOINTS}
    for index, cell in enumerate(cells):
        label = cell.get("step_id", f"cell-{index}")
        if cell["status"] != "ok":
            errors.append(label + ": " + cell["status"])
            continue
        row = by_step.get(label) if correlated else (rows[index] if index < len(rows) else None)
        if row is None:
            errors.append(label + ": no durable observation")
            continue
        usage = {}
        for part in cell["usage"]:
            if part:
                usage.update(part)
        try:
            if cell["client"] == "messages":
                fresh = usage["input_tokens"]
                read, write = usage.get("cache_read_input_tokens", 0), usage.get("cache_creation_input_tokens", 0)
                output = usage["output_tokens"]
            elif cell["client"] == "gemini":
                read, write = usage.get("cached_content_token_count", 0), 0
                fresh = usage["prompt_token_count"] - read + usage.get("tool_use_prompt_token_count", 0)
                output = usage["candidates_token_count"] + usage.get("thoughts_token_count", 0)
            else:
                chat = cell["client"] == "chat"
                details = usage.get("prompt_tokens_details" if chat else "input_tokens_details") or {}
                read, write = details.get("cached_tokens", 0), details.get("cache_write_tokens", 0)
                fresh = usage["prompt_tokens" if chat else "input_tokens"] - read - write
                output = usage["completion_tokens" if chat else "output_tokens"]
            measured = tuple(row[field] for field in ("tokens_in", "cache_read_tokens", "cache_creation_tokens")) + (
                row["tokens_out"] + ((row.get("reasoning_tokens") or 0) if row.get("reasoning_included") in (False, 0) else 0),)
            if sum(measured[:3]) < (1000 if report.get("cache_pairs") else 1):
                errors.append(label + ": implausibly empty input measurement for the known synthetic prompt")
            if (fresh, read, write, output) != measured:
                errors.append(f"{label}: SDK {(fresh, read, write, output)} != durable {measured}")
            else:
                matched += 1
            if row["cache_read_status"] == "reported" and row["cache_read_tokens"] > 0:
                hits[cell["backend"]] += 1
        except (KeyError, TypeError) as error:
            errors.append(f"{label}: incomplete usage evidence ({error})")
    tracked = None
    if report.get("accounting") == "tracked":
        tracked, tracked_errors = verify_tracked(report, by_step)
        errors.extend(tracked_errors)
    return {"cells": len(cells), "durable_observations": len(rows), "sdk_usage_matches": matched,
            "tracked_evidence_matches": tracked,
            "correlation": "step_id" if correlated else "legacy sequential order",
            "positive_reported_cache_reads": hits, "errors": errors}


if __name__ == "__main__":
    main()
