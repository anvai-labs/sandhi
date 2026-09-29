#!/usr/bin/env python3
"""Publish short-lived Codex access leases; never copy or rotate refresh tokens.

Use only for the login owner's authorized clients. The upstream account's
subscription restrictions and aggregate quota still apply to every virtual key.
All output is metadata; secrets are accepted only from private files.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
from pathlib import Path
import re
import stat
import time
import urllib.error
import urllib.parse
import urllib.request


def read_private_json(path: Path) -> dict:
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd) as stream:
        info = os.fstat(stream.fileno())
        if (
            not stat.S_ISREG(info.st_mode)
            or info.st_uid != os.getuid()
            or info.st_mode & 0o077
        ):
            raise ValueError("credential file must be owner-only and regular")
        if info.st_size > 1024 * 1024:
            raise ValueError("credential file exceeds size limit")
        data = json.load(stream)
    if not isinstance(data, dict):
        raise ValueError("credential file must contain an object")
    return data


def make_lease(auth: dict, expected_account: str, *, now: int | None = None) -> dict:
    now = int(time.time()) if now is None else now
    tokens = auth.get("tokens") or {}
    access = tokens.get("access_token")
    account = tokens.get("account_id")
    if (
        not expected_account
        or account != expected_account
        or not isinstance(access, str)
    ):
        raise ValueError("subscription login missing or account mismatch")
    try:
        encoded = access.split(".")[1]
        claims = json.loads(
            base64.urlsafe_b64decode(encoded + "=" * (-len(encoded) % 4))
        )
        expiry = claims["exp"]
        if type(expiry) is not int:
            raise ValueError()
    except (IndexError, KeyError, ValueError, TypeError) as exc:
        raise ValueError("access token has no usable expiry") from exc
    # JWT parsing is scheduling only, NOT signature/identity verification. The
    # upstream authenticates the bearer; the owner explicitly pins the account.
    expiry = min(expiry, now + 300)
    if expiry <= now + 60:
        raise ValueError("subscription access token expired; renew with Codex login")
    return {"access_token": access, "account_id": account, "expires_at": expiry}


def validate_gateway(url: str) -> str:
    parsed = urllib.parse.urlsplit(url)
    if (
        not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
        or parsed.path not in ("", "/")
        or not (
            parsed.scheme == "https"
            or (
                parsed.scheme == "http"
                and parsed.hostname in ("127.0.0.1", "::1", "localhost")
            )
        )
    ):
        raise ValueError("gateway must be an HTTPS or loopback HTTP root URL")
    return url.rstrip("/")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class AdminClient:
    def __init__(self, url: str, token: str):
        self.url = validate_gateway(url)
        if not isinstance(token, str) or len(token) < 32:
            raise ValueError("strong admin credential required")
        self.token = token
        self.opener = urllib.request.build_opener(
            urllib.request.ProxyHandler({}), NoRedirect()
        )

    def post(self, path: str, data: dict) -> dict:
        req = urllib.request.Request(
            self.url + path,
            data=json.dumps(data).encode(),
            headers={
                "Authorization": "Bearer " + self.token,
                "Content-Type": "application/json",
            },
            method="POST",
        )
        try:
            with self.opener.open(req, timeout=15) as response:
                return json.loads(response.read(1024 * 1024))
        except (OSError, ValueError) as exc:
            # Do not print response bodies, auth headers, or low-level errors.
            raise RuntimeError("gateway request failed") from None


def publish(client: AdminClient, auth_path: Path, account: str, label: str) -> None:
    value = make_lease(read_private_json(auth_path), account)
    client.post(
        "/admin/keys",
        {
            "provider": "openai",
            "label": label,
            "scheme": "oauth",
            "secret": json.dumps(value),
        },
    )


def write_private(path: Path, value: dict) -> None:
    # Never silently replace a client credential; rotation is an explicit operation.
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "w") as stream:
        json.dump(value, stream, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gateway", default="http://127.0.0.1:18789")
    parser.add_argument(
        "--admin-file",
        required=True,
        type=Path,
        help="private JSON containing admin_token",
    )
    parser.add_argument(
        "--auth-file", type=Path, default=Path.home() / ".codex/auth.json"
    )
    parser.add_argument(
        "--account", required=True, help="expected subscription account ID"
    )
    parser.add_argument("--label", default="subscription")
    parser.add_argument(
        "--watch",
        action="store_true",
        help="renew the lease every 60s; never refresh the OAuth grant",
    )
    parser.add_argument(
        "--client",
        help="compatibility token-mode client only; OIDC clients use /auth/keys",
    )
    parser.add_argument(
        "--client-file",
        type=Path,
        help="new private output JSON with gateway and virtual_key",
    )
    parser.add_argument(
        "--model",
        action="append",
        help="explicit model allowlist; required for provisioning",
    )
    parser.add_argument("--daily-tokens", type=int, default=100000)
    args = parser.parse_args()
    if not re.fullmatch(r"[a-zA-Z0-9_-]{1,64}", args.label):
        parser.error("invalid label")
    if args.client and (
        not re.fullmatch(r"[a-zA-Z0-9_-]{1,64}", args.client)
        or not args.model
        or not args.client_file
        or args.client_file.exists()
        or args.daily_tokens <= 0
    ):
        parser.error(
            "client requires a name, model allowlist, positive budget, and new output file"
        )
    try:
        client = AdminClient(
            args.gateway, read_private_json(args.admin_file)["admin_token"]
        )
        publish(client, args.auth_file, args.account, args.label)
        if args.client:
            scope = "victor:" + args.client
            client.post(
                "/admin/budget",
                {
                    "scope": scope,
                    "limit_tokens": args.daily_tokens,
                    "window": "daily",
                    "policy": "block",
                },
            )
            result = client.post(
                "/admin/keys/share",
                {
                    "upstream": "openai:" + args.label,
                    "subject": scope,
                    "group": "subscription-owner",
                    "models": args.model,
                    "budget_scope": scope,
                    "rate_limit_per_min": 10,
                    "expires_at": time.strftime(
                        "%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() + 86400 * 30)
                    ),
                },
            )
            key = result.get("virtual_key")
            if not isinstance(key, str) or not key:
                raise RuntimeError(
                    "gateway did not return a virtual key; reconcile inventory before retry"
                )
            write_private(
                args.client_file,
                {
                    "gateway": {"url": args.gateway, "virtual_key": key},
                    "provider": "openai",
                    "models": args.model,
                    "virtual_key_id": result.get("id"),
                },
            )
            print("Client provisioned; credential saved privately. No secret printed.")
        print("Subscription lease published (at most five minutes).")
        if args.watch:
            while True:
                time.sleep(60)
                try:
                    publish(client, args.auth_file, args.account, args.label)
                except (OSError, ValueError, RuntimeError, KeyError):
                    print(
                        "Lease renewal unavailable; dispatch stops at expiry. Check login and gateway.",
                        flush=True,
                    )
    except (OSError, ValueError, RuntimeError, KeyError):
        print(
            "Subscription setup failed; check private files, account, login and gateway. No credentials printed."
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
