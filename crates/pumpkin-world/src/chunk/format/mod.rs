use std::{
    path::PathBuf,
    sync::{
        RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use bytes::Bytes;
use pumpkin_data::{Block, BlockStateId, chunk::ChunkStatus, fluid::Fluid};
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_util::resource_location::{FromResourceLocation, ToResourceLocation};
use rustc_hash::FxHashMap;

use crate::{
    chunk::{
        ChunkEntityData, ChunkReadingError, ChunkSerializingError,
        format::anvil::{SingleChunkDataSerializer, WORLD_DATA_VERSION},
        io::{Dirtiable, file_manager::PathFromLevelFolder},
    },
    generation::section_coords,
    level::LevelFolder,
    tick::{ScheduledTick, scheduler::ChunkTickScheduler},
};
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector2::Vector2;

use super::{
    ChunkData, ChunkHeightmaps, ChunkLight, ChunkParsingError, ChunkSections,
    palette::{BiomePalette, BlockPalette},
};
pub mod anvil;
pub mod linear;
pub mod pump;

/// Root tags Pumpkin rewrites from its in-memory state.
/// Other chunk metadata rides through a load/save cycle untouched, including
/// the original Bukkit values used as a fallback for Pumpkin custom data.
const MODELLED_ROOT_TAGS: &[&str] = &[
    "DataVersion",
    "xPos",
    "yPos",
    "zPos",
    "Status",
    "Heightmaps",
    "sections",
    "block_entities",
    "block_ticks",
    "fluid_ticks",
    "isLightOn",
    "InhabitedTime",
    "PumpkinCustomData",
];

fn preserved_root_tags(root_tag: &NbtCompound) -> NbtCompound {
    let mut preserved = NbtCompound::new();
    for (name, tag) in &root_tag.child_tags {
        // Starlight's version describes generated lighting caches, which Pumpkin
        // rebuilds without the corresponding section flags and sentinel sections.
        if !MODELLED_ROOT_TAGS.contains(&&**name) && &**name != "starlight.light_version" {
            preserved.put(name, tag.clone());
        }
    }
    preserved
}

impl SingleChunkDataSerializer for ChunkData {
    #[inline]
    fn from_bytes(bytes: &Bytes, pos: Vector2<i32>) -> Result<Self, ChunkReadingError> {
        Self::internal_from_bytes(bytes, pos).map_err(ChunkReadingError::ParsingError)
    }

    #[inline]
    fn to_bytes(&self) -> Result<Bytes, ChunkSerializingError> {
        self.internal_to_bytes()
    }

    #[inline]
    fn position(&self) -> (i32, i32) {
        (self.x, self.z)
    }
}

impl PathFromLevelFolder for ChunkData {
    #[inline]
    fn file_path(folder: &LevelFolder, file_name: &str) -> PathBuf {
        folder.region_folder.join(file_name)
    }
}

impl Dirtiable for ChunkData {
    #[inline]
    fn mark_dirty(&self, flag: bool) {
        self.dirty.store(flag, Ordering::Relaxed);
    }

    #[inline]
    fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }
}

/// The section stores `Y` as a byte, short, int or long depending on who wrote
/// the file. The datafixer writes ints. Reading only bytes would map every int
/// section to `Y = 0`, and they would overwrite each other.
fn section_y(section: &NbtCompound) -> i32 {
    use pumpkin_nbt::tag::NbtTag;

    match section.get("Y") {
        Some(NbtTag::Byte(value)) => i32::from(*value),
        Some(NbtTag::Short(value)) => i32::from(*value),
        Some(NbtTag::Int(value)) => *value,
        Some(NbtTag::Long(value)) => i32::try_from(*value).unwrap_or(0),
        _ => 0,
    }
}

/// What vanilla does when `yPos` is missing, for example after it upgrades a
/// world in place: the lowest section that stores biomes, and never above 0.
/// That gives `-4` in the Overworld and `0` in the Nether and End, which are
/// the section minimums of those dimensions.
fn lowest_biome_section_y(root_tag: &NbtCompound) -> Option<i32> {
    let sections = root_tag.get_list("sections")?;
    sections
        .iter()
        .filter_map(|tag| match tag {
            pumpkin_nbt::tag::NbtTag::Compound(compound) if compound.has("biomes") => {
                Some(section_y(compound))
            }
            _ => None,
        })
        .min()
        .map(|y| y.min(0))
}

fn extract_u16_array(
    tag: &pumpkin_nbt::tag::NbtTag,
    position: Vector2<i32>,
    section_y: i32,
) -> Result<Option<Box<[BlockStateId]>>, ChunkParsingError> {
    use pumpkin_nbt::tag::NbtTag;

    let palette = match tag {
        NbtTag::IntArray(arr) => arr
            .iter()
            .map(|&x| BlockStateId::new_or_air(x as u16))
            .collect(),
        NbtTag::ByteArray(arr) => arr
            .iter()
            .map(|&x| BlockStateId::new_or_air(x as u16))
            .collect(),
        NbtTag::LongArray(arr) => arr
            .iter()
            .map(|&x| BlockStateId::new_or_air(x as u16))
            .collect(),
        NbtTag::List(list) => list
            .iter()
            .enumerate()
            .map(|(palette_index, tag)| {
                let invalid = |cause| ChunkParsingError::InvalidBlockPalette {
                    position,
                    section_y,
                    palette_index,
                    cause,
                };
                Ok(match tag {
                    NbtTag::Int(x) => BlockStateId::new_or_air(*x as u16),
                    NbtTag::Short(x) => BlockStateId::new_or_air(*x as u16),
                    NbtTag::Byte(x) => BlockStateId::new_or_air(*x as u16),
                    NbtTag::Long(x) => BlockStateId::new_or_air(*x as u16),
                    NbtTag::Compound(compound) => {
                        let entry = crate::generation::structure::template::PaletteEntry::from_nbt_compound(compound)
                            .map_err(|err| invalid(err.to_string()))?;
                        crate::generation::structure::template::BlockStateResolver::resolve_simple(&entry)
                            .ok_or_else(|| invalid(format!("Unknown block {}", entry.name)))?
                            .id
                    }
                    _ => return Err(invalid("Unsupported block palette entry".to_string())),
                })
            })
            .collect::<Result<Box<[_]>, ChunkParsingError>>()?,
        _ => return Ok(None),
    };
    Ok(Some(palette))
}

fn extract_u8_array(tag: &pumpkin_nbt::tag::NbtTag) -> Option<Box<[u8]>> {
    match tag {
        pumpkin_nbt::tag::NbtTag::ByteArray(arr) => Some(arr.iter().map(|&x| x as u8).collect()),
        pumpkin_nbt::tag::NbtTag::IntArray(arr) => Some(arr.iter().map(|&x| x as u8).collect()),
        pumpkin_nbt::tag::NbtTag::List(list) => {
            let bytes: Box<[u8]> = list
                .iter()
                .map(|t| match t {
                    pumpkin_nbt::tag::NbtTag::Byte(x) => *x as u8,
                    pumpkin_nbt::tag::NbtTag::Int(x) => *x as u8,
                    pumpkin_nbt::tag::NbtTag::Short(x) => *x as u8,
                    pumpkin_nbt::tag::NbtTag::String(s) => {
                        let name = s.strip_prefix("minecraft:").unwrap_or(s);
                        pumpkin_data::biome::Biome::from_name(name).map_or(0, |b| b.id)
                    }
                    _ => 0,
                })
                .collect();
            Some(bytes)
        }
        _ => None,
    }
}

fn parse_scheduled_ticks<T>(
    root: &NbtCompound,
    name: &str,
) -> Result<Vec<ScheduledTick<T>>, ChunkParsingError>
where
    T: FromResourceLocation,
{
    let Some(tag) = root.get(name) else {
        return Ok(Vec::new());
    };
    let pumpkin_nbt::tag::NbtTag::List(list) = tag else {
        return Err(ChunkParsingError::ErrorDeserializingChunk(format!(
            "{name} must be a list"
        )));
    };
    list.iter()
        .enumerate()
        .map(|(index, tag)| {
            let tick = match tag {
                pumpkin_nbt::tag::NbtTag::Compound(compound) => {
                    ScheduledTick::from_nbt_compound(compound)
                }
                _ => None,
            };
            tick.ok_or_else(|| {
                ChunkParsingError::ErrorDeserializingChunk(format!(
                    "Invalid {name}[{index}]: expected a known tick type and integer x/y/z/t/p"
                ))
            })
        })
        .collect()
}

impl ChunkData {
    #[allow(clippy::too_many_lines)]
    pub fn internal_from_bytes(
        chunk_data: &[u8],
        position: Vector2<i32>,
    ) -> Result<Self, ChunkParsingError> {
        let is_named = chunk_data.len() >= 3
            && chunk_data[0] == 0x0a
            && chunk_data[1] == 0x00
            && chunk_data[2] == 0x00;

        let mut cursor = std::io::Cursor::new(chunk_data);
        let mut reader = pumpkin_nbt::deserializer::NbtReadHelperJava::new_preserving(&mut cursor);
        let nbt = if is_named {
            pumpkin_nbt::Nbt::read_complete(&mut reader)
        } else {
            pumpkin_nbt::Nbt::read_unnamed_complete(&mut reader)
        }
        .map_err(|e| ChunkParsingError::ErrorDeserializingChunk(e.to_string()))?;

        let root_tag = nbt.root_tag;

        let x_pos = root_tag.get_int("xPos").ok_or_else(|| {
            ChunkParsingError::ErrorDeserializingChunk("Missing xPos".to_string())
        })?;
        let z_pos = root_tag.get_int("zPos").ok_or_else(|| {
            ChunkParsingError::ErrorDeserializingChunk("Missing zPos".to_string())
        })?;

        if x_pos != position.x || z_pos != position.y {
            return Err(ChunkParsingError::ErrorDeserializingChunk(format!(
                "Expected data for chunk {},{} but got it for {},{}!",
                position.x, position.y, x_pos, z_pos,
            )));
        }

        // Vanilla omits yPos when it upgrades a world in place. It uses the
        // dimension minimum for such chunks, which is the lowest section that
        // stores biomes. Do the same instead of rejecting the chunk.
        let min_y_section = match root_tag.get_int("yPos") {
            Some(y_pos) => y_pos,
            None => lowest_biome_section_y(&root_tag).ok_or_else(|| {
                ChunkParsingError::ErrorDeserializingChunk("Missing yPos".to_string())
            })?,
        };

        let mut max_y_section = min_y_section as i8;
        if let Some(sections_list) = root_tag.get_list("sections") {
            for section_tag in sections_list {
                if let pumpkin_nbt::tag::NbtTag::Compound(section_compound) = section_tag {
                    let y = section_y(section_compound) as i8;
                    if y > max_y_section {
                        max_y_section = y;
                    }
                }
            }
        }

        let section_count = (max_y_section as i32 - min_y_section + 1).max(0) as usize;
        let mut block_lights = vec![LightContainer::Empty(0); section_count];
        let mut sky_lights = vec![LightContainer::Empty(0); section_count];
        let mut block_palettes = vec![BlockPalette::default(); section_count];
        let mut biome_palettes = vec![BiomePalette::default(); section_count];

        if let Some(sections_list) = root_tag.get_list("sections") {
            for section_tag in sections_list {
                if let pumpkin_nbt::tag::NbtTag::Compound(section_compound) = section_tag {
                    let y = section_y(section_compound);
                    let index = (y - min_y_section) as usize;
                    if index >= section_count {
                        continue;
                    }

                    let block_light = section_compound
                        .get("BlockLight")
                        .and_then(|tag| tag.extract_byte_array())
                        .map(|arr| {
                            // SAFETY: `arr` is an `i8` slice (`&[i8]`). `u8` and `i8` have identical memory layout, alignment (1 byte), and lifetime.
                            unsafe {
                                Box::from(std::slice::from_raw_parts(
                                    arr.as_ptr().cast::<u8>(),
                                    arr.len(),
                                ))
                            }
                        });

                    let sky_light = section_compound
                        .get("SkyLight")
                        .and_then(|tag| tag.extract_byte_array())
                        .map(|arr| {
                            // SAFETY: `arr` is an `i8` slice (`&[i8]`). `u8` and `i8` have identical memory layout, alignment (1 byte), and lifetime.
                            unsafe {
                                Box::from(std::slice::from_raw_parts(
                                    arr.as_ptr().cast::<u8>(),
                                    arr.len(),
                                ))
                            }
                        });

                    block_lights[index] =
                        block_light.map_or(LightContainer::Empty(0), LightContainer::Full);
                    sky_lights[index] =
                        sky_light.map_or(LightContainer::Empty(0), LightContainer::Full);

                    if let Some(bs_compound) = section_compound.get_compound("block_states") {
                        let data = bs_compound
                            .get_long_array("data")
                            .map(|arr| arr.to_vec().into_boxed_slice());
                        let palette = bs_compound
                            .get("palette")
                            .map(|tag| {
                                extract_u16_array(tag, position, section_y(section_compound))
                            })
                            .transpose()?
                            .flatten()
                            .unwrap_or_else(|| vec![BlockStateId::AIR].into_boxed_slice());

                        block_palettes[index] =
                            BlockPalette::from_disk_nbt(ChunkSectionBlockStates { data, palette });
                    } else {
                        block_palettes[index] = BlockPalette::default();
                    }

                    if let Some(b_compound) = section_compound.get_compound("biomes") {
                        let data = b_compound
                            .get_long_array("data")
                            .map(|arr| arr.to_vec().into_boxed_slice());
                        let palette = b_compound
                            .get("palette")
                            .and_then(extract_u8_array)
                            .unwrap_or_else(|| vec![0].into_boxed_slice());

                        biome_palettes[index] =
                            BiomePalette::from_disk_nbt(ChunkSectionBiomes { data, palette });
                    } else {
                        biome_palettes[index] = BiomePalette::default();
                    }
                }
            }
        }

        // Assemble the LightEngine
        let light_engine = ChunkLight {
            block_light: block_lights.into_boxed_slice(),
            sky_light: sky_lights.into_boxed_slice(),
        };

        // Assemble the ChunkSections
        let min_y = section_coords::section_to_block(min_y_section);
        let (random_tick_sections, randomly_ticking_mask) =
            ChunkSections::build_random_tick_sections_cache(&block_palettes);
        let section = ChunkSections {
            count: block_palettes.len(),
            block_sections: RwLock::new(block_palettes.into_boxed_slice()),
            random_tick_sections: RwLock::new(random_tick_sections),
            randomly_ticking_mask: std::sync::atomic::AtomicU32::new(randomly_ticking_mask),
            biome_sections: RwLock::new(biome_palettes.into_boxed_slice()),
            min_y,
        };

        let heightmaps = root_tag.get_compound("Heightmaps").map_or(
            ChunkHeightmaps {
                world_surface: None,
                motion_blocking: None,
                motion_blocking_no_leaves: None,
            },
            |h_compound| ChunkHeightmaps {
                world_surface: h_compound
                    .get_long_array("WORLD_SURFACE")
                    .map(|a| a.to_vec().into_boxed_slice()),
                motion_blocking: h_compound
                    .get_long_array("MOTION_BLOCKING")
                    .map(|a| a.to_vec().into_boxed_slice()),
                motion_blocking_no_leaves: h_compound
                    .get_long_array("MOTION_BLOCKING_NO_LEAVES")
                    .map(|a| a.to_vec().into_boxed_slice()),
            },
        );
        let block_ticks = parse_scheduled_ticks::<&'static Block>(&root_tag, "block_ticks")?;
        let fluid_ticks = parse_scheduled_ticks::<&'static Fluid>(&root_tag, "fluid_ticks")?;

        let mut block_entities = FxHashMap::default();
        if let Some(tag) = root_tag.get("block_entities") {
            let invalid =
                |message: &str| ChunkParsingError::ErrorDeserializingChunk(message.to_owned());
            let list = tag
                .extract_list()
                .ok_or_else(|| invalid("Invalid block_entities list"))?;
            for tag in list {
                let nbt = tag
                    .extract_compound()
                    .ok_or_else(|| invalid("Invalid block entity record"))?;
                let (Some(x), Some(y), Some(z)) =
                    (nbt.get_int("x"), nbt.get_int("y"), nbt.get_int("z"))
                else {
                    return Err(invalid("Missing or invalid block entity position"));
                };
                if block_entities
                    .insert(BlockPos::new(x, y, z), nbt.clone())
                    .is_some()
                {
                    return Err(invalid("Duplicate block entity position"));
                }
            }
        }

        let light_correct = root_tag.get_bool("isLightOn").unwrap_or(false);

        let status_str = root_tag.get_string("Status").unwrap_or("minecraft:empty");
        let status = match status_str {
            "minecraft:structure_starts" => ChunkStatus::StructureStarts,
            "minecraft:structure_references" => ChunkStatus::StructureReferences,
            "minecraft:biomes" => ChunkStatus::Biomes,
            "minecraft:terrain" | "minecraft:noise" | "minecraft:surface" | "minecraft:carvers" => {
                ChunkStatus::Terrain
            }
            "minecraft:features" => ChunkStatus::Features,
            "minecraft:initialize_light" => ChunkStatus::InitializeLight,
            "minecraft:light" => ChunkStatus::Light,
            "minecraft:spawn" => ChunkStatus::Spawn,
            "minecraft:full" => ChunkStatus::Full,
            _ => ChunkStatus::Empty,
        };

        let custom_data = root_tag
            .get_compound("PumpkinCustomData")
            .or_else(|| root_tag.get_compound("BukkitValues"))
            .cloned()
            .unwrap_or_default();

        Ok(Self {
            section,
            heightmap: std::sync::Mutex::new(heightmaps),
            x: position.x,
            z: position.y,
            // This chunk is read from disk, so it has not been modified
            dirty: AtomicBool::new(false),
            block_ticks: ChunkTickScheduler::from_iter(block_ticks),
            fluid_ticks: ChunkTickScheduler::from_iter(fluid_ticks),
            pending_block_entities: std::sync::Mutex::new(block_entities),
            light_engine: std::sync::Mutex::new(light_engine),
            light_populated: AtomicBool::new(light_correct),
            status,
            blending_data: None,
            inhabited_time: AtomicU64::new(root_tag.get_long("InhabitedTime").unwrap_or(0) as u64),
            custom_data: std::sync::Mutex::new(custom_data),
            preserved_tags: std::sync::Mutex::new(preserved_root_tags(&root_tag)),
            structure_spawns: std::sync::Mutex::default(),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn internal_to_bytes(&self) -> Result<Bytes, ChunkSerializingError> {
        use pumpkin_nbt::tag::NbtTag;

        fn extract_light_ref(light: Option<&LightContainer>) -> Option<&[u8]> {
            match light {
                Some(LightContainer::Full(data)) => Some(data.as_ref()),
                _ => None,
            }
        }

        let is_light_correct = self
            .light_populated
            .load(std::sync::atomic::Ordering::Relaxed);

        let block_entities_nbt = {
            let entities_guard = self
                .pending_block_entities
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entities_guard.values().cloned().collect::<Vec<_>>()
        };

        let light_lock = self
            .light_engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let heightmap_lock = self
            .heightmap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let block_lock = self
            .section
            .block_sections
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let biome_lock = self
            .section
            .biome_sections
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let min_section_y = (self.section.min_y >> 4) as i8;

        let mut root_compound = NbtCompound::new();

        // First, so that every tag this server does model overwrites whatever
        // the chunk arrived with. Going the other way round would let a stale
        // value survive any name missing from `MODELLED_ROOT_TAGS`, which is a
        // worse failure than the dropped tags this exists to prevent.
        {
            let preserved = self
                .preserved_tags
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (name, tag) in &preserved.child_tags {
                root_compound.put(name, tag.clone());
            }
        }

        root_compound.put_int("DataVersion", WORLD_DATA_VERSION);
        root_compound.put_int("xPos", self.x);
        root_compound.put_int("zPos", self.z);
        root_compound.put_int("yPos", section_coords::block_to_section(self.section.min_y));

        let status_str = match self.status {
            ChunkStatus::Empty => "minecraft:empty",
            ChunkStatus::StructureStarts => "minecraft:structure_starts",
            ChunkStatus::StructureReferences => "minecraft:structure_references",
            ChunkStatus::Biomes => "minecraft:biomes",
            ChunkStatus::Terrain => "minecraft:terrain",
            ChunkStatus::Features => "minecraft:features",
            ChunkStatus::InitializeLight => "minecraft:initialize_light",
            ChunkStatus::Light => "minecraft:light",
            ChunkStatus::Spawn => "minecraft:spawn",
            ChunkStatus::Full => "minecraft:full",
        };
        root_compound.put_string("Status", status_str.to_string());

        let mut heightmaps_compound = NbtCompound::new();
        if let Some(ref arr) = heightmap_lock.world_surface {
            heightmaps_compound.put("WORLD_SURFACE", NbtTag::LongArray(arr.to_vec()));
        }
        if let Some(ref arr) = heightmap_lock.motion_blocking {
            heightmaps_compound.put("MOTION_BLOCKING", NbtTag::LongArray(arr.to_vec()));
        }
        if let Some(ref arr) = heightmap_lock.motion_blocking_no_leaves {
            heightmaps_compound.put("MOTION_BLOCKING_NO_LEAVES", NbtTag::LongArray(arr.to_vec()));
        }
        root_compound.put_compound("Heightmaps", heightmaps_compound);

        let mut sections_list = Vec::new();
        for i in 0..self.section.count {
            let mut section_comp = NbtCompound::new();
            let y_val = i as i8 + min_section_y;
            section_comp.put_byte("Y", y_val);

            // block_states
            let block_states_nbt = block_lock[i].to_disk_nbt();
            let mut bs_comp = NbtCompound::new();
            if let Some(ref data_arr) = block_states_nbt.data {
                bs_comp.put("data", NbtTag::LongArray(data_arr.to_vec()));
            }
            let palette_tags: Vec<NbtTag> = block_states_nbt
                .palette
                .iter()
                .map(|&id| {
                    NbtTag::Compound(
                        crate::generation::structure::template::PaletteEntry::from_block_state(id)
                            .to_nbt_compound(),
                    )
                })
                .collect();
            bs_comp.put_list("palette", palette_tags);
            section_comp.put_compound("block_states", bs_comp);

            // biomes
            let biomes_nbt = biome_lock[i].to_disk_nbt();
            let mut b_comp = NbtCompound::new();
            if let Some(ref data_arr) = biomes_nbt.data {
                b_comp.put("data", NbtTag::LongArray(data_arr.to_vec()));
            }
            let biome_palette_tags: Vec<NbtTag> = biomes_nbt
                .palette
                .iter()
                .map(|&val| {
                    let name = pumpkin_data::biome::Biome::from_id(val)
                        .map_or("plains", |b| b.registry_id);
                    let full_name = if name.starts_with("minecraft:") {
                        name.to_string()
                    } else {
                        format!("minecraft:{name}")
                    };
                    NbtTag::String(full_name.into())
                })
                .collect();
            b_comp.put_list("palette", biome_palette_tags);
            section_comp.put_compound("biomes", b_comp);

            // block_light
            if let Some(light_data) = extract_light_ref(light_lock.block_light.get(i)) {
                let bytes: Box<[i8]> = light_data.iter().map(|&x| x as i8).collect();
                section_comp.put("BlockLight", NbtTag::ByteArray(bytes));
            }

            // sky_light
            if let Some(light_data) = extract_light_ref(light_lock.sky_light.get(i)) {
                let bytes: Box<[i8]> = light_data.iter().map(|&x| x as i8).collect();
                section_comp.put("SkyLight", NbtTag::ByteArray(bytes));
            }

            sections_list.push(NbtTag::Compound(section_comp));
        }
        root_compound.put_list("sections", sections_list);

        let mut block_ticks_list = Vec::new();
        for tick in self.block_ticks.to_vec() {
            let mut tick_comp = NbtCompound::new();
            tick_comp.put_int("x", tick.position.0.x);
            tick_comp.put_int("y", tick.position.0.y);
            tick_comp.put_int("z", tick.position.0.z);
            tick_comp.put_int("t", tick.delay);
            tick_comp.put_int("p", tick.priority as i32);
            tick_comp.put_string("i", tick.value.to_resource_location());
            block_ticks_list.push(NbtTag::Compound(tick_comp));
        }
        root_compound.put_list("block_ticks", block_ticks_list);

        let mut fluid_ticks_list = Vec::new();
        for tick in self.fluid_ticks.to_vec() {
            let mut tick_comp = NbtCompound::new();
            tick_comp.put_int("x", tick.position.0.x);
            tick_comp.put_int("y", tick.position.0.y);
            tick_comp.put_int("z", tick.position.0.z);
            tick_comp.put_int("t", tick.delay);
            tick_comp.put_int("p", tick.priority as i32);
            tick_comp.put_string("i", tick.value.to_resource_location());
            fluid_ticks_list.push(NbtTag::Compound(tick_comp));
        }
        root_compound.put_list("fluid_ticks", fluid_ticks_list);

        let mut block_entities_list = Vec::new();
        for entity_comp in block_entities_nbt {
            block_entities_list.push(NbtTag::Compound(entity_comp));
        }
        root_compound.put_list("block_entities", block_entities_list);

        root_compound.put_bool("isLightOn", is_light_correct);
        root_compound.put_long(
            "InhabitedTime",
            self.inhabited_time.load(Ordering::Relaxed) as i64,
        );

        let custom_data = self
            .custom_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // An empty Pumpkin tag prevents the legacy Bukkit fallback from
        // restoring custom data that Pumpkin has explicitly removed.
        if !custom_data.is_empty() || root_compound.has("BukkitValues") {
            root_compound.put_compound("PumpkinCustomData", custom_data.clone());
        }

        let nbt = pumpkin_nbt::Nbt::from(root_compound);
        nbt.try_write_preserving()
            .map_err(ChunkSerializingError::ErrorSerializingChunk)
    }

    pub fn set_custom_data(&self, namespace: &str, key: &str, value: pumpkin_nbt::tag::NbtTag) {
        let mut custom_data = self
            .custom_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut namespace_data = custom_data
            .child_tags
            .remove(namespace)
            .and_then(|tag| match tag {
                pumpkin_nbt::tag::NbtTag::Compound(compound) => Some(compound),
                _ => None,
            })
            .unwrap_or_default();

        namespace_data.child_tags.insert(key.into(), value);
        custom_data.child_tags.insert(
            namespace.into(),
            pumpkin_nbt::tag::NbtTag::Compound(namespace_data),
        );
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn get_custom_data(&self, namespace: &str, key: &str) -> Option<pumpkin_nbt::tag::NbtTag> {
        let custom_data = self
            .custom_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        custom_data
            .get(namespace)?
            .extract_compound()?
            .get(key)
            .cloned()
    }

    pub fn remove_custom_data(&self, namespace: &str, key: &str) {
        let mut custom_data = self
            .custom_data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let Some(pumpkin_nbt::tag::NbtTag::Compound(mut namespace_data)) =
            custom_data.child_tags.remove(namespace)
        else {
            return;
        };

        namespace_data.child_tags.remove(key);
        if !namespace_data.is_empty() {
            custom_data.child_tags.insert(
                namespace.into(),
                pumpkin_nbt::tag::NbtTag::Compound(namespace_data),
            );
        }
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn has_custom_data(&self, namespace: &str, key: &str) -> bool {
        self.get_custom_data(namespace, key).is_some()
    }
}

impl PathFromLevelFolder for ChunkEntityData {
    #[inline]
    fn file_path(folder: &LevelFolder, file_name: &str) -> PathBuf {
        folder.entities_folder.join(file_name)
    }
}

impl Dirtiable for ChunkEntityData {
    #[inline]
    fn mark_dirty(&self, flag: bool) {
        self.dirty.store(flag, Ordering::Relaxed);
    }

    #[inline]
    fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Relaxed)
    }

    fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }
}

impl SingleChunkDataSerializer for ChunkEntityData {
    #[inline]
    fn from_bytes(bytes: &Bytes, pos: Vector2<i32>) -> Result<Self, ChunkReadingError> {
        Self::internal_from_bytes(bytes, pos).map_err(ChunkReadingError::ParsingError)
    }

    #[inline]
    fn to_bytes(&self) -> Result<Bytes, ChunkSerializingError> {
        self.internal_to_bytes()
    }

    #[inline]
    fn position(&self) -> (i32, i32) {
        (self.x, self.z)
    }
}

impl ChunkEntityData {
    fn internal_from_bytes(
        chunk_data: &[u8],
        position: Vector2<i32>,
    ) -> Result<Self, ChunkParsingError> {
        let is_named = chunk_data.len() >= 3
            && chunk_data[0] == 0x0a
            && chunk_data[1] == 0x00
            && chunk_data[2] == 0x00;
        let mut cursor = std::io::Cursor::new(chunk_data);
        let mut reader = pumpkin_nbt::deserializer::NbtReadHelperJava::new_preserving(
            pumpkin_nbt::deserializer::NbtStreamReader(&mut cursor),
        );
        let nbt = if is_named {
            pumpkin_nbt::Nbt::read_complete(&mut reader)
        } else {
            pumpkin_nbt::Nbt::read_unnamed_complete(&mut reader)
        }
        .map_err(|e| ChunkParsingError::ErrorDeserializingChunk(e.to_string()))?;

        let invalid =
            |message: &str| ChunkParsingError::ErrorDeserializingChunk(message.to_owned());
        let pos_array = if let Some(position_tag) = nbt.get("Position") {
            let Some(&[x, z]) = position_tag.extract_int_array() else {
                return Err(invalid("Invalid entity chunk Position"));
            };
            [x, z]
        } else {
            match (nbt.get_int("Position-X"), nbt.get_int("Position-Z")) {
                (Some(x), Some(z)) => [x, z],
                _ => return Err(invalid("Missing entity chunk Position")),
            }
        };

        if pos_array[0] != position.x || pos_array[1] != position.y {
            return Err(ChunkParsingError::ErrorDeserializingChunk(format!(
                "Expected data for entity chunk {},{} but got it for {},{}!",
                position.x, position.y, pos_array[0], pos_array[1],
            )));
        }

        let entities = match nbt.get("Entities") {
            Some(pumpkin_nbt::tag::NbtTag::List(list)) => list
                .iter()
                .map(|tag| {
                    tag.extract_compound()
                        .cloned()
                        .ok_or_else(|| invalid("Invalid entity chunk record"))
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(invalid("Invalid entity chunk Entities list")),
            None => Vec::new(),
        };
        let mut preserved_tags = nbt.root_tag;
        for key in [
            "DataVersion",
            "Position",
            "Position-X",
            "Position-Z",
            "Entities",
        ] {
            preserved_tags.child_tags.remove(key);
        }

        Ok(Self {
            x: position.x,
            z: position.y,
            data: std::sync::Mutex::new(entities),
            preserved_tags: std::sync::Mutex::new(preserved_tags),
            live: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
        })
    }

    fn internal_to_bytes(&self) -> Result<Bytes, ChunkSerializingError> {
        let mut root = self
            .preserved_tags
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        root.put_int("DataVersion", WORLD_DATA_VERSION);
        root.put(
            "Position",
            pumpkin_nbt::tag::NbtTag::IntArray(vec![self.x, self.z]),
        );
        let entities_tag: Vec<pumpkin_nbt::tag::NbtTag> = self
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|c| pumpkin_nbt::tag::NbtTag::Compound(c.clone()))
            .collect();
        root.put_list("Entities", entities_tag);

        let nbt = pumpkin_nbt::Nbt::from(root);
        nbt.try_write_preserving()
            .map_err(ChunkSerializingError::ErrorSerializingChunk)
    }
}

#[derive(Clone)]
pub struct ChunkSectionBiomes {
    pub(crate) data: Option<Box<[i64]>>,
    pub(crate) palette: Box<[u8]>,
}

#[derive(Clone)]
pub struct ChunkSectionBlockStates {
    pub(crate) data: Option<Box<[i64]>>,
    pub(crate) palette: Box<[BlockStateId]>,
}

#[derive(Debug, Clone)]
pub enum LightContainer {
    Empty(u8),
    Full(Box<[u8]>),
}

impl LightContainer {
    pub const DIM: usize = 16;
    pub const ARRAY_SIZE: usize = Self::DIM * Self::DIM * Self::DIM / 2;

    #[must_use]
    pub fn new_empty(default: u8) -> Self {
        assert!(default <= 15, "Default value must be between 0 and 15");
        Self::Empty(default)
    }

    #[must_use]
    pub fn new(data: Box<[u8]>) -> Self {
        assert!(
            data.len() == Self::ARRAY_SIZE,
            "Data length must be {}",
            Self::ARRAY_SIZE
        );
        Self::Full(data)
    }

    #[must_use]
    pub fn new_filled(default: u8) -> Self {
        assert!(default <= 15, "Default value must be between 0 and 15");
        let value = default << 4 | default;
        Self::Full([value; Self::ARRAY_SIZE].into())
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        matches!(self, Self::Empty(_))
    }

    #[inline]
    const fn index(x: usize, y: usize, z: usize) -> usize {
        y * 16 * 16 + z * 16 + x
    }

    #[inline]
    #[must_use]
    pub fn get(&self, x: usize, y: usize, z: usize) -> u8 {
        match self {
            Self::Full(data) => {
                let index = Self::index(x, y, z);
                (data[index >> 1] >> (4 * (index & 1))) & 0x0F
            }
            Self::Empty(default) => *default,
        }
    }

    #[inline]
    pub fn set(&mut self, x: usize, y: usize, z: usize, value: u8) {
        match self {
            Self::Full(data) => {
                let index = Self::index(x, y, z);
                let shift = 4 * (index & 1);
                let mask = 0x0F << shift;
                data[index >> 1] = (data[index >> 1] & !mask) | (value << shift);
            }
            Self::Empty(default) => {
                if value != *default {
                    *self = Self::new_filled(*default);
                    self.set(x, y, z, value);
                }
            }
        }
    }

    #[inline]
    pub fn set_column_y_range(
        &mut self,
        x: usize,
        z: usize,
        y_start: usize,
        y_end: usize,
        value: u8,
    ) {
        if y_start >= y_end {
            return;
        }
        match self {
            Self::Full(data) => {
                let shift = 4 * (x & 1);
                let mask = 0x0F << shift;
                let val = (value & 0x0F) << shift;
                let mut byte_idx = (y_start * 256 + z * 16 + x) >> 1;
                for _ in y_start..y_end {
                    data[byte_idx] = (data[byte_idx] & !mask) | val;
                    byte_idx += 128;
                }
            }
            Self::Empty(default) => {
                if value != *default {
                    *self = Self::new_filled(*default);
                    self.set_column_y_range(x, z, y_start, y_end, value);
                }
            }
        }
    }

    #[inline]
    pub fn fill(&mut self, value: u8) {
        *self = Self::new_filled(value);
    }
}

impl Default for LightContainer {
    fn default() -> Self {
        Self::new_empty(15)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_data::Block;
    use pumpkin_nbt::compound::NbtCompound;
    use pumpkin_nbt::tag::NbtTag;

    fn entity_chunk_root(position: Vector2<i32>) -> NbtCompound {
        let mut root = NbtCompound::new();
        root.put_int("DataVersion", WORLD_DATA_VERSION);
        root.put("Position", NbtTag::IntArray(vec![position.x, position.y]));
        root.put_list("Entities", Vec::new());
        root
    }

    fn decode_preserving(bytes: &[u8]) -> pumpkin_nbt::Nbt {
        let mut reader = pumpkin_nbt::deserializer::NbtReadHelperJava::new_preserving(
            std::io::Cursor::new(bytes),
        );
        pumpkin_nbt::Nbt::read_complete(&mut reader).expect("valid complete saved NBT")
    }

    fn assert_complete_documents<T: SingleChunkDataSerializer>(mut root: NbtCompound) {
        root.put_int("duplicate", 1);
        for named in [true, false] {
            let nbt = pumpkin_nbt::Nbt::from(root.clone());
            let bytes = if named {
                nbt.try_write_preserving().unwrap()
            } else {
                nbt.try_write_unnamed_preserving().unwrap()
            };
            assert!(T::from_bytes(&bytes, Vector2::new(0, 0)).is_ok());
            let mut trailing = bytes.to_vec();
            trailing.push(42);
            assert!(T::from_bytes(&trailing.into(), Vector2::new(0, 0)).is_err());
            let truncated = Bytes::copy_from_slice(&bytes[..bytes.len() - 1]);
            assert!(T::from_bytes(&truncated, Vector2::new(0, 0)).is_err());
            let mut duplicate = bytes[..bytes.len() - 1].to_vec();
            duplicate.extend_from_slice(&[
                3, 0, 9, b'd', b'u', b'p', b'l', b'i', b'c', b'a', b't', b'e', 0, 0, 0, 2, 0,
            ]);
            assert!(T::from_bytes(&duplicate.into(), Vector2::new(0, 0)).is_err());
        }
    }

    #[test]
    fn chunk_endpoints_require_complete_unambiguous_documents() {
        assert_complete_documents::<ChunkData>(
            test_chunk(vec![test_section(-4, "minecraft:stone", true)]).root_tag,
        );
        assert_complete_documents::<ChunkEntityData>(entity_chunk_root(Vector2::new(0, 0)));
    }

    #[test]
    fn entity_chunk_position_is_exact_and_modern_position_wins_over_aliases() {
        let position = Vector2::new(-7, 12);
        let mut root = entity_chunk_root(position);
        root.put_int("Position-X", 900);
        root.put_int("Position-Z", 901);
        let load = |root: NbtCompound| {
            let bytes = pumpkin_nbt::Nbt::from(root).try_write_preserving().unwrap();
            ChunkEntityData::from_bytes(&bytes, position)
        };
        let chunk = load(root.clone()).unwrap();
        assert_eq!(chunk.position(), (-7, 12));
        let saved = decode_preserving(&chunk.to_bytes().unwrap());
        assert_eq!(saved.get_int_array("Position"), Some([-7, 12].as_slice()));
        assert!(saved.get("Position-X").is_none() && saved.get("Position-Z").is_none());

        root.put_int("Position-X", position.x);
        root.put_int("Position-Z", position.y);
        for invalid in [
            NbtTag::IntArray(vec![]),
            NbtTag::IntArray(vec![-7]),
            NbtTag::IntArray(vec![-7, 12, 99]),
            NbtTag::IntArray(vec![-8, 12]),
            NbtTag::LongArray(vec![-7, 12]),
            NbtTag::String("-7,12".into()),
        ] {
            let mut malformed = root.clone();
            malformed.put("Position", invalid);
            assert!(
                load(malformed).is_err(),
                "modern position must not fall back"
            );
        }
        root.child_tags.remove("Position");
        assert_eq!(load(root.clone()).unwrap().position(), (-7, 12));
        for key in ["Position-X", "Position-Z"] {
            let mut missing = root.clone();
            missing.child_tags.remove(key);
            assert!(load(missing).is_err());
            let mut wrong_type = root.clone();
            wrong_type.put_short(key, 0);
            assert!(load(wrong_type).is_err());
        }
        assert!(load(NbtCompound::new()).is_err());
    }

    #[test]
    fn entity_chunk_rejects_invalid_entity_lists_without_dropping_records() {
        let position = Vector2::new(0, 0);
        for invalid in [
            NbtTag::Int(1),
            NbtTag::Compound(NbtCompound::new()),
            NbtTag::List(vec![NbtTag::String("not a record".into())]),
            NbtTag::List(vec![NbtTag::Int(1), NbtTag::Int(2)]),
        ] {
            let mut root = entity_chunk_root(position);
            root.put("Entities", invalid);
            let bytes = pumpkin_nbt::Nbt::from(root).try_write_preserving().unwrap();
            assert!(ChunkEntityData::from_bytes(&bytes, position).is_err());
        }
        let mut root = entity_chunk_root(position);
        root.child_tags.remove("Entities");
        let bytes = pumpkin_nbt::Nbt::from(root).try_write_preserving().unwrap();
        assert!(
            ChunkEntityData::from_bytes(&bytes, position)
                .unwrap()
                .data
                .lock()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn entity_chunk_keeps_typed_root_metadata_and_mixed_component_lists() {
        use pumpkin_util::{text::TextComponent, version::JavaMinecraftVersion};

        let position = Vector2::new(-7, 12);
        let mut metadata = NbtCompound::new();
        metadata.put("bytes", NbtTag::ByteArray(vec![-128, 0, 127].into()));
        metadata.put("longs", NbtTag::LongArray(vec![i64::MIN, i64::MAX]));
        let mut empty_name = NbtCompound::new();
        empty_name.put_string("", "opaque empty-name compound".into());
        metadata.put_list(
            "ordered",
            vec![NbtTag::Compound(empty_name), NbtTag::Int(42)],
        );
        let name = TextComponent::text("Farm cart")
            .add_child(TextComponent::text(" plain"))
            .add_child(TextComponent::text(" styled").bold());
        let mut record = NbtCompound::new();
        record.put_string("id", "minecraft:minecart".into());
        record.put("UUID", NbtTag::IntArray(vec![1, 2, 3, 4]));
        record.put(
            "CustomName",
            name.to_nbt_tag_for_version(&JavaMinecraftVersion::V_26_3),
        );
        let mut root = entity_chunk_root(position);
        root.put_compound("future:metadata", metadata);
        root.put_list("Entities", vec![NbtTag::Compound(record)]);
        let bytes = pumpkin_nbt::Nbt::from(root).try_write_preserving().unwrap();
        let original = decode_preserving(&bytes);
        let chunk = ChunkEntityData::from_bytes(&bytes, position).unwrap();
        chunk.data.lock().unwrap()[0].put_int("RuntimeChange", 9);
        let saved = decode_preserving(&chunk.to_bytes().unwrap());
        assert_eq!(
            saved.get("future:metadata"),
            original.get("future:metadata")
        );
        assert_eq!(saved.get_int("DataVersion"), Some(WORLD_DATA_VERSION));
        let saved_record = saved.get_list("Entities").unwrap()[0]
            .extract_compound()
            .unwrap();
        let original_record = original.get_list("Entities").unwrap()[0]
            .extract_compound()
            .unwrap();
        assert_eq!(
            saved_record.get("CustomName"),
            original_record.get("CustomName")
        );
        assert_eq!(
            TextComponent::try_from_nbt(saved_record.get("CustomName").unwrap()).unwrap(),
            name
        );
        assert_eq!(saved_record.get_int("RuntimeChange"), Some(9));

        // Clearing live records must not expose a stale Entities copy from the retained root.
        chunk.data.lock().unwrap().clear();
        let empty = decode_preserving(&chunk.to_bytes().unwrap());
        assert!(empty.get_list("Entities").unwrap().is_empty());
        assert_eq!(
            empty.get("future:metadata"),
            original.get("future:metadata")
        );
    }

    #[test]
    fn terrain_rejects_lossy_block_entity_records_but_retains_unknown_types() {
        let mut record = NbtCompound::new();
        record.put_string("id", "future:unsupported_block_entity".into());
        record.put_int("x", 1);
        record.put_int("y", 64);
        record.put_int("z", 2);
        let mut opaque = NbtCompound::new();
        opaque.put("bytes", NbtTag::ByteArray(vec![0, -1, 42].into()));
        record.put_compound("opaque", opaque);
        let mut root = test_chunk(vec![test_section(-4, "minecraft:stone", true)]).root_tag;
        let load = |root: NbtCompound| {
            let bytes = pumpkin_nbt::Nbt::from(root).try_write_preserving().unwrap();
            ChunkData::from_bytes(&bytes, Vector2::new(0, 0))
        };
        root.put_list("block_entities", vec![NbtTag::Compound(record.clone())]);
        let valid = load(root.clone()).unwrap();
        let saved = decode_preserving(&valid.to_bytes().unwrap());
        assert_eq!(
            saved.get_list("block_entities").unwrap(),
            &[NbtTag::Compound(record.clone())]
        );

        for invalid in [
            NbtTag::String("not a list".into()),
            NbtTag::Compound(record.clone()),
            NbtTag::List(vec![NbtTag::Int(1)]),
            NbtTag::List(vec![
                NbtTag::Compound(record.clone()),
                NbtTag::Compound(record.clone()),
            ]),
        ] {
            let mut malformed = root.clone();
            malformed.put("block_entities", invalid);
            assert!(load(malformed).is_err());
        }
        for key in ["x", "y", "z"] {
            let mut missing = record.clone();
            missing.child_tags.remove(key);
            let mut malformed = root.clone();
            malformed.put_list("block_entities", vec![NbtTag::Compound(missing)]);
            assert!(load(malformed).is_err());
            let mut wrong_type = record.clone();
            wrong_type.put_string(key, "1".into());
            let mut malformed = root.clone();
            malformed.put_list("block_entities", vec![NbtTag::Compound(wrong_type)]);
            assert!(load(malformed).is_err());
        }
    }

    #[test]
    fn chunk_endpoints_propagate_invalid_stored_tag_errors() {
        let terrain = ChunkData::empty(0, 0);
        terrain
            .preserved_tags
            .lock()
            .unwrap()
            .put("invalid", NbtTag::End);
        assert!(matches!(
            terrain.to_bytes(),
            Err(ChunkSerializingError::ErrorSerializingChunk(
                pumpkin_nbt::Error::NamedEndTag(_)
            ))
        ));
        let root = entity_chunk_root(Vector2::new(0, 0));
        let bytes = pumpkin_nbt::Nbt::from(root).try_write_preserving().unwrap();
        let entities = ChunkEntityData::from_bytes(&bytes, Vector2::new(0, 0)).unwrap();
        entities
            .preserved_tags
            .lock()
            .unwrap()
            .put("invalid", NbtTag::End);
        assert!(matches!(
            entities.to_bytes(),
            Err(ChunkSerializingError::ErrorSerializingChunk(
                pumpkin_nbt::Error::NamedEndTag(_)
            ))
        ));
        entities.preserved_tags.lock().unwrap().child_tags.clear();
        let mut invalid_record = NbtCompound::new();
        invalid_record.put_list("invalid", vec![NbtTag::End]);
        entities.data.lock().unwrap().push(invalid_record);
        assert!(matches!(
            entities.to_bytes(),
            Err(ChunkSerializingError::ErrorSerializingChunk(
                pumpkin_nbt::Error::InvalidListTag(0)
            ))
        ));
    }

    fn test_section(y: i32, block: &str, with_biomes: bool) -> NbtCompound {
        let mut block_states = NbtCompound::new();
        let mut air = NbtCompound::new();
        air.put_string("Name", "minecraft:air".to_string());
        let mut solid = NbtCompound::new();
        solid.put_string("Name", block.to_string());
        block_states.put(
            "palette",
            NbtTag::List(vec![NbtTag::Compound(air), NbtTag::Compound(solid)]),
        );
        block_states.put("data", NbtTag::LongArray(vec![1; 256]));

        let mut section = NbtCompound::new();
        section.put_int("Y", y);
        section.put("block_states", NbtTag::Compound(block_states));
        if with_biomes {
            let mut biomes = NbtCompound::new();
            biomes.put(
                "palette",
                NbtTag::List(vec![NbtTag::String("minecraft:plains".into())]),
            );
            section.put("biomes", NbtTag::Compound(biomes));
        }
        section
    }

    fn test_chunk(sections: Vec<NbtCompound>) -> pumpkin_nbt::Nbt {
        let mut root = NbtCompound::new();
        root.put_int("DataVersion", 4903);
        root.put_int("xPos", 0);
        root.put_int("zPos", 0);
        root.put_string("Status", "minecraft:full".to_string());
        root.put(
            "sections",
            NbtTag::List(sections.into_iter().map(NbtTag::Compound).collect()),
        );
        pumpkin_nbt::Nbt::new(String::new(), root)
    }

    fn test_scheduled_tick(x: i32, delay: i32, id: &str) -> NbtCompound {
        let mut tick = NbtCompound::new();
        tick.put_int("x", x);
        tick.put_int("y", 64);
        tick.put_int("z", 0);
        tick.put_int("t", delay);
        tick.put_int("p", 0);
        tick.put_string("i", id.to_owned());
        tick
    }

    #[test]
    fn scheduled_ticks_keep_signed_int_delays_through_chunk_load_and_save() {
        // Negative delays occur in the imported vanilla 26.3 world; future
        // values cover the old byte wheel and larger-than-short boundaries.
        let delays = [
            i32::MIN,
            -1_425_489,
            -134,
            -1,
            0,
            255,
            256,
            300,
            70_000,
            i32::MAX,
        ];
        let mut nbt = test_chunk(vec![test_section(-4, "minecraft:stone", true)]);
        for (name, id) in [
            ("block_ticks", "minecraft:stone"),
            ("fluid_ticks", "minecraft:water"),
        ] {
            nbt.root_tag.put_list(
                name,
                delays
                    .iter()
                    .enumerate()
                    .map(|(x, &delay)| NbtTag::Compound(test_scheduled_tick(x as i32, delay, id)))
                    .collect(),
            );
        }
        let chunk = ChunkData::from_bytes(&nbt.clone().write(), Vector2::new(0, 0)).unwrap();
        let saved = decode_preserving(&chunk.to_bytes().unwrap());
        for name in ["block_ticks", "fluid_ticks"] {
            assert_eq!(
                saved.get(name),
                nbt.root_tag.get(name),
                "{name} changed before ticking"
            );
        }
        assert_eq!(chunk.block_ticks.step_tick().len(), 5);
        assert_eq!(chunk.fluid_ticks.step_tick().len(), 5);
        let advanced = decode_preserving(&chunk.to_bytes().unwrap());
        for name in ["block_ticks", "fluid_ticks"] {
            let delays: Vec<_> = advanced
                .get_list(name)
                .unwrap()
                .iter()
                .map(|tag| {
                    let NbtTag::Compound(tick) = tag else {
                        panic!("tick must be a compound")
                    };
                    tick.get_int("t").unwrap()
                })
                .collect();
            assert_eq!(delays, [254, 255, 299, 69_999, 2_147_483_646]);
        }
    }

    #[test]
    fn malformed_scheduled_ticks_fail_loading_instead_of_disappearing() {
        for (name, id) in [
            ("block_ticks", "minecraft:stone"),
            ("fluid_ticks", "minecraft:water"),
        ] {
            let valid = test_scheduled_tick(0, -134, id);
            let mut invalid = vec![NbtTag::Int(1), NbtTag::List(vec![NbtTag::Int(1)])];
            for key in ["x", "y", "z", "t", "p", "i"] {
                let mut missing = valid.clone();
                missing.child_tags.remove(key);
                invalid.push(NbtTag::List(vec![NbtTag::Compound(missing)]));
                let mut wrong_type = valid.clone();
                wrong_type.put(key, NbtTag::Long(1));
                invalid.push(NbtTag::List(vec![NbtTag::Compound(wrong_type)]));
            }
            let mut unknown = valid.clone();
            unknown.put_string("i", "example:unknown_tick_type".to_owned());
            invalid.push(NbtTag::List(vec![
                NbtTag::Compound(valid),
                NbtTag::Compound(unknown),
            ]));
            for tag in invalid {
                let mut nbt = test_chunk(vec![test_section(-4, "minecraft:stone", true)]);
                nbt.root_tag.put(name, tag);
                let Err(error) = ChunkData::from_bytes(&nbt.write(), Vector2::new(0, 0)) else {
                    panic!("malformed {name} must not load");
                };
                assert!(format!("{error:?}").contains(name));
            }
        }
    }

    #[test]
    fn saved_tick_priority_clamps_like_vanilla() {
        for (encoded, expected) in [(-100, -3), (-1, -1), (0, 0), (1, 1), (100, 3)] {
            let mut nbt = test_scheduled_tick(0, -134, "minecraft:stone");
            nbt.put_int("p", encoded);
            let tick = ScheduledTick::<&'static Block>::from_nbt_compound(&nbt).unwrap();
            assert_eq!(tick.to_nbt_compound().get_int("p"), Some(expected));
            assert_eq!(tick.delay, -134);
        }
    }

    #[test]
    fn chunk_without_y_pos_uses_the_lowest_biome_section() {
        use crate::chunk::ChunkData;
        use pumpkin_util::math::vector2::Vector2;

        // The datafixer writes Y as an int, so the test must too.
        let bytes = test_chunk(vec![test_section(-4, "minecraft:stone", true)]).write();
        let chunk =
            ChunkData::from_bytes(&bytes, Vector2::new(0, 0)).expect("chunk without yPos parses");
        assert_eq!(
            chunk.section.get_block_absolute_y(0, -64, 0),
            Some(Block::STONE.default_state.id)
        );
    }

    #[test]
    fn chunk_with_int_y_sections_keeps_every_section() {
        use crate::chunk::ChunkData;
        use pumpkin_util::math::vector2::Vector2;

        let bytes = test_chunk(vec![
            test_section(-4, "minecraft:stone", true),
            test_section(0, "minecraft:dirt", true),
        ])
        .write();
        let chunk = ChunkData::from_bytes(&bytes, Vector2::new(0, 0)).expect("int Y chunk parses");
        assert_eq!(
            chunk.section.get_block_absolute_y(0, -64, 0),
            Some(Block::STONE.default_state.id)
        );
        assert_eq!(
            chunk.section.get_block_absolute_y(0, 0, 0),
            Some(Block::DIRT.default_state.id)
        );
    }

    #[test]
    fn chunk_with_mixed_numeric_y_tags_keeps_sections() {
        use crate::chunk::ChunkData;
        use pumpkin_util::math::vector2::Vector2;

        // What a real upgraded chunk looks like: the map's own sections store Y
        // as a byte, the datafixer writes ints. The int section must still land
        // at its own height instead of collapsing to Y = 0.
        let mut byte_section = test_section(-4, "minecraft:air", true);
        byte_section.put_byte("Y", -4);

        let bytes = test_chunk(vec![
            byte_section,
            test_section(0, "minecraft:stone", true),
            test_section(4, "minecraft:dirt", true),
        ])
        .write();
        let chunk =
            ChunkData::from_bytes(&bytes, Vector2::new(0, 0)).expect("mixed Y chunk parses");
        assert_eq!(
            chunk.section.get_block_absolute_y(0, 0, 0),
            Some(Block::STONE.default_state.id)
        );
        assert_eq!(
            chunk.section.get_block_absolute_y(0, 64, 0),
            Some(Block::DIRT.default_state.id)
        );
    }

    #[test]
    fn fallback_ignores_light_only_sections_below_zero() {
        use crate::chunk::ChunkData;
        use pumpkin_util::math::vector2::Vector2;

        // Old worlds keep a light grid at Y = -1 after the upgrade. It has no
        // biomes and must not decide where the chunk starts.
        let mut light_section = NbtCompound::new();
        light_section.put_byte("Y", -1);
        light_section.put(
            "BlockLight",
            NbtTag::ByteArray(vec![0x0Fi8; 2048].into_boxed_slice()),
        );

        let bytes = test_chunk(vec![
            light_section,
            test_section(0, "minecraft:stone", true),
        ])
        .write();
        let chunk =
            ChunkData::from_bytes(&bytes, Vector2::new(0, 0)).expect("light section ignored");
        assert_eq!(
            chunk.section.get_block_absolute_y(0, 0, 0),
            Some(Block::STONE.default_state.id)
        );
        assert_eq!(chunk.section.get_block_absolute_y(0, -1, 0), None);
    }

    #[test]
    fn fallback_never_goes_above_zero() {
        use crate::chunk::ChunkData;
        use pumpkin_util::math::vector2::Vector2;

        let bytes = test_chunk(vec![test_section(1, "minecraft:stone", true)]).write();
        let chunk = ChunkData::from_bytes(&bytes, Vector2::new(0, 0)).expect("caps at zero");
        // The only section sits at Y=1, so the cap has to bring the chunk down
        // to Y=0. Without the cap the chunk would start at Y=1 and a block at
        // Y=0 would be out of range.
        assert_eq!(
            chunk.section.get_block_absolute_y(0, 0, 0),
            Some(Block::AIR.default_state.id)
        );
        assert_eq!(
            chunk.section.get_block_absolute_y(0, 16, 0),
            Some(Block::STONE.default_state.id)
        );
    }

    #[test]
    fn chunk_without_y_pos_or_biomes_still_fails() {
        use crate::chunk::ChunkData;
        use pumpkin_util::math::vector2::Vector2;

        let mut light_section = NbtCompound::new();
        light_section.put_byte("Y", -1);
        light_section.put(
            "BlockLight",
            NbtTag::ByteArray(vec![0x0Fi8; 2048].into_boxed_slice()),
        );

        let bytes = test_chunk(vec![light_section]).write();
        let Err(error) = ChunkData::from_bytes(&bytes, Vector2::new(0, 0)) else {
            panic!("chunk without yPos and without biomes must fail");
        };
        assert!(format!("{error:?}").contains("Missing yPos"));
    }

    #[test]
    fn unknown_block_in_existing_chunk_fails_to_load() {
        let bytes = test_chunk(vec![test_section(-4, "minecraft:removed_block", true)]).write();
        let Err(ChunkReadingError::ParsingError(ChunkParsingError::InvalidBlockPalette {
            position,
            section_y,
            palette_index,
            cause,
        })) = ChunkData::from_bytes(&bytes, Vector2::new(0, 0))
        else {
            panic!("unknown registry entries must fail with palette context");
        };
        assert_eq!(position, Vector2::new(0, 0));
        assert_eq!(section_y, -4);
        assert_eq!(palette_index, 1);
        assert!(cause.contains("minecraft:removed_block"));
    }

    #[test]
    fn extract_u16_array_from_vanilla_compound_palette() {
        let mut entry1 = NbtCompound::new();
        entry1.put_string("Name", "minecraft:stone".to_string());

        let mut entry2 = NbtCompound::new();
        entry2.put_string("Name", "minecraft:repeater".to_string());
        let mut props = NbtCompound::new();
        props.put_string("facing", "north".to_string());
        props.put_string("delay", "2".to_string());
        props.put_string("locked", "false".to_string());
        props.put_string("powered", "false".to_string());
        entry2.put_compound("Properties", props);

        let list_tag = NbtTag::List(vec![NbtTag::Compound(entry1), NbtTag::Compound(entry2)]);
        let result = extract_u16_array(&list_tag, Vector2::new(0, 0), 0)
            .expect("palette is valid")
            .expect("should extract palette");

        assert_eq!(result.len(), 2);
        assert_eq!(result[0], Block::STONE.default_state.id);

        let repeater_state = Block::REPEATER
            .from_properties(&[
                ("facing", "north"),
                ("delay", "2"),
                ("locked", "false"),
                ("powered", "false"),
            ])
            .to_state_id(&Block::REPEATER);
        assert_eq!(result[1], repeater_state);
    }

    #[test]
    fn modern_chunk_palette_round_trips_named_state_properties() {
        let chunk = ChunkData::empty(0, 0);
        let hopper = Block::HOPPER
            .from_properties(&[("facing", "east"), ("enabled", "false")])
            .to_state_id(&Block::HOPPER);
        chunk.section.set_block_absolute_y(2, 10, 3, hopper);
        let bytes = chunk.internal_to_bytes().expect("chunk serializes");
        let mut cursor = std::io::Cursor::new(bytes.as_ref());
        let root = pumpkin_nbt::Nbt::read(&mut pumpkin_nbt::deserializer::NbtReadHelperJava::new(
            &mut cursor,
        ))
        .unwrap()
        .root_tag;
        assert_eq!(root.get_int("DataVersion"), Some(5023));
        for section in root.get_list("sections").unwrap() {
            for entry in section
                .extract_compound()
                .unwrap()
                .get_compound("block_states")
                .unwrap()
                .get_list("palette")
                .unwrap()
            {
                let entry = entry.extract_compound().unwrap();
                assert!(entry.get_string("id").is_some());
                assert!(entry.get("Name").is_none() && entry.get("Properties").is_none());
            }
        }
        let reloaded = ChunkData::internal_from_bytes(&bytes, Vector2::new(0, 0)).unwrap();
        assert_eq!(
            reloaded.section.get_block_absolute_y(2, 10, 3),
            Some(hopper)
        );
    }

    #[test]
    fn extract_u8_array_from_vanilla_string_palette() {
        let list_tag = NbtTag::List(vec![
            NbtTag::String("minecraft:plains".to_string().into()),
            NbtTag::String("minecraft:the_void".to_string().into()),
        ]);
        let result = extract_u8_array(&list_tag).expect("should extract biome palette");

        assert_eq!(result.len(), 2);
        assert_eq!(
            result[0],
            pumpkin_data::biome::Biome::from_name("plains").unwrap().id
        );
        assert_eq!(
            result[1],
            pumpkin_data::biome::Biome::from_name("the_void")
                .unwrap()
                .id
        );
    }

    /// A vanilla chunk carries tags this server has no field for. Dropping them
    /// on save destroys map data: `structures` is what makes a village a
    /// village to every structure-aware feature, and the generation-phase tags
    /// decide what still has to happen to a chunk that is not finished yet.
    #[test]
    fn saving_keeps_the_top_level_tags_this_server_never_reads() {
        use crate::chunk::ChunkData;
        use pumpkin_util::math::vector2::Vector2;

        let mut nbt = test_chunk(vec![test_section(-4, "minecraft:stone", true)]);

        let mut structures = NbtCompound::new();
        let mut start = NbtCompound::new();
        start.put_string("id", "minecraft:village".to_string());
        start.put_int("ChunkX", 0);
        start.put_int("ChunkZ", 0);
        let mut child = NbtCompound::new();
        child.put("BB", NbtTag::IntArray(vec![0, 64, 0, 15, 80, 15]));
        child.put_string("id", "minecraft:jigsaw".to_string());
        start.put_list("Children", vec![NbtTag::Compound(child)]);
        let mut starts = NbtCompound::new();
        starts.put_compound("minecraft:village", start);
        structures.put_compound("starts", starts);
        let mut references = NbtCompound::new();
        references.put(
            "minecraft:village",
            NbtTag::LongArray(vec![0, 4_294_967_297]),
        );
        structures.put_compound("References", references);
        nbt.root_tag.put("structures", NbtTag::Compound(structures));
        nbt.root_tag.put(
            "PostProcessing",
            NbtTag::List(vec![NbtTag::List(vec![NbtTag::Short(17)])]),
        );
        nbt.root_tag
            .put("carving_mask", NbtTag::LongArray(vec![1, i64::MIN, -1]));
        nbt.root_tag.put_long("LastUpdate", 1_287_805);
        let mut blending_data = NbtCompound::new();
        blending_data.put_int("min_section", -4);
        blending_data.put_int("max_section", 20);
        nbt.root_tag.put_compound("blending_data", blending_data);
        let mut plugin_data = NbtCompound::new();
        plugin_data.put("payload", NbtTag::ByteArray(vec![0, -1, 42].into()));
        nbt.root_tag.put_compound("example:metadata", plugin_data);
        nbt.root_tag.put_int("starlight.light_version", 9);

        let written = nbt.root_tag.clone();
        let chunk = ChunkData::from_bytes(&nbt.write(), Vector2::new(0, 0)).expect("chunk parses");
        let saved = chunk.to_bytes().expect("chunk serializes");

        let mut cursor = std::io::Cursor::new(saved.as_ref());
        let mut reader = pumpkin_nbt::deserializer::NbtReadHelperJava::new(&mut cursor);
        let reloaded = pumpkin_nbt::Nbt::read(&mut reader).expect("saved chunk parses");

        assert!(reloaded.root_tag.get("starlight.light_version").is_none());

        for key in [
            "structures",
            "PostProcessing",
            "carving_mask",
            "LastUpdate",
            "blending_data",
            "example:metadata",
        ] {
            assert_eq!(
                reloaded.root_tag.get(key),
                written.get(key),
                "{key} did not survive the save"
            );
        }
    }

    #[test]
    fn saving_keeps_bukkit_values_without_restoring_removed_custom_data() {
        let mut nbt = test_chunk(vec![test_section(-4, "minecraft:stone", true)]);
        let mut namespace = NbtCompound::new();
        namespace.put_int("owner", 42);
        let mut bukkit_values = NbtCompound::new();
        bukkit_values.put_compound("example", namespace);
        nbt.root_tag
            .put_compound("BukkitValues", bukkit_values.clone());

        let chunk = ChunkData::from_bytes(&nbt.write(), Vector2::new(0, 0)).expect("chunk parses");
        assert_eq!(
            chunk.get_custom_data("example", "owner"),
            Some(NbtTag::Int(42))
        );
        chunk.remove_custom_data("example", "owner");

        let saved = chunk.to_bytes().expect("chunk serializes");
        let mut cursor = std::io::Cursor::new(saved.as_ref());
        let mut reader = pumpkin_nbt::deserializer::NbtReadHelperJava::new(&mut cursor);
        let reloaded = pumpkin_nbt::Nbt::read(&mut reader).expect("saved chunk parses");
        assert_eq!(
            reloaded.root_tag.get_compound("BukkitValues"),
            Some(&bukkit_values)
        );
        assert_eq!(
            reloaded.root_tag.get_compound("PumpkinCustomData"),
            Some(&NbtCompound::new())
        );
        let chunk = ChunkData::from_bytes(&saved, Vector2::new(0, 0)).expect("chunk reloads");
        assert_eq!(chunk.get_custom_data("example", "owner"), None);
    }

    #[test]
    fn saving_uses_edited_managed_data_alongside_original_bukkit_values() {
        let mut nbt = test_chunk(vec![test_section(-4, "minecraft:stone", true)]);
        let mut namespace = NbtCompound::new();
        namespace.put_int("owner", 7);
        let mut custom_data = NbtCompound::new();
        custom_data.put_compound("example", namespace);
        nbt.root_tag.put_compound("PumpkinCustomData", custom_data);
        let mut bukkit_values = NbtCompound::new();
        bukkit_values.put_string("example:legacy", "original".to_string());
        nbt.root_tag
            .put_compound("BukkitValues", bukkit_values.clone());
        nbt.root_tag.put_long("InhabitedTime", 123);

        let mut chunk =
            ChunkData::from_bytes(&nbt.write(), Vector2::new(0, 0)).expect("chunk parses");
        chunk.set_custom_data("example", "owner", NbtTag::Int(8));
        chunk.inhabited_time.store(456, Ordering::Relaxed);
        chunk.status = ChunkStatus::Light;
        chunk.set_block_absolute_y(0, -64, 0, Block::DIRT.default_state.id);

        let saved = chunk.to_bytes().expect("chunk serializes");
        let mut cursor = std::io::Cursor::new(saved.as_ref());
        let mut reader = pumpkin_nbt::deserializer::NbtReadHelperJava::new(&mut cursor);
        let reloaded = pumpkin_nbt::Nbt::read(&mut reader).expect("saved chunk parses");
        assert_eq!(
            reloaded.root_tag.get_compound("BukkitValues"),
            Some(&bukkit_values)
        );
        assert_eq!(reloaded.root_tag.get_long("InhabitedTime"), Some(456));
        assert_eq!(
            reloaded.root_tag.get_string("Status"),
            Some("minecraft:light")
        );
        let chunk = ChunkData::from_bytes(&saved, Vector2::new(0, 0)).expect("chunk reloads");
        assert_eq!(
            chunk.get_custom_data("example", "owner"),
            Some(NbtTag::Int(8))
        );
        assert_eq!(
            chunk.section.get_block_absolute_y(0, -64, 0),
            Some(Block::DIRT.default_state.id)
        );
    }

    /// `MODELLED_ROOT_TAGS` can fall behind the writer. When it does, the name
    /// lands in the preserved bag as well, and the value the server computed
    /// still has to be the one that reaches disk — a stale tag winning would be
    /// a worse bug than the dropped tags this preservation exists to prevent.
    #[test]
    fn a_preserved_tag_never_shadows_one_this_server_owns() {
        use crate::chunk::ChunkData;
        use pumpkin_util::math::vector2::Vector2;

        let nbt = test_chunk(vec![test_section(-4, "minecraft:stone", true)]);
        let chunk = ChunkData::from_bytes(&nbt.write(), Vector2::new(0, 0)).expect("chunk parses");
        chunk
            .preserved_tags
            .lock()
            .unwrap()
            .put_int("DataVersion", 1);

        let saved = chunk.to_bytes().expect("chunk serializes");
        let mut cursor = std::io::Cursor::new(saved.as_ref());
        let mut reader = pumpkin_nbt::deserializer::NbtReadHelperJava::new(&mut cursor);
        let reloaded = pumpkin_nbt::Nbt::read(&mut reader).expect("saved chunk parses");

        assert_eq!(
            reloaded.root_tag.get_int("DataVersion"),
            Some(WORLD_DATA_VERSION)
        );
    }
}
