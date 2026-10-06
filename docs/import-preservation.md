# Import preservation

This branch adds safeguards for imported worlds and players. It remains experimental: passing these
storage tests does not establish normal survival compatibility or make a historical-world cutover safe.

## What the changes cover

- Unrecognized block identifiers and detected corruption in existing terrain, entity or POI data stop loading rather
  than being treated as absent chunks. Missing files can still generate normally. A failed chunk system
  can leave the process running; administrators must investigate its logged error before restarting.
- Recursive passenger trees keep saved UUIDs and mount relationships. Unsupported trees are retained
  as dormant records rather than discarded. Concurrent entity loads share a canonical chunk instance.
  Persistent endermites retain their saved age without normal despawn aging.
- Item stacks retain unsupported component payloads and unmanaged fields. Component removal and
  mutation remain authoritative, including nested inventory updates. Text byte values round-trip.
- Vanilla advancement and statistic files retain unknown identifiers and progress alongside recognized
  data. Player saves use atomic replacement and serialize reconnects with final disconnect saves.
  Invalid player input refuses login and cannot overwrite the retained file.
- The player NBT boundary checks complete gzip/NBT payloads and preserves compound/list wire structure;
  it rejects malformed runtime trees instead of publishing a truncated or silently reshaped file.

The native plugin ABI is **3**. Rebuild every native plugin against the exact server revision; libraries
compiled against the earlier ABI must not be reused.

## Import limits

The current protocol targets Minecraft 26.3, while the world/player data version remains 4903 (26.2).
Use official conversion on copies and verify the target format. Increasing a version constant alone is
not a world upgrade. Offline player conversion must cover inactive players too; a vanilla region upgrade
does not by itself update every player's saved inventory, advancements and statistics.

These safeguards do not complete the engine's import or survival behavior:

- Dimensions still share a seed; independent historical dimension seeds require a separate change.
- Frog eating/froglight production, kelp growth and structure-specific spawn overrides are incomplete.
- Imported hopper facing and modern villager profession/type/level need initialization fixes.
- Historical dragon-fight metadata and player RootVehicle-only entity restoration are not fully imported.
  Retaining an original RootVehicle payload is not the same as spawning and saving its current tree.
- Mounted AI/physics and farm operation require real-client tests.
- Entity chunk eviction/write ordering and dirty region flush failures remain unresolved. The canonical
  load fix does not establish reliable persistence through every unload/reload or IO failure.
- Generic world NBT serialization has remaining unchecked/reshaping paths outside the player boundary.
  Its reader can also accept some incomplete compounds, including truncated POI payloads. The strict
  player codec does not close every world-file corruption path.
- Supported mob and block-entity importers do not yet retain every unmanaged field. Keeping a dormant
  unsupported tree intact does not prove that a live supported entity round-trips all historical data.

Native plugin API compatibility is separate from Bukkit plugin compatibility. A rebuilt PatchBukkit
bridge can address selected Java API problems, but each retained plugin still needs runtime verification.
BlueMap's standalone CLI can render converted files independently of the server; that alone does not
verify continuous updates, live-player overlays or every historical marker.

Keep an immutable original backup, an independently verified off-host copy and isolated conversion/test
worlds. Do not open a newer saved world in an older server or use a successfully rendered sample as proof
that the entire world can be migrated safely.
