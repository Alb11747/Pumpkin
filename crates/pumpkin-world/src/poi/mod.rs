use crate::chunk::format::{anvil::WORLD_DATA_VERSION, require_current_data_version};
use pumpkin_nbt::{Nbt, NbtCompound, tag::NbtTag};
use rustc_hash::FxHashMap;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use tracing::info;

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector3::Vector3;
use serde::{Deserialize, Serialize};

/// POI type identifier for nether portals
pub const POI_TYPE_NETHER_PORTAL: &str = "minecraft:nether_portal";

/// MCA format constants
const SECTOR_SIZE: usize = 4096;
const REGION_SIZE: usize = 32;
const CHUNK_COUNT: usize = REGION_SIZE * REGION_SIZE;
const HEADER_SIZE: usize = SECTOR_SIZE * 2; // Location table + timestamp table

/// Compression type for MCA format
const COMPRESSION_ZLIB: u8 = 2;

const DATA_VERSION: i32 = WORLD_DATA_VERSION;

/// A single Point of Interest entry (serializable)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoiEntry {
    pub x: i32,
    pub y: i32,
    pub z: i32,
    #[serde(rename = "type")]
    pub poi_type: String,
    pub free_tickets: i32,
    #[serde(skip)]
    source: NbtCompound,
}

impl PoiEntry {
    #[must_use]
    pub fn new_portal(pos: BlockPos) -> Self {
        Self {
            x: pos.0.x,
            y: pos.0.y,
            z: pos.0.z,
            poi_type: POI_TYPE_NETHER_PORTAL.to_string(),
            free_tickets: 0,
            source: NbtCompound::new(),
        }
    }

    #[must_use]
    pub const fn pos(&self) -> BlockPos {
        BlockPos(Vector3::new(self.x, self.y, self.z))
    }
}

/// POI section data (serializable) - vanilla format
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PoiSectionData {
    #[serde(default)]
    pub valid: i8,
    #[serde(default)]
    pub records: Vec<PoiEntry>,
    #[serde(skip)]
    source: NbtCompound,
}

/// POI chunk data (serializable) - vanilla format
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PoiChunkData {
    pub data_version: i32,
    /// Sections keyed by Y section coordinate (e.g., "-1", "0", "1", "4")
    pub sections: FxHashMap<String, PoiSectionData>,
    #[serde(skip)]
    source: NbtCompound,
}

/// POI data for a single region (32x32 chunks) using MCA format
#[derive(Debug, Default)]
pub struct PoiRegion {
    /// Entries indexed by position
    entries: FxHashMap<(i32, i32, i32), PoiEntry>,
    // Includes empty sections and unmanaged root/section metadata.
    chunks: FxHashMap<usize, PoiChunkData>,
    /// Track which chunks are dirty
    dirty_chunks: rustc_hash::FxHashSet<(i32, i32)>,
    dirty: bool,
}

impl PoiRegion {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    const fn pos_key(pos: &BlockPos) -> (i32, i32, i32) {
        (pos.0.x, pos.0.y, pos.0.z)
    }

    /// Get chunk index in MCA file (0-1023)
    const fn chunk_index(chunk_x: i32, chunk_z: i32) -> usize {
        let local_x = chunk_x & 31;
        let local_z = chunk_z & 31;
        ((local_z << 5) | local_x) as usize
    }

    /// Returns section key as just the Y section coordinate (like vanilla)
    fn section_key(pos: &BlockPos) -> String {
        let section_y = pos.0.y >> 4;
        section_y.to_string()
    }

    pub fn add(&mut self, mut entry: PoiEntry) {
        let chunk_x = entry.x >> 4;
        let chunk_z = entry.z >> 4;
        self.dirty_chunks.insert((chunk_x, chunk_z));
        let key = (entry.x, entry.y, entry.z);
        if let Some(previous) = self.entries.get(&key)
            && previous.poi_type == entry.poi_type
        {
            entry.source = previous.source.clone();
        }
        self.entries.insert(key, entry);
        self.dirty = true;
    }

    pub fn remove(&mut self, pos: &BlockPos) -> bool {
        let key = Self::pos_key(pos);
        if self.entries.remove(&key).is_some() {
            let chunk_x = pos.0.x >> 4;
            let chunk_z = pos.0.z >> 4;
            self.dirty_chunks.insert((chunk_x, chunk_z));
            self.dirty = true;
            return true;
        }
        false
    }

    #[must_use]
    pub fn get_all(&self) -> Vec<&PoiEntry> {
        self.entries.values().collect()
    }

    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_clean(&mut self) {
        self.dirty = false;
        self.dirty_chunks.clear();
    }

