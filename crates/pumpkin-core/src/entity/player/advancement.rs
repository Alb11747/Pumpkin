pub mod trigger;
mod visibility_evaluator;

use crate::data::advancement_data::AdvancementManager;
use crate::entity::EntityBase;
use crate::entity::player::Player;
use indexmap::IndexMap;
use pumpkin_data::advancement_data::{
    AdvancementNode, AdvancementProgressData, AdvancementRequirement, AdvancementReward, Criteria,
};
use pumpkin_data::{ADVANCEMENT_TREE, Advancement, translation};
use pumpkin_protocol::bedrock::server::text::SText;
use pumpkin_protocol::java::client::play::{
    CSelectAdvancementsTab, CSystemChatMessage, CUpdateAdvancements,
};
use pumpkin_util::identifier::Identifier;
use pumpkin_util::text::TextComponent;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::to_string_pretty;
use std::collections::{HashMap, HashSet};
use std::fs::read;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::{SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;
use tracing::error;
use uuid::Uuid;

#[derive(Clone, Debug, Default)]
pub struct CriterionProgress(pub Option<SystemTime>);

// AdvancementProgress.OBTAINED_TIME_CODEC in the official server uses this
// offset-bearing date format, with second precision.
const OBTAINED_TIME_FORMAT: &[time::format_description::BorrowedFormatItem<'static>] = time::macros::format_description!(
    "[year]-[month]-[day] [hour]:[minute]:[second] [offset_hour sign:mandatory][offset_minute]"
);

impl Serialize for CriterionProgress {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Some(time) => {
                let nanos = match time.duration_since(UNIX_EPOCH) {
                    Ok(duration) => duration.as_nanos() as i128,
                    Err(error) => -(error.duration().as_nanos() as i128),
                };
                OffsetDateTime::from_unix_timestamp_nanos(nanos)
                    .map_err(serde::ser::Error::custom)?
                    .format(OBTAINED_TIME_FORMAT)
                    .map_err(serde::ser::Error::custom)?
                    .serialize(serializer)
            }
            None => serializer.serialize_none(),
        }
    }
}

impl<'de> Deserialize<'de> for CriterionProgress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum StoredTime {
            Vanilla(String),
            // Existing Pumpkin files used serde's SystemTime representation.
            Pumpkin(Option<SystemTime>),
        }
        match StoredTime::deserialize(deserializer)? {
            StoredTime::Vanilla(date) => OffsetDateTime::parse(&date, OBTAINED_TIME_FORMAT)
                .map(|date| Self(Some(date.into())))
                .map_err(serde::de::Error::custom),
            StoredTime::Pumpkin(time) => Ok(Self(time)),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAdvancementProgress {
    #[serde(default)]
    criteria: HashMap<Arc<str>, CriterionProgress>,
    #[serde(default = "default_done")]
    done: bool,
}

enum StoredProgress {
    Vanilla(StoredAdvancementProgress),
    Pumpkin(HashMap<Arc<str>, CriterionProgress>),
}

impl<'de> Deserialize<'de> for StoredProgress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        // Do not reinterpret a malformed vanilla record as a legacy criteria map.
        if value.get("criteria").is_some()
            || value.get("done").is_some()
            || value.as_object().is_some_and(serde_json::Map::is_empty)
        {
            serde_json::from_value(value)
                .map(Self::Vanilla)
                .map_err(serde::de::Error::custom)
        } else {
            serde_json::from_value(value)
                .map(Self::Pumpkin)
                .map_err(serde::de::Error::custom)
        }
    }
}

impl StoredProgress {
    fn into_stored(self) -> StoredAdvancementProgress {
        match self {
            Self::Vanilla(progress) => progress,
            Self::Pumpkin(criteria) => StoredAdvancementProgress {
                done: criteria.values().any(CriterionProgress::is_done),
                criteria,
            },
        }
    }
}

const fn default_done() -> bool {
    true
}

impl CriterionProgress {
    pub fn grant(&mut self) {
        self.0 = Some(SystemTime::now());
    }

    pub const fn revoke(&mut self) {
        self.0 = None;
    }

    #[must_use]
    pub const fn is_done(&self) -> bool {
        self.0.is_some()
    }
}

/// Represents the progress of a given advancement for a player.
///
/// Tracks whether the advancement has been fully completed. In the future,
/// this will also track specific criteria progress.
#[derive(Debug, Clone, Default)]
pub struct AdvancementProgress {
    /// Indicates the different progress of all criteria currently only a boolean
    pub criteria: HashMap<Arc<str>, CriterionProgress>,
    /// The Requirement for the Advancement to be mark as complete
    pub requirements: AdvancementRequirement,
}

