"""Verify pinned release inputs and retain only the explicitly named artifacts."""

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tomllib


ROOT = Path.cwd()
TEMP = Path(os.environ["RUNNER_TEMP"])
DIST = ROOT / "dist"
STATE = TEMP / "fobbitmc-provenance.json"
ENGINE_URL = "https://github.com/Alb11747/Pumpkin.git"


def command(*args, cwd=None):
    return subprocess.check_output(args, cwd=cwd, text=True).strip()


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def git_source(path, expected):
    actual = command("git", "rev-parse", "HEAD", cwd=path)
    if actual != expected:
        raise ValueError(f"Source commit mismatch: {path.name}")
    files = command("git", "ls-files", cwd=path).splitlines()
    return {
        "commit": actual,
        "tree": command("git", "rev-parse", "HEAD^{tree}", cwd=path),
        "files": {name: sha256(path / name) for name in files},
    }


def verify_pins(path, expected_count):
    manifest = tomllib.loads((path / "Cargo.toml").read_text(encoding="utf-8"))
    pins = [dep for dep in manifest["dependencies"].values()
            if isinstance(dep, dict) and dep.get("git") == ENGINE_URL]
    if len(pins) != expected_count or any(dep.get("rev") != os.environ["ENGINE_SHA"] for dep in pins):
        raise ValueError(f"Wrong exact Pumpkin dependency pins: {path}")
    lock = tomllib.loads((path / "Cargo.lock").read_text(encoding="utf-8"))
    sources = {package["source"] for package in lock["package"]
               if package.get("source", "").startswith("git+" + ENGINE_URL)}
    expected = f"git+{ENGINE_URL}?rev={os.environ['ENGINE_SHA']}#{os.environ['ENGINE_SHA']}"
    if sources != {expected}:
        raise ValueError(f"Cargo.lock has mismatched Pumpkin source: {path}")


