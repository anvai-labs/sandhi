"""Stable boundary: edit business logic, not this protocol, for a new evaluator."""

import hashlib
import json
import math

MAX_FRAME = 2097152
MAX_TEXT = 262144


def strict_json(data):
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError("duplicate field")
            result[key] = value
        return result

    def invalid(_):
        raise ValueError("nonfinite number")

    return json.loads(data, object_pairs_hook=pairs, parse_constant=invalid)


def checked_bytes(path, expected, limit):
    if len(expected) != 64 or any(c not in "0123456789abcdef" for c in expected):
        raise ValueError("digest")
    with open(path, "rb") as source:
        value = source.read(limit + 1)
    if len(value) > limit or hashlib.sha256(value).hexdigest() != expected:
        raise ValueError("artifact")
    return value


def request_view(value):
    if not isinstance(value, dict) or set(value) != {"version", "id", "text", "joined"}:
        raise ValueError("request contract")
    if type(value["version"]) is not int or value["version"] != 1:
        raise ValueError("version")
    identifier = value["id"]
    if (
        not isinstance(identifier, str)
        or not 1 <= len(identifier) <= 20
        or not identifier.isascii()
        or not identifier.isdigit()
    ):
        raise ValueError("identifier")
    for key in ("text", "joined"):
        if not isinstance(value[key], str) or len(value[key].encode()) > MAX_TEXT:
            raise ValueError("text bound")
    return value


def score(value):
    if (
        type(value) not in (int, float)
        or not math.isfinite(value)
        or not 0 <= value <= 1
    ):
        raise ValueError("score contract")
    return float(value)


def result(value, identifier):
    if not isinstance(value, dict) or set(value) != {"version", "id", "score"}:
        raise ValueError("result contract")
    if (
        type(value["version"]) is not int
        or value["version"] != 1
        or value["id"] != identifier
    ):
        raise ValueError("result identity")
    return {"version": 1, "id": identifier, "score": score(value["score"])}
