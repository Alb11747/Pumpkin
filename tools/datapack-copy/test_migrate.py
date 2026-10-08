"""Offline migration safety and resource-layout tests, using synthetic ZIPs only."""

import importlib.util
import json
from pathlib import Path
import stat
import subprocess
import sys
import zipfile

import pytest

spec = importlib.util.spec_from_file_location("datapack_copy", Path(__file__).with_name("migrate.py"))
migration = importlib.util.module_from_spec(spec)
spec.loader.exec_module(migration)


def make_pack(source, entries, name="example.zip"):
    source.mkdir(exist_ok=True)
    path = source / name
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr("pack.mcmeta", '{"pack":{"pack_format":10,"description":"fixture"}}')
        for resource, raw in entries.items():
            if "\\" in resource:
                # ZipInfo normally rewrites Windows separators when constructing a fixture.
                entry = zipfile.ZipInfo("placeholder")
                entry.filename = resource
                entry.orig_filename = resource
                resource = entry
            archive.writestr(resource, raw)
    return path


def test_dry_run_copy_and_idempotence_preserve_archive_and_contents(tmp_path):
    source, target = tmp_path / "archives", tmp_path / "copied"
    body = b"# original CRLF\r\nsay hello\r\n"
    archive = make_pack(source, {
        "data/demo/functions/nested/hello.mcfunction": body,
        "data/minecraft/tags/functions/load.json": b'{"values":["demo:nested/hello"]}',
        "data/demo/loot_tables/example.json": b'{"pools":[]}',
    })
    original = archive.read_bytes()
    preview = migration.migrate(source, target)
    assert preview["state"] == "new" and not target.exists()
    assert len(preview["packs"][0]["renamed"]) == 2
    result = migration.migrate(source, target, dry_run=False)
    assert result["state"] == "new"
    assert archive.read_bytes() == original
    assert (target / "example.zip/data/demo/function/nested/hello.mcfunction").read_bytes() == body
    assert (target / "example.zip/data/minecraft/tags/function/load.json").is_file()
    assert not (target / "example.zip/data/demo/functions").exists()
    assert (target / "example.zip/data/demo/loot_tables/example.json").read_bytes() == b'{"pools":[]}'
    before = {p: p.stat().st_mtime_ns for p in target.rglob("*")}
    assert migration.migrate(source, target, dry_run=False)["state"] == "unchanged"
    assert {p: p.stat().st_mtime_ns for p in target.rglob("*")} == before


def test_callback_bootstrap_requires_unique_function_and_verified_delay(tmp_path):
    source, target = tmp_path / "archives", tmp_path / "copied"
    callbacks = [{"function": f"demo{n}:tick", "delay": "1t"} for n in range(4)]
    make_pack(source, {f"data/demo{n}/functions/tick.mcfunction":
                      f"schedule function demo{n}:tick 1t\nsay fixture{n}\n" for n in range(4)})
    migration.migrate(source, target, dry_run=False, callbacks=callbacks, bootstrap_pack_format=121)
    bootstrap = target / migration.BOOTSTRAP_PACK
    lines = (bootstrap / "data/migration_callbacks/function/bootstrap.mcfunction").read_text().splitlines()
    assert lines[1:] == [f"schedule function demo{n}:tick 1t replace" for n in range(4)]
    assert json.loads((bootstrap / "data/minecraft/tags/function/load.json").read_text()) == {
        "values": ["migration_callbacks:bootstrap"]}
    with pytest.raises(ValueError, match="delay is not verified"):
        migration.migrate(source, tmp_path / "bad-delay", callbacks=[{
            "function": "demo0:tick", "delay": "2t"}], bootstrap_pack_format=121)
    with pytest.raises(ValueError, match="exactly one source"):
        migration.migrate(source, tmp_path / "missing", callbacks=[{
            "function": "missing:tick", "delay": "1t"}], bootstrap_pack_format=121)
    make_pack(source, {"data/demo0/function/tick.mcfunction": "schedule function demo0:tick 1t"},
              name="duplicate.zip")
    with pytest.raises(ValueError, match="exactly one source"):
        migration.migrate(source, tmp_path / "duplicate", callbacks=callbacks, bootstrap_pack_format=121)


@pytest.mark.parametrize("extra", ["extra.txt", "extra-directory/"])
def test_existing_target_conflicts_do_not_modify_any_file(tmp_path, extra):
    source, target = tmp_path / "archives", tmp_path / "copied"
    make_pack(source, {"data/demo/functions/a.mcfunction": "say original"})
    migration.migrate(source, target, dry_run=False)
    resource = target / "example.zip/data/demo/function/a.mcfunction"
    resource.write_bytes(b"user change")
    if extra.endswith("/"):
        (target / extra).mkdir()
    else:
        (target / extra).write_bytes(b"keep")
    result = migration.migrate(source, target, dry_run=False)
    assert result["state"] == "conflict"
    assert any(row["change"] == "modified" for row in result["diff"])
    assert resource.read_bytes() == b"user change"
    assert (target / extra).exists()


