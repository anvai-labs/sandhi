#!/usr/bin/env python3
"""Private local service runner; no credentials in process arguments or output."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import time


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
        mode = config.get("auth_mode", "oidc")
        if mode not in ("oidc", "tokens"):
            raise SystemExit("auth_mode must be explicitly oidc or tokens")
        if mode == "oidc" and not config.get("oidc_config"):
            raise SystemExit("OIDC configuration is required; no token-mode fallback")
        secret = (
            lease.read_private_json(args.state / "admin.json")["admin_token"]
            if mode == "tokens"
            else None
        )
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
        if mode == "oidc":
            env["SANDHI_OIDC_CONFIG"] = config["oidc_config"]
        else:
            env["SANDHI_ADMIN_TOKEN"] = secret
        os.umask(0o077)
        os.execve(str(args.state / "sandhi-proxy"), ["sandhi-proxy"], env)
    elif args.mode == "publisher":
        client = lease.AdminClient(
            config["gateway"],
            lease.read_private_json(args.state / "admin.json")["admin_token"],
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
        env = dict(os.environ)
        for key in list(env):
            if key.startswith("SANDHI_GATEWAY_"):
                del env[key]
        env.update(
            SANDHI_GATEWAY_URL=entry["gateway"]["url"],
            SANDHI_GATEWAY_VIRTUAL_KEY_OPENAI=entry["gateway"]["virtual_key"],
            PYTHONPATH=config["victor_worktree"],
        )
        command = [config["python"], "-m", "victor"] + remaining
        os.execve(config["python"], command, env)


if __name__ == "__main__":
    main()
