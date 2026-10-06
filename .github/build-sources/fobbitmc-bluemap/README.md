# FobbitMC BlueMap player exporter

A small native Pumpkin plugin exports connected players once per second for the
BlueMap 5.28 CLI live-player supplier. It does not run an HTTP server or proxy.
Pumpkin and BlueMap remain separate processes; BlueMap serves its own HTTP and
SSE player updates after reading this file.

The supported Pumpkin revision is the exact git revision in `Cargo.toml`. Native
Rust plugin ABI compatibility requires rebuilding this library and Pumpkin with
the same revision, Rust toolchain and target. An API version match alone does not
prove Rust ABI compatibility. This source targets native API 7 at the exact reviewed revision
`db2027bf3b11eb9d5fb1a2c0803f65be25bdba90`.

## Build and load

Build on the same OS/architecture as Pumpkin using Rust 1.96 or newer and the
same actual toolchain as the server:

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
```

The first controlled build must generate and retain `Cargo.lock`; subsequently
use `--locked`. The pure snapshot/policy tests can also be run with
`cargo test --locked --no-default-features` without compiling the native engine.
Validate the full default feature build before loading the library.

Copy `target/release/libfobbitmc_bluemap.so` (Linux),
`target/release/fobbitmc_bluemap.dll` (Windows), or
`target/release/libfobbitmc_bluemap.dylib` (macOS) into the server's `plugins/`
directory. Pumpkin's native loader discovers these libraries. Prepare the
required private visibility file before starting or loading the plugin:

```sh
mkdir -p plugins/data/fobbitmc-bluemap
python scripts/migrate_visibility.py /private/cold/pluginState.json plugins/data/fobbitmc-bluemap/visibility.json
```

On Windows, create the same relative directory with PowerShell `New-Item
-ItemType Directory`. The migration reads the root `hidden-players` list from a
verified cold BlueMap plugin state. It requires the key, unique canonical UUIDs,
valid UTF-8 and unambiguous JSON; it never turns missing privacy state into an
empty list. Keep the input and generated configuration private. Do not check
player UUIDs or live snapshots into source control.

The exact private policy schema is:

```json
{"version":1,"hiddenPlayers":[],"vanishSource":"pumpkin-observer-hides"}
```

An empty list is authoritative only if the migration source actually has an
empty hidden-player set. Missing, malformed, oversized or unsupported policy
files fail plugin loading visibly. The plugin reads policy once during loading;
unload/reload it to apply a new verified policy. No policy default is generated.

The output, relative to Pumpkin's working directory, is:

```text
plugins/data/fobbitmc-bluemap/live-players.json
```

Point the CLI supplier's snapshot setting at this same file. A single named
worker samples on a one-second deadline. It flushes a sibling `.tmp` file and
renames it over the snapshot. The loader waits for successful startup. Unloading
wakes and joins the owned worker after writing a final empty snapshot, so no
plugin task remains active after library unloading. Initial loading writes an
empty snapshot before regular sampling. Filesystem failures and unexpected
capture failures are visible errors; capture/validation failures clear players,
and a write/clear failure stops the worker. An inaccessible filesystem can
prevent the empty write; the reader's five-second stale timeout remains the
fallback. Startup and unload wait for owned filesystem work to finish.

## State contract and paths

Snapshot version 1 contains `generatedAt` as Unix epoch milliseconds and a
`players` array. Every player has a lowercase canonical `uuid`, `name`,
`worldPath`, vanilla `dimension`, `position` (`x/y/z`), `rotation` (`pitch/yaw`),
lowercase `gamemode`, and required nullable `invisible`, `sneaking`, `vanished`,
`hidden`, `skyLight` and `blockLight` fields.

- Position, rotation, game mode, invisibility and sneaking come from the actual
  connected Pumpkin player/entity. Closed clients are omitted. A transition
  that changes world during capture is omitted until the next sample. Players
  awaiting registration in the destination world's entity tracker are also
  omitted, so a dimension transition cannot pair its new world with old coordinates.
- `hidden` is membership in the explicitly migrated `hiddenPlayers` set.
- `vanished` is true if **any connected Pumpkin observer** currently hides the
  player using the engine's actual `hidden_players` set. Otherwise it is false.
  This conservative observer-hide contract is selected explicitly by
  `vanishSource`. Bukkit plugins that only set metadata or use a separate vanish
  state need another adapter; this is not generic Bukkit vanish parity.
- Light is read from actual populated, currently loaded chunk light arrays at
  the exported position. Missing chunks/sections, incomplete light population
  or out-of-range height yield JSON `null`. No chunks are loaded for sampling.
  The engine sky-light convenience getter is deliberately avoided because it
  substitutes numeric values for missing data.
- `worldPath` is the filesystem-canonical **saved-world root** from
  `world.level.level_folder.root_folder`, not the dimension directory. Vanilla
  dimension keys distinguish Overworld, Nether and End. Windows verbatim drive
  and UNC prefixes are normalized to Java's `toRealPath` spelling; Linux paths
  are unchanged. No path aliases or deployment remapping are invented.

Both processes must see the world at the same canonical absolute path. For
containers, mount the same saved-world root at the same absolute path inside
both containers, and expose the snapshot directory read-only to BlueMap. A
matching host directory mounted at different in-container paths will not match.
Configure BlueMap's world root to the actual canonical root and verify the
serialized spelling against BlueMap's resolved root before accepting runtime
visibility. Canonicalization errors clear the sampled players.

Only the three vanilla dimension keys are supported. Snapshots above 1024
players or 1 MiB, non-finite/out-of-range coordinates and invalid rotations are
rejected and replaced by an empty snapshot. The paired CLI reader clears
snapshots older than five seconds or more than one second in the future, and
fails closed when a configured privacy/light policy needs unknown state. Clocks
must therefore be synchronized across the two processes.

Wasm API v0.2 exposes player flags and light access, but not the canonical saved
world root. Its numeric light access also cannot represent missing light data.
The native implementation uses the exact loader's metadata, API-version and
factory symbols rather than the SDK macro's process-lifetime global runtime;
one joined worker keeps library/task lifetime explicit.

## Validation

`python -m unittest discover -s scripts -p 'test_*.py'` checks strict privacy
migration using synthetic identifiers. Rust tests cover required null fields,
invalid sampled state, UUID uniqueness/player bounds, atomic replacement,
worker stop/empty output, explicit privacy parsing and Windows path spelling.

Runtime acceptance additionally requires a synthetic client joining, moving,
changing dimensions and disconnecting while checking the actual BlueMap HTTP
and SSE routes. Verify privacy policy effects, loader unload/empty output and
clock/staleness behavior. Unit tests do not prove connected-player routing,
native ABI compatibility or actual world-root matching. Production deployment
is a separate operation; this repository contains no production configuration.
