"""Copy ZIP datapacks to directory packs; never edit archives or an existing copy."""

import argparse
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import tempfile
import zipfile


MAX_ARCHIVE_BYTES = 64 * 1024 * 1024
MAX_FILE_BYTES = 16 * 1024 * 1024
MAX_TOTAL_BYTES = 256 * 1024 * 1024
MAX_FILES = 10000
BOOTSTRAP_PACK = "migration_recurring_callbacks"
BOOTSTRAP_ID = "migration_callbacks:bootstrap"
FUNCTION_ID = re.compile(r"[a-z0-9_.-]+:[a-z0-9_./-]+")
WINDOWS_RESERVED = re.compile(r"(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\..*)?", re.I)


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def safe_parts(name):
    parts = name.split("/")
    if (not name or "\\" in name or any(
        part in ("", ".", "..") or part.endswith((" ", "."))
        or re.search(r'[<>:"|?*\x00-\x1f]', part) or WINDOWS_RESERVED.fullmatch(part)
        for part in parts
    )):
        raise ValueError(f"Unsafe or nonportable path: {name!r}")
    return parts


def modern_name(name):
    parts = safe_parts(name)
    if len(parts) >= 4 and parts[0] == "data":
        if parts[2] == "functions":
            parts[2] = "function"
        elif len(parts) >= 5 and parts[2:4] == ["tags", "functions"]:
            parts[3] = "function"
    return "/".join(parts)


def is_link(path):
    return path.is_symlink() or path.is_junction()


def check_no_links(path):
    for parent in (path, *path.parents):
        if is_link(parent):
            raise ValueError(f"Symlink/junction paths are not allowed: {parent}")


def read_bounded(path, limit):
    if path.stat().st_size > limit:
        raise ValueError(f"File exceeds {limit} byte limit: {path}")
    with path.open("rb") as stream:
        raw = stream.read(limit + 1)
    if len(raw) > limit:
        raise ValueError(f"File exceeds {limit} byte limit: {path}")
    return raw


def json_bytes(value):
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


def load_callbacks(path):
    if path is None:
        return []
    manifest = json.loads(read_bounded(path, 64 * 1024))
    if not isinstance(manifest, dict) or set(manifest) != {"callbacks"}:
        raise ValueError("Reseed manifest must contain only a callbacks array")
    callbacks = manifest["callbacks"]
    if not isinstance(callbacks, list) or not 1 <= len(callbacks) <= 64:
        raise ValueError("Reseed manifest needs 1 to 64 callbacks")
    seen = set()
    for callback in callbacks:
        if not isinstance(callback, dict) or set(callback) != {"function", "delay"}:
            raise ValueError("Each callback needs exactly function and delay")
        ident, delay = callback["function"], callback["delay"]
        if (not isinstance(ident, str) or not FUNCTION_ID.fullmatch(ident)
                or any(p in ("", ".", "..") for p in ident.split(":")[1].split("/"))):
            raise ValueError(f"Invalid callback function: {ident!r}")
        if not isinstance(delay, str) or not re.fullmatch(r"[1-9][0-9]*t", delay):
            raise ValueError("Callback delay must be a positive integer tick duration")
        if int(delay[:-1]) > 2147483647 or ident in seen:
            raise ValueError("Duplicate callback or tick duration out of range")
        seen.add(ident)
    return callbacks