impl AdvancementProgress {
    /// Returns `true` if the advancement is done.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.requirements.test(|s| self.is_criterion_done(s))
    }

    /// Check if a criterion his mark has complete
    fn is_criterion_done(&self, criterion: &str) -> bool {
        self.criteria
            .get(criterion)
            .is_some_and(CriterionProgress::is_done)
    }

    /// Returns `true` if the advancement has any progress. Currently just returns if it is fully complete.
    #[must_use]
    pub fn has_progress(&self) -> bool {
        for value in self.criteria.values() {
            if value.is_done() {
                return true;
            }
        }
        false
    }

    pub fn grant_progress(&mut self, name: &str) -> bool {
        if let Some(value) = self.criteria.get_mut(name)
            && !value.is_done()
        {
            value.grant();
            true
        } else {
            false
        }
    }

    pub fn revoke_progress(&mut self, name: &str) -> bool {
        if let Some(value) = self.criteria.get_mut(name)
            && value.is_done()
        {
            value.revoke();
            true
        } else {
            false
        }
    }

    pub fn update(&mut self, requirements: AdvancementRequirement) {
        let names = requirements.names();
        self.criteria.retain(|key, _criterion| names.contains(key));
        for name in names {
            self.criteria.entry(name).or_default();
        }
        self.requirements = requirements;
    }

    #[inline]
    pub fn get_remaining_criteria(&self) -> impl Iterator<Item = Arc<str>> {
        self.criteria
            .iter()
            .filter(|&(_id, criterion)| !criterion.is_done())
            .map(|(id, _criterion)| id.clone())
    }

    #[inline]
    pub fn get_completed_criteria(&self) -> impl Iterator<Item = Arc<str>> {
        self.criteria
            .iter()
            .filter(|&(_id, criterion)| criterion.is_done())
            .map(|(id, _criterion)| id.clone())
    }
}

impl Serialize for AdvancementProgress {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        StoredAdvancementProgress {
            criteria: self
                .criteria
                .iter()
                .filter(|(_, progress)| progress.is_done())
                .map(|(name, progress)| (name.clone(), progress.clone()))
                .collect(),
            done: self.is_done(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AdvancementProgress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let criteria = StoredProgress::deserialize(deserializer)?
            .into_stored()
            .criteria;
        Ok(Self {
            criteria,
            requirements: AdvancementRequirement::default(),
        })
    }
}

#[derive(Clone, Default)]
pub struct AdvancementProgressMap {
    pub map: IndexMap<&'static Advancement, AdvancementProgress>,
}

impl AdvancementProgressMap {
    /// Gets a mutable reference to the current progress for a given advancement. Creates the state entry if missing.
    pub fn get_mut_or_start_progress(
        &mut self,
        advancement: &'static Advancement,
    ) -> &mut AdvancementProgress {
        self.map.entry(advancement).or_insert_with(|| {
            let mut progress = AdvancementProgress::default();
            progress.update(AdvancementRequirement::from_const(advancement.requirements));
            progress
        })
    }

    #[inline]
    pub fn clear(&mut self) {
        self.map.clear();
    }

    #[inline]
    pub fn insert(&mut self, advancement: &'static Advancement, progress: AdvancementProgress) {
        self.map.insert(advancement, progress);
    }

    #[must_use]
    #[inline]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[must_use]
    #[inline]
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Manages a player's collection of advancements.
///
/// This handles saving, loading, and tracking the state of granted / revoked advancements.
pub struct PlayerAdvancement {
    pub progress: AdvancementProgressMap,
    pub is_first_packet: bool,
    pub roots_to_update: HashSet<&'static AdvancementNode>,
    pub visible: HashSet<&'static Advancement>,
    pub progress_changed: HashSet<&'static Advancement>,
    pub manager: Arc<AdvancementManager>,
    pub path: PathBuf,
    pub last_selected_tab: Option<&'static Advancement>,
    // Keep datapack advancements and criteria absent from Pumpkin's current registry
    // so a save cannot erase history that a vanilla server can still understand.
    stored_progress: HashMap<String, StoredAdvancementProgress>,
    data_version: i32,
    load_failed: bool,
    /// A weak reference to the player who owns these advancements.
    pub player: Weak<Player>,
}

/// The state changes produced by awarding an advancement criterion.
///
/// Completion side effects are deliberately deferred until after the caller releases the
/// player's advancement lock.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[must_use]
pub struct AdvancementAward {
    awarded: bool,
    completed: bool,
}

impl AdvancementAward {
    #[must_use]
    pub const fn awarded(self) -> bool {
        self.awarded
    }

