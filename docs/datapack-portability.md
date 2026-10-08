# Datapack copies and recurring callbacks

This is a bounded resource-layout migration tool, not a datapack interpreter or a complete version
upgrade. It prepares separate copies for inspection and later isolated runtime qualification.

## Source review at f7e9d6ef1291cc5bb9b8925ad837658808fd96af

| Boundary | Implemented behavior and limits |
| --- | --- |
| ZIP packs | `scan_datapacks_dir()` in `crates/pumpkin-core/src/data/datapack/mod.rs` skips every non-directory. `/datapack list` can advertise a ZIP without loading its functions. Extracted directory packs must be enabled. |
| Functions | `load_pack_contents()` reads both `data/<namespace>/function/` and legacy `functions/`. `function_loader.rs` removes blank/comment lines and a leading slash. `execute_function()` dispatches each remaining line through the ordinary command dispatcher. No new execution subsystem is needed for basic functions. |
| Function tags | Both `tags/function/` and `tags/functions/` load. Only string entries in `values` are read; object entries, `replace` semantics and nested tag expansion are not implemented by this loader. Missing members of a known tag are silently skipped. |
| Load/tick hooks | `server/mod.rs` executes `#minecraft:load` after startup loading and after reload; `tick_worlds()` executes `#minecraft:tick` before the scheduled-function queue. |
| Schedule | `/schedule function <id> <time> [replace\|append]` adds a server-tick-relative entry. Default and explicit `replace` remove previous entries with that ID. Scheduled functions run as console. Missing functions can be scheduled, and execution errors are discarded. |
| Restart | `ScheduledFunctionQueue` in `server/scheduler.rs` starts empty and has no saved-data import/export. Old `ScheduledEvents` or modern `scheduled_events.dat` are not restored. Reseeding named loops does not restore any other queued rows or their remaining delays. |
| Command compatibility | Functions use Pumpkin's existing command parsers. The simple `/function` implementation has no argument/macro binding. The loader does not validate function bodies or enforce a function command-chain limit. Successful copy/loading or a returned line count is not proof that commands succeeded. |
| Pack precedence | Directory traversal is not ordered by the enabled-pack list. Duplicate function IDs can overwrite one another. The copy report lists duplicates; named reseed targets must occur exactly once. |