    /// Rebuild managed records while retaining empty sections and opaque metadata.
    fn get_chunk_data(&self, index: usize) -> PoiChunkData {
        let mut chunk = self
            .chunks
            .get(&index)
            .cloned()
            .unwrap_or_else(|| PoiChunkData {
                data_version: DATA_VERSION,
                ..PoiChunkData::default()
            });
        let mut pending: FxHashMap<_, _> = self
            .entries
            .iter()
            .filter(|(_, entry)| Self::chunk_index(entry.x >> 4, entry.z >> 4) == index)
            .map(|(position, entry)| (*position, entry.clone()))
            .collect();
        for section in chunk.sections.values_mut() {
            // Original order survives updates; removed entries cannot come back from raw NBT.
            section.records = section
                .records
                .iter()
                .filter_map(|entry| pending.remove(&(entry.x, entry.y, entry.z)))
                .collect();
        }
        let mut added: Vec<_> = pending.into_values().collect();
        added.sort_by_key(|entry| (entry.x, entry.y, entry.z));
        for entry in added {
            chunk
                .sections
                .entry(Self::section_key(&entry.pos()))
                .or_insert_with(|| PoiSectionData {
                    valid: 1,
                    ..PoiSectionData::default()
                })
                .records
                .push(entry);
        }
        chunk
    }