    pub const fn combine(self, other: Self) -> Self {
        Self {
            awarded: self.awarded || other.awarded,
            completed: self.completed || other.completed,
        }
    }
}

/// Errors that can occur when saving or loading advancement data.
#[derive(Debug, thiserror::Error)]
pub enum AdvancementDataError {
    #[error("IO error: {0}")]
    Io(std::io::Error),
    #[error("JSON error: {0}")]
    Json(serde_json::Error),
    #[error("invalid advancement identifier: {0}")]
    Identifier(pumpkin_util::identifier::IdentifierError),
    #[error("duplicate advancement identifier: {0}")]
    DuplicateIdentifier(String),
    #[error("refusing to overwrite advancement data after a failed load: {0}")]
    FailedLoad(PathBuf),
}

impl PlayerAdvancement {
    /// Creates a new instance of `PlayerAdvancement`.
    #[must_use]
    pub fn new(manager: Arc<AdvancementManager>, uuid: Uuid) -> Self {
        Self {
            progress: AdvancementProgressMap::default(),
            path: manager.advancement_path.join(format!("{uuid}.json")),
            manager,
            player: Weak::new(),
            is_first_packet: true,
            roots_to_update: HashSet::default(),
            visible: HashSet::default(),
            progress_changed: HashSet::default(),
            last_selected_tab: None,
            stored_progress: HashMap::new(),
            data_version: crate::entity::player::DATA_VERSION,
            load_failed: false,
        }
    }

    /// Associates the `PlayerAdvancement` data with the given player.
    pub fn set_player(&mut self, player: &Arc<Player>) {
        self.player = Arc::downgrade(player);
    }

    /// Returns whether advancement saving is enabled for this player.
    #[must_use]
    #[inline]
    pub fn is_save_enabled(&self) -> bool {
        self.manager.save_enabled
    }

    ///reload the advancements from the file
    pub fn reload(&mut self) -> Result<(), AdvancementDataError> {
        //self.stopListening(); TODO
        // Parse before resetting state: a failed reload must not discard live progress.
        self.load()?;
        self.visible.clear();
        self.roots_to_update.clear();
        self.progress_changed.clear();
        self.is_first_packet = true;
        self.last_selected_tab = None;
        self.progress_changed.extend(self.progress.map.keys());
        self.roots_to_update
            .extend(self.progress.map.keys().filter_map(|advancement| {
                ADVANCEMENT_TREE
                    .get_node_from_id(&advancement.id)
                    .map(AdvancementNode::root)
            }));
        Ok(())
    }

    /// Saves the player's advancement progress to disk as JSON.
    pub async fn save(&self) -> Result<(), AdvancementDataError> {
        if !self.is_save_enabled() {
            return Ok(());
        }
        self.ensure_saveable()?;
        let json = to_string_pretty(self).map_err(AdvancementDataError::Json)?;
        let Some(parent) = self.path.parent() else {
            return Ok(());
        };
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            error!("Failed to create player advancement directory : {e}");
            return Err(AdvancementDataError::Io(e));
        }
        crate::data::player_progress::atomic_write_async(self.path.clone(), json.into_bytes())
            .await
            .map_err(AdvancementDataError::Io)?;
        Ok(())
    }

    pub fn ensure_saveable(&self) -> Result<(), AdvancementDataError> {
        if self.load_failed {
            return Err(AdvancementDataError::FailedLoad(self.path.clone()));
        }
        Ok(())
    }

    /// Loads vanilla progress, or the flat criteria map written by older Pumpkin builds.
    pub fn load(&mut self) -> Result<(), AdvancementDataError> {
        if !self.is_save_enabled() {
            return Ok(());
        }
        // Set before any IO or parsing; every save entry point respects this guard.
        self.load_failed = true;
        self.path = crate::data::player_progress::existing_progress_path(&self.path)
            .map_err(AdvancementDataError::Io)?;
        let json = match read(&self.path) {
            Ok(json) => json,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.progress.clear();
                self.stored_progress.clear();
                self.data_version = crate::entity::player::DATA_VERSION;
                self.load_failed = false;
                return Ok(());
            }
            Err(error) => return Err(AdvancementDataError::Io(error)),
        };
        let mut entries: HashMap<String, serde_json::Value> =
            serde_json::from_slice(&json).map_err(AdvancementDataError::Json)?;
        let version = entries
            .remove("DataVersion")
            .map(serde_json::from_value::<i32>)
            .transpose()
            .map_err(AdvancementDataError::Json)?
            .unwrap_or(crate::entity::player::DATA_VERSION);
        let mut stored = HashMap::new();
        for (id, value) in entries {
            let id = Identifier::parse(&id)
                .map_err(AdvancementDataError::Identifier)?
                .to_string();
            let mut progress = serde_json::from_value::<StoredProgress>(value)
                .map_err(AdvancementDataError::Json)?
                .into_stored();
            progress.criteria.retain(|_, criterion| criterion.is_done());
            if stored.insert(id.clone(), progress).is_some() {
                return Err(AdvancementDataError::DuplicateIdentifier(id));
            }
        }
        self.progress.clear();
        self.stored_progress = stored;
        self.data_version = version.max(crate::entity::player::DATA_VERSION);
        for (id, stored) in self.stored_progress.clone() {
            if let Some(advancement) = Advancement::from_minecraft_name(&id) {
                let mut progress = AdvancementProgress {
                    criteria: stored.criteria,
                    requirements: AdvancementRequirement::default(),
                };
                progress.update(AdvancementRequirement::from_const(advancement.requirements));
                self.progress.insert(advancement, progress);
                self.progress_changed.insert(advancement);
                self.mark_for_visibility_update(advancement);
            }
        }
        self.load_failed = false;
        Ok(())
    }