Official Minecraft 26.3 has data-pack format **121.0**, confirmed by its server `version.json` and
the [official 26.3 release notes](https://feedback.minecraft.net/hc/en-us/articles/48913133328013-Minecraft-Java-Edition-26-3).
Modern function resources use `function/` and `tags/function/`. Pumpkin also accepts the plural paths,
so extracting directories addresses its ZIP-loader gap; the singular rename makes those resources
discoverable by official 26.3. Other resources and older command bodies still need their own review.

## Copy-only CLI

Requires Python 3.12 or newer and uses only its standard library. From the repository root:

```text
python -B tools/datapack-copy/migrate.py --source /scratch/archive-packs --copy-target /scratch/copied-packs --dry-run
python -B tools/datapack-copy/migrate.py --source /scratch/archive-packs --copy-target /scratch/copied-packs
```

The source is a directory with 1 to 128 top-level `.zip` files. Other files and directory packs are
outside this tool's scope and are not copied. The explicit target must be outside the source tree;
its parent must already exist when applying. Original archives are only read. Each output directory
keeps the full archive filename, including `.zip`, so an existing enabled ID such as
`file/example.zip` still identifies the directory copy. Retain immutable ZIPs separately: a ZIP and
a directory with the same filename cannot coexist in one datapacks directory.

Only `data/<namespace>/functions/...` and `data/<namespace>/tags/functions/...` paths are renamed.
Every file's contents, including `pack.mcmeta`, stay byte-identical. Resource JSON, item NBT/component
syntax, loot tables, predicates, advancement schemas and other plural resource directories are not
converted. Do not bump old pack metadata as a substitute for that review. Packs with overlays are
refused because overlay selection needs a separate version-specific migration.

Stdout is a JSON review report containing source archive SHA256s, each rename, every planned output
file SHA256, duplicate function IDs and a target diff. `--dry-run` does not create directories or
write files. Applying creates only a previously absent target, after validating all archives and
building the complete output plan. A matching existing output is an unchanged no-op, including
timestamps. Any modified, missing or unexpected file/directory in an existing target causes a
`conflict` report and exit 1 without changing it. Even a preexisting empty target is refused.
To revise a copy, choose a fresh target and review the diff.

Symlinks, junctions, special ZIP entries, unsafe/nonportable paths, duplicate paths, case collisions,
file/directory conflicts and old/new resource collisions are refused. Per-archive compressed input is
limited to 64 MiB, each extracted file to 16 MiB, all extracted content to 256 MiB and 10,000 files.
Preparation uses a private sibling staging directory. Publication reserves a new target exclusively,
then moves the prepared packs into it. Interruption can leave an incomplete isolated target; a rerun
reports conflict and preserves it for inspection. Do not modify the copy target concurrently.

## Explicit recurring-callback seed

Supply a private manifest based on archived queue/function evidence. No callbacks or delays are
inferred or supplied by default. This synthetic example illustrates its entire schema:

```json
{"callbacks": [{"function": "example:tick", "delay": "1t"}]}
```

```text
python -B tools/datapack-copy/migrate.py --source /scratch/archive-packs --copy-target /scratch/copied-packs --reseed-manifest /scratch/callbacks.json --bootstrap-pack-format 121 --dry-run
```

Omit `--dry-run` to prepare the reviewed copy. Each manifest target must exist exactly once and its
body must contain `schedule function <same-id> <same-delay>` with default or explicit `replace`.
The tool accepts only positive integer tick durations and refuses mismatches. This validates a
claimed loop's identity and interval; the operator must establish which archived callbacks were
consumed. No world NBT or scheduled-events file is edited.

The generated `migration_recurring_callbacks` directory contains a modern `minecraft:load` tag and
`migration_callbacks:bootstrap`, with one `schedule function <id> <delay> replace` line per callback.
This queues each loop after load/reload and replaces existing entries of the same ID. It does not run
the callback body immediately, alter its body, or put the callback into `minecraft:tick`. Enabling the
bootstrap also affects subsequent reloads and restarts; it resets the selected loops' next delays.

On a separate stopped runtime copy, integrate the prepared directories and retain original archives
elsewhere. Confirm which source packs are already enabled and enable any missing directory IDs,
then enable `file/migration_recurring_callbacks` last. This tool does not edit world configuration or
execute those commands. Because enable/reload runs load hooks, each command can have side effects.

## Runtime acceptance still required

Use the exact matched server/plugin bundle and a disposable world copy. Confirm logged loaded
function counts for each enabled directory and resolve parser errors in function bodies. Check
the selected callbacks' actual effects over several ticks, reload, and restart. For a HUD, observe
the intended display with a real player; for coordinate/tracking behavior, exercise the relevant
triggers and score changes; for trader changes, inspect actual generated offers. Record runtime
errors and before/after state. Four queued seed lines are preparation, not restored gameplay proof.

Saved schedule persistence, vanilla function-tag semantics, pack precedence and command/macro
compatibility are separate engine work. Implementing them requires focused parity tests and runtime
qualification. The tool neither invents a replacement interpreter nor claims these gaps are closed.

## Offline validation

```text
python -B -m pytest tools/datapack-copy/test_migrate.py -q -p no:cacheprovider --basetemp /scratch/unique-datapack-test-run
```

The tests use synthetic ZIPs to exercise byte preservation, nested path/tag renames, four explicit
seeds, delay/identity validation, no-write dry runs, repeat-run idempotence, conflict refusal and
malformed/colliding ZIP entries. They do not load real worlds, execute server functions or prove
gameplay compatibility.
