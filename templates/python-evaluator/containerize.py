"""Prepare a new, immutable deployment snapshot; never starts Docker or overwrites it."""

import argparse
import ipaddress
import json
import math
import os
from pathlib import Path
import re
import ssl
import stat

from evaluator_app.protocol import checked_bytes
from evaluator_app.settings import Settings, read_auth_key


def material(path, private=False):
    path = Path(path)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as source:
        info = os.fstat(source.fileno())
        if not stat.S_ISREG(info.st_mode):
            raise ValueError("regular material file required")
        if private and (info.st_uid != os.getuid() or info.st_mode & 0o077):
            raise ValueError("private material required")
        data = source.read(65537)
    if not data or len(data) > 65536:
        raise ValueError("material bound")
    return data


def validate_tls(cert, key, ca):
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    context.load_verify_locations(cafile=ca)


def write(path, data):
    with path.open("xb") as output:
        output.write(data)
    path.chmod(0o400)


def json_file(path, value):
    write(path, (json.dumps(value, indent=2) + "\n").encode())


def prepare(
    destination,
    model_paths,
    image,
    auth_file,
    cert_file,
    key_file,
    ca_file,
    *,
    port=9088,
    bind_ip="127.0.0.1",
    cpus=1.0,
    memory_mb=512,
    pids=64,
    network="internal",
):
    if network not in {"internal", "published"}:
        raise ValueError("explicit network profile required")
    uid, gid = os.getuid(), os.getgid()
    if uid == 0:
        raise ValueError("prepare and run under a dedicated non-root user")
    if not re.fullmatch(
        r"(?:[a-zA-Z0-9][a-zA-Z0-9._/:\-]*@)?sha256:[a-f0-9]{64}", image
    ):
        raise ValueError("immutable image digest required")
    if type(port) is not int or not 1024 <= port <= 65535:
        raise ValueError("unprivileged port required")
    ipaddress.ip_address(bind_ip)
    if (
        type(cpus) not in (int, float)
        or not math.isfinite(cpus)
        or not 0.25 <= cpus <= 64
    ):
        raise ValueError("CPU bound")
    if type(memory_mb) is not int or not 128 <= memory_mb <= 32768:
        raise ValueError("memory bound")
    if type(pids) is not int or not 16 <= pids <= 256:
        raise ValueError("process bound")
    if not 1 <= len(model_paths) <= 4:
        raise ValueError("model count")
    models = [Settings.load(path) for path in model_paths]
    if (
        len({m.name for m in models}) != len(models)
        or sum(m.pool_size for m in models) > 4
    ):
        raise ValueError("duplicate model or worker cap")
    # Validate everything before creating the destination; copy verified bytes so
    # later source rebuilds cannot change the release's model or code provenance.
    payloads = [
        (
            checked_bytes(m.bundle, m.bundle_sha256, 262144),
            checked_bytes(m.artifact, m.artifact_sha256, 1048576),
        )
        for m in models
    ]
    secrets = {
        "service.key": read_auth_key(auth_file) + b"\n",
        "server.crt": material(cert_file),
        "server.key": material(key_file, True),
        "client-ca.crt": material(ca_file),
    }
    validate_tls(cert_file, key_file, ca_file)
    target = Path(destination).absolute()
    target.mkdir(
        mode=0o700
    )  # Never overwrite a release or follow a destination symlink.
    configs, secret_dir = target / "config", target / "secrets"
    configs.mkdir(mode=0o700)
    secret_dir.mkdir(mode=0o700)
    for name, data in secrets.items():
        write(secret_dir / name, data)
    paths, provenance = [], []
    for index, (model, (bundle, artifact)) in enumerate(zip(models, payloads)):
        folder = configs / f"model-{index}"
        folder.mkdir(mode=0o700)
        prefix = f"/etc/evaluator/model-{index}"
        write(folder / "worker.pyz", bundle)
        write(folder / "artifact.json", artifact)
        value = {
            "version": 1,
            "name": model.name,
            "python": "/usr/local/bin/python",
            "bundle": prefix + "/worker.pyz",
            "bundle_sha256": model.bundle_sha256,
            "artifact": prefix + "/artifact.json",
            "artifact_sha256": model.artifact_sha256,
            "auth_file": "/run/evaluator-secrets/service.key",
            "pool_size": model.pool_size,
            "startup_timeout_ms": model.startup_timeout_ms,
            "evaluation_timeout_ms": model.evaluation_timeout_ms,
            "port": 9088,
        }
        json_file(folder / "http.json", value)
        paths.append(prefix + "/http.json")
        provenance.append(
            {
                "name": model.name,
                "code_sha256": model.bundle_sha256,
                "artifact_sha256": model.artifact_sha256,
                "pool_size": model.pool_size,
            }
        )
    json_file(
        configs / "service.json",
        {
            "version": 1,
            "port": 9088,
            "bind_host": "0.0.0.0",
            "auth_file": "/run/evaluator-secrets/service.key",
            "evaluators": paths,
            "tls_certfile": "/run/evaluator-secrets/server.crt",
            "tls_keyfile": "/run/evaluator-secrets/server.key",
            "tls_cafile": "/run/evaluator-secrets/client-ca.crt",
        },
    )

    def mount(source, destination):
        return {
            "type": "bind",
            "source": str(source).replace("$", "$$"),
            "target": destination,
            "read_only": True,
            "bind": {"create_host_path": False},
        }

    compose = {
        "services": {
            "evaluator": {
                "image": image,
                "pull_policy": "never",
                "user": f"{uid}:{gid}",
                "init": True,
                "read_only": True,
                "cap_drop": ["ALL"],
                "security_opt": ["no-new-privileges:true"],
                "cpus": cpus,
                "mem_limit": memory_mb * 1048576,
                "memswap_limit": memory_mb * 1048576,
                "pids_limit": pids,
                "ulimits": {"core": 0, "nofile": {"soft": 1024, "hard": 1024}},
                "tmpfs": ["/tmp:rw,noexec,nosuid,nodev,size=64m,mode=1777"],
                "shm_size": "16m",
                "restart": "on-failure:3",
                "stop_grace_period": "20s",
                "ports": [
                    {
                        "target": 9088,
                        "published": str(port),
                        "host_ip": bind_ip,
                        "protocol": "tcp",
                    }
                ],
                "volumes": [
                    mount(configs, "/etc/evaluator"),
                    mount(secret_dir, "/run/evaluator-secrets"),
                ],
                "networks": ["evaluation"],
                "logging": {
                    "driver": "local",
                    "options": {"max-size": "1m", "max-file": "2"},
                },
            }
        },
        "networks": {"evaluation": {"internal": network == "internal"}},
    }
    if network == "internal":
        compose["services"]["evaluator"].pop("ports")
    json_file(target / "compose.json", compose)
    json_file(
        target / "deployment.json",
        {
            "version": 1,
            "image": image,
            "models": provenance,
            "cpus": cpus,
            "memory_mb": memory_mb,
            "pids": pids,
            "uid": uid,
            "gid": gid,
            "network_profile": network,
            "bind_ip": bind_ip,
            "port": port,
        },
    )
    return target


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--model", action="append", required=True, type=Path)
    for flag in ("image", "auth-file", "cert-file", "key-file", "ca-file"):
        parser.add_argument("--" + flag, required=True)
    parser.add_argument("--port", type=int, default=9088)
    parser.add_argument("--bind-ip", default="127.0.0.1")
    parser.add_argument("--cpus", type=float, default=1)
    parser.add_argument("--memory-mb", type=int, default=512)
    parser.add_argument("--pids", type=int, default=64)
    parser.add_argument(
        "--network", choices=("internal", "published"), default="internal"
    )
    args = parser.parse_args()
    try:
        target = prepare(
            args.destination,
            args.model,
            args.image,
            args.auth_file,
            args.cert_file,
            args.key_file,
            args.ca_file,
            port=args.port,
            bind_ip=args.bind_ip,
            cpus=args.cpus,
            memory_mb=args.memory_mb,
            pids=args.pids,
            network=args.network,
        )
    except (ValueError, OSError, TypeError):
        parser.exit(
            2, "invalid or unavailable deployment input; release was not activated\n"
        )
    print(
        json.dumps({"deployment": str(target), "compose": str(target / "compose.json")})
    )


if __name__ == "__main__":
    main()