def build_plan(source, callbacks, bootstrap_pack_format):
    """Read bounded archives and return every output byte before publishing anything."""
    files, packs, functions = {}, [], {}
    total_bytes = 0
    archives = sorted(source.glob("*.zip"))
    if not archives or len(archives) > 128:
        raise ValueError("Source directory needs 1 to 128 top-level .zip packs")
    pack_names = set()
    for archive in archives:
        safe_parts(archive.name)
        if archive.name.startswith("."):
            raise ValueError("Hidden pack names are skipped by Pumpkin's loader")
        if is_link(archive) or not archive.is_file():
            raise ValueError(f"Archive is not a regular file: {archive}")
        folded_pack = archive.name.casefold()
        if folded_pack in pack_names or folded_pack == BOOTSTRAP_PACK.casefold():
            raise ValueError("Pack names collide")
        pack_names.add(folded_pack)
        raw = read_bounded(archive, MAX_ARCHIVE_BYTES)
        contents, destinations, renamed, spellings = {}, set(), [], {}
        with zipfile.ZipFile(io.BytesIO(raw)) as z:
            if len(z.infolist()) > MAX_FILES:
                raise ValueError("Archive has too many entries")
            for entry in z.infolist():
                # ZipInfo normalizes Windows separators and truncates NULs in filename.
                # Validate the unmodified central-directory name before using any path.
                name = entry.orig_filename.rstrip("/") if entry.is_dir() else entry.orig_filename
                safe_parts(name)
                mode = stat.S_IFMT(entry.external_attr >> 16)
                if mode not in (0, stat.S_IFREG, stat.S_IFDIR):
                    raise ValueError(f"Special ZIP entry is not allowed: {name}")
                if entry.is_dir():
                    continue
                dest = modern_name(name)
                if dest.casefold() in destinations:
                    raise ValueError(f"Resource paths collide after migration: {dest}")
                destinations.add(dest.casefold())
                for index in range(1, len(dest.split("/")) + 1):
                    prefix = "/".join(dest.split("/")[:index])
                    previous = spellings.setdefault(prefix.casefold(), prefix)
                    if previous != prefix:
                        raise ValueError(f"Inconsistent path casing: {dest}")
                total_bytes += entry.file_size
                if entry.file_size > MAX_FILE_BYTES or total_bytes > MAX_TOTAL_BYTES:
                    raise ValueError("Uncompressed ZIP data exceeds copy limits")
                if len(files) + len(contents) >= MAX_FILES:
                    raise ValueError("Total file count exceeds copy limit")
                contents[dest] = z.read(entry)
                if dest != name:
                    renamed.append({"from": name, "to": dest})
        if "pack.mcmeta" not in contents:
            raise ValueError(f"Archive needs root pack.mcmeta: {archive.name}")
        metadata = json.loads(contents["pack.mcmeta"])
        if not isinstance(metadata, dict) or not isinstance(metadata.get("pack"), dict):
            raise ValueError(f"Invalid pack metadata: {archive.name}")
        if "overlays" in metadata:
            raise ValueError("Pack overlays need a separate version-specific review")
        # Detect file/directory conflicts before any filesystem write, including on Windows.
        for name in contents:
            for parent in PurePosixPath(name).parents:
                if parent.as_posix().casefold() in destinations:
                    raise ValueError(f"File/directory paths collide: {name}")
            parts = name.split("/")
            if len(parts) >= 4 and parts[0] == "data" and parts[2] == "function" \
                    and name.endswith(".mcfunction"):
                ident = parts[1] + ":" + "/".join(parts[3:])[:-len(".mcfunction")]
                functions.setdefault(ident, []).append((archive.name, contents[name]))
            files[archive.name + "/" + name] = contents[name]
        packs.append({"pack_id": "file/" + archive.name, "source_sha256": digest(raw),
                      "files": len(contents), "renamed": renamed})

    if callbacks:
        if bootstrap_pack_format is None or not 1 <= bootstrap_pack_format <= 4294967295:
            raise ValueError("Reseeding requires an explicit positive --bootstrap-pack-format")
        if BOOTSTRAP_ID in functions:
            raise ValueError("Bootstrap function conflicts with an existing pack")
        for callback in callbacks:
            ident, delay = callback["function"], callback["delay"]
            matches = functions.get(ident, [])
            if len(matches) != 1:
                raise ValueError(f"Callback needs exactly one source function: {ident}")
            lines = [line.strip().removeprefix("/") for line in
                     matches[0][1].decode("utf-8-sig").splitlines()]
            command = f"schedule function {ident} {delay}"
            if command not in lines and command + " replace" not in lines:
                raise ValueError(f"Callback delay is not verified by its source body: {ident}")
        base = BOOTSTRAP_PACK + "/"
        files[base + "pack.mcmeta"] = json_bytes({"pack": {
            "pack_format": bootstrap_pack_format,
            "min_format": bootstrap_pack_format, "max_format": bootstrap_pack_format,
            "description": "Explicit recurring callback reseed on server load"}})
        files[base + "data/minecraft/tags/function/load.json"] = json_bytes({
            "values": [BOOTSTRAP_ID]})
        files[base + "data/migration_callbacks/function/bootstrap.mcfunction"] = (
            "# Delays verified against supplied self-scheduling function bodies.\n" +
            "".join(f"schedule function {c['function']} {c['delay']} replace\n"
                    for c in callbacks)).encode("utf-8")
    report = {"packs": packs, "callbacks": callbacks,
              "bootstrap_pack_id": "file/" + BOOTSTRAP_PACK if callbacks else None,
              "duplicate_function_ids": sorted(k for k, v in functions.items() if len(v) > 1),
              "output_files": {name: digest(raw) for name, raw in sorted(files.items())}}
    return files, report


