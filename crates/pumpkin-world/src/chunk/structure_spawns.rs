use std::sync::Arc;

use pumpkin_data::structures::{
    BoundingBoxType, SpawnEntry, Structure, StructureKeys, TerrainAdaptation,
};
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_nbt::tag::NbtTag;
use pumpkin_util::math::{block_box::BlockBox, position::BlockPos, vector2::Vector2};

use crate::{
    chunk::{ChunkData, io::FileIO},
    level::Level,
};

const GENERATED_STRUCTURE_SPAWNS: &str = "structure_spawn_geometry";

/// Persist Pumpkin's generated spawn geometry without inventing vanilla piece payloads.
#[must_use]
pub fn generated_spawn_data(starts: &[StructureSpawnStart]) -> NbtCompound {
    let mut geometry = NbtCompound::new();
    for start in starts {
        let mut nbt = NbtCompound::new();
        nbt.put_string("id", start.key.to_name().to_owned());
        nbt.put(
            "Children",
            NbtTag::List(
                start
                    .pieces
                    .iter()
                    .map(|piece| {
                        let mut nbt = NbtCompound::new();
                        nbt.put(
                            "BB",
                            NbtTag::IntArray(vec![
                                piece.min.x,
                                piece.min.y,
                                piece.min.z,
                                piece.max.x,
                                piece.max.y,
                                piece.max.z,
                            ]),
                        );
                        NbtTag::Compound(nbt)
                    })
                    .collect(),
            ),
        );
        geometry.put(start.key.to_name(), NbtTag::Compound(nbt));
    }
    let mut custom = NbtCompound::new();
    custom.put(GENERATED_STRUCTURE_SPAWNS, NbtTag::Compound(geometry));
    custom
}

/// Transient geometry for natural spawn overrides, independent of serialized structure pieces.
#[derive(Default)]
pub enum StructureSpawnState {
    #[default]
    Pending,
    Loading,
    Ready(Vec<StructureSpawnStart>),
    Failed,
}

pub struct StructureSpawnStart {
    pub key: StructureKeys,
    pub bounding_box: BlockBox,
    pub pieces: Vec<BlockBox>,
}

impl StructureSpawnStart {
    #[must_use]
    pub fn from_pieces(key: StructureKeys, pieces: Vec<BlockBox>) -> Option<Self> {
        let mut bounding_box = BlockBox::encompass_all(pieces.iter().copied())?;
        // Structure.adjustBoundingBox expands terrain-adapted starts by twelve blocks.
        if Structure::get(&key).terrain_adaptation != TerrainAdaptation::None {
            bounding_box = BlockBox::new(
                bounding_box.min.x.saturating_sub(12),
                bounding_box.min.y.saturating_sub(12),
                bounding_box.min.z.saturating_sub(12),
                bounding_box.max.x.saturating_add(12),
                bounding_box.max.y.saturating_add(12),
                bounding_box.max.z.saturating_add(12),
            );
        }
        Some(Self {
            key,
            bounding_box,
            pieces,
        })
    }

    fn load(key: StructureKeys, nbt: &NbtCompound) -> Result<Self, String> {
        if nbt.get_string("id").and_then(StructureKeys::from_name) != Some(key) {
            return Err("Missing or mismatched structure start id".into());
        }
        let children = nbt
            .get_list("Children")
            .ok_or("Missing structure Children")?;
        if children.len() > 4096 {
            return Err("Too many structure pieces".into());
        }
        let pieces = children
            .iter()
            .map(|child| {
                let child = child.extract_compound().ok_or("Invalid structure child")?;
                let &[x0, y0, z0, x1, y1, z1] =
                    child.get_int_array("BB").ok_or("Missing piece BB")?
                else {
                    return Err("Invalid piece BB");
                };
                if x0 > x1 || y0 > y1 || z0 > z1 {
                    return Err("Inverted piece BB");
                }
                Ok(BlockBox::new(x0, y0, z0, x1, y1, z1))
            })
            .collect::<Result<Vec<_>, &str>>()?;
        Self::from_pieces(key, pieces).ok_or_else(|| "Empty structure start".into())
    }
}

