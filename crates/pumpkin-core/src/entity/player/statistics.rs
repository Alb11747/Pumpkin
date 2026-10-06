pub use crate::data::player_progress::PlayerSaveState as StatisticsSaveState;
use crate::data::player_progress::{
    PlayerSaveChannel, PlayerSaveError, PlayerSaveSession, PlayerSaveTicket,
};
pub use pumpkin_data::statistic::{CustomStatistic, StatisticCategory};
use pumpkin_data::{Block, BlockId, entity::EntityType, item::Item};
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_nbt::tag::NbtTag;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
struct StoredStatistics {
    #[serde(default)]
    stats: HashMap<String, HashMap<String, i32>>,
    #[serde(rename = "DataVersion", default = "current_data_version")]
    data_version: i32,
}

const fn current_data_version() -> i32 {
    crate::entity::player::DATA_VERSION
}

#[derive(Debug, thiserror::Error)]
pub enum StatisticsDataError {
    #[error("statistics IO error: {0}")]
    Io(#[from] io::Error),
    #[error("statistics JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("refusing to overwrite statistics after a failed load: {0}")]
    FailedLoad(PathBuf),
    #[error("refusing statistics snapshot from a replaced player session: {0}")]
    ReplacedSession(PathBuf),
    #[error("invalid statistic identifier: {0}")]
    InvalidIdentifier(String),
    #[error("duplicate statistic identifier: {0}")]
    DuplicateIdentifier(String),
    #[error("cannot persist unknown numeric statistic ({0}, {1}) as a registry name")]
    UnknownNumeric(i32, i32),
}

/// A snapshot can be written on the existing player data IO worker without
/// keeping the player's statistics lock held.
pub struct StatisticsSnapshot {
    path: PathBuf,
    json: String,
    ticket: PlayerSaveTicket,
}

impl StatisticsSnapshot {
    pub fn save(self) -> Result<(), StatisticsDataError> {
        let _writer = self.ticket.writer();
        if !self.ticket.is_current() {
            return Ok(());
        }
        // The lock covers the complete replacement, so readers and competing
        // writers can only observe a complete file from the newest snapshot.
        crate::data::player_progress::atomic_write(&self.path, self.json.as_bytes())?;
        Ok(())
    }
}

pub struct Statistics {
    /// (Category ID, Statistic ID) -> Value
    pub stats: FxHashMap<(i32, i32), i32>,
    stored: StoredStatistics,
    path: Option<PathBuf>,
    load_failed: bool,
    session: PlayerSaveSession,
    save_state: Arc<StatisticsSaveState>,
}

impl Default for Statistics {
    fn default() -> Self {
        let save_state = Arc::new(StatisticsSaveState::default());
        let session = save_state.begin_session();
        Self {
            stats: FxHashMap::default(),
            stored: StoredStatistics {
                stats: HashMap::new(),
                data_version: current_data_version(),
            },
            path: None,
            load_failed: false,
            session,
            save_state,
        }
    }
}

impl Statistics {
    #[cfg(test)]
    pub(crate) fn use_save_state(&mut self, state: &Arc<StatisticsSaveState>) {
        let session = {
            let _writer = state.writer();
            state.begin_session()
        };
        self.use_save_session(session);
    }

    pub(crate) fn use_save_session(&mut self, session: PlayerSaveSession) {
        self.save_state = session.state();
        self.session = session;
    }

    fn ticket(&self) -> Result<PlayerSaveTicket, StatisticsDataError> {
        self.session
            .ticket(PlayerSaveChannel::Statistics)
            .map_err(|error| match error {
                PlayerSaveError::ReplacedSession => {
                    StatisticsDataError::ReplacedSession(self.path.clone().unwrap_or_default())
                }
                PlayerSaveError::FailedLoad => {
                    StatisticsDataError::FailedLoad(self.path.clone().unwrap_or_default())
                }
                PlayerSaveError::FinishedSession => {
                    StatisticsDataError::Io(io::Error::other(error))
                }
            })
    }

