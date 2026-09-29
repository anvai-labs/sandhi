"""Operator configuration and private service-key loading; never request controlled."""

from dataclasses import dataclass
import os
from pathlib import Path
import stat
import ipaddress

from .protocol import strict_json


@dataclass(frozen=True)
class Settings:
    name: str
    python: str
    bundle: Path
    bundle_sha256: str
    artifact: Path
    artifact_sha256: str
    auth_file: Path
    pool_size: int = 1
    startup_timeout_ms: int = 5000
    evaluation_timeout_ms: int = 200
    port: int = 9088

    @classmethod
    def load(cls, path):
        with open(path, "rb") as source:
            raw = source.read(65537)
        if len(raw) > 65536:
            raise ValueError("configuration bound")
        value = strict_json(raw)
        if (
            not isinstance(value, dict)
            or type(value.get("version")) is not int
            or value.pop("version", None) != 1
        ):
            raise ValueError("configuration version")
        result = cls(**value)
        for key in ("bundle", "artifact", "auth_file"):
            object.__setattr__(result, key, Path(getattr(result, key)))
        if (
            not result.name
            or len(result.name) > 64
            or any(
                not c.isascii() or not (c.isalnum() or c in "._-") for c in result.name
            )
        ):
            raise ValueError("evaluator name")
        if any(
            not Path(p).is_absolute()
            for p in (result.python, result.bundle, result.artifact, result.auth_file)
        ):
            raise ValueError("absolute deployment paths required")
        for number, upper in (
            (result.pool_size, 4),
            (result.startup_timeout_ms, 10000),
            (result.evaluation_timeout_ms, 2000),
            (result.port, 65535),
        ):
            if type(number) is not int or not 1 <= number <= upper:
                raise ValueError("configuration bound")
        for digest in (result.bundle_sha256, result.artifact_sha256):
            if (
                not isinstance(digest, str)
                or len(digest) != 64
                or any(c not in "0123456789abcdef" for c in digest)
            ):
                raise ValueError("digest")
        return result


def read_auth_key(path):
    # Unix template: reject symlinks, non-regular files, foreign owners and
    # group/world access. Do not put this credential in manifests or argv.
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as source:
        info = os.fstat(source.fileno())
        if (
            not stat.S_ISREG(info.st_mode)
            or info.st_uid != os.getuid()
            or info.st_mode & 0o077
        ):
            raise ValueError("private key file required")
        value = source.read(257).strip()
    if not 32 <= len(value) <= 128 or any(
        c not in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
        for c in value
    ):
        raise ValueError("service key format")
    return value


@dataclass(frozen=True)
class ServiceSettings:
    auth_file: Path
    port: int
    evaluators: dict
    bind_host: str = "127.0.0.1"
    tls_certfile: str | None = None
    tls_keyfile: str | None = None
    tls_cafile: str | None = None

    @classmethod
    def load(cls, path):
        with open(path, "rb") as source:
            raw = source.read(65537)
        if len(raw) > 65536:
            raise ValueError("configuration bound")
        value = strict_json(raw)
        if (
            not isinstance(value, dict)
            or not {"version", "auth_file", "port", "evaluators"}.issubset(value)
            or set(value)
            - {
                "version",
                "auth_file",
                "port",
                "evaluators",
                "bind_host",
                "tls_certfile",
                "tls_keyfile",
                "tls_cafile",
            }
            or type(value["version"]) is not int
            or value["version"] != 1
            or type(value["port"]) is not int
            or not 1 <= value["port"] <= 65535
            or not isinstance(value["evaluators"], list)
            or not 1 <= len(value["evaluators"]) <= 4
        ):
            raise ValueError("service configuration")
        auth = Path(value["auth_file"])
        if not auth.is_absolute():
            raise ValueError("absolute credential path required")
        models = {}
        for path in value["evaluators"]:
            if not isinstance(path, str) or not Path(path).is_absolute():
                raise ValueError("absolute evaluator path required")
            model = Settings.load(path)
            if model.name in models:
                raise ValueError("duplicate evaluator")
            models[model.name] = model
        if sum(model.pool_size for model in models.values()) > 4:
            raise ValueError("service worker cap")
        bind_host = value.get("bind_host", "127.0.0.1")
        address = ipaddress.ip_address(bind_host)
        cert, key, ca = (
            value.get(k) for k in ("tls_certfile", "tls_keyfile", "tls_cafile")
        )
        if (
            bool(cert) != bool(key)
            or (ca and not cert)
            or (not address.is_loopback and not cert)
        ):
            raise ValueError("TLS required for non-loopback listener")
        for filename in (cert, key, ca):
            if filename is not None:
                path = Path(filename)
                if not path.is_absolute() or path.is_symlink() or not path.is_file():
                    raise ValueError("TLS file")
                if path.stat().st_size > 65536:
                    raise ValueError("TLS file bound")
        if key:
            info = Path(key).stat()
            if info.st_uid != os.getuid() or info.st_mode & 0o077:
                raise ValueError("private TLS key required")
        return cls(auth, value["port"], models, bind_host, cert, key, ca)
