use pumpkin_nbt::compound::NbtCompound;
use std::fs::{File, create_dir_all};
use std::io;
use std::path::PathBuf;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tracing::{debug, error};
use uuid::Uuid;

/// Manages the storage and retrieval of player data from disk and memory cache.
///
/// This struct provides functions to load and save player data to/from NBT files,
/// with a memory cache to handle player disconnections temporarily.
pub struct PlayerDataStorage {
    /// Path to the directory where player data is stored
    data_path: PathBuf,
    /// Whether player data saving is enabled
    save_enabled: bool,
    load_failures: Mutex<HashMap<Uuid, Arc<Mutex<bool>>>>,
}

#[derive(Debug, thiserror::Error)]
pub enum PlayerDataError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("NBT error: {0}")]
    Nbt(String),
}

impl PlayerDataStorage {
    /// Creates a new `PlayerDataStorage` with the specified data path and cache expiration time.
    pub fn new(data_path: impl Into<PathBuf>, enabled: bool) -> Self {
        let path = data_path.into();
        if !path.exists()
            && let Err(e) = create_dir_all(&path)
        {
            error!(
                "Failed to create player data directory at {}: {e}",
                path.display()
            );
        }

        Self {
            data_path: path,
            save_enabled: enabled,
            load_failures: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub const fn get_data_path(&self) -> &PathBuf {
        &self.data_path
    }

    #[must_use]
    pub const fn is_save_enabled(&self) -> bool {
        self.save_enabled
    }

    pub const fn set_save_enabled(&mut self, enabled: bool) {
        self.save_enabled = enabled;
    }

    /// Returns the path for a player's data file based on their UUID.
    #[must_use]
    pub fn get_player_data_path(&self, uuid: &Uuid) -> PathBuf {
        self.get_data_path().join(format!("{uuid}.dat"))
    }

    /// Loads player data from NBT file or cache.
    ///
    /// This function first checks if player data exists in the cache.
    /// If not, it attempts to load the data from a .dat file on disk.
    ///
    /// # Arguments
    ///
    /// * `uuid` - The UUID of the player to load data for.
    ///
    /// # Returns
    ///
    /// A Result containing either the player's NBT data or an error.
    pub fn load_player_data(&self, uuid: &Uuid) -> Result<(bool, NbtCompound), PlayerDataError> {
        // If player data saving is disabled, return empty data
        if !self.is_save_enabled() {
            return Ok((false, NbtCompound::new()));
        }

        let state = self.load_failure(uuid);
        let mut failed = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *failed = true;
        let path = self.get_player_data_path(uuid);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                *failed = false;
                return Ok((false, NbtCompound::new()));
            }
            Err(error) => return Err(PlayerDataError::Io(error)),
        };

        match read_player_compound(file) {
            Ok(nbt) => {
                *failed = false;
                debug!("Loaded player data for {uuid} from disk");
                Ok((true, nbt))
            }
            Err(e) => {
                error!("Failed to read player data for {uuid}: {e}");
                Err(PlayerDataError::Nbt(e.to_string()))
            }
        }
    }

    /// Saves player data to NBT file and updates cache.
    ///
    /// This function saves the player's data to a .dat file on disk and also
    /// updates the in-memory cache with the latest data.
    ///
    /// # Arguments
    ///
    /// * `uuid` - The UUID of the player to save data for.
    /// * `data` - The NBT compound data to save.
    ///
    /// # Returns
    ///
    /// A Result indicating success or the error that occurred.
    pub fn save_player_data(&self, uuid: &Uuid, data: NbtCompound) -> Result<(), PlayerDataError> {
        // Skip saving if disabled in config
        if !self.is_save_enabled() {
            return Ok(());
        }

        let state = self.load_failure(uuid);
        let failed = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *failed {
            return Err(PlayerDataError::Io(io::Error::other(format!(
                "refusing to overwrite player data after a failed load: {uuid}"
            ))));
        }
        let path = self.get_player_data_path(uuid);
        pumpkin_util::atomic_file::atomic_write(&path, |file| {
            write_player_compound(data, file)
                .map_err(|error| PlayerDataError::Nbt(error.to_string()))
        })?;
        debug!("Saved player data for {uuid} to disk");
        Ok(())
    }

    fn load_failure(&self, uuid: &Uuid) -> Arc<Mutex<bool>> {
        self.load_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(*uuid)
            .or_default()
            .clone()
    }
}