    /// Vanilla PoiRecord.Packed uses BlockPos.CODEC (an int array), not x/y/z fields.
    fn compress_chunk_data(chunk: &PoiChunkData) -> std::io::Result<Vec<u8>> {
        let mut root = chunk.source.clone();
        if root.is_empty() || root.get("DataVersion").is_some() {
            root.put_int("DataVersion", chunk.data_version);
        }
        let mut sections = NbtCompound::new();
        for (key, section) in &chunk.sections {
            let mut section_nbt = section.source.clone();
            if section.valid != 0 || section_nbt.get("Valid").is_some() {
                section_nbt.put_byte("Valid", section.valid);
            }
            let records = section
                .records
                .iter()
                .map(|entry| {
                    let mut record = entry.source.clone();
                    for alias in ["x", "y", "z"] {
                        record.child_tags.remove(alias);
                    }
                    record.put("pos", NbtTag::IntArray(vec![entry.x, entry.y, entry.z]));
                    record.put_string("type", entry.poi_type.clone());
                    if entry.free_tickets != 0 || record.get("free_tickets").is_some() {
                        record.put_int("free_tickets", entry.free_tickets);
                    }
                    NbtTag::Compound(record)
                })
                .collect();
            section_nbt.put_list("Records", records);
            sections.put_compound(key, section_nbt);
        }
        root.put_compound("Sections", sections);
        let bytes = Nbt::from(root)
            .try_write_preserving()
            .map_err(invalid_poi)?;
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&bytes)?;
        encoder.finish()
    }

    fn decompress_chunk_data(compressed: &[u8]) -> std::io::Result<PoiChunkData> {
        const MAX_CHUNK_BYTES: usize = 64 * 1024 * 1024;
        let mut decoder = ZlibDecoder::new(compressed);
        let mut bytes = Vec::new();
        decoder
            .by_ref()
            .take(MAX_CHUNK_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_CHUNK_BYTES || decoder.total_in() != compressed.len() as u64 {
            return Err(invalid_poi("oversized or trailing compressed POI data"));
        }
        let mut reader =
            pumpkin_nbt::deserializer::NbtReadHelperJava::new_preserving(Cursor::new(bytes));
        let root = Nbt::read_complete(&mut reader)
            .map_err(invalid_poi)?
            .root_tag;
        let data_version = require_current_data_version(&root).map_err(invalid_poi)?;
        let section_tags = root
            .get_compound("Sections")
            .ok_or_else(|| invalid_poi("missing or invalid Sections compound"))?;
        let mut sections = FxHashMap::default();
        for (key, tag) in &section_tags.child_tags {
            let section_y: i32 = key
                .parse()
                .map_err(|_| invalid_poi("invalid POI section coordinate"))?;
            if section_y.to_string() != key.as_ref() {
                return Err(invalid_poi("noncanonical POI section coordinate"));
            }
            let NbtTag::Compound(section) = tag else {
                return Err(invalid_poi("POI section must be a compound"));
            };
            // PoiSection.Packed defaults an absent Valid flag to false.
            let valid = match section.get("Valid") {
                None => 0,
                Some(NbtTag::Byte(value @ (0 | 1))) => *value,
                Some(_) => return Err(invalid_poi("invalid POI Valid flag")),
            };
            let record_tags = section
                .get_list("Records")
                .ok_or_else(|| invalid_poi("missing or invalid POI Records list"))?;
            let mut records = Vec::new();
            for tag in record_tags {
                let NbtTag::Compound(record) = tag else {
                    return Err(invalid_poi("POI record must be a compound"));
                };
                let [x, y, z] = match record.get("pos") {
                    Some(NbtTag::IntArray(pos)) if pos.len() == 3 => [pos[0], pos[1], pos[2]],
                    Some(_) => return Err(invalid_poi("POI pos must be a three-integer array")),
                    // Compatibility with earlier Pumpkin saves; modern pos takes precedence.
                    None => match (
                        record.get_int("x"),
                        record.get_int("y"),
                        record.get_int("z"),
                    ) {
                        (Some(x), Some(y), Some(z)) => [x, y, z],
                        _ => return Err(invalid_poi("missing POI position")),
                    },
                };
                if y >> 4 != section_y {
                    return Err(invalid_poi("POI record is stored in the wrong section"));
                }
                let poi_type = record
                    .get_string("type")
                    .ok_or_else(|| invalid_poi("missing or invalid POI type"))?
                    .to_owned();
                let free_tickets = match record.get("free_tickets") {
                    None => 0,
                    Some(NbtTag::Int(value)) => *value,
                    Some(_) => return Err(invalid_poi("free_tickets must be an integer")),
                };
                records.push(PoiEntry {
                    x,
                    y,
                    z,
                    poi_type,
                    free_tickets,
                    source: record.clone(),
                });
            }
            sections.insert(
                key.to_string(),
                PoiSectionData {
                    valid,
                    records,
                    source: section.clone(),
                },
            );
        }
        Ok(PoiChunkData {
            data_version,
            sections,
            source: root,
        })
    }

    pub fn save(&mut self, path: &Path) -> std::io::Result<()> {
        if !self.dirty {
            return Ok(());
        }

        let region_pos = region_coordinates(path)?;
        if self
            .entries
            .values()
            .any(|entry| (entry.x >> 9, entry.z >> 9) != region_pos)
        {
            return Err(invalid_poi("POI position is outside the target region"));
        }
        let mut chunk_indices: rustc_hash::FxHashSet<_> = self.chunks.keys().copied().collect();
        chunk_indices.extend(
            self.entries
                .values()
                .map(|entry| Self::chunk_index(entry.x >> 4, entry.z >> 4)),
        );
        let mut chunk_data_map: FxHashMap<usize, Vec<u8>> = FxHashMap::default();
        let mut saved_chunks = FxHashMap::default();
        for index in chunk_indices {
            let chunk = self.get_chunk_data(index);
            chunk_data_map.insert(index, Self::compress_chunk_data(&chunk)?);
            saved_chunks.insert(index, chunk);
        }

        // Build MCA file
        let mut location_table = [0u32; CHUNK_COUNT];
        let mut timestamp_table = [0u32; CHUNK_COUNT];
        let mut sector_data: Vec<Vec<u8>> = Vec::new();

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as u32);

        // Start after header (2 sectors)
        let mut current_sector: u32 = 2;

        for index in 0..CHUNK_COUNT {
            if let Some(compressed) = chunk_data_map.get(&index) {
                // Calculate sector count needed
                let data_len = compressed.len() + 5; // 4 bytes length + 1 byte compression + data
                let sector_count = data_len.div_ceil(SECTOR_SIZE) as u32;
                if sector_count > u8::MAX as u32 || current_sector > 0x00ff_ffff {
                    return Err(invalid_poi("POI chunk exceeds inline region allocation"));
                }

                // Build padded sector data
                let mut padded = Vec::with_capacity(sector_count as usize * SECTOR_SIZE);
                let length = (compressed.len() + 1) as u32; // +1 for compression byte
                padded.extend_from_slice(&length.to_be_bytes());
                padded.push(COMPRESSION_ZLIB);
                padded.extend_from_slice(compressed);
                // Pad to sector boundary
                padded.resize(sector_count as usize * SECTOR_SIZE, 0);

                location_table[index] = (current_sector << 8) | sector_count;
                timestamp_table[index] = timestamp;
                sector_data.push(padded);

                current_sector += sector_count;
            }
        }

        pumpkin_util::atomic_file::atomic_write(path, |file| -> std::io::Result<()> {
            for location in &location_table {
                file.write_all(&location.to_be_bytes())?;
            }
            for timestamp in &timestamp_table {
                file.write_all(&timestamp.to_be_bytes())?;
            }
            for data in &sector_data {
                file.write_all(data)?;
            }
            Ok(())
        })?;

        self.chunks = saved_chunks;
        self.dirty = false;
        self.dirty_chunks.clear();
        Ok(())
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        let file_data = match std::fs::read(path) {
            Ok(data) => data,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(err) => return Err(err),
        };
        // Vanilla RegionFile opens with CREATE before reading. Untouched POI
        // regions can therefore remain zero bytes even after a clean close.
        if file_data.is_empty() {
            return Ok(Self::new());
        }
        let (region_x, region_z) = region_coordinates(path)?;
        if file_data.len() < HEADER_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "POI region header is truncated",
            ));
        }

        let mut region = Self::new();
        let mut occupied = vec![false; file_data.len().div_ceil(SECTOR_SIZE)];
        for index in 0..CHUNK_COUNT {
            let invalid = |cause: &str| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Invalid POI chunk at index {index}: {cause}"),
                )
            };
            let offset = index * 4;
            let location = u32::from_be_bytes([
                file_data[offset],
                file_data[offset + 1],
                file_data[offset + 2],
                file_data[offset + 3],
            ]);
            let sector_offset = (location >> 8) as usize;
            let sector_count = (location & 0xFF) as usize;
            if sector_offset == 0 && sector_count == 0 {
                continue;
            }
            if sector_offset < 2 || sector_count == 0 {
                return Err(invalid("invalid sector location"));
            }
            let byte_offset = sector_offset * SECTOR_SIZE;
            let byte_end = byte_offset + sector_count * SECTOR_SIZE;
            if byte_end > file_data.len() {
                return Err(invalid("chunk sectors extend beyond the region file"));
            }

            for sector in &mut occupied[sector_offset..sector_offset + sector_count] {
                if *sector {
                    return Err(invalid("overlapping chunk sectors"));
                }
                *sector = true;
            }
            let chunk_bytes = &file_data[byte_offset..byte_end];
            let length = u32::from_be_bytes([
                chunk_bytes[0],
                chunk_bytes[1],
                chunk_bytes[2],
                chunk_bytes[3],
            ]) as usize;
            let compression = chunk_bytes[4];
            if compression != COMPRESSION_ZLIB {
                return Err(invalid("unsupported compression"));
            }
            if length < 1 || length > chunk_bytes.len() - 4 {
                return Err(invalid("invalid chunk length"));
            }
            let compressed = &chunk_bytes[5..5 + length - 1];
            let chunk_data = Self::decompress_chunk_data(compressed)
                .map_err(|err| invalid(&format!("failed to decode chunk: {err}")))?;
            for section in chunk_data.sections.values() {
                for entry in &section.records {
                    if (entry.x >> 9, entry.z >> 9) != (region_x, region_z)
                        || Self::chunk_index(entry.x >> 4, entry.z >> 4) != index
                    {
                        return Err(invalid("POI record is stored in the wrong chunk"));
                    }
                    if region
                        .entries
                        .insert((entry.x, entry.y, entry.z), entry.clone())
                        .is_some()
                    {
                        return Err(invalid("duplicate POI position"));
                    }
                }
            }
            region.chunks.insert(index, chunk_data);
        }
        region.dirty = false;
        Ok(region)
    }
}