def prepare(target):
    engine = git_source(ROOT / "engine", os.environ["ENGINE_SHA"])
    bridge = git_source(ROOT / "bridge", os.environ["BRIDGE_SHA"])
    verify_pins(ROOT / "bridge/rust", 6)
    api_source = (ROOT / "engine/crates/pumpkin-core/src/plugin/mod.rs").read_text(encoding="utf-8")
    match = re.search(r"pub const PLUGIN_API_VERSION: u32 = (\d+);", api_source)
    if match is None or match.group(1) != "7":
        raise ValueError("Expected reviewed native API 7 engine")
    state = {
        "engine": engine, "bridge": bridge, "nativeApi": 7, "target": target,
        "workflowCommit": command("git", "rev-parse", "HEAD", cwd=ROOT / "build-control"),
        "rustc": command("rustc", "+" + os.environ["RUST_TOOLCHAIN"], "-Vv"),
        "cargo": command("cargo", "+" + os.environ["RUST_TOOLCHAIN"], "-V"),
        "java": subprocess.run(["java", "-version"], capture_output=True, text=True, check=True).stderr.strip(),
        "releaseEnvironment": {key: value for key, value in os.environ.items()
                               if key.startswith("CARGO_PROFILE_RELEASE_") or key in
                               ("RUSTFLAGS", "CARGO_BUILD_JOBS", "CARGO_INCREMENTAL")},
        "runner": {key: os.environ.get(key) for key in
                   ("RUNNER_OS", "RUNNER_ARCH", "ImageOS", "ImageVersion", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT")},
    }
    if os.environ["BUILD_EXPORTER"] == "true":
        source = ROOT / "build-control/.github/build-sources/fobbitmc-bluemap"
        provenance = json.loads(source.with_suffix(".json").read_text(encoding="utf-8"))
        if provenance["commit"] != os.environ["EXPORTER_SHA"]:
            raise ValueError("Vendored exporter commit mismatch")
        files = provenance["files"]
        actual_files = {path.relative_to(source).as_posix() for path in source.rglob("*") if path.is_file()}
        if actual_files != set(files) or any(sha256(source / name) != digest for name, digest in files.items()):
            raise ValueError("Vendored exporter file hashes mismatch")
        verify_pins(source, 2)
        shutil.copytree(source, TEMP / "fobbitmc-bluemap")
        state["exporter"] = provenance
    STATE.write_text(json.dumps(state, indent=2) + "\n", encoding="utf-8", newline="\n")
    DIST.mkdir()


def autosave_negative():
    state = json.loads(STATE.read_text(encoding="utf-8"))
    state["autosaveNegative"] = git_source(ROOT / "engine", os.environ["AUTOSAVE_BASELINE_SHA"])
    state["autosaveNegative"]["assertion"] = "autosave must commit live metadata before shutdown"
    STATE.write_text(json.dumps(state, indent=2) + "\n", encoding="utf-8", newline="\n")


def merchant_negative(target):
    test = "entity::passive::villager::tests::successful_trade_player_click_returns_and_preserves_payment"
    assertion = "successful trade player click must return after its callback"
    source = git_source(ROOT / "engine", os.environ["MERCHANT_BASELINE_SHA"])
    log = TEMP / f"merchant-negative-{target}.log"
    args = ["cargo", "+" + os.environ["RUST_TOOLCHAIN"], "test", "--locked",
            "--release", "--target", target, "-p", "pumpkin-core", "--lib", test,
            "--", "--exact", "--color", "never"]
    with log.open("w", encoding="utf-8", newline="\n") as stream:
        result = subprocess.run(args, cwd=ROOT / "engine", stdout=stream,
                                stderr=subprocess.STDOUT, timeout=1800, check=False)
    output = log.read_text(encoding="utf-8")
    print(output, end="")
    lines = output.splitlines()
    if (result.returncode != 101 or "running 1 test" not in lines
            or f"test {test} ... FAILED" not in lines or assertion not in output
            or re.search(r"^test result: FAILED\. 0 passed; 1 failed; 0 ignored; 0 measured; [0-9]+ filtered out;", output, re.MULTILINE) is None):
        raise ValueError("Merchant negative gate did not fail on the exact deadlock regression")
    state = json.loads(STATE.read_text(encoding="utf-8"))
    state["merchantNegative"] = {**source, "test": test, "assertion": assertion,
                                 "exitCode": result.returncode, "logSha256": sha256(log)}
    STATE.write_text(json.dumps(state, indent=2) + "\n", encoding="utf-8", newline="\n")


def persistence_negative(target):
    test = "world::entity_storage::tests::unload_crossing_frog_keeps_source_and_live_neighbor_records"
    assertion = "unload crossing must retain both root UUIDs without replacing the live neighbor"
    source = git_source(ROOT / "engine", os.environ["PERSISTENCE_BASELINE_SHA"])
    log = TEMP / f"persistence-negative-{target}.log"
    args = ["cargo", "+" + os.environ["RUST_TOOLCHAIN"], "test", "--locked",
            "--release", "--target", target, "-p", "pumpkin-core", "--lib", test,
            "--", "--exact", "--color", "never"]
    with log.open("w", encoding="utf-8", newline="\n") as stream:
        result = subprocess.run(args, cwd=ROOT / "engine", stdout=stream,
                                stderr=subprocess.STDOUT, timeout=1800, check=False)
    output = log.read_text(encoding="utf-8")
    print(output, end="")
    lines = output.splitlines()
    if (result.returncode != 101 or "running 1 test" not in lines
            or f"test {test} ... FAILED" not in lines or assertion not in output
            or re.search(r"^test result: FAILED\. 0 passed; 1 failed; 0 ignored; 0 measured; [0-9]+ filtered out;", output, re.MULTILINE) is None):
        raise ValueError("Persistence control did not fail on the exact neighboring-entity loss assertion")
    state = json.loads(STATE.read_text(encoding="utf-8"))
    state["persistenceNegative"] = {**source, "test": test, "assertion": assertion,
                                    "exitCode": result.returncode, "logSha256": sha256(log)}
    STATE.write_text(json.dumps(state, indent=2) + "\n", encoding="utf-8", newline="\n")


# Each focused command must run the expected named tests; zero-test success fails.
POSITIVE_GATES = {
    "entity-persistence": [
        ("pumpkin-core", "world::entity_storage::tests::unload_crossing_frog_keeps_source_and_live_neighbor_records", [
            "world::entity_storage::tests::unload_crossing_frog_keeps_source_and_live_neighbor_records",
        ]),
        ("pumpkin-core", "world::entity_storage::tests::unload_waits_for_cloned_tick_before_classifying_crossing_entities", [
            "world::entity_storage::tests::unload_waits_for_cloned_tick_before_classifying_crossing_entities",
        ]),
        ("pumpkin-core", "world::entity_storage::tests::unload_does_not_resurrect_a_legitimately_removed_crossing_frog", [
            "world::entity_storage::tests::unload_does_not_resurrect_a_legitimately_removed_crossing_frog",
        ]),
        ("pumpkin-core", "world::entity_storage::tests::snapshot_waits_for_tick_and_includes_new_roots_without_blocking_runtime", [
            "world::entity_storage::tests::snapshot_waits_for_tick_and_includes_new_roots_without_blocking_runtime",
        ]),
        ("pumpkin-core", "entity::passive::villager::tests::only_assigned_job_sites_restock_and_respect_the_second_restock_cooldown", [
            "entity::passive::villager::tests::only_assigned_job_sites_restock_and_respect_the_second_restock_cooldown",
        ]),
        ("pumpkin-core", "entity::living::tests::damage_events_use_exact_registry_keys_instead_of_translation_or_debug_names", [
            "entity::living::tests::damage_events_use_exact_registry_keys_instead_of_translation_or_debug_names",
        ]),
    ],
    "entity-crossing-repeat": [
        ("pumpkin-core", "world::entity_storage::tests::unload_crossing_frog_keeps_source_and_live_neighbor_records", [
            "world::entity_storage::tests::unload_crossing_frog_keeps_source_and_live_neighbor_records",
        ]),
    ] * 3,
    "save-scheduler": [
        ("pumpkin-world", "chunk_system::schedule::save_lineage_tests::save_lineage_fifo_counts_unload_save_and_no_batch_per_world", [
            "chunk_system::schedule::save_lineage_tests::save_lineage_fifo_counts_unload_save_and_no_batch_per_world",
        ]),
    ],
    "save-console": [
        ("pumpkin-core", "world::save_lineage_tests::save_lineage_console_keeps_world_batch_retry_and_autosave_context", [
            "world::save_lineage_tests::save_lineage_console_keeps_world_batch_retry_and_autosave_context",
        ]),
    ],
    "intangible": [
        ("pumpkin-protocol", "codec::data_component::tests::intangible_projectile_", [
            "codec::data_component::tests::intangible_projectile_writes_compound_and_preserves_following_byte",
            "codec::data_component::tests::intangible_projectile_accepts_nonempty_compound",
            "codec::data_component::tests::intangible_projectile_rejects_absent_truncated_and_noncompound_nbt",
        ]),
    ],
    "charged": [
        ("pumpkin-protocol", "codec::data_component::tests::charged_projectiles_", [
            "codec::data_component::tests::charged_projectiles_creative_arrow_has_exact_template_wire",
            "codec::data_component::tests::charged_projectiles_preserve_creative_firework_nested_components",
            "codec::data_component::tests::charged_projectiles_accept_65_and_1024_and_reject_1025",
            "codec::data_component::tests::charged_projectiles_preserve_count_components_and_saved_nbt",
            "codec::data_component::tests::charged_projectiles_reject_empty_templates_and_invalid_lengths",
        ]),
    ],
    "preservation": [
        ("pumpkin-protocol", "java::client::play::merchant_offers::tests::merchant_stock_flag_encodes_and_decodes_like_vanilla", [
            "java::client::play::merchant_offers::tests::merchant_stock_flag_encodes_and_decodes_like_vanilla",
        ]),
        ("pumpkin-protocol", "java::client::play::set_equipment::tests::charged_crossbow_equipment_uses_nonempty_item_templates_for_26_3", [
            "java::client::play::set_equipment::tests::charged_crossbow_equipment_uses_nonempty_item_templates_for_26_3",
        ]),
        ("pumpkin-core", "world::tests::automatic_save_persists_live_clock_and_rules_only_when_enabled", [
            "world::tests::automatic_save_persists_live_clock_and_rules_only_when_enabled",
        ]),
    ],
}


def verify_positive_output(output, exit_code, tests):
    lines = output.splitlines()
    count = len(tests)
    running = f"running {count} test" + ("s" if count != 1 else "")
    passed = [match.group(1) for line in lines
              if (match := re.fullmatch(r"test (.+) \.\.\. ok", line))]
    summary = rf"^test result: ok\. {count} passed; 0 failed; 0 ignored; 0 measured; [0-9]+ filtered out;"
    if (exit_code != 0 or lines.count(running) != 1
            or len(passed) != count or set(passed) != set(tests)
            or re.search(summary, output, re.MULTILINE) is None):
        raise ValueError("Focused positive gate did not pass the exact named tests/count")


def positive(target, group):
    cases = []
    for index, (package, selector, tests) in enumerate(POSITIVE_GATES[group]):
        log = TEMP / f"positive-{group}-{index}-{target}.log"
        args = ["cargo", "+" + os.environ["RUST_TOOLCHAIN"], "test", "--locked",
                "--release", "--target", target, "-p", package, "--lib", selector, "--"]
        if len(tests) == 1:
            args.append("--exact")
        args.extend(["--color", "never"])
        # The console gate gets a separate test process: its global subscriber
        # must cover autosave tasks and scheduler threads together.
        with log.open("w", encoding="utf-8", newline="\n") as stream:
            result = subprocess.run(args, cwd=ROOT / "engine", stdout=stream,
                                    stderr=subprocess.STDOUT, timeout=1800, check=False)
        output = log.read_text(encoding="utf-8")
        print(output, end="")
        verify_positive_output(output, result.returncode, tests)
        cases.append({"package": package, "filter": selector, "tests": tests,
                      "count": len(tests), "exitCode": result.returncode,
                      "logFile": log.name, "logSha256": sha256(log)})
    state = json.loads(STATE.read_text(encoding="utf-8"))
    state.setdefault("positiveGates", {})[group] = {"cases": cases}
    STATE.write_text(json.dumps(state, indent=2) + "\n", encoding="utf-8", newline="\n")


def verify_engine():
    state = json.loads(STATE.read_text(encoding="utf-8"))
    if git_source(ROOT / "engine", os.environ["ENGINE_SHA"]) != state["engine"]:
        raise ValueError("Restored engine source differs from verified release source")


def collect(component, target):
    windows = target.endswith("windows-msvc")
    filenames = {
        "engine": "pumpkin.exe" if windows else "pumpkin",
        "bridge": "patchbukkit.dll" if windows else "libpatchbukkit.so",
        "exporter": "fobbitmc_bluemap.dll" if windows else "libfobbitmc_bluemap.so",
    }
    products = TEMP / "fobbitmc-release-target"
    shutil.copy2(products / target / "release" / filenames[component], DIST / filenames[component])
    if component == "bridge":
        shutil.copy2(ROOT / "bridge/java/patchbukkit/build/libs/patchbukkit.jar", DIST / "patchbukkit.jar")
    # This is a fixed, task-owned directory on an ephemeral GitHub-hosted runner.
    shutil.rmtree(products)


def manifest(target):
    state = json.loads(STATE.read_text(encoding="utf-8"))
    state["artifacts"] = {path.name: {"sha256": sha256(path), "bytes": path.stat().st_size}
                          for path in sorted(DIST.iterdir())}
    if target.endswith("linux-gnu"):
        ceilings = {}
        for path in DIST.iterdir():
            if path.suffix == ".jar":
                continue
            symbols = command("readelf", "--version-info", str(path))
            versions = {(int(major), int(minor)) for major, minor in re.findall(r"GLIBC_(\d+)\.(\d+)", symbols)}
            if not versions or max(versions) > (2, 35):
                raise ValueError(f"Unsupported glibc requirement: {path.name}: {sorted(versions)}")
            ceilings[path.name] = ".".join(map(str, max(versions)))
        state["maxRequiredGlibc"] = ceilings
    (DIST / "provenance.json").write_text(json.dumps(state, indent=2) + "\n", encoding="utf-8", newline="\n")
    (DIST / "SHA256SUMS").write_text("".join(f"{sha256(path)}  {path.name}\n" for path in sorted(DIST.iterdir())), encoding="utf-8", newline="\n")


if __name__ == "__main__":
    mode, *args = sys.argv[1:]
    {"prepare": prepare, "autosave_negative": autosave_negative, "merchant_negative": merchant_negative, "persistence_negative": persistence_negative, "verify_engine": verify_engine,
     "collect": collect, "manifest": manifest, "positive": positive}[mode](*args)
