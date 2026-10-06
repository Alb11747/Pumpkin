"""Migrate an explicit BlueMap hidden-player set into a private exporter policy."""

import argparse
import json
from pathlib import Path
import re

MAX_BYTES = 1024 * 1024
UUID_PATTERN = re.compile(
    r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"
)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def invalid_constant(value):
    raise ValueError(f"non-JSON numeric constant: {value}")


def convert(source: bytes) -> dict:
    if len(source) > MAX_BYTES:
        raise ValueError("pluginState.json exceeds 1 MiB")
    state = json.loads(
        source.decode("utf-8"),
        object_pairs_hook=unique_object,
        parse_constant=invalid_constant,
    )
    if not isinstance(state, dict) or "hidden-players" not in state:
        raise ValueError("pluginState.json must contain an explicit hidden-players list")
    hidden = state["hidden-players"]
    if not isinstance(hidden, list):
        raise ValueError("hidden-players must be a list")
    if any(not isinstance(value, str) or not UUID_PATTERN.fullmatch(value) for value in hidden):
        raise ValueError("hidden-players requires canonical lowercase UUID strings")
    if len(set(hidden)) != len(hidden):
        raise ValueError("hidden-players contains duplicate UUIDs")
    return {
        "version": 1,
        "hiddenPlayers": hidden,
        "vanishSource": "pumpkin-observer-hides",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path, help="private cold BlueMap pluginState.json")
    parser.add_argument("destination", type=Path, help="private exporter visibility.json")
    args = parser.parse_args()
    if args.source.resolve() == args.destination.resolve():
        parser.error("source and destination must be different files")
    with args.source.open("rb") as source:
        policy = convert(source.read(MAX_BYTES + 1))
    encoded = json.dumps(policy, indent=2) + "\n"
    if len(encoded.encode("utf-8")) > MAX_BYTES:
        raise ValueError("generated visibility.json exceeds 1 MiB")
    temporary = args.destination.with_name(args.destination.name + ".tmp")
    with temporary.open("w", encoding="utf-8", newline="\n") as output:
        output.write(encoded)
    temporary.replace(args.destination)
    print(f"Migrated {len(policy['hiddenPlayers'])} hidden players to visibility.json")


if __name__ == "__main__":
    main()