    /// Load JSON after legacy player NBT so the portable, named statistics are
    /// authoritative when both persistence formats exist.
    pub fn load(&mut self, players_path: &Path, uuid: Uuid) -> Result<(), StatisticsDataError> {
        let _writer = self.save_state.writer();
        // Invalidate queued statistics writes before reading or rejecting the file.
        self.ticket()?;
        let modern = players_path.join("stats").join(format!("{uuid}.json"));
        self.path = Some(modern.clone());
        self.load_failed = true;
        let path = crate::data::player_progress::existing_progress_path(&modern)?;
        self.path = Some(path.clone());
        let json = match fs::read(path) {
            Ok(json) => json,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.load_failed = false;
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        let mut stored: StoredStatistics = serde_json::from_slice(&json)?;
        let mut stats = self.stats.clone();
        let mut named_stats: HashMap<String, HashMap<String, i32>> = HashMap::new();
        for (category, values) in stored.stats {
            let category = pumpkin_util::identifier::Identifier::parse(&category)
                .map_err(|_| StatisticsDataError::InvalidIdentifier(category))?
                .to_string();
            let named_values = named_stats.entry(category.clone()).or_default();
            for (name, value) in values {
                let name = pumpkin_util::identifier::Identifier::parse(&name)
                    .map_err(|_| StatisticsDataError::InvalidIdentifier(name))?
                    .to_string();
                if let Some(category) = StatisticCategory::from_registry_key(&category)
                    && let Some(id) = stat_id(category, &name)
                {
                    stats.insert((category as i32, id), value);
                }
                if named_values.insert(name.clone(), value).is_some() {
                    return Err(StatisticsDataError::DuplicateIdentifier(format!(
                        "{category}/{name}"
                    )));
                }
            }
        }
        stored.stats = named_stats;
        self.stats = stats;
        self.stored = stored;
        self.load_failed = false;
        Ok(())
    }

    pub fn snapshot(&self) -> Result<Option<StatisticsSnapshot>, StatisticsDataError> {
        if self.path.is_none() {
            return Ok(None);
        }
        // Tickets use only the short counter lock, never the disk writer lock.
        self.snapshot_with_ticket(self.ticket()?)
    }

    pub(crate) fn snapshot_with_ticket(
        &self,
        ticket: PlayerSaveTicket,
    ) -> Result<Option<StatisticsSnapshot>, StatisticsDataError> {
        let Some(path) = &self.path else {
            return Ok(None);
        };
        if self.load_failed {
            return Err(StatisticsDataError::FailedLoad(path.clone()));
        }
        let mut stored = self.stored.clone();
        stored.data_version = stored.data_version.max(current_data_version());
        for ((category, id), value) in &self.stats {
            let category = StatisticCategory::from_i32(*category)
                .ok_or(StatisticsDataError::UnknownNumeric(*category, *id))?;
            let name = stat_name(category, *id)
                .ok_or(StatisticsDataError::UnknownNumeric(category as i32, *id))?;
            stored
                .stats
                .entry(category.registry_key().to_owned())
                .or_default()
                .insert(name, *value);
        }
        Ok(Some(StatisticsSnapshot {
            path: path.clone(),
            json: serde_json::to_string_pretty(&stored)?,
            ticket,
        }))
    }

    pub fn increment(&mut self, category: StatisticCategory, stat: i32, amount: i32) {
        let entry = self.stats.entry((category as i32, stat)).or_insert(0);
        *entry = entry.saturating_add(amount);
    }

    pub fn increment_custom(&mut self, stat: CustomStatistic, amount: i32) {
        self.increment(StatisticCategory::Custom, stat as i32, amount);
    }

    pub fn set(&mut self, category: StatisticCategory, stat: i32, value: i32) {
        self.stats.insert((category as i32, stat), value);
    }

    #[must_use]
    pub fn get(&self, category: StatisticCategory, stat: i32) -> i32 {
        *self.stats.get(&(category as i32, stat)).unwrap_or(&0)
    }

    pub fn write_nbt(&self, nbt: &mut NbtCompound) {
        let mut stats_compound = NbtCompound::new();
        for ((category, stat), value) in &self.stats {
            stats_compound.put_int(&format!("{category}:{stat}"), *value);
        }
        nbt.put_compound("Statistics", stats_compound);
    }

    pub fn read_nbt(&mut self, nbt: &NbtCompound) {
        if let Some(stats_compound) = nbt.get_compound("Statistics") {
            for (key, tag) in &stats_compound.child_tags {
                let parts: Vec<&str> = key.split(':').collect();
                if let (NbtTag::Int(value), [cat_str, stat_str]) = (tag, parts.as_slice())
                    && let (Ok(category), Ok(stat)) =
                        (cat_str.parse::<i32>(), stat_str.parse::<i32>())
                {
                    self.stats.insert((category, stat), *value);
                }
            }
        }
    }
}

fn stat_id(category: StatisticCategory, name: &str) -> Option<i32> {
    match category {
        StatisticCategory::Mined => {
            Block::from_name(name).map(|block| i32::from(block.id.as_u16()))
        }
        StatisticCategory::Crafted
        | StatisticCategory::Used
        | StatisticCategory::Broken
        | StatisticCategory::PickedUp
        | StatisticCategory::Dropped => {
            Item::from_registry_key(name).map(|item| i32::from(item.id))
        }
        StatisticCategory::Killed | StatisticCategory::KilledBy => {
            EntityType::from_name(name).map(|entity| i32::from(entity.id))
        }
        StatisticCategory::Custom => {
            CustomStatistic::from_registry_key(name).map(|stat| stat as i32)
        }
    }
}

fn stat_name(category: StatisticCategory, id: i32) -> Option<String> {
    match category {
        StatisticCategory::Mined => BlockId::new(u16::try_from(id).ok()?)
            .map(|block| format!("minecraft:{}", block.to_block().name)),
        StatisticCategory::Crafted
        | StatisticCategory::Used
        | StatisticCategory::Broken
        | StatisticCategory::PickedUp
        | StatisticCategory::Dropped => Item::from_id(u16::try_from(id).ok()?)
            .map(|item| format!("minecraft:{}", item.registry_key)),
        StatisticCategory::Killed | StatisticCategory::KilledBy => {
            EntityType::from_raw(u16::try_from(id).ok()?)
                .map(|entity| format!("minecraft:{}", entity.resource_name))
        }
        StatisticCategory::Custom => {
            CustomStatistic::from_i32(id).map(|stat| stat.registry_key().to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // ServerStatsCounter.toJson groups named values by statistic type.
    const VANILLA_STATS: &str = r#"{
        "DataVersion": 5023,
        "stats": {
            "minecraft:custom": {"minecraft:play_time": 12000, "datapack:custom_stat": 9},
            "minecraft:mined": {"minecraft:stone": 14},
            "minecraft:crafted": {"minecraft:crafting_table": 3},
            "minecraft:used": {"minecraft:iron_pickaxe": 25},
            "minecraft:broken": {"minecraft:iron_pickaxe": 1},
            "minecraft:picked_up": {"minecraft:diamond": 7},
            "minecraft:dropped": {"minecraft:dirt": 20},
            "minecraft:killed": {"minecraft:zombie": 5},
            "minecraft:killed_by": {"minecraft:creeper": 2},
            "datapack:category": {"datapack:statistic": 42}
        }
    }"#;

    #[test]
    fn vanilla_history_updates_and_preserves_every_category_and_unknown_name() {
        let temp = tempdir().unwrap();
        let players = temp.path().join("players");
        let uuid = Uuid::new_v4();
        let path = players.join("stats").join(format!("{uuid}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, VANILLA_STATS).unwrap();
        let mut statistics = Statistics::default();
        // A stale Pumpkin NBT value must not replace the named JSON history.
        let mut nbt = NbtCompound::new();
        let mut legacy = NbtCompound::new();
        legacy.put_int("8:1", 1);
        nbt.put_compound("Statistics", legacy);
        statistics.read_nbt(&nbt);
        statistics.load(&players, uuid).unwrap();
        assert_eq!(
            statistics.get(StatisticCategory::Custom, CustomStatistic::PlayTime as i32),
            12000
        );
        assert_eq!(statistics.stats.len(), 9);
        statistics.increment_custom(CustomStatistic::PlayTime, 20);
        statistics.snapshot().unwrap().unwrap().save().unwrap();
        let actual: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        let mut expected: serde_json::Value = serde_json::from_str(VANILLA_STATS).unwrap();
        expected["stats"]["minecraft:custom"]["minecraft:play_time"] = 12020.into();
        assert_eq!(actual, expected);
    }

    #[test]
    fn legacy_statistics_path_and_pumpkin_nbt_convert_to_vanilla_json() {
        let temp = tempdir().unwrap();
        let players = temp.path().join("players");
        let uuid = Uuid::new_v4();
        let path = temp.path().join("stats").join(format!("{uuid}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, VANILLA_STATS).unwrap();
        let mut statistics = Statistics::default();
        statistics.load(&players, uuid).unwrap();
        statistics.increment_custom(CustomStatistic::Jump, 2);
        statistics.snapshot().unwrap().unwrap().save().unwrap();
        assert!(!players.join("stats").join(format!("{uuid}.json")).exists());
        let actual: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(actual["stats"]["minecraft:custom"]["minecraft:jump"], 2);

        let new_uuid = Uuid::new_v4();
        let mut statistics = Statistics::default();
        let mut nbt = NbtCompound::new();
        let mut legacy = NbtCompound::new();
        legacy.put_int("8:23", 31);
        nbt.put_compound("Statistics", legacy);
        statistics.read_nbt(&nbt);
        statistics.load(&players, new_uuid).unwrap();
        statistics.snapshot().unwrap().unwrap().save().unwrap();
        let actual: serde_json::Value = serde_json::from_slice(
            &fs::read(players.join("stats").join(format!("{new_uuid}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(actual["stats"]["minecraft:custom"]["minecraft:jump"], 31);
    }

    #[test]
    fn late_snapshot_cannot_replace_newer_progress_or_reconnected_session() {
        let temp = tempdir().unwrap();
        let players = temp.path().join("players");
        let uuid = Uuid::new_v4();
        let path = players.join("stats").join(format!("{uuid}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, VANILLA_STATS).unwrap();
        let mut statistics = Statistics::default();
        statistics.load(&players, uuid).unwrap();
        statistics.increment_custom(CustomStatistic::Jump, 1);
        let older = statistics.snapshot().unwrap().unwrap();
        statistics.increment_custom(CustomStatistic::Jump, 2);
        statistics.snapshot().unwrap().unwrap().save().unwrap();
        let newer = fs::read(&path).unwrap();
        // Models a detached periodic worker finally running after disconnect save.
        older.save().unwrap();
        assert_eq!(fs::read(&path).unwrap(), newer);
        let saved: serde_json::Value = serde_json::from_slice(&newer).unwrap();
        assert_eq!(saved["stats"]["minecraft:custom"]["minecraft:jump"], 3);
        assert_eq!(
            saved["stats"]["datapack:category"]["datapack:statistic"],
            42
        );

        statistics.increment_custom(CustomStatistic::Jump, 4);
        let previous_session = statistics.snapshot().unwrap().unwrap();
        let mut reconnected = Statistics::default();
        reconnected.use_save_state(&statistics.save_state);
        reconnected.load(&players, uuid).unwrap();
        previous_session.save().unwrap();
        assert!(matches!(
            statistics.snapshot(),
            Err(StatisticsDataError::ReplacedSession(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), newer);
        reconnected.increment_custom(CustomStatistic::Jump, 10);
        reconnected.snapshot().unwrap().unwrap().save().unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["stats"]["minecraft:custom"]["minecraft:jump"], 13);
    }

    #[cfg(windows)]
    #[test]
    fn failed_replace_keeps_existing_file_bytes_intact() {
        use std::os::windows::fs::OpenOptionsExt;
        // Permit reads/writes, but forbid deleting/replacing this open file.
        const FILE_SHARE_READ_AND_WRITE: u32 = 0x0000_0001 | 0x0000_0002;
        let temp = tempdir().unwrap();
        let players = temp.path().join("players");
        let uuid = Uuid::new_v4();
        let path = players.join("stats").join(format!("{uuid}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, VANILLA_STATS).unwrap();
        let mut statistics = Statistics::default();
        statistics.load(&players, uuid).unwrap();
        statistics.increment_custom(CustomStatistic::Jump, 2);
        let snapshot = statistics.snapshot().unwrap().unwrap();
        let held_file = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ_AND_WRITE)
            .open(&path)
            .unwrap();
        assert!(snapshot.save().is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), VANILLA_STATS);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        drop(held_file);
        statistics.snapshot().unwrap().unwrap().save().unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["stats"]["minecraft:custom"]["minecraft:jump"], 2);
    }

    #[test]
    fn failed_atomic_replacement_preserves_original_and_cleans_temporary_file() {
        let temp = tempdir().unwrap();
        let players = temp.path().join("players");
        let uuid = Uuid::new_v4();
        let path = players.join("stats").join(format!("{uuid}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, VANILLA_STATS).unwrap();
        let mut statistics = Statistics::default();
        statistics.load(&players, uuid).unwrap();
        statistics.increment_custom(CustomStatistic::Jump, 2);
        let snapshot = statistics.snapshot().unwrap().unwrap();
        // A nonempty directory cannot be replaced by a file on either platform.
        let original = path.with_extension("original");
        fs::rename(&path, &original).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("retained"), VANILLA_STATS).unwrap();
        assert!(snapshot.save().is_err());
        assert_eq!(fs::read_to_string(&original).unwrap(), VANILLA_STATS);
        assert_eq!(
            fs::read_to_string(path.join("retained")).unwrap(),
            VANILLA_STATS
        );
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 2);
    }

    #[test]
    fn malformed_statistics_cannot_be_overwritten_by_empty_state() {
        let temp = tempdir().unwrap();
        let players = temp.path().join("players");
        let uuid = Uuid::new_v4();
        let path = players.join("stats").join(format!("{uuid}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for malformed in [
            r#"{"stats":{"minecraft:custom":{"minecraft:play_time":"bad"}}}"#,
            r#"{"stats":{"minecraft:custom":{"minecraft:play_time":2147483648}}}"#,
            r#"{"stats":{"invalid identifier":{"minecraft:play_time":10}}}"#,
        ] {
            fs::write(&path, malformed).unwrap();
            let mut statistics = Statistics::default();
            assert!(statistics.load(&players, uuid).is_err());
            assert!(statistics.snapshot().is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), malformed);
        }
    }
}