fn region_coordinates(path: &Path) -> std::io::Result<(i32, i32)> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_poi("invalid POI region filename"))?;
    let parts: Vec<_> = name.split('.').collect();
    match parts.as_slice() {
        ["r", x, z, "mca"] => Ok((
            x.parse().map_err(invalid_poi)?,
            z.parse().map_err(invalid_poi)?,
        )),
        _ => Err(invalid_poi("invalid POI region filename")),
    }
}

fn invalid_poi(cause: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, cause.to_string())
}

/// Region-based POI storage using MCA format
pub struct PoiStorage {
    /// Path to the poi folder
    folder: PathBuf,
    /// Loaded regions, keyed by (`region_x`, `region_z`)
    regions: FxHashMap<(i32, i32), PoiRegion>,
}

impl PoiStorage {
    #[must_use]
    pub fn new(poi_folder: PathBuf) -> Self {
        Self {
            folder: poi_folder,
            regions: FxHashMap::default(),
        }
    }

    const fn region_coords(pos: &BlockPos) -> (i32, i32) {
        let chunk_x = pos.0.x >> 4;
        let chunk_z = pos.0.z >> 4;
        (chunk_x >> 5, chunk_z >> 5)
    }

    fn region_path(&self, rx: i32, rz: i32) -> PathBuf {
        self.folder.join(format!("r.{rx}.{rz}.mca"))
    }

