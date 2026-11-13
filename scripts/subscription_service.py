#!/usr/bin/env python3
"""Private local service runner; no credentials in process arguments or output."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import time


def auth_mode(config):
    mode = config.get("auth_mode", "oidc" if "oidc_config" in config else "none")
    if mode not in ("none", "oidc", "tokens"):
        raise ValueError("auth_mode must be none, oidc or tokens")
    if mode == "none" and "oidc_config" in config:
        raise ValueError("OIDC configuration conflicts with no-auth mode")
    if mode == "oidc" and not config.get("oidc_config"):
        raise ValueError("OIDC configuration is required; no authentication fallback")
    return mode


def operator_token(lease, state, mode):
    path = state / "admin.json"
    if mode == "tokens" or path.exists() or path.is_symlink():
        token = lease.read_private_json(path)["admin_token"]
        if not isinstance(token, str) or len(token) < 32:
            raise ValueError("strong configured admin credential required")
        return token
    return None


def client_environment(entry, config, inherited):
    gateway = entry.get("gateway")
    if not isinstance(gateway, dict) or set(gateway) - {"url", "virtual_key", "auth_mode"}:
        raise ValueError("unsupported gateway client configuration")
    url = gateway.get("url")
    if not isinstance(url, str) or not url.strip():
        raise ValueError("gateway URL is required")
    mode = gateway.get("auth_mode", "tokens" if "virtual_key" in gateway else "none")
    if mode not in ("none", "tokens"):
        raise ValueError("use a Victor OIDC profile for OIDC clients")
    if mode == "none" and "virtual_key" in gateway:
        raise ValueError("no-auth client conflicts with configured credentials")
    token = gateway.get("virtual_key")
    if mode == "tokens" and (not isinstance(token, str) or not token.strip()):
        raise ValueError("configured gateway virtual key must not be empty")
    env = {key: value for key, value in inherited.items() if not key.startswith("SANDHI_GATEWAY_")}
    env.update(SANDHI_GATEWAY_URL=url, PYTHONPATH=config["victor_worktree"])
    if mode == "tokens":
        env["SANDHI_GATEWAY_VIRTUAL_KEY_OPENAI"] = token
    return env


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["gateway", "publisher", "victor"])
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--client", default="victor-cli")
    args, remaining = parser.parse_known_args()
    spec = importlib.util.spec_from_file_location(
        "lease", args.state / "subscription_lease.py"
    )
    lease = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(lease)
    config = lease.read_private_json(args.state / "service.json")
    if args.mode == "gateway":
        mode = auth_mode(config)
        secret = operator_token(lease, args.state, mode) if mode != "oidc" else None
        # Do not inherit upstream API-key shortcuts from the operator's shell.
        env = {
            k: os.environ[k]
            for k in ("HOME", "PATH", "TMPDIR", "LANG")
            if k in os.environ
        }
        env.update(
            SANDHI_BIND=config["bind"],
            SANDHI_STORE=str(args.state / "usage.db"),
            SANDHI_AUTH_MODE=mode,
            SANDHI_VAULT_BACKEND="keyring",
            SANDHI_PUBLIC_URL=config["gateway"],
            SANDHI_ERROR_DETAIL="redacted",
            SANDHI_DASHBOARD_PUBLIC="0",
        )
        if "listener_config" in config:
            listener_config = config["listener_config"]
            if not isinstance(listener_config, str) or not listener_config.strip():
                raise ValueError("listener_config must be a nonempty configuration-file path")
            env["SANDHI_CONFIG"] = listener_config
        if mode == "oidc":
            env["SANDHI_OIDC_CONFIG"] = config["oidc_config"]
        elif secret is not None:
            env["SANDHI_ADMIN_TOKEN"] = secret
        os.umask(0o077)
        os.execve(str(args.state / "sandhi-proxy"), ["sandhi-proxy"], env)
    elif args.mode == "publisher":
        client = lease.AdminClient(
            config["gateway"],
            operator_token(lease, args.state, auth_mode(config)),
        )
        while True:
            try:
                lease.publish(
                    client,
                    Path(config["auth_file"]),
                    config["account"],
                    config["label"],
                )
            except (OSError, ValueError, RuntimeError, KeyError):
                print(
                    "Subscription lease unavailable; dispatch stops at expiry. Check login and gateway.",
                    flush=True,
                )
            time.sleep(60)
    else:
        if not args.client.replace("-", "").isalnum():
            parser.error("invalid client name")
        entry = lease.read_private_json(args.state / (args.client + ".json"))
        env = client_environment(entry, config, os.environ)
        command = [config["python"], "-m", "victor"] + remaining
        os.execve(config["python"], command, env)


if __name__ == "__main__":
    main()