    fn mark_for_visibility_update(&mut self, advancement: &'static Advancement) {
        let node = ADVANCEMENT_TREE.get_node_from_id(&advancement.id);
        if let Some(node) = node {
            self.roots_to_update.insert(node.root());
        }
    }

    fn update_tree_visibility(
        &mut self,
        root: &AdvancementNode,
        added: &mut Vec<&'static Advancement>,
        removed: &mut Vec<Identifier>,
    ) {
        visibility_evaluator::evaluate_visibility(
            root,
            self,
            &mut |player_advancement, node| {
                player_advancement
                    .progress
                    .get_mut_or_start_progress(node.value)
                    .is_done()
            },
            &mut move |player_advancement, node, should_be_visible| {
                let advancement = node.value;
                if should_be_visible {
                    if player_advancement.visible.insert(advancement) {
                        added.push(advancement);
                        if player_advancement.progress.map.contains_key(advancement) {
                            player_advancement.progress_changed.insert(advancement);
                        }
                    }
                } else if player_advancement.visible.remove(advancement) {
                    removed.push(advancement.id.clone());
                }
            },
        );
    }

    /// Flushes any pending advancement state down to the client.
    pub fn flush_dirty(&mut self, player: &Player, show_advancement: bool) {
        if self.is_first_packet
            || !self.roots_to_update.is_empty()
            || !self.progress_changed.is_empty()
        {
            let mut progress: HashMap<Identifier, &AdvancementProgress> = HashMap::new();
            let mut added: Vec<&Advancement> = Vec::new();
            let mut removed: Vec<Identifier> = Vec::new();
            for root in self.roots_to_update.clone() {
                self.update_tree_visibility(root, &mut added, &mut removed);
            }
            self.roots_to_update.clear();
            for advancement in &self.progress_changed {
                if self.visible.contains(advancement) {
                    progress.insert(advancement.id.clone(), &self.progress.map[advancement]);
                }
            }
            self.progress_changed.clear();
            if !progress.is_empty() || !added.is_empty() || !removed.is_empty() {
                let parsed_progress: Vec<AdvancementProgressData> = progress
                    .into_iter()
                    .map(|(key, val)| AdvancementProgressData {
                        id: key,
                        progress: val
                            .criteria
                            .iter()
                            .map(|(key, val)| Criteria {
                                criterion_id: key.clone(),
                                achieve_date: val.0.map(|time| {
                                    time.duration_since(UNIX_EPOCH)
                                        .map_or(0, |d| d.as_millis() as i64)
                                }),
                            })
                            .collect(),
                    })
                    .collect();
                player.try_send_client_packet(&CUpdateAdvancements::new(
                    self.is_first_packet,
                    added,
                    parsed_progress,
                    removed,
                    show_advancement,
                ));
            }
        }
        self.is_first_packet = false;
    }

    /// Grants the rewards (like experience) associated with completing an advancement.
    pub fn grant_reward(player: &Arc<Player>, reward: &'static AdvancementReward) {
        player.add_experience_points(reward.experience);
    }

    /// Records a criterion award while the player's advancement state is locked.
    ///
    /// Call [`Self::finish_award`] after releasing the lock so completion events and rewards can
    /// safely re-enter advancement APIs.
    pub fn award(
        &mut self,
        advancement: &'static Advancement,
        criterion: &str,
    ) -> AdvancementAward {
        let mut result = AdvancementAward::default();
        let progress = self.progress.get_mut_or_start_progress(advancement);
        let was_done = progress.is_done();
        if progress.grant_progress(criterion) {
            result.awarded = true;
            self.progress_changed.insert(advancement);
            if !was_done && progress.is_done() {
                result.completed = true;
            }
        }
        if !was_done && progress.is_done() {
            self.mark_for_visibility_update(advancement);
        }
        result
    }