// Player reads must consume complete gzip/NBT payloads. The compound/list
// reader below propagates EOF and preserves vanilla wire structures directly.
fn read_player_compound(file: File) -> Result<NbtCompound, pumpkin_nbt::Error> {
    use pumpkin_nbt::deserializer::NbtReadHelper;
    use std::io::Read;
    let mut bytes = Vec::new();
    flate2::read::MultiGzDecoder::new(file)
        .read_to_end(&mut bytes)
        .map_err(pumpkin_nbt::Error::Incomplete)?;
    let mut reader = pumpkin_nbt::deserializer::NbtReadHelperJava::new(std::io::Cursor::new(bytes));
    let tag_id = reader.get_u8()?;
    if tag_id != pumpkin_nbt::COMPOUND_ID {
        return Err(pumpkin_nbt::Error::NoRootCompound(tag_id));
    }
    reader.get_string()?;
    let root = read_player_compound_content(&mut reader, 0)?;
    let input = reader.reader();
    if input.position() != input.get_ref().len() as u64 {
        return Err(pumpkin_nbt::Error::Incomplete(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing NBT bytes after player root compound",
        )));
    }
    validate_player_compound(&root, 0)?;
    Ok(root)
}

fn write_player_compound(data: NbtCompound, file: &mut File) -> Result<(), pumpkin_nbt::Error> {
    use pumpkin_nbt::serializer::NbtWriteHelper;
    validate_player_compound(&data, 0)?;
    let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut writer = pumpkin_nbt::serializer::NbtWriteHelperJava::new(&mut encoder);
    writer.write_u8(pumpkin_nbt::COMPOUND_ID)?;
    writer.write_string("")?;
    // Propagate serializer errors directly instead of Nbt::write's partial bytes.
    write_player_compound_content(data, &mut writer)?;
    encoder.finish().map_err(pumpkin_nbt::Error::Incomplete)?;
    Ok(())
}

type PlayerNbtReader = pumpkin_nbt::deserializer::NbtReadHelperJava<std::io::Cursor<Vec<u8>>>;

// Keep compound/list wire structure intact: the generic decoder flattens
// empty-name-child compounds in lists, while its writer wraps them again.
fn read_player_compound_content(
    reader: &mut PlayerNbtReader,
    depth: usize,
) -> Result<NbtCompound, pumpkin_nbt::Error> {
    use pumpkin_nbt::deserializer::NbtReadHelper;
    if depth > pumpkin_nbt::MAX_NBT_DEPTH {
        return Err(pumpkin_nbt::Error::MaxDepthExceeded);
    }
    let mut compound = NbtCompound::new();
    loop {
        let tag_id = reader.get_u8()?;
        if tag_id == pumpkin_nbt::END_ID {
            return Ok(compound);
        }
        let name = reader.get_string()?.into_owned();
        if compound.child_tags.contains_key(name.as_str()) {
            return Err(pumpkin_nbt::Error::SerdeError(format!(
                "duplicate player NBT compound key: {name}",
            )));
        }
        let tag = read_player_tag(reader, tag_id, depth + 1)?;
        compound.child_tags.insert(name.into(), tag);
    }
}

fn read_player_tag(
    reader: &mut PlayerNbtReader,
    tag_id: u8,
    depth: usize,
) -> Result<pumpkin_nbt::tag::NbtTag, pumpkin_nbt::Error> {
    use pumpkin_nbt::{Error, deserializer::NbtReadHelper, tag::NbtTag};
    if depth > pumpkin_nbt::MAX_NBT_DEPTH {
        return Err(Error::MaxDepthExceeded);
    }
    match tag_id {
        pumpkin_nbt::COMPOUND_ID => {
            read_player_compound_content(reader, depth + 1).map(NbtTag::Compound)
        }
        pumpkin_nbt::LIST_ID => {
            let element_id = reader.get_u8()?;
            let count = reader.get_i32()?;
            if count < 0 {
                return Err(Error::NegativeLength(count));
            }
            if element_id > pumpkin_nbt::LONG_ARRAY_ID
                || (element_id == pumpkin_nbt::END_ID && count > 0)
            {
                return Err(Error::InvalidListTag(element_id));
            }
            let count = count as usize;
            if count > pumpkin_nbt::MAX_ARRAY_LENGTH {
                return Err(Error::LargeLength(count));
            }
            let mut elements = Vec::new();
            for _ in 0..count {
                elements.push(read_player_tag(reader, element_id, depth + 1)?);
            }
            Ok(NbtTag::List(elements))
        }
        _ => NbtTag::deserialize_data_depth(reader, tag_id, depth),
    }
}