@pytest.mark.parametrize("unsafe", ["../escape", "/absolute", "data/demo/functions/CON.txt",
                                   "data\\demo\\functions\\file", "data/demo/functions/trailing."])
def test_unsafe_archive_paths_refused_before_output(tmp_path, unsafe):
    source, target = tmp_path / "archives", tmp_path / "copied"
    make_pack(source, {unsafe: "payload"})
    with pytest.raises(ValueError, match="Unsafe"):
        migration.migrate(source, target, dry_run=False)
    assert not target.exists()


@pytest.mark.parametrize("entries", [
    {"data/demo/functions/a.mcfunction": "old", "data/demo/function/a.mcfunction": "new"},
    {"data/demo/functions/a.mcfunction": "first", "data/demo/functions/A.mcfunction": "second"},
    {"data/demo/functions/sub": "file", "data/demo/functions/sub/child.mcfunction": "child"},
    {"data/demo/functions/One/a.mcfunction": "a", "data/demo/functions/one/b.mcfunction": "b"},
])
def test_layout_and_case_collisions_refused(tmp_path, entries):
    source, target = tmp_path / "archives", tmp_path / "copied"
    make_pack(source, entries)
    with pytest.raises(ValueError, match="collide|casing"):
        migration.migrate(source, target, dry_run=False)
    assert not target.exists()


def test_links_and_size_limits_refused(tmp_path, monkeypatch):
    source, target = tmp_path / "archives", tmp_path / "copied"
    archive = make_pack(source, {"data/demo/functions/a.mcfunction": "say hello"})
    link = zipfile.ZipInfo("data/demo/functions/link")
    link.create_system = 3
    link.external_attr = (stat.S_IFLNK | 0o777) << 16
    with zipfile.ZipFile(archive, "a") as z:
        z.writestr(link, "../../escape")
    with pytest.raises(ValueError, match="Special ZIP"):
        migration.migrate(source, target, dry_run=False)
    make_pack(source, {"data/demo/functions/a.mcfunction": "say hello"})
    monkeypatch.setattr(migration, "MAX_FILE_BYTES", 3)
    with pytest.raises(ValueError, match="copy limits"):
        migration.migrate(source, target, dry_run=False)
    assert not target.exists()


def test_source_overlap_and_preexisting_empty_target_refused(tmp_path):
    source = tmp_path / "archives"
    make_pack(source, {"data/demo/functions/a.mcfunction": "say hello"})
    with pytest.raises(ValueError, match="non-overlapping"):
        migration.migrate(source, source / "copy", dry_run=False)
    target = tmp_path / "empty"
    target.mkdir()
    assert migration.migrate(source, target, dry_run=False)["state"] == "conflict"
    assert list(target.iterdir()) == []


@pytest.mark.parametrize("callbacks", [[], [{"function": "demo:tick", "delay": "0t"}],
                                      [{"function": "demo:../tick", "delay": "1t"}],
                                      [{"function": "demo:tick", "delay": "1t", "other": True}],
                                      [{"function": "demo:tick", "delay": "1t"}] * 2])
def test_reseed_manifest_validation(tmp_path, callbacks):
    path = tmp_path / "callbacks.json"
    path.write_text(json.dumps({"callbacks": callbacks}), encoding="utf-8", newline="\n")
    with pytest.raises(ValueError):
        migration.load_callbacks(path)


def test_cli_dry_run_and_conflict_exit_status(tmp_path):
    source, target = tmp_path / "archives", tmp_path / "copied"
    make_pack(source, {"data/demo/functions/tick.mcfunction": "schedule function demo:tick 1t"})
    manifest = tmp_path / "callbacks.json"
    manifest.write_text('{"callbacks":[{"function":"demo:tick","delay":"1t"}]}',
                        encoding="utf-8", newline="\n")
    command = [sys.executable, "-B", str(Path(__file__).with_name("migrate.py")),
               "--source", str(source), "--copy-target", str(target),
               "--reseed-manifest", str(manifest), "--bootstrap-pack-format", "121"]
    preview = subprocess.run(command + ["--dry-run"], capture_output=True, text=True, check=True)
    assert json.loads(preview.stdout)["state"] == "new" and not target.exists()
    subprocess.run(command, capture_output=True, text=True, check=True)
    (target / "unexpected.txt").write_bytes(b"keep")
    conflict = subprocess.run(command, capture_output=True, text=True, check=False)
    assert conflict.returncode == 1 and json.loads(conflict.stdout)["state"] == "conflict"
    assert (target / "unexpected.txt").read_bytes() == b"keep"
