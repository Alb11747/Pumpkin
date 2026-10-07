"""Verify an upstream counterfactual with the fixed branch's exact regression."""
import hashlib
import json
import os
from pathlib import Path
import subprocess

BASE = "f1c0871f492182a228fa585e9d7c74e683e242f9"
kind = os.environ["CONTRIBUTION_KIND"]
package = os.environ["CONTRIBUTION_PACKAGE"]
test = os.environ["CONTRIBUTION_TEST"]
output = Path(os.environ["RUNNER_TEMP"]) / "contribution-proof"
output.mkdir(exist_ok=True)

if kind == "region":
    production = Path("crates/pumpkin-world/src/chunk/format/anvil.rs")
    fixture = Path("crates/pumpkin-world/src/chunk/io/file_manager.rs")
else:
    production = Path("crates/pumpkin-core/src/entity/player.rs")
    fixture = production

fixed = production.read_bytes()


def test_bytes():
    data = fixture.read_bytes()
    return data[data.index(b"#[cfg(test)]"):]


fixture_hash = hashlib.sha256(test_bytes()).hexdigest()
try:
    if kind == "region":
        original = subprocess.run(
            ["git", "show", f"{BASE}:{production.as_posix()}"],
            check=True, capture_output=True,
        ).stdout
        production.write_bytes(original)
    else:
        hunk = (
            "            // Watch the destination without waiting for client confirmation or movement.\n"
            "            if let Some(player) = self.world().get_player_by_uuid(self.gameprofile.id) {\n"
            "                crate::world::chunker::update_position(&player);\n"
            "            }\n"
        )
        # Git may check out CRLF on Windows; remove only the production hunk
        # while retaining every fixture byte and the checkout's line endings.
        hunk_bytes = hunk.encode()
        if b"\r\n" in fixed:
            hunk_bytes = hunk_bytes.replace(b"\n", b"\r\n")
        assert fixed.count(hunk_bytes) == 1
        production.write_bytes(fixed.replace(hunk_bytes, b""))
    assert hashlib.sha256(test_bytes()).hexdigest() == fixture_hash
    baseline_hash = hashlib.sha256(production.read_bytes()).hexdigest()
    result = subprocess.run(
        ["cargo", "test", "--locked", "-p", package, test, "--", "--exact", "--nocapture"],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=2700,
    )
    text = result.stdout.decode("utf-8", errors="replace")
    (output / "negative.log").write_bytes(result.stdout)
    if kind == "region":
        matched = "running 1 test" in text and (
            "stack overflow" in text.lower()
            or "has overflowed its stack" in text.lower()
        )
    else:
        matched = (
            "running 1 test" in text
            and "left: Vector2 { x: 0, y: 0 }" in text
            and "right: Vector2 { x: 9, y: 21 }" in text
        )
    proof = {
        "kind": kind, "baseline": BASE, "returncode": result.returncode,
        "regression": test, "identical_fixture_sha256": fixture_hash,
        "fixed_production_sha256": hashlib.sha256(fixed).hexdigest(),
        "baseline_production_sha256": baseline_hash,
        "expected_failure_matched": matched,
    }
    (output / "negative.json").write_text(json.dumps(proof, indent=2), encoding="utf-8")
    print(json.dumps(proof))
    if result.returncode == 0:
        raise RuntimeError("Upstream regression passed: do not claim this reproduction proves the fix")
    if not matched:
        print(text[-8000:])
        raise RuntimeError("Baseline failure did not match the expected regression")
finally:
    production.write_bytes(fixed)
