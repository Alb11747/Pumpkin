# Importing dimension generation and End fight metadata

Pumpkin normally uses the root world's generation settings for all dimensions. An import of
separate historical worlds can preserve their generation seeds and settings explicitly:

```
<world>/dimensions/minecraft/overworld/data/minecraft/world_gen_settings.dat
<world>/dimensions/minecraft/the_nether/data/minecraft/world_gen_settings.dat
<world>/dimensions/minecraft/the_end/data/minecraft/world_gen_settings.dat
```

These per-dimension generation files are a Pumpkin migration extension. Vanilla has a single
world-generation settings object; a vanilla restart does not honor independent dimension seeds.
Each file uses the normal gzip NBT `data` payload and contains the original `seed` and
`dimensions` generator definitions. Preserve the complete original payload when importing,
including unmodeled settings. The root file remains the fallback for dimensions without an
explicit override. Legacy `level.dat` generation metadata in the selected dimension folder is
also supported. Existing malformed metadata returns an error rather than choosing a new seed.

Generation settings loaded from legacy `level.dat` retain their full generator configuration.
The modeled seed, structures flag, bonus-chest flag, generator and biome-source fields are
updated on save while unmodeled tags remain in the existing file. `generate_structures=false`
disables new structure starts through the existing structure-set filter. This does not remove
structures from existing chunks. Bonus-chest settings are retained; this import does not add
bonus-chest spawning. Unsupported stored dimension types, custom noise settings and biome
sources refuse loading instead of replacing them with a built-in generator. Flat imports also
reject unknown biome/block IDs, invalid layer heights and unsupported decoration, lakes or
structure generation options instead of producing plains, air or incomplete decoration. Datapack-driven
generation remains outside the supported import boundary.

For the End, `dimensions/minecraft/the_end/data/minecraft/ender_dragon_fight.dat` takes precedence.
Its gzip NBT root contains `DataVersion` and a `data` compound with the vanilla 26.3 fields:
`needs_state_scanning`, `dragon_killed`, `previously_killed`, `dragon_uuid`,
`exit_portal_location`, `gateways`, `respawn_stage`, `respawn_time` and `respawn_crystals`.
Without that file, Pumpkin imports `Data/DragonFight` or
`Data/DimensionData/1/DragonFight` from the selected dimension's `level.dat`, then the root
`level.dat`. Dragon UUIDs, portal positions, completion flags and remaining gateway order
remain authoritative. Unknown fight and modern root tags survive saves. Legacy respawn
booleans restart the respawn animation at its first stage after reacquiring all crystals
intersecting the four ritual positions around the saved exit portal. If that ritual cannot yet
be recovered, its pending state remains unchanged across saves. Modern stage/time/crystal
references retain their saved values.

Nearby fight participants activate an owned terrain ticket and counted watches for the central
nine entity chunks, independently of player view distance. Fight ticking waits until those
entity records have been completely admitted. The ticket and watches are released when fight
participants leave; player-owned watches remain intact. Respawn stages increment their timer
before dispatching the previous tick value, preserving the next stage's time-zero effects.

Vanilla 26.3 fills an empty gateway queue with seeded shuffled slots 0 through 19 during
startup, including an exhausted historical queue. Pumpkin follows this behavior. The next
successful dragon kill can therefore place a gateway at a previously used slot after a restart.
The queue is not replenished again during that same run. Account for this vanilla behavior
when assessing historical gateway builds; metadata retention does not eliminate that risk.

Validate on disposable copies before importing a historical world. Verify future chunk seeds,
End completion state, portal location and remaining gateway order after save/reload. Join with
a real client to test End entry, a respawn sequence and a subsequent kill, and capture any
changes around existing portals or gateway builds. Storage tests alone do not establish full
End-fight or terrain-generation parity.

### Pending player vehicles across world transfers

Pumpkin annotates `RootVehicle` with `pumpkin:origin`, a compound containing the
source `dimension` and `world`. The world key is the root-folder path relative to
the configured server root (`.` for the main save), with forward slashes. Roots
outside that directory use their configured path. No runtime world UUID is saved.
A vanilla record without this extension is bound once to its saved player
`Dimension` and the world root selected when loading the player. Explicit malformed
or unfamiliar origin records remain intact and cannot restore automatically.

Both chunk-vehicle matching and RootVehicle-only materialization require the source
world identity. A pending tree therefore remains recoverable when the player moves,
saves, or reconnects elsewhere. Fresh live snapshots bind to the vehicle's actual
world. This also distinguishes custom world roots sharing one dimension type.

Mounting an unrelated vehicle moves an unresolved primary record into the player
field `pumpkin:retained_root_vehicle`. That record can restore automatically when
the player returns to its source world. If another unresolved record displaces it,
the older record is appended to `pumpkin:vehicle_archive` as an opaque compound
`{record: <original tag>}`. Those older entries are inert and require manual recovery;
normal riding is never blocked by preservation. Failed/cancelled mounts do not
consume pending data. Unsupported and malformed retained records round-trip.

These Pumpkin extension fields are not a vanilla restoration feature. Initial
imports without provenance cannot disambiguate custom roots that vanilla player
Dimension alone did not identify. Moving an external root or renaming a custom root
requires adjusting its stored origin before automatic restoration.