def target_diff(target, files):
    expected_dirs = {p.as_posix() for name in files for p in PurePosixPath(name).parents
                     if p.as_posix() != "."}
    if not target.exists():
        return [{"path": name, "change": "add"} for name in sorted(files)]
    if not target.is_dir():
        raise ValueError("Copy target is not a directory")
    actual_files, actual_dirs = {}, set()
    for path in target.rglob("*"):
        if is_link(path):
            raise ValueError(f"Link in copy target: {path}")
        name = path.relative_to(target).as_posix()
        if path.is_dir():
            actual_dirs.add(name)
        elif path.is_file():
            # Oversized unexpected files need no read to establish a conflict.
            actual_files[name] = digest(read_bounded(path, MAX_FILE_BYTES)) if name in files else None
        else:
            raise ValueError(f"Special file in copy target: {path}")
    changes = []
    for name in sorted(files.keys() | actual_files.keys()):
        if name not in actual_files:
            changes.append({"path": name, "change": "missing"})
        elif name not in files:
            changes.append({"path": name, "change": "unexpected"})
        elif digest(files[name]) != actual_files[name]:
            changes.append({"path": name, "change": "modified"})
    changes.extend({"path": name + "/", "change": "unexpected_directory"}
                   for name in sorted(actual_dirs - expected_dirs))
    return changes


def migrate(source, target, *, dry_run=True, callbacks=None, bootstrap_pack_format=None):
    source, target = Path(source).absolute(), Path(target).absolute()
    check_no_links(source)
    check_no_links(target)
    source, target = source.resolve(), target.resolve()
    if source == target or source in target.parents or target in source.parents:
        raise ValueError("Source and copy target must be separate, non-overlapping trees")
    if not source.is_dir():
        raise ValueError("Source must be a directory of ZIP archives")
    files, report = build_plan(source, callbacks or [], bootstrap_pack_format)
    changes = target_diff(target, files)
    state = "conflict" if target.exists() and changes else "unchanged" if not changes else "new"
    report.update({"state": state, "dry_run": dry_run, "diff": changes})
    if dry_run or state == "conflict" or state == "unchanged":
        return report
    if not target.parent.is_dir():
        raise ValueError("Copy target parent must already exist")
    staging = Path(tempfile.mkdtemp(prefix=".datapack-copy-", dir=target.parent))
    try:
        for name, raw in files.items():
            path = staging / name
            path.parent.mkdir(parents=True, exist_ok=True)
            with path.open("xb") as stream:
                stream.write(raw)
        # Exclusive reservation prevents POSIX rename from replacing a raced empty target.
        # A failure after this point leaves an incomplete isolated copy for inspection.
        target.mkdir()
        for child in staging.iterdir():
            child.rename(target / child.name)
    finally:
        if staging.exists():
            shutil.rmtree(staging)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--copy-target", required=True, type=Path)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--reseed-manifest", type=Path)
    parser.add_argument("--bootstrap-pack-format", type=int)
    args = parser.parse_args()
    try:
        callbacks = load_callbacks(args.reseed_manifest)
        report = migrate(args.source, args.copy_target, dry_run=args.dry_run,
                         callbacks=callbacks, bootstrap_pack_format=args.bootstrap_pack_format)
    except (ValueError, OSError, zipfile.BadZipFile) as error:
        parser.exit(1, f"datapack copy failed: {error}\n")
    print(json.dumps(report, indent=2, sort_keys=True))
    return 1 if report["state"] == "conflict" else 0


if __name__ == "__main__":
    raise SystemExit(main())
