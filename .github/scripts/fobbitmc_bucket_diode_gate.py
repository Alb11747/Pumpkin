"""Prove bucket persistence and hopper support regressions detect the old behavior."""
import hashlib
import os
from pathlib import Path
import subprocess
import sys

engine = Path("engine")
target = sys.argv[1]
toolchain = os.environ["RUST_TOOLCHAIN"]
temporary = Path(os.environ["RUNNER_TEMP"])
prefix = "world::entity_storage::tests::"
mob = engine / "crates/pumpkin-core/src/entity/mob/mod.rs"
support = engine / "crates/pumpkin-core/src/block/blocks/redstone/abstract_redstone_gate.rs"
original = {path: path.read_bytes() for path in (mob, support)}


def test(name, expected_success):
    log = temporary / f"bucket-diode-{name}-{target}.log"
    command = [
        "cargo", "+" + toolchain, "test", "--locked", "--release", "--target", target,
        "-p", "pumpkin-core", "--lib", prefix + name, "--", "--exact", "--color", "never",
    ]
    with log.open("wb") as output:
        result = subprocess.run(command, cwd=engine, stdout=output, stderr=subprocess.STDOUT)
    text = log.read_text(encoding="utf-8", errors="replace")
    print(text, end="")
    if "running 1 test\n" not in text:
        raise RuntimeError(f"Exact named regression did not run: {name}")
    if expected_success:
        if result.returncode or f"test {prefix + name} ... ok" not in text:
            raise RuntimeError(f"Positive regression failed: {name}")
    elif result.returncode != 101 or f"test {prefix + name} ... FAILED" not in text:
        raise RuntimeError(f"Old behavior did not fail the regression: {name}")
    elif "panicked at" not in text or "error[E" in text:
        raise RuntimeError(f"Negative result was not an assertion failure: {name}")


def substitute(path, old, new):
    raw = original[path]
    if raw.count(old) != 1:
        raise RuntimeError(f"Production guard is not unique: {path}")
    path.write_bytes(raw.replace(old, new))


try:
    substitute(
        mob,
        b"if self.persistence_required.load(Relaxed) || mob.requires_custom_persistence() {",
        b"if self.persistence_required.load(Relaxed) {",
    )
    test("mob_custom_persistence_prevents_distance_despawn", False)
    mob.write_bytes(original[mob])
    substitute(
        support,
        b"state.is_side_solid(BlockDirection::Up) || state.id.to_block() == &Block::HOPPER",
        b"state.is_side_solid(BlockDirection::Up)",
    )
    test("imported_diodes_survive_all_hopper_states_and_disk_reload", False)
finally:
    for path, raw in original.items():
        path.write_bytes(raw)
        if hashlib.sha256(path.read_bytes()).digest() != hashlib.sha256(raw).digest():
            raise RuntimeError(f"Could not restore exact production bytes: {path}")

subprocess.run(["git", "diff", "--exit-code"], cwd=engine, check=True)
for name in (
    "mob_custom_persistence_prevents_distance_despawn",
    "loaded_bucket_mobs_survive_distant_players_and_round_trip_vanilla_data",
    "newly_released_bucket_mobs_gain_persistent_provenance",
    "imported_diodes_survive_all_hopper_states_and_disk_reload",
    "unsupported_diodes_still_break_and_drop_one_item",
):
    test(name, True)