    /// Runs the callback-producing side effects of a completed award.
    ///
    /// This must only be called after releasing the player's advancement lock.
    pub fn finish_award(
        player: &Arc<Player>,
        advancement: &'static Advancement,
        result: AdvancementAward,
    ) {
        if !result.completed {
            return;
        }

        if let Some(server) = player.world().server.upgrade() {
            let mut event =
                crate::plugin::api::events::player::player_advancement_done::PlayerAdvancementDoneEvent::new(
                    player.clone(),
                    advancement.id.to_string(),
                );
            server.plugin_manager.fire_blocking(&server, &mut event);
        }
        Self::grant_reward(player, advancement.reward);
        if let Some(display) = advancement.display
            && display.announce_to_chat
            && player
                .world()
                .level_info
                .load()
                .game_rules
                .show_advancement_messages
        {
            let player_name = player.get_display_name();
            let je_component = TextComponent::translate(
                display.frame_type.get_translation(),
                [player_name.clone(), advancement.name()],
            );
            let je_packet = CSystemChatMessage::new(&je_component, false);

            let be_packet = SText::translation(
                translation::bedrock::CHAT_TYPE_ACHIEVEMENT.to_string(),
                vec![
                    player_name.0.to_bedrock_string(),
                    display.get_title().0.to_bedrock_string(),
                ],
            );

            player.world().broadcast_editioned(&je_packet, &be_packet);
        }
    }

    /// Revokes a previously awarded advancement, clearing its progress state.
    pub fn revoke(&mut self, advancement: &'static Advancement, criterion: &str) -> bool {
        let mut result = false;
        let progress = self.progress.get_mut_or_start_progress(advancement);
        let was_done = progress.is_done();
        if progress.revoke_progress(criterion) {
            //TODO listener
            self.progress_changed.insert(advancement);
            result = true;
        }

        if was_done && !progress.is_done() {
            self.mark_for_visibility_update(advancement);
        }
        result
    }

    /// set the selected advancement tab of the player
    pub fn set_selected_tab(&mut self, advancement: Option<&'static Advancement>) {
        let old = self.last_selected_tab;
        if let Some(value) = advancement
            && value.is_root()
            && value.display.is_some()
        {
            self.last_selected_tab = advancement;
        } else {
            self.last_selected_tab = None;
        }
        if old != self.last_selected_tab
            && let Some(player) = self.player.upgrade()
        {
            let tab_id = self.last_selected_tab.map(|adv| adv.id.clone());
            player.try_send_client_packet(&CSelectAdvancementsTab::new(tab_id));
        }
    }
}

impl Serialize for PlayerAdvancement {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.ensure_saveable().map_err(serde::ser::Error::custom)?;
        let mut stored = self.stored_progress.clone();
        for (advancement, progress) in &self.progress.map {
            let id = advancement.id.to_string();
            let entry = stored
                .entry(id.clone())
                .or_insert_with(|| StoredAdvancementProgress {
                    criteria: HashMap::new(),
                    done: false,
                });
            // Keep unknown criteria while applying grants and revocations to known ones.
            for (name, criterion) in &progress.criteria {
                if criterion.is_done() {
                    entry.criteria.insert(name.clone(), criterion.clone());
                } else {
                    entry.criteria.remove(name);
                }
            }
            entry.done = progress.is_done();
            if entry.criteria.is_empty() {
                stored.remove(&id);
            }
        }
        let mut map = serializer.serialize_map(Some(stored.len() + 1))?;
        map.serialize_entry("DataVersion", &self.data_version)?;
        for (id, progress) in stored {
            map.serialize_entry(&id, &progress)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::advancement_data::AdvancementManager;
    use pumpkin_data::Advancement;
    use tempfile::tempdir;

    // Fixture shape and date format follow the official 26.3
    // AdvancementProgress.CODEC and PlayerAdvancements.Data.CODEC.
    const VANILLA_PROGRESS: &str = r#"{
        "DataVersion": 5023,
        "minecraft:story/root": {
            "criteria": {
                "crafting_table": "2026-01-01 12:30:45 -0400",
                "removed_criterion": "2025-12-31 23:59:59 +0000"
            },
            "done": true
        },
        "datapack:story/custom": {
            "criteria": { "first_step": "2026-01-01 17:00:00 +0000" },
            "done": false
        },
        "datapack:empty": {}
    }"#;