/// `NaturalSpawner`'s nether-brick fortress check precedes `ChunkGenerator`'s overrides.
#[must_use]
pub fn spawn_override(
    starts: &[StructureSpawnStart],
    category: &str,
    pos: &BlockPos,
    nether_bricks_below: bool,
) -> Option<&'static [SpawnEntry]> {
    if category == "monster"
        && nether_bricks_below
        && starts.iter().any(|start| {
            start.key == StructureKeys::Fortress
                && start.bounding_box.contains(pos.0.x, pos.0.y, pos.0.z)
        })
    {
        return Structure::FORTRESS
            .spawn_overrides
            .iter()
            .find(|entry| entry.category == category)
            .map(|entry| entry.spawns);
    }
    starts.iter().find_map(|start| {
        let entry = Structure::get(&start.key)
            .spawn_overrides
            .iter()
            .find(|entry| entry.category == category)?;
        let inside = match entry.bounding_box {
            BoundingBoxType::Full => start.bounding_box.contains(pos.0.x, pos.0.y, pos.0.z),
            BoundingBoxType::Piece => start
                .pieces
                .iter()
                .any(|piece| piece.contains(pos.0.x, pos.0.y, pos.0.z)),
        };
        inside.then_some(entry.spawns)
    })
}

fn references(chunk: &ChunkData) -> Result<Vec<(StructureKeys, Vector2<i32>)>, String> {
    let root = chunk
        .preserved_tags
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(structures) = root.get_compound("structures") else {
        if root.get("structures").is_some() {
            return Err("Invalid structures tag".into());
        }
        return Ok(Vec::new());
    };
    let references = structures
        .get_compound("References")
        .ok_or("Missing structure References")?;
    let mut result = Vec::new();
    for (name, tag) in &references.child_tags {
        let key = StructureKeys::from_name(name)
            .ok_or_else(|| format!("Unknown referenced structure {name}"))?;
        if Structure::get(&key).spawn_overrides.is_empty() {
            continue;
        }
        let positions = tag
            .extract_long_array()
            .ok_or("Invalid structure references")?;
        if positions.len() > 17 * 17 {
            return Err("Too many structure references".into());
        }
        for &packed in positions {
            let pos = Vector2::new(packed as i32, (packed >> 32) as i32);
            // Vanilla rejects references outside its eight-chunk lookup radius.
            if (i64::from(pos.x) - i64::from(chunk.x)).abs() > 8
                || (i64::from(pos.y) - i64::from(chunk.z)).abs() > 8
            {
                return Err("Structure reference outside lookup radius".into());
            }
            result.push((key, pos));
        }
    }
    Ok(result)
}

fn start_from_chunk(chunk: &ChunkData, key: StructureKeys) -> Result<StructureSpawnStart, String> {
    let root = chunk
        .preserved_tags
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let starts = root
        .get_compound("structures")
        .and_then(|s| s.get_compound("starts"))
        .ok_or("Missing structure starts")?;
    let start = starts
        .child_tags
        .iter()
        .find_map(|(name, tag)| {
            (StructureKeys::from_name(name) == Some(key))
                .then(|| tag.extract_compound())
                .flatten()
        })
        .ok_or("Missing referenced structure start")?;
    StructureSpawnStart::load(key, start)
}

impl Level {
    /// Resolve imported structure starts off the tick thread, including unloaded start chunks.
    pub fn prepare_structure_spawns(self: &Arc<Self>, chunk: &Arc<ChunkData>) -> bool {
        {
            let mut state = chunk
                .structure_spawns
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &*state {
                StructureSpawnState::Ready(_) => return true,
                StructureSpawnState::Loading | StructureSpawnState::Failed => return false,
                StructureSpawnState::Pending => *state = StructureSpawnState::Loading,
            }
        }
        let level = self.clone();
        let chunk = chunk.clone();
        self.spawn_task(async move {
            let result = level.load_structure_spawns(&chunk).await;
            let mut state = chunk
                .structure_spawns
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *state = match result {
                Ok(starts) => StructureSpawnState::Ready(starts),
                Err(cause) => {
                    tracing::error!(
                        "Cannot resolve structure spawns for chunk {},{}: {cause}",
                        chunk.x,
                        chunk.z
                    );
                    StructureSpawnState::Failed
                }
            };
        });
        false
    }