fn write_player_compound_content<W: pumpkin_nbt::serializer::NbtWriteHelper>(
    data: NbtCompound,
    writer: &mut W,
) -> Result<(), pumpkin_nbt::Error> {
    for (name, tag) in data.child_tags {
        writer.write_u8(tag.get_type_id())?;
        writer.write_string(&name)?;
        write_player_tag(tag, writer)?;
    }
    writer.write_u8(pumpkin_nbt::END_ID)
}

fn write_player_tag<W: pumpkin_nbt::serializer::NbtWriteHelper>(
    tag: pumpkin_nbt::tag::NbtTag,
    writer: &mut W,
) -> Result<(), pumpkin_nbt::Error> {
    use pumpkin_nbt::tag::NbtTag;
    match tag {
        NbtTag::Compound(compound) => write_player_compound_content(compound, writer),
        NbtTag::List(elements) => {
            writer.write_u8(
                elements
                    .first()
                    .map_or(pumpkin_nbt::END_ID, NbtTag::get_type_id),
            )?;
            writer.write_i32(elements.len() as i32)?;
            for element in elements {
                write_player_tag(element, writer)?;
            }
            Ok(())
        }
        _ => tag.serialize_data(writer),
    }
}

// The generic serializer accepts named END tags and silently wraps mixed
// lists. Those can truncate or change player state despite successful IO.
// Validate the in-memory tree before serializing, using the reader's depth bound.
fn validate_player_compound(data: &NbtCompound, depth: usize) -> Result<(), pumpkin_nbt::Error> {
    if depth > pumpkin_nbt::MAX_NBT_DEPTH {
        return Err(pumpkin_nbt::Error::MaxDepthExceeded);
    }
    for (name, tag) in &data.child_tags {
        if matches!(tag, pumpkin_nbt::tag::NbtTag::End) {
            return Err(pumpkin_nbt::Error::SerdeError(format!(
                "named TAG_End is not valid player NBT: {name}",
            )));
        }
        validate_player_tag(tag, depth + 1)?;
    }
    Ok(())
}