    #[tokio::test]
    async fn vanilla_history_survives_load_save_and_revoke() {
        let temp = tempdir().unwrap();
        let manager = Arc::new(AdvancementManager::new(temp.path().join("players"), true));
        let mut player = PlayerAdvancement::new(manager.clone(), Uuid::new_v4());
        std::fs::write(&player.path, VANILLA_PROGRESS).unwrap();
        player.load().unwrap();
        let progress = &player.progress.map[Advancement::STORY_ROOT];
        assert!(progress.is_done());
        assert_eq!(
            progress.criteria["crafting_table"]
                .0
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            1_767_285_045
        );
        player.save().await.unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&player.path).unwrap()).unwrap();
        assert_eq!(saved["DataVersion"], 5023);
        assert_eq!(
            saved["minecraft:story/root"]["criteria"]["crafting_table"],
            "2026-01-01 16:30:45 +0000"
        );
        assert_eq!(
            saved["minecraft:story/root"]["criteria"]["removed_criterion"],
            "2025-12-31 23:59:59 +0000"
        );
        assert_eq!(saved["datapack:story/custom"]["done"], false);
        assert_eq!(saved["datapack:empty"]["done"], true);
        assert!(player.revoke(Advancement::STORY_ROOT, "crafting_table"));
        player.save().await.unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&player.path).unwrap()).unwrap();
        assert!(
            saved["minecraft:story/root"]["criteria"]
                .get("crafting_table")
                .is_none()
        );
        assert_eq!(saved["minecraft:story/root"]["done"], false);
    }

    #[tokio::test]
    async fn saving_replaces_the_file_without_truncating_the_previous_history() {
        let temp = tempdir().unwrap();
        let manager = Arc::new(AdvancementManager::new(temp.path(), true));
        let mut player = PlayerAdvancement::new(manager, Uuid::new_v4());
        std::fs::write(&player.path, VANILLA_PROGRESS).unwrap();
        player.load().unwrap();
        let previous_history = temp.path().join("previous-history.json");
        std::fs::hard_link(&player.path, &previous_history).unwrap();

        assert!(player.revoke(Advancement::STORY_ROOT, "crafting_table"));
        player.save().await.unwrap();

        // An in-place write would also truncate this link to the old file.
        assert_eq!(
            std::fs::read(previous_history).unwrap(),
            VANILLA_PROGRESS.as_bytes()
        );
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&player.path).unwrap()).unwrap();
        assert!(
            saved["minecraft:story/root"]["criteria"]
                .get("crafting_table")
                .is_none()
        );
        assert_eq!(saved["datapack:story/custom"]["done"], false);
        assert_eq!(
            std::fs::read_dir(&player.manager.advancement_path)
                .unwrap()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn malformed_history_blocks_save_without_discarding_live_progress() {
        let temp = tempdir().unwrap();
        let manager = Arc::new(AdvancementManager::new(temp.path(), true));
        let mut player = PlayerAdvancement::new(manager, Uuid::new_v4());
        std::fs::write(&player.path, VANILLA_PROGRESS).unwrap();
        player.load().unwrap();
        let malformed = VANILLA_PROGRESS.replace("2026-01-01 12:30:45 -0400", "invalid date");
        std::fs::write(&player.path, &malformed).unwrap();
        assert!(player.reload().is_err());
        assert!(player.progress.map[Advancement::STORY_ROOT].is_done());
        assert!(player.save().await.is_err());
        assert!(serde_json::to_string(&player).is_err());
        assert_eq!(std::fs::read_to_string(&player.path).unwrap(), malformed);
        for invalid in [
            r#"{"minecraft:story/root":{"criteria":null}}"#,
            r#"{"minecraft:story/root":{"done":null}}"#,
        ] {
            std::fs::write(&player.path, invalid).unwrap();
            assert!(player.load().is_err());
            assert!(player.save().await.is_err());
            assert_eq!(std::fs::read_to_string(&player.path).unwrap(), invalid);
        }
    }

    #[tokio::test]
    async fn legacy_pumpkin_history_and_legacy_world_path_are_supported() {
        let temp = tempdir().unwrap();
        let manager = Arc::new(AdvancementManager::new(temp.path().join("players"), true));
        let uuid = Uuid::new_v4();
        let legacy = temp
            .path()
            .join("advancements")
            .join(format!("{uuid}.json"));
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, r#"{"minecraft:story/root":{"crafting_table":{"secs_since_epoch":1767285045,"nanos_since_epoch":123456700}}}"#).unwrap();
        let mut player = PlayerAdvancement::new(manager, uuid);
        player.load().unwrap();
        assert_eq!(player.path, legacy);
        assert!(player.progress.map[Advancement::STORY_ROOT].is_done());
        assert_eq!(
            player.progress.map[Advancement::STORY_ROOT].criteria["crafting_table"]
                .0
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_nanos(),
            123_456_700
        );
        player.save().await.unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&legacy).unwrap()).unwrap();
        assert_eq!(
            saved["minecraft:story/root"]["criteria"]["crafting_table"],
            "2026-01-01 16:30:45 +0000"
        );
    }

    #[test]
    fn advancement_progress() {
        let mut criteria = HashMap::new();
        criteria.insert(Arc::from("testCriteria"), CriterionProgress::default());
        criteria.insert(Arc::from("testCriteria2"), CriterionProgress::default());
        let requirements = AdvancementRequirement {
            requirements: vec![
                vec![Arc::from("testCriteria")],
                vec![Arc::from("testCriteria2")],
            ],
        };
        let mut progress = AdvancementProgress {
            criteria,
            requirements,
        };
        assert!(!progress.is_done());
        assert!(!progress.has_progress());
        progress.grant_progress("testCriteria");
        assert!(!progress.is_done());
        assert!(progress.has_progress());
        progress.grant_progress("testCriteria2");
        assert!(progress.is_done());
        assert!(progress.has_progress());
    }

    #[test]
    fn new_player_advancement() {
        let temp_dir = tempdir().unwrap();
        let manager = Arc::new(AdvancementManager::new(temp_dir.path(), true));
        let id = Uuid::new_v4();
        let pa = PlayerAdvancement::new(manager, id);
        assert!(pa.is_save_enabled());
        assert!(pa.is_first_packet);
        assert!(pa.roots_to_update.is_empty());
        assert!(pa.progress.is_empty());
    }

    #[test]
    fn get_or_start_progress() {
        let temp_dir = tempdir().unwrap();
        let manager = Arc::new(AdvancementManager::new(temp_dir.path(), true));
        let id = Uuid::new_v4();
        let mut pa = PlayerAdvancement::new(manager, id);
        let adv = Advancement::STORY_ROOT;
        let progress = pa.progress.get_mut_or_start_progress(adv);
        assert!(
            !progress.is_done(),
            "New progress should not be marked done by default"
        );
    }

    #[test]
    fn revoke_advancement() {
        let temp_dir = tempdir().unwrap();
        let manager = Arc::new(AdvancementManager::new(temp_dir.path(), true));
        let id = Uuid::new_v4();
        let mut pa = PlayerAdvancement::new(manager, id);
        let adv = Advancement::STORY_ROOT;
        {
            let progress_mut = pa.progress.get_mut_or_start_progress(adv);
            progress_mut.grant_progress("crafting_table");
        };
        assert!(pa.progress.get_mut_or_start_progress(adv).is_done());
        pa.revoke(adv, "crafting_table");
        assert!(!pa.progress.get_mut_or_start_progress(adv).is_done());
    }

    #[tokio::test]
    async fn save_advancement_progress() {
        let temp_dir = tempdir().unwrap();
        let manager = Arc::new(AdvancementManager::new(temp_dir.path(), true));
        let id = Uuid::new_v4();
        let mut pa = PlayerAdvancement::new(manager, id);

        // Add some advancement progress
        let adv = Advancement::STORY_ROOT;
        {
            let progress_mut = pa.progress.get_mut_or_start_progress(adv);
            progress_mut.grant_progress("crafting_table");
        };

        // Save should succeed
        assert!(pa.save().await.is_ok(), "Save should succeed");

        // File should exist
        assert!(pa.path.exists(), "Saved file should exist");

        // Content should be valid JSON
        let content = std::fs::read_to_string(&pa.path).unwrap();
        assert!(!content.is_empty(), "Saved file should not be empty");
        let _: serde_json::Value =
            serde_json::from_str(&content).expect("Saved content should be valid JSON");
    }

    #[tokio::test]
    async fn save_disabled() {
        let temp_dir = tempdir().unwrap();
        let manager = Arc::new(AdvancementManager::new(temp_dir.path(), false));
        let id = Uuid::new_v4();
        let mut pa = PlayerAdvancement::new(manager, id);

        // Add some advancement progress
        let adv = Advancement::STORY_ROOT;
        {
            let progress_mut = pa.progress.get_mut_or_start_progress(adv);
            progress_mut.grant_progress("crafting_table");
        };

        // Save should return Ok but not actually save
        assert!(
            pa.save().await.is_ok(),
            "Save with disabled saving should return Ok"
        );
        assert!(
            !pa.path.exists(),
            "File should not be created when saving is disabled"
        );
    }

    #[tokio::test]
    async fn load_nonexistent_file() {
        let temp_dir = tempdir().unwrap();
        let manager = Arc::new(AdvancementManager::new(temp_dir.path(), true));
        let id = Uuid::new_v4();
        let mut pa = PlayerAdvancement::new(manager, id);

        // Load from nonexistent file should return Ok (not error)
        assert!(
            pa.load().is_ok(),
            "Loading from nonexistent file should return Ok"
        );
        assert!(pa.progress.is_empty(), "Advancements should remain empty");
    }

    #[tokio::test]
    async fn load_advancement_progress() {
        let temp_dir = tempdir().unwrap();
        let manager = Arc::new(AdvancementManager::new(temp_dir.path(), true));

        let id = Uuid::new_v4();
        let mut pa = PlayerAdvancement::new(manager, id);
        // Create a JSON file with advancement data
        let adv = Advancement::STORY_ROOT;
        let mut progress = AdvancementProgress::default();
        progress.update(AdvancementRequirement::from_const(adv.requirements));
        progress.grant_progress("crafting_table");
        let data = serde_json::json!({ adv.id.to_string():progress });
        std::fs::write(&pa.path, data.to_string()).unwrap();

        // Load the file
        assert!(pa.load().is_ok(), "Load should succeed");

        // Verify the advancement was loaded
        let loaded_progress = pa.progress.get_mut_or_start_progress(adv);
        assert!(
            loaded_progress.is_done(),
            "Loaded advancement should be marked complete"
        );
    }

    #[tokio::test]
    async fn save_load_roundtrip() {
        let temp_dir = tempdir().unwrap();

        // Create and save advancements
        let manager = Arc::new(AdvancementManager::new(temp_dir.path(), true));
        let id = Uuid::new_v4();
        let mut pa = PlayerAdvancement::new(manager.clone(), id);

        let adv = Advancement::STORY_ROOT;
        {
            let progress_mut = pa.progress.get_mut_or_start_progress(adv);
            progress_mut.grant_progress("crafting_table");
        };

        assert!(pa.save().await.is_ok(), "Save should succeed");

        // Load the saved advancements into a new instance
        let mut pa_loaded = PlayerAdvancement::new(manager, id);
        assert!(pa_loaded.load().is_ok(), "Load should succeed");

        // Verify the loaded data matches the saved data
        let loaded_progress = pa_loaded.progress.get_mut_or_start_progress(adv);
        assert!(
            loaded_progress.is_done(),
            "Loaded progress should match saved progress"
        );
        assert_eq!(
            pa_loaded.progress.len(),
            pa.progress.len(),
            "Loaded advancements count should match"
        );
    }

    #[tokio::test]
    async fn load_invalid_advancement_id() {
        let temp_dir = tempdir().unwrap();
        let manager = Arc::new(AdvancementManager::new(temp_dir.path(), true));

        // Create a JSON file with invalid advancement ID
        let mut criteria = HashMap::new();
        criteria.insert(Arc::from("testCriteria"), CriterionProgress::default());
        let requirements = AdvancementRequirement {
            requirements: vec![vec![Arc::from("testCriteria")]],
        };
        let progress = AdvancementProgress {
            criteria,
            requirements,
        };
        let data = serde_json::json!({
            "invalid_advancement_id_12345": progress
        });
        let id = Uuid::new_v4();
        let mut pa = PlayerAdvancement::new(manager, id);
        std::fs::write(&pa.path, data.to_string()).unwrap();

        // Load should still succeed but skip the invalid entry

        assert!(
            pa.load().is_ok(),
            "Load should succeed even with invalid IDs"
        );
        assert!(
            pa.progress.is_empty(),
            "Invalid advancements should be skipped"
        );
    }

    #[tokio::test]
    async fn save_multiple_advancements() {
        let temp_dir = tempdir().unwrap();
        let manager = Arc::new(AdvancementManager::new(temp_dir.path(), true));
        let id = Uuid::new_v4();
        let mut pa = PlayerAdvancement::new(manager, id);

        // Add multiple advancements
        let adv1 = Advancement::STORY_ROOT;
        let adv2 = Advancement::NETHER_ROOT;

        {
            let progress_mut1 = pa.progress.get_mut_or_start_progress(adv1);
            progress_mut1.grant_progress("crafting_table");
        };
        {
            let progress_mut2 = pa.progress.get_mut_or_start_progress(adv2);
            progress_mut2.grant_progress("entered_nether");
        };

        assert!(pa.save().await.is_ok(), "Save should succeed");

        // Verify both were saved
        let content = std::fs::read_to_string(&pa.path).unwrap();
        let saved_data: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(saved_data.get(adv1.id.to_string()).is_some());
        assert!(saved_data.get(adv2.id.to_string()).is_some());
    }

    #[tokio::test]
    async fn ignore_loading() {
        let temp_dir = tempdir().unwrap();
        let manager = Arc::new(AdvancementManager::new(temp_dir.path(), false));

        let id = Uuid::new_v4();
        let mut pa = PlayerAdvancement::new(manager, id);
        // Create a JSON file with advancement data
        let adv = Advancement::STORY_ROOT;
        let data = serde_json::json!({ adv.id.to_string(): { "complete": true } });
        std::fs::write(&pa.path, data.to_string()).unwrap();

        //try load the file
        assert!(pa.load().is_ok(), "Load should succeed");

        // Verify that the advancement was not loaded
        assert!(
            pa.progress.is_empty(),
            "The advancement shouldn't have been loaded"
        );
    }
}
