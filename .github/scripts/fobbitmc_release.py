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
    {"prepare": prepare, "collect": collect, "manifest": manifest}[mode](*args)