    #[expect(
        clippy::panic,
        reason = "infallible POI access must not replace unreadable existing regions"
    )]
    fn get_or_load_region(&mut self, rx: i32, rz: i32) -> &mut PoiRegion {
        let path = self.region_path(rx, rz);
        self.regions.entry((rx, rz)).or_insert_with(|| {
            PoiRegion::load(&path).unwrap_or_else(|cause| {
                panic!("Failed to load POI region {}: {cause}", path.display());
            })
        })
    }

    pub fn add(&mut self, pos: BlockPos, poi_type: &str) {
        self.add_with_free_tickets(pos, poi_type, 0);
    }

    pub fn add_with_free_tickets(&mut self, pos: BlockPos, poi_type: &str, free_tickets: i32) {
        let (rx, rz) = Self::region_coords(&pos);
        let region = self.get_or_load_region(rx, rz);
        region.add(PoiEntry {
            x: pos.0.x,
            y: pos.0.y,
            z: pos.0.z,
            poi_type: poi_type.to_string(),
            free_tickets,
            source: NbtCompound::new(),
        });
    }

    pub fn add_portal(&mut self, pos: BlockPos) {
        self.add(pos, POI_TYPE_NETHER_PORTAL);
    }

    pub fn remove(&mut self, pos: &BlockPos) -> bool {
        let (rx, rz) = Self::region_coords(pos);
        let region = self.get_or_load_region(rx, rz);
        region.remove(pos)
    }

    /// Get all POI positions within a square radius (for portal search)
    #[expect(clippy::similar_names)]
    pub fn get_in_square(
        &mut self,
        center: BlockPos,
        radius: i32,
        poi_type: Option<&str>,
    ) -> Vec<BlockPos> {
        let min_x = center.0.x - radius;
        let max_x = center.0.x + radius;
        let min_z = center.0.z - radius;
        let max_z = center.0.z + radius;

        // Calculate which regions we need to check
        let min_rx = (min_x >> 4) >> 5;
        let max_rx = (max_x >> 4) >> 5;
        let min_rz = (min_z >> 4) >> 5;
        let max_rz = (max_z >> 4) >> 5;

        let mut results = Vec::new();

        for rx in min_rx..=max_rx {
            for rz in min_rz..=max_rz {
                let region = self.get_or_load_region(rx, rz);
                for entry in region.get_all() {
                    if let Some(filter_type) = poi_type
                        && entry.poi_type != filter_type
                    {
                        continue;
                    }

                    let dx = (entry.x - center.0.x).abs();
                    let dz = (entry.z - center.0.z).abs();
                    if dx <= radius && dz <= radius {
                        results.push(entry.pos());
                    }
                }
            }
        }

        results
    }

    /// Finds the closest POI whose type matches `matches`, considering
    /// entries within `radius` blocks of `center` on the x/z axes (like
    /// vanilla's `PoiManager.findClosestWithType`: a chebyshev square gather
    /// followed by picking the smallest 3D squared distance).
    ///
    /// Returns the entry's position together with its type.
    pub fn find_closest_matching(
        &mut self,
        center: BlockPos,
        radius: i32,
        matches: impl Fn(&str) -> bool,
    ) -> Option<(BlockPos, String)> {
        let min_rx = ((center.0.x - radius) >> 4) >> 5;
        let max_rx = ((center.0.x + radius) >> 4) >> 5;
        let min_rz = ((center.0.z - radius) >> 4) >> 5;
        let max_rz = ((center.0.z + radius) >> 4) >> 5;

        let mut best: Option<(BlockPos, String, i64)> = None;

        for rx in min_rx..=max_rx {
            for rz in min_rz..=max_rz {
                let region = self.get_or_load_region(rx, rz);
                for entry in region.get_all() {
                    if (entry.x - center.0.x).abs() > radius
                        || (entry.z - center.0.z).abs() > radius
                        || !matches(&entry.poi_type)
                    {
                        continue;
                    }

                    let dx = i64::from(entry.x - center.0.x);
                    let dy = i64::from(entry.y - center.0.y);
                    let dz = i64::from(entry.z - center.0.z);
                    let distance_sq = dx * dx + dy * dy + dz * dz;

                    if best.as_ref().is_none_or(|(_, _, d)| distance_sq < *d) {
                        best = Some((entry.pos(), entry.poi_type.clone(), distance_sq));
                    }
                }
            }
        }

        best.map(|(pos, poi_type, _)| (pos, poi_type))
    }

    pub fn save_all(&mut self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.folder)?;

        let mut saved = 0;
        for ((rx, rz), region) in &mut self.regions {
            if region.is_dirty() {
                let path = self.folder.join(format!("r.{rx}.{rz}.mca"));
                region.save(&path)?;
                saved += 1;
            }
        }

        if saved > 0 {
            info!("Saved {saved} POI region(s)");
        }
        Ok(())
    }

    /// Get count of loaded regions
    #[must_use]
    pub fn loaded_region_count(&self) -> usize {
        self.regions.len()
    }

    /// Get total POI count across all loaded regions
    #[must_use]
    pub fn total_poi_count(&self) -> usize {
        self.regions.values().map(|r| r.get_all().len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_root() -> NbtCompound {
        // PoiRecord.Packed / PoiSection.Packed schema from the official 26.3 server.
        let mut record = NbtCompound::new();
        record.put("pos", NbtTag::IntArray(vec![1, 64, 2]));
        record.put_string("type", "minecraft:home".to_owned());
        record.put_int("free_tickets", 1);
        record.put_string("record_extension", "keep".to_owned());
        let mut section = NbtCompound::new();
        section.put_byte("Valid", 1);
        section.put_list("Records", vec![NbtTag::Compound(record)]);
        section.put_long("section_extension", 42);
        let mut empty = NbtCompound::new();
        empty.put_byte("Valid", 0);
        empty.put_list("Records", Vec::new());
        let mut sections = NbtCompound::new();
        sections.put_compound("4", section);
        sections.put_compound("5", empty);
        let mut root = NbtCompound::new();
        root.put_int("DataVersion", DATA_VERSION);
        root.put_compound("Sections", sections);
        root.put_long("root_extension", 99);
        root
    }

    fn fixture_section(root: &mut NbtCompound) -> &mut NbtCompound {
        let NbtTag::Compound(sections) = root.child_tags.get_mut("Sections").unwrap() else {
            panic!()
        };
        let NbtTag::Compound(section) = sections.child_tags.get_mut("4").unwrap() else {
            panic!()
        };
        section
    }

    fn first_record(section: &mut NbtCompound) -> &mut NbtCompound {
        let NbtTag::List(records) = section.child_tags.get_mut("Records").unwrap() else {
            panic!()
        };
        let NbtTag::Compound(record) = &mut records[0] else {
            panic!()
        };
        record
    }

    fn compressed_root(root: NbtCompound) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(&Nbt::from(root).try_write_preserving().unwrap())
            .unwrap();
        encoder.finish().unwrap()
    }

    fn region_bytes(root: NbtCompound) -> Vec<u8> {
        let compressed = compressed_root(root);
        let sectors = (compressed.len() + 5).div_ceil(SECTOR_SIZE);
        let mut bytes = vec![0; HEADER_SIZE + sectors * SECTOR_SIZE];
        bytes[..4].copy_from_slice(&((2 << 8) | sectors as u32).to_be_bytes());
        bytes[HEADER_SIZE..HEADER_SIZE + 4]
            .copy_from_slice(&((compressed.len() + 1) as u32).to_be_bytes());
        bytes[HEADER_SIZE + 4] = COMPRESSION_ZLIB;
        bytes[HEADER_SIZE + 5..HEADER_SIZE + 5 + compressed.len()].copy_from_slice(&compressed);
        bytes
    }

    #[test]
    fn poi_admission_requires_current_int_data_version_without_rewriting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("r.0.0.mca");
        let current = region_bytes(fixture_root());
        std::fs::write(&path, &current).unwrap();
        let region = PoiRegion::load(&path).expect("current schema must load");
        assert_eq!(region.chunks[&0].data_version, DATA_VERSION);
        assert_eq!(std::fs::read(&path).unwrap(), current);

        for version in [
            None,
            Some(NbtTag::Int(3465)),
            Some(NbtTag::Int(3578)),
            Some(NbtTag::Int(4435)),
            Some(NbtTag::Int(4903)),
            Some(NbtTag::Int(DATA_VERSION - 1)),
            Some(NbtTag::Int(DATA_VERSION + 1)),
            Some(NbtTag::Long(i64::from(DATA_VERSION))),
            Some(NbtTag::String(DATA_VERSION.to_string().into())),
        ] {
            let mut root = fixture_root();
            root.child_tags.remove("DataVersion");
            if let Some(version) = version {
                root.put("DataVersion", version);
            }
            let original = region_bytes(root);
            std::fs::write(&path, &original).unwrap();
            let Err(error) = PoiRegion::load(&path) else {
                panic!("unsupported schema must not become a writable POI region");
            };
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(
                error
                    .to_string()
                    .contains(&format!("TAG_Int {DATA_VERSION}"))
            );
            assert!(
                error
                    .to_string()
                    .contains("--forceUpgrade --recreateRegionFiles")
            );
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
    }

    #[test]
    fn vanilla_poi_positions_metadata_and_empty_sections_survive_updates() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("r.0.0.mca");
        std::fs::write(&path, region_bytes(fixture_root())).unwrap();
        let mut region = PoiRegion::load(&path).unwrap();
        let position = BlockPos::new(1, 64, 2);
        assert_eq!(region.get_all()[0].pos(), position);
        let mut changed = region.get_all()[0].clone();
        changed.free_tickets = 0;
        region.add(changed);
        region.save(&path).unwrap();
        let reloaded = PoiRegion::load(&path).unwrap();
        let chunk = &reloaded.chunks[&0];
        assert_eq!(chunk.data_version, DATA_VERSION);
        assert_eq!(chunk.source.get_long("root_extension"), Some(99));
        assert_eq!(
            chunk.sections["4"].source.get_long("section_extension"),
            Some(42)
        );
        assert_eq!(chunk.sections["4"].records[0].free_tickets, 0);
        assert_eq!(
            chunk.sections["4"].records[0]
                .source
                .get_string("record_extension"),
            Some("keep")
        );
        assert_eq!(chunk.sections["5"].valid, 0);
        assert!(chunk.sections["5"].records.is_empty());

        region.remove(&position);
        region.save(&path).unwrap();
        let emptied = PoiRegion::load(&path).unwrap();
        assert!(emptied.get_all().is_empty());
        assert!(emptied.chunks[&0].sections["4"].records.is_empty());
        assert_eq!(
            emptied.chunks[&0].source.get_long("root_extension"),
            Some(99)
        );
        region.add(PoiEntry::new_portal(position));
        region.save(&path).unwrap();
        let replaced = PoiRegion::load(&path).unwrap();
        assert!(
            replaced.get_all()[0]
                .source
                .get("record_extension")
                .is_none()
        );
    }

    #[test]
    fn legacy_poi_coordinates_upgrade_and_modern_coordinates_take_precedence() {
        let mut root = fixture_root();
        let record = first_record(fixture_section(&mut root));
        record.put_int("x", 9);
        record.put_int("y", 64);
        record.put_int("z", 9);
        let modern = PoiRegion::decompress_chunk_data(&compressed_root(root.clone())).unwrap();
        assert_eq!(modern.sections["4"].records[0].x, 1);
        let saved =
            PoiRegion::decompress_chunk_data(&PoiRegion::compress_chunk_data(&modern).unwrap())
                .unwrap();
        assert!(saved.sections["4"].records[0].source.get("x").is_none());
        let record = first_record(fixture_section(&mut root));
        record.child_tags.remove("pos");
        let legacy = PoiRegion::decompress_chunk_data(&compressed_root(root)).unwrap();
        assert_eq!(legacy.sections["4"].records[0].x, 9);
        let saved =
            PoiRegion::decompress_chunk_data(&PoiRegion::compress_chunk_data(&legacy).unwrap())
                .unwrap();
        assert_eq!(
            saved.sections["4"].records[0].source.get("pos"),
            Some(&NbtTag::IntArray(vec![9, 64, 9]))
        );
        assert!(saved.sections["4"].records[0].source.get("x").is_none());
    }

    #[test]
    fn malformed_poi_schema_is_rejected_without_replacing_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("r.0.0.mca");
        for mutation in 0..8 {
            let mut root = fixture_root();
            if mutation == 0 {
                root.child_tags.remove("Sections");
            } else {
                let section = fixture_section(&mut root);
                if mutation == 1 {
                    section.put_string("Valid", "wrong type".into());
                } else if mutation == 2 {
                    section.put_list("Records", vec![NbtTag::Int(1)]);
                } else {
                    let record = first_record(section);
                    match mutation {
                        3 => {
                            record.child_tags.remove("type");
                        }
                        4 => record.put("pos", NbtTag::IntArray(vec![1, 64])),
                        5 => record.put("pos", NbtTag::IntArray(vec![1, 80, 2])),
                        6 => record.put("pos", NbtTag::IntArray(vec![512, 64, 2])),
                        _ => record.put_string("free_tickets", "bad".into()),
                    }
                }
            }
            let original = region_bytes(root);
            std::fs::write(&path, &original).unwrap();
            assert!(
                PoiRegion::load(&path).is_err(),
                "accepted mutation {mutation}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
        let mut compressed = compressed_root(fixture_root());
        compressed.push(42);
        assert!(PoiRegion::decompress_chunk_data(&compressed).is_err());
    }

    #[test]
    fn failed_poi_serialization_keeps_prior_file_and_dirty_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("r.0.0.mca");
        let original = region_bytes(fixture_root());
        std::fs::write(&path, &original).unwrap();
        let mut region = PoiRegion::load(&path).unwrap();
        region
            .entries
            .get_mut(&(1, 64, 2))
            .unwrap()
            .source
            .put_string("invalid", "x".repeat(65_536));
        region.dirty = true;
        assert!(region.save(&path).is_err());
        assert!(region.is_dirty());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn vanilla_empty_poi_region_loads_without_rewriting_and_can_store_new_entries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("r.0.0.mca");
        std::fs::write(&path, []).unwrap();
        let region = PoiRegion::load(&path).unwrap();
        assert!(region.get_all().is_empty());
        assert!(!region.is_dirty());

        let mut storage = PoiStorage::new(directory.path().to_path_buf());
        let portal = BlockPos::new(0, 64, 0);
        assert!(
            storage
                .get_in_square(portal, 0, Some(POI_TYPE_NETHER_PORTAL))
                .is_empty()
        );
        storage.save_all().unwrap();
        assert!(std::fs::read(&path).unwrap().is_empty());

        storage.add_portal(portal);
        storage.save_all().unwrap();
        let loaded = PoiRegion::load(&path).unwrap();
        assert_eq!(loaded.get_all().len(), 1);
        assert_eq!(loaded.get_all()[0].pos(), portal);
        assert_eq!(loaded.get_all()[0].poi_type, POI_TYPE_NETHER_PORTAL);
    }

    #[test]
    fn malformed_existing_poi_regions_never_become_saveable_replacements() {
        let mut truncated = vec![0; HEADER_SIZE];
        truncated[..4].copy_from_slice(&[0, 0, 2, 1]);
        let mut corrupt_payload = vec![0; HEADER_SIZE + SECTOR_SIZE];
        corrupt_payload[..4].copy_from_slice(&[0, 0, 2, 1]);
        corrupt_payload[HEADER_SIZE..HEADER_SIZE + 4].copy_from_slice(&4u32.to_be_bytes());
        corrupt_payload[HEADER_SIZE + 4] = COMPRESSION_ZLIB;
        corrupt_payload[HEADER_SIZE + 5..HEADER_SIZE + 8].copy_from_slice(b"bad");
        let mut unsupported = corrupt_payload.clone();
        unsupported[HEADER_SIZE + 4] = 127;
        for original in [vec![0], truncated, corrupt_payload, unsupported] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("r.0.0.mca");
            std::fs::write(&path, &original).unwrap();
            assert_eq!(
                PoiRegion::load(&path).err().unwrap().kind(),
                std::io::ErrorKind::InvalidData
            );
            let mut storage = PoiStorage::new(directory.path().to_path_buf());
            let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                storage.add_portal(BlockPos::new(0, 64, 0));
            }));
            assert!(failure.is_err());
            assert_eq!(storage.loaded_region_count(), 0);
            storage.save_all().unwrap();
            assert_eq!(std::fs::read(path).unwrap(), original);
        }
    }

    #[test]
    fn poi_entry() {
        let entry = PoiEntry::new_portal(BlockPos(Vector3::new(100, 64, 200)));
        assert_eq!(entry.x, 100);
        assert_eq!(entry.y, 64);
        assert_eq!(entry.z, 200);
        assert_eq!(entry.poi_type, POI_TYPE_NETHER_PORTAL);
    }

    #[test]
    fn poi_region() {
        let mut region = PoiRegion::new();
        region.add(PoiEntry::new_portal(BlockPos(Vector3::new(100, 64, 200))));
        region.add(PoiEntry::new_portal(BlockPos(Vector3::new(101, 64, 200))));

        assert_eq!(region.get_all().len(), 2);
        assert!(region.is_dirty());

        region.remove(&BlockPos(Vector3::new(100, 64, 200)));
        assert_eq!(region.get_all().len(), 1);
    }

    #[test]
    fn poi_find_closest_matching() {
        let mut storage = PoiStorage::new(std::env::temp_dir().join("pumpkin_poi_closest_test"));

        storage.add_portal(BlockPos(Vector3::new(100, 64, 100)));
        storage.add_portal(BlockPos(Vector3::new(120, 64, 100)));
        storage.add(BlockPos(Vector3::new(101, 64, 100)), "minecraft:home");

        let center = BlockPos(Vector3::new(105, 64, 100));
        let (pos, poi_type) = storage
            .find_closest_matching(center, 256, |t| t == POI_TYPE_NETHER_PORTAL)
            .unwrap();
        assert_eq!(pos, BlockPos(Vector3::new(100, 64, 100)));
        assert_eq!(poi_type, POI_TYPE_NETHER_PORTAL);

        // The overall closest one ignores the type filter mismatch above.
        let (pos, poi_type) = storage
            .find_closest_matching(center, 256, |_| true)
            .unwrap();
        assert_eq!(pos, BlockPos(Vector3::new(101, 64, 100)));
        assert_eq!(poi_type, "minecraft:home");

        assert!(
            storage
                .find_closest_matching(center, 256, |t| t == "minecraft:lodestone")
                .is_none()
        );
        // Out of horizontal range.
        assert!(
            storage
                .find_closest_matching(BlockPos(Vector3::new(1000, 64, 100)), 16, |_| true)
                .is_none()
        );
    }

    #[test]
    fn poi_storage_mca() {
        let dir = std::env::temp_dir().join("pumpkin_poi_mca_test");
        let _ = std::fs::remove_dir_all(&dir);

        let mut storage = PoiStorage::new(dir.join("poi"));

        storage.add_portal(BlockPos(Vector3::new(100, 64, 100)));
        storage.add_portal(BlockPos(Vector3::new(110, 64, 100)));
        storage.add_portal(BlockPos(Vector3::new(1000, 64, 1000))); // Different region

        let results = storage.get_in_square(
            BlockPos(Vector3::new(105, 64, 100)),
            16,
            Some(POI_TYPE_NETHER_PORTAL),
        );
        assert_eq!(results.len(), 2);

        storage.save_all().unwrap();

        // Verify .mca file was created
        let mca_path = dir.join("poi").join("r.0.0.mca");
        assert!(mca_path.exists());

        // Reload and verify
        let mut storage2 = PoiStorage::new(dir.join("poi"));
        let results2 = storage2.get_in_square(
            BlockPos(Vector3::new(105, 64, 100)),
            16,
            Some(POI_TYPE_NETHER_PORTAL),
        );
        assert_eq!(results2.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