    async fn load_structure_spawns(
        &self,
        chunk: &Arc<ChunkData>,
    ) -> Result<Vec<StructureSpawnStart>, String> {
        {
            let custom = chunk
                .custom_data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(geometry) = custom.get_compound(GENERATED_STRUCTURE_SPAWNS) {
                return geometry
                    .child_tags
                    .iter()
                    .map(|(key, value)| {
                        let key =
                            StructureKeys::from_name(key).ok_or("Unknown generated structure")?;
                        StructureSpawnStart::load(
                            key,
                            value
                                .extract_compound()
                                .ok_or("Invalid generated structure")?,
                        )
                    })
                    .collect();
            }
            if custom.get(GENERATED_STRUCTURE_SPAWNS).is_some() {
                return Err("Invalid generated structure geometry".into());
            }
        }
        let mut starts = Vec::new();
        for (key, pos) in references(chunk)? {
            if pos == Vector2::new(chunk.x, chunk.z) {
                starts.push(start_from_chunk(chunk, key)?);
            } else if let Some(start) =
                self.read_chunk_sync(&pos, |start_chunk| start_from_chunk(start_chunk, key))
            {
                starts.push(start?);
            } else {
                // Read metadata without promoting the start chunk to a ticking/generated chunk.
                let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
                self.chunk_saver
                    .fetch_chunks(&self.level_folder, &[pos], sender)
                    .await;
                match receiver.recv().await {
                    Some(crate::chunk::io::LoadedData::Loaded(start_chunk)) => {
                        starts.push(start_from_chunk(&start_chunk, key)?);
                    }
                    Some(crate::chunk::io::LoadedData::Error((_, cause))) => {
                        return Err(format!(
                            "Cannot read referenced start at {},{}: {cause}",
                            pos.x, pos.y
                        ));
                    }
                    _ => {
                        return Err(format!(
                            "Cannot read referenced start at {},{}",
                            pos.x, pos.y
                        ));
                    }
                }
            }
        }
        Ok(starts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::format::anvil::SingleChunkDataSerializer;
    use pumpkin_config::world::LevelConfig;
    use pumpkin_data::dimension::Dimension;

    fn start_nbt(id: &str, boxes: &[[i32; 6]]) -> NbtCompound {
        let mut start = NbtCompound::new();
        start.put_string("id", id.to_owned());
        start.put_list(
            "Children",
            boxes
                .iter()
                .map(|bounds| {
                    let mut child = NbtCompound::new();
                    child.put("BB", NbtTag::IntArray(bounds.to_vec()));
                    NbtTag::Compound(child)
                })
                .collect(),
        );
        start
    }

    #[test]
    fn imported_piece_and_full_bounds_have_inclusive_limits() {
        let hut = StructureSpawnStart::load(
            StructureKeys::SwampHut,
            &start_nbt("minecraft:swamp_hut", &[[2, 64, 2, 8, 70, 10]]),
        )
        .unwrap();
        assert_eq!(
            spawn_override(&[hut], "monster", &BlockPos::new(8, 70, 10), false).unwrap()[0]
                .entity_type,
            "minecraft:witch"
        );
        let hut = StructureSpawnStart::load(
            StructureKeys::SwampHut,
            &start_nbt("minecraft:swamp_hut", &[[2, 64, 2, 8, 70, 10]]),
        )
        .unwrap();
        assert!(spawn_override(&[hut], "monster", &BlockPos::new(9, 70, 10), false).is_none());
        let monument = StructureSpawnStart::load(
            StructureKeys::Monument,
            &start_nbt(
                "minecraft:monument",
                &[[0, 39, 0, 4, 61, 4], [8, 39, 8, 12, 61, 12]],
            ),
        )
        .unwrap();
        let starts = [monument];
        assert_eq!(
            spawn_override(&starts, "monster", &BlockPos::new(6, 61, 6), false).unwrap()[0]
                .entity_type,
            "minecraft:guardian"
        );
        assert!(
            spawn_override(&starts, "axolotls", &BlockPos::new(6, 61, 6), false)
                .unwrap()
                .is_empty()
        );
        assert!(spawn_override(&starts, "monster", &BlockPos::new(6, 62, 6), false).is_none());
    }

    #[test]
    fn fortress_full_box_requires_nether_bricks_and_takes_precedence() {
        let fortress = StructureSpawnStart::load(
            StructureKeys::Fortress,
            &start_nbt(
                "minecraft:fortress",
                &[[0, 48, 0, 4, 70, 4], [8, 48, 8, 12, 70, 12]],
            ),
        )
        .unwrap();
        let starts = [fortress];
        let between_pieces = BlockPos::new(6, 55, 6);
        assert!(spawn_override(&starts, "monster", &between_pieces, false).is_none());
        assert!(
            spawn_override(&starts, "monster", &between_pieces, true)
                .unwrap()
                .iter()
                .any(|e| e.entity_type == "minecraft:wither_skeleton")
        );
        assert!(spawn_override(&starts, "creature", &between_pieces, true).is_none());
        assert!(spawn_override(&starts, "monster", &BlockPos::new(4, 70, 4), false).is_some());
        assert!(spawn_override(&starts, "monster", &BlockPos::new(13, 70, 4), true).is_none());
        let suppressed = StructureSpawnStart::load(
            StructureKeys::AncientCity,
            &start_nbt("minecraft:ancient_city", &[[0, 48, 0, 12, 70, 12]]),
        )
        .unwrap();
        assert!(
            !spawn_override(
                &[suppressed, starts.into_iter().next().unwrap()],
                "monster",
                &between_pieces,
                true
            )
            .unwrap()
            .is_empty()
        );
    }

    #[tokio::test]
    async fn imported_unloaded_start_resolves_from_disk_and_preserves_tags() {
        let directory = tempfile::TempDir::new().unwrap();
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            directory.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        )
        .unwrap();
        let start_chunk = Arc::new(ChunkData::empty(8, 0));
        start_chunk
            .dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut starts = NbtCompound::new();
        starts.put_compound(
            "minecraft:swamp_hut",
            start_nbt("minecraft:swamp_hut", &[[0, 64, 0, 6, 70, 8]]),
        );
        let mut structures = NbtCompound::new();
        structures.put_compound("starts", starts);
        structures.put_compound("References", NbtCompound::new());
        start_chunk
            .preserved_tags
            .lock()
            .unwrap()
            .put_compound("structures", structures);
        level
            .chunk_saver
            .save_chunks(&level.level_folder, vec![(Vector2::new(8, 0), start_chunk)])
            .await
            .unwrap();
        let chunk = Arc::new(ChunkData::empty(0, 0));
        let mut refs = NbtCompound::new();
        refs.put("minecraft:swamp_hut", NbtTag::LongArray(vec![8]));
        let mut structures = NbtCompound::new();
        structures.put_compound("References", refs);
        structures.put_compound("starts", NbtCompound::new());
        chunk
            .preserved_tags
            .lock()
            .unwrap()
            .put_compound("structures", structures);
        let original = chunk.preserved_tags.lock().unwrap().clone();
        let resolved = level.load_structure_spawns(&chunk).await.unwrap();
        assert_eq!(
            spawn_override(&resolved, "monster", &BlockPos::new(6, 70, 8), false).unwrap()[0]
                .entity_type,
            "minecraft:witch"
        );
        assert!(!level.is_chunk_loaded(&Vector2::new(8, 0)));
        assert_eq!(*chunk.preserved_tags.lock().unwrap(), original);
        let mut refs = NbtCompound::new();
        refs.put("minecraft:swamp_hut", NbtTag::LongArray(vec![7]));
        let mut missing = NbtCompound::new();
        missing.put_compound("References", refs);
        chunk
            .preserved_tags
            .lock()
            .unwrap()
            .put_compound("structures", missing);
        assert!(level.load_structure_spawns(&chunk).await.is_err());
        level.shutdown().await;
    }

    #[tokio::test]
    async fn generated_geometry_survives_chunk_serialization() {
        let directory = tempfile::TempDir::new().unwrap();
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            directory.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        )
        .unwrap();
        let chunk = ChunkData::empty(0, 0);
        let starts = [StructureSpawnStart::from_pieces(
            StructureKeys::SwampHut,
            vec![BlockBox::new(1, 64, 1, 7, 70, 9)],
        )
        .unwrap()];
        *chunk.custom_data.lock().unwrap() = generated_spawn_data(&starts);
        let bytes = chunk.to_bytes().unwrap();
        let reloaded =
            Arc::new(ChunkData::internal_from_bytes(&bytes, Vector2::new(0, 0)).unwrap());
        let resolved = level.load_structure_spawns(&reloaded).await.unwrap();
        assert_eq!(
            spawn_override(&resolved, "monster", &BlockPos::new(7, 70, 9), false).unwrap()[0]
                .entity_type,
            "minecraft:witch"
        );
        assert!(spawn_override(&resolved, "monster", &BlockPos::new(8, 70, 9), false).is_none());
        level.shutdown().await;
    }
}