fn validate_player_tag(
    tag: &pumpkin_nbt::tag::NbtTag,
    depth: usize,
) -> Result<(), pumpkin_nbt::Error> {
    use pumpkin_nbt::{Error, tag::NbtTag};
    if depth > pumpkin_nbt::MAX_NBT_DEPTH {
        return Err(Error::MaxDepthExceeded);
    }
    let length = match tag {
        NbtTag::List(elements) => elements.len(),
        NbtTag::ByteArray(elements) => elements.len(),
        NbtTag::IntArray(elements) => elements.len(),
        NbtTag::LongArray(elements) => elements.len(),
        _ => 0,
    };
    if length > pumpkin_nbt::MAX_ARRAY_LENGTH {
        return Err(Error::LargeLength(length));
    }
    match tag {
        NbtTag::Compound(compound) => validate_player_compound(compound, depth + 1),
        NbtTag::List(elements) => {
            if let Some(first) = elements.first() {
                let expected = first.get_type_id();
                if expected == pumpkin_nbt::END_ID {
                    return Err(Error::InvalidListTag(expected));
                }
                for element in elements {
                    if element.get_type_id() != expected {
                        return Err(Error::SerdeError(
                            "mixed tag types in player NBT list".to_owned(),
                        ));
                    }
                    validate_player_tag(element, depth + 1)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn corrupt_player_input_is_not_replaced_by_empty_data() {
        let temp = tempdir().unwrap();
        let storage = PlayerDataStorage::new(temp.path(), true);
        let uuid = Uuid::new_v4();
        let path = storage.get_player_data_path(&uuid);
        let corrupt = b"existing corrupt gzip player data";
        std::fs::write(&path, corrupt).unwrap();
        assert!(storage.load_player_data(&uuid).is_err());
        assert!(
            storage
                .save_player_data(&uuid, NbtCompound::new())
                .unwrap_err()
                .to_string()
                .contains("refusing to overwrite")
        );
        assert_eq!(std::fs::read(&path).unwrap(), corrupt);
    }

    #[test]
    fn valid_gzip_with_missing_nbt_end_is_rejected_and_preserved() {
        let temp = tempdir().unwrap();
        let storage = PlayerDataStorage::new(temp.path(), true);
        let uuid = Uuid::new_v4();
        let path = storage.get_player_data_path(&uuid);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        // Named root compound, empty name, byte tag "x" = 1; missing TAG_End.
        encoder.write_all(&[10, 0, 0, 1, 0, 1, b'x', 1]).unwrap();
        let corrupt = encoder.finish().unwrap();
        std::fs::write(&path, &corrupt).unwrap();
        assert!(storage.load_player_data(&uuid).is_err());
        assert!(storage.save_player_data(&uuid, NbtCompound::new()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), corrupt);
    }

    #[test]
    fn empty_name_child_compound_list_preserves_its_wire_tree() {
        use pumpkin_nbt::tag::NbtTag;
        let temp = tempdir().unwrap();
        let storage = PlayerDataStorage::new(temp.path(), true);
        let uuid = Uuid::new_v4();
        let path = storage.get_player_data_path(&uuid);
        // Root list of COMPOUND elements. Each element has one empty-name
        // child, with different child types. They are not a mixed NBT list.
        let mut payload = vec![10, 0, 0, 9, 0, 11];
        payload.extend_from_slice(b"plugin:list");
        payload.extend_from_slice(&[10, 0, 0, 0, 2, 3, 0, 0, 0, 0, 0, 7, 0, 8, 0, 0, 0, 5]);
        payload.extend_from_slice(b"value");
        payload.extend_from_slice(&[0, 0]);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&payload).unwrap();
        std::fs::write(&path, encoder.finish().unwrap()).unwrap();
        let data = storage.load_player_data(&uuid).unwrap().1;
        let mut integer = NbtCompound::new();
        integer.put_int("", 7);
        let mut string = NbtCompound::new();
        string.put_string("", "value".to_owned());
        assert_eq!(
            data.get("plugin:list"),
            Some(&NbtTag::List(vec![
                NbtTag::Compound(integer),
                NbtTag::Compound(string),
            ]))
        );
        storage.save_player_data(&uuid, data.clone()).unwrap();
        assert_eq!(storage.load_player_data(&uuid).unwrap().1, data);
        let mut saved_payload = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::MultiGzDecoder::new(File::open(&path).unwrap()),
            &mut saved_payload,
        )
        .unwrap();
        assert_eq!(saved_payload, payload);
    }

    #[test]
    fn duplicate_keys_and_invalid_collection_bounds_fail_closed() {
        let temp = tempdir().unwrap();
        let storage = PlayerDataStorage::new(temp.path(), true);
        let uuid = Uuid::new_v4();
        let path = storage.get_player_data_path(&uuid);
        let too_many = (pumpkin_nbt::MAX_ARRAY_LENGTH as i32 + 1).to_be_bytes();
        let mut large_array = vec![10, 0, 0, 7, 0, 1, b'x'];
        large_array.extend_from_slice(&too_many);
        let mut large_list = vec![10, 0, 0, 9, 0, 1, b'x', 1];
        large_list.extend_from_slice(&too_many);
        for payload in [
            vec![10, 0, 0, 1, 0, 1, b'x', 1, 1, 0, 1, b'x', 2, 0],
            vec![10, 0, 0, 9, 0, 1, b'x', 0, 0, 0, 0, 1, 0],
            vec![10, 0, 0, 9, 0, 1, b'x', 1, 255, 255, 255, 255, 0],
            large_array,
            large_list,
        ] {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&payload).unwrap();
            let corrupt = encoder.finish().unwrap();
            std::fs::write(&path, &corrupt).unwrap();
            assert!(storage.load_player_data(&uuid).is_err());
            assert!(storage.save_player_data(&uuid, NbtCompound::new()).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), corrupt);
        }
    }

    #[test]
    fn invalid_runtime_end_and_mixed_list_tags_cannot_replace_player_data() {
        use pumpkin_nbt::tag::NbtTag;
        let temp = tempdir().unwrap();
        let storage = PlayerDataStorage::new(temp.path(), true);
        let uuid = Uuid::new_v4();
        let mut original = NbtCompound::new();
        original.put_int("XpLevel", 27);
        storage.save_player_data(&uuid, original).unwrap();
        let path = storage.get_player_data_path(&uuid);
        let retained = std::fs::read(&path).unwrap();
        let mut runtime_components = NbtCompound::new();
        runtime_components.put("minecraft:unimplemented_component", NbtTag::End);
        let mut stack = NbtCompound::new();
        stack.put_compound("components", runtime_components);
        for invalid in [
            NbtTag::List(vec![NbtTag::Compound(stack)]),
            NbtTag::List(vec![NbtTag::End]),
            NbtTag::List(vec![NbtTag::Int(1), NbtTag::String("mixed".into())]),
            NbtTag::ByteArray(vec![0; pumpkin_nbt::MAX_ARRAY_LENGTH + 1].into()),
        ] {
            let mut replacement = NbtCompound::new();
            replacement.put("Inventory", invalid);
            assert!(matches!(
                storage.save_player_data(&uuid, replacement),
                Err(PlayerDataError::Nbt(_))
            ));
            assert_eq!(std::fs::read(&path).unwrap(), retained);
            assert_eq!(
                storage
                    .load_player_data(&uuid)
                    .unwrap()
                    .1
                    .get_int("XpLevel"),
                Some(27)
            );
        }
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn valid_gzip_with_trailing_nbt_is_rejected_and_not_overwritten() {
        let temp = tempdir().unwrap();
        let storage = PlayerDataStorage::new(temp.path(), true);
        let uuid = Uuid::new_v4();
        let path = storage.get_player_data_path(&uuid);
        let gzip = |payload: &[u8]| {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(payload).unwrap();
            encoder.finish().unwrap()
        };
        // Trailing NBT in one member, and a second complete gzip/NBT member.
        let mut members = gzip(&[10, 0, 0, 0]);
        members.extend(gzip(&[10, 0, 0, 0]));
        for corrupt in [gzip(&[10, 0, 0, 0, 1]), members] {
            std::fs::write(&path, &corrupt).unwrap();
            assert!(
                storage
                    .load_player_data(&uuid)
                    .unwrap_err()
                    .to_string()
                    .contains("trailing NBT")
            );
            assert!(storage.save_player_data(&uuid, NbtCompound::new()).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), corrupt);
        }
    }

    #[test]
    fn serialization_error_cannot_commit_a_partial_player_file() {
        let temp = tempdir().unwrap();
        let storage = PlayerDataStorage::new(temp.path(), true);
        let uuid = Uuid::new_v4();
        let mut original = NbtCompound::new();
        original.put_int("XpLevel", 27);
        storage.save_player_data(&uuid, original).unwrap();
        let path = storage.get_player_data_path(&uuid);
        let retained = std::fs::read(&path).unwrap();
        let mut invalid = NbtCompound::new();
        // Java NBT strings have a u16 wire length; this is a real serializer error.
        invalid.put_string("plugin:runtime", "x".repeat(65_536));
        assert!(matches!(
            storage.save_player_data(&uuid, invalid),
            Err(PlayerDataError::Nbt(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), retained);
        assert_eq!(
            storage
                .load_player_data(&uuid)
                .unwrap()
                .1
                .get_int("XpLevel"),
            Some(27)
        );
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[test]
    fn interrupted_gzip_replacement_preserves_the_actual_player_file() {
        let temp = tempdir().unwrap();
        let storage = PlayerDataStorage::new(temp.path(), true);
        let uuid = Uuid::new_v4();
        let mut data = NbtCompound::new();
        data.put_int("XpLevel", 27);
        storage.save_player_data(&uuid, data).unwrap();
        let path = storage.get_player_data_path(&uuid);
        let retained = std::fs::read(&path).unwrap();
        let result: io::Result<()> = pumpkin_util::atomic_file::atomic_write(&path, |file| {
            file.write_all(&[0x1f, 0x8b, 0x08])?;
            Err(io::Error::other("interrupted gzip writer"))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), retained);
        assert_eq!(
            storage
                .load_player_data(&uuid)
                .unwrap()
                .1
                .get_int("XpLevel"),
            Some(27)
        );
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn failed_player_file_replace_preserves_bytes_and_valid_gzip() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ_AND_WRITE: u32 = 0x0000_0001 | 0x0000_0002;
        let temp = tempdir().unwrap();
        let storage = PlayerDataStorage::new(temp.path(), true);
        let uuid = Uuid::new_v4();
        let mut original = NbtCompound::new();
        original.put_int("XpLevel", 27);
        storage.save_player_data(&uuid, original).unwrap();
        let path = storage.get_player_data_path(&uuid);
        let retained = std::fs::read(&path).unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ_AND_WRITE)
            .open(&path)
            .unwrap();
        let mut replacement = NbtCompound::new();
        replacement.put_int("XpLevel", 99);
        assert!(storage.save_player_data(&uuid, replacement).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), retained);
        assert_eq!(
            storage
                .load_player_data(&uuid)
                .unwrap()
                .1
                .get_int("XpLevel"),
            Some(27)
        );
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
        drop(held);
    }
}
