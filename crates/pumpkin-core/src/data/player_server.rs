use crate::data::player_progress::{
    PlayerSaveChannel, PlayerSaveSession, PlayerSaveState, PlayerSaveTicket,
};
use crate::entity::player::statistics::{StatisticsDataError, StatisticsSnapshot};
use crate::{
    entity::{NBTStorage, player::Player},
    server::Server,
};
use crossbeam::atomic::AtomicCell;
use pumpkin_inventory::screen_handler::ScreenHandler;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_world::data::player_data::{PlayerDataError, PlayerDataStorage};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use tracing::{debug, error};

/// Helper for managing player data in the server context.
///
/// This struct provides server-wide access to the `PlayerDataStorage` and
/// convenience methods for player handling.
pub struct ServerPlayerData {
    storage: Arc<PlayerDataStorage>,
    save_interval: Duration,
    last_save: AtomicCell<Instant>,
    save_states: Mutex<HashMap<uuid::Uuid, Arc<PlayerSaveState>>>,
}

struct PlayerNbtSnapshot {
    data: NbtCompound,
    ticket: PlayerSaveTicket,
}

impl PlayerNbtSnapshot {
    #[cfg(test)]
    fn new(data: NbtCompound, session: &PlayerSaveSession) -> Result<Self, PlayerDataError> {
        let ticket = session
            .ticket(PlayerSaveChannel::Nbt)
            .map_err(|reason| PlayerDataError::Io(std::io::Error::other(reason)))?;
        Ok(Self { data, ticket })
    }

    fn save(self, storage: &PlayerDataStorage, uuid: &uuid::Uuid) -> Result<(), PlayerDataError> {
        let _writer = self.ticket.writer();
        if !self.ticket.is_current() {
            return Ok(());
        }
        storage.save_player_data(uuid, self.data)
    }
}

impl ServerPlayerData {
    /// Creates a new `ServerPlayerData` with specified configuration.
    pub fn new(data_path: impl Into<PathBuf>, save_interval: Duration, enabled: bool) -> Self {
        Self {
            storage: Arc::new(PlayerDataStorage::new(data_path, enabled)),
            save_interval,
            last_save: AtomicCell::new(Instant::now()),
            save_states: Mutex::new(HashMap::new()),
        }
    }

    /// Handles a player leaving the server.
    ///
    /// This function saves player data when they disconnect.
    ///
    /// # Arguments
    ///
    /// * `player` - The player who left.
    ///
    /// # Returns
    ///
    /// A Result indicating success or the error that occurred.
    pub fn handle_player_leave(&self, player: &Arc<Player>) -> Result<(), PlayerDataError> {
        player
            .player_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .on_closed(player.as_ref());
        player.on_handled_screen_closed();

        let session = Self::player_session(player)?;
        let (nbt_ticket, statistics_ticket) = session
            .final_tickets()
            .map_err(|error| PlayerDataError::Io(std::io::Error::other(error)))?;
        let mut data = NbtCompound::new();
        player.write_nbt(&mut data);
        let statistics = player
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot_with_ticket(statistics_ticket);
        Self::save_snapshot(
            &self.storage,
            player.gameprofile.id,
            Ok(PlayerNbtSnapshot {
                data,
                ticket: nbt_ticket,
            }),
            statistics,
        )
    }

    /// Performs periodic maintenance tasks.
    ///
    /// This function is called synchronously on the server tick loop to check
    /// if it is time to save player data.
    pub fn tick(&self, server: &Server) {
        let now = Instant::now();

        // Only save players periodically based on save_interval
        let last_save = self.last_save.load();
        let should_save = now.duration_since(last_save) >= self.save_interval;

        if should_save && self.storage.is_save_enabled() {
            self.last_save.store(now);
            // Snapshot all online players periodically across all worlds
            let mut snapshots = Vec::new();
            for world in server.worlds.load().iter() {
                for player in world.players.load().iter() {
                    let nbt = Self::nbt_snapshot(player);
                    let statistics = Self::statistics_snapshot(player);
                    // Removal can finish while this tick retains an old list.
                    // The final disconnect snapshot has already reserved its order.
                    if Self::player_session(player).is_ok_and(|session| session.snapshots_closed())
                    {
                        continue;
                    }
                    snapshots.push((player.gameprofile.id, nbt, statistics));
                }
            }

            if snapshots.is_empty() {
                return;
            }

            let storage = self.storage.clone();
            rayon::spawn(move || {
                for (uuid, nbt, statistics) in snapshots {
                    if let Err(error) = Self::save_snapshot(&storage, uuid, nbt, statistics) {
                        error!("Failed to save player progress for {uuid}: {error}");
                    }
                }
                debug!("Periodic player data save completed");
            });
        }
    }

    /// Saves all players' data immediately.
    ///
    /// This function immediately saves all online players' data to disk.
    /// Useful for server shutdown or backup operations.
    pub fn save_all_players(&self, server: &Server) -> Result<(), PlayerDataError> {
        let mut total_players = 0;
        let mut failures = Vec::new();

        // Save players from all worlds
        for world in server.worlds.load().iter() {
            for player in world.players.load().iter() {
                match self.extract_data_and_save_player(player) {
                    Ok(()) => total_players += 1,
                    Err(error) => failures.push(format!("{}: {error}", player.gameprofile.id)),
                }
            }
        }

        debug!("Saved data for {total_players} online players");
        if failures.is_empty() {
            Ok(())
        } else {
            Err(PlayerDataError::Io(std::io::Error::other(
                failures.join("; "),
            )))
        }
    }

    /// Loads player data and applies it to a player.
    ///
    /// This function loads a player's data and applies it to their Player instance.
    /// For new players, it creates default data without errors.
    ///
    /// # Arguments
    ///
    /// * `player` - The player to load data for and apply to.
    ///
    /// # Returns
    ///
    /// A Result indicating success or the error that occurred.
    pub fn load_data(&self, uuid: &uuid::Uuid) -> Result<Option<NbtCompound>, PlayerDataError> {
        self.load_session(uuid).map(|(data, _)| data)
    }

    fn save_state(&self, uuid: &uuid::Uuid) -> Arc<PlayerSaveState> {
        self.save_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(*uuid)
            .or_default()
            .clone()
    }

    /// Admission and disconnect share this gate through final persistence.
    /// It is owned and asynchronous: cancellation releases it without blocking
    /// Tokio threads or carrying a synchronous mutex guard across await.
    pub(crate) async fn admission_guard(
        &self,
        uuid: &uuid::Uuid,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        self.save_state(uuid).admission_guard().await
    }

    pub(crate) fn load_session(
        &self,
        uuid: &uuid::Uuid,
    ) -> Result<(Option<NbtCompound>, PlayerSaveSession), PlayerDataError> {
        let state = self.save_state(uuid);
        let _writer = state.writer();
        let session = state.begin_session();
        session.set_load_failed(true);
        let (present, data) = self.storage.load_player_data(uuid)?;
        session.set_load_failed(false);
        Ok((present.then_some(data), session))
    }

    pub(crate) fn reject_player(player: &Player) {
        if let Some(session) = player
            .data_save_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            session.set_load_failed(true);
        }
    }

    pub fn load_statistics(&self, player: &Player) -> Result<(), StatisticsDataError> {
        if !self.storage.is_save_enabled() {
            return Ok(());
        }
        let players = self
            .storage
            .get_data_path()
            .parent()
            .ok_or_else(|| std::io::Error::other("player data directory has no parent"))?;
        let session = player
            .data_save_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| std::io::Error::other("player save session is not initialized"))?;
        let mut stats = player
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stats.use_save_session(session);
        stats.load(players, player.gameprofile.id)
    }

    fn player_session(player: &Player) -> Result<PlayerSaveSession, PlayerDataError> {
        player
            .data_save_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                PlayerDataError::Io(std::io::Error::other(
                    "player save session is not initialized",
                ))
            })
    }

    fn nbt_snapshot(player: &Player) -> Result<PlayerNbtSnapshot, PlayerDataError> {
        let session = Self::player_session(player)?;
        // Reserve the order before reading player state. A slow earlier
        // serialization must not supersede a later disconnect snapshot.
        let ticket = session
            .ticket(PlayerSaveChannel::Nbt)
            .map_err(|reason| PlayerDataError::Io(std::io::Error::other(reason)))?;
        let mut data = NbtCompound::new();
        player.write_nbt(&mut data);
        Ok(PlayerNbtSnapshot { data, ticket })
    }

    fn statistics_snapshot(
        player: &Player,
    ) -> Result<Option<StatisticsSnapshot>, StatisticsDataError> {
        player
            .stats
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot()
    }

    // Both saves are attempted before returning an error. Statistics failures
    // must not prevent inventory, experience, and other player NBT from saving.
    fn save_snapshot(
        storage: &PlayerDataStorage,
        uuid: uuid::Uuid,
        nbt: Result<PlayerNbtSnapshot, PlayerDataError>,
        statistics: Result<Option<StatisticsSnapshot>, StatisticsDataError>,
    ) -> Result<(), PlayerDataError> {
        let statistics_result =
            statistics.and_then(|snapshot| snapshot.map_or(Ok(()), StatisticsSnapshot::save));
        let nbt_result = nbt.and_then(|snapshot| snapshot.save(storage, &uuid));
        match (statistics_result, nbt_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Ok(()), Err(error)) => Err(error),
            (Err(statistics), Ok(())) => Err(PlayerDataError::Io(std::io::Error::other(format!(
                "statistics save failed (player NBT saved): {statistics}"
            )))),
            (Err(statistics), Err(nbt)) => Err(PlayerDataError::Io(std::io::Error::other(
                format!("statistics save failed: {statistics}; player NBT save failed: {nbt}"),
            ))),
        }
    }

    /// Extracts and saves data from a player.
    ///
    /// This function extracts NBT data from a player and saves it to disk.
    ///
    /// # Arguments
    ///
    /// * `player` - The player to extract and save data for.
    ///
    /// # Returns
    ///
    /// A Result indicating success or the error that occurred.
    pub fn extract_data_and_save_player(&self, player: &Player) -> Result<(), PlayerDataError> {
        if !self.storage.is_save_enabled() {
            return Ok(());
        }

        let uuid = player.gameprofile.id;
        Self::save_snapshot(
            &self.storage,
            uuid,
            Self::nbt_snapshot(player),
            Self::statistics_snapshot(player),
        )
    }
}

#[cfg(test)]
mod test {
    use crate::data::player_progress::PlayerSaveState;
    use crate::data::player_server::{PlayerNbtSnapshot, ServerPlayerData};
    use pumpkin_nbt::compound::NbtCompound;
    use pumpkin_world::data::player_data::PlayerDataStorage;
    use std::sync::Arc;
    use std::time::Duration;
    use std::time::Instant;
    use tempfile::tempdir;
    use uuid::Uuid;

    #[tokio::test]
    async fn disconnect_handoff_waits_for_final_storage_and_cancelled_waiter_does_not_unlock_it() {
        use crate::entity::player::statistics::{CustomStatistic, Statistics};
        use pumpkin_nbt::tag::NbtTag;
        use std::{
            future::{Future, poll_fn},
            task::Poll,
        };
        let temp = tempdir().unwrap();
        let players = temp.path().join("players");
        let server_data =
            ServerPlayerData::new(players.join("data"), Duration::from_secs(60), true);
        let uuid = Uuid::new_v4();
        let (_, session) = server_data.load_session(&uuid).unwrap();
        let mut statistics = Statistics::default();
        statistics.use_save_session(session.clone());
        statistics.load(&players, uuid).unwrap();
        let mut initial = NbtCompound::new();
        initial.put_int("XpLevel", 1);
        server_data
            .storage
            .save_player_data(&uuid, initial.clone())
            .unwrap();
        let queued_tick = PlayerNbtSnapshot::new(initial, &session).unwrap();

        // Both disconnect drivers reserve this before World::remove_player can
        // unregister the UUID, then retain it through asynchronous leave events.
        let handoff = server_data.admission_guard(&uuid).await;
        let mut reconnect = Box::pin(async {
            let _admission = server_data.admission_guard(&uuid).await;
            server_data.load_session(&uuid)
        });
        assert!(
            poll_fn(|cx| Poll::Ready(reconnect.as_mut().poll(cx).is_pending())).await,
            "reconnect must wait before reading the stale experience/inventory file"
        );
        let mut cancelled = Box::pin(server_data.admission_guard(&uuid));
        assert!(poll_fn(|cx| Poll::Ready(cancelled.as_mut().poll(cx).is_pending())).await);
        drop(cancelled);
        tokio::task::yield_now().await;

        // Screen-close/leave-event mutations are represented by a changed real
        // inventory and experience, not by a session-counter assertion.
        let mut final_data = NbtCompound::new();
        final_data.put_int("XpLevel", 42);
        let mut stack = NbtCompound::new();
        stack.put_byte("Slot", 0);
        stack.put_string("id", "minecraft:diamond".to_owned());
        stack.put_int("count", 3);
        let inventory = NbtTag::List(vec![NbtTag::Compound(stack)]);
        final_data.put("Inventory", inventory.clone());
        statistics.increment_custom(CustomStatistic::Jump, 9);
        statistics.increment_custom(CustomStatistic::LeaveGame, 1);
        let (nbt_ticket, statistics_ticket) = session.final_tickets().unwrap();
        ServerPlayerData::save_snapshot(
            &server_data.storage,
            uuid,
            Ok(PlayerNbtSnapshot {
                data: final_data,
                ticket: nbt_ticket,
            }),
            statistics.snapshot_with_ticket(statistics_ticket),
        )
        .unwrap();
        let path = server_data.storage.get_player_data_path(&uuid);
        let final_bytes = std::fs::read(&path).unwrap();
        queued_tick.save(&server_data.storage, &uuid).unwrap();
        assert!(PlayerNbtSnapshot::new(NbtCompound::new(), &session).is_err());
        assert!(statistics.snapshot().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), final_bytes);
        assert!(poll_fn(|cx| Poll::Ready(reconnect.as_mut().poll(cx).is_pending())).await);
        drop(handoff);

        let (data, new_session) = reconnect.await.unwrap();
        let data = data.unwrap();
        assert_eq!(data.get_int("XpLevel"), Some(42));
        assert_eq!(data.get("Inventory"), Some(&inventory));
        statistics.use_save_session(new_session);
        statistics.load(&players, uuid).unwrap();
        assert_eq!(
            statistics.get(
                pumpkin_data::statistic::StatisticCategory::Custom,
                CustomStatistic::Jump as i32
            ),
            9
        );
        assert_eq!(
            statistics.get(
                pumpkin_data::statistic::StatisticCategory::Custom,
                CustomStatistic::LeaveGame as i32
            ),
            1
        );
        assert!(PlayerNbtSnapshot::new(NbtCompound::new(), &session).is_err());
    }

    #[test]
    fn paired_nbt_and_statistics_snapshots_are_ordered_across_reconnects() {
        use crate::entity::player::statistics::{CustomStatistic, Statistics};
        let temp = tempdir().unwrap();
        let players = temp.path().join("players");
        let server_data =
            ServerPlayerData::new(players.join("data"), Duration::from_secs(60), true);
        let uuid = Uuid::new_v4();
        let (_, session) = server_data.load_session(&uuid).unwrap();
        let mut statistics = Statistics::default();
        statistics.use_save_session(session.clone());
        statistics.load(&players, uuid).unwrap();
        let nbt_at_level = |level| {
            let mut nbt = NbtCompound::new();
            nbt.put_int("XpLevel", level);
            nbt
        };
        let older_nbt = PlayerNbtSnapshot::new(nbt_at_level(10), &session);
        statistics.set(
            crate::entity::player::statistics::StatisticCategory::Custom,
            CustomStatistic::Jump as i32,
            10,
        );
        let older_stats = statistics.snapshot();
        let newer_nbt = PlayerNbtSnapshot::new(nbt_at_level(20), &session);
        statistics.set(
            crate::entity::player::statistics::StatisticCategory::Custom,
            CustomStatistic::Jump as i32,
            20,
        );
        ServerPlayerData::save_snapshot(
            &server_data.storage,
            uuid,
            newer_nbt,
            statistics.snapshot(),
        )
        .unwrap();
        let path = server_data.storage.get_player_data_path(&uuid);
        let stats_path = players.join("stats").join(format!("{uuid}.json"));
        let latest_nbt = std::fs::read(&path).unwrap();
        let latest_stats = std::fs::read(&stats_path).unwrap();
        ServerPlayerData::save_snapshot(&server_data.storage, uuid, older_nbt, older_stats)
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), latest_nbt);
        assert_eq!(std::fs::read(&stats_path).unwrap(), latest_stats);
        assert_eq!(
            server_data
                .storage
                .load_player_data(&uuid)
                .unwrap()
                .1
                .get_int("XpLevel"),
            Some(20)
        );

        let queued_previous_session = PlayerNbtSnapshot::new(nbt_at_level(70), &session).unwrap();
        let (_, new_session) = server_data.load_session(&uuid).unwrap();
        queued_previous_session
            .save(&server_data.storage, &uuid)
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), latest_nbt);
        assert!(PlayerNbtSnapshot::new(nbt_at_level(80), &session).is_err());
        assert!(statistics.snapshot().is_err());
        statistics.use_save_session(new_session.clone());
        statistics.load(&players, uuid).unwrap();
        let newer = PlayerNbtSnapshot::new(nbt_at_level(30), &new_session);
        ServerPlayerData::save_snapshot(&server_data.storage, uuid, newer, statistics.snapshot())
            .unwrap();
        assert_eq!(
            server_data
                .storage
                .load_player_data(&uuid)
                .unwrap()
                .1
                .get_int("XpLevel"),
            Some(30)
        );

        // A rejected load disables both channels for that current player too.
        let queued = PlayerNbtSnapshot::new(nbt_at_level(90), &new_session).unwrap();
        new_session.set_load_failed(true);
        queued.save(&server_data.storage, &uuid).unwrap();
        assert!(matches!(
            statistics.snapshot(),
            Err(crate::entity::player::statistics::StatisticsDataError::FailedLoad(_))
        ));
        assert!(PlayerNbtSnapshot::new(nbt_at_level(90), &new_session).is_err());
        assert_eq!(
            server_data
                .storage
                .load_player_data(&uuid)
                .unwrap()
                .1
                .get_int("XpLevel"),
            Some(30)
        );
    }

    #[test]
    fn failed_nbt_load_propagates_and_invalidates_pending_writes() {
        let temp = tempdir().unwrap();
        let server_data = ServerPlayerData::new(
            temp.path().join("players/data"),
            Duration::from_secs(60),
            true,
        );
        let uuid = Uuid::new_v4();
        let (_, session) = server_data.load_session(&uuid).unwrap();
        let mut old = NbtCompound::new();
        old.put_int("XpLevel", 20);
        let queued = PlayerNbtSnapshot::new(old, &session).unwrap();
        let path = server_data.storage.get_player_data_path(&uuid);
        let corrupt = b"retained unreadable player NBT";
        std::fs::write(&path, corrupt).unwrap();
        assert!(server_data.load_data(&uuid).is_err());
        queued.save(&server_data.storage, &uuid).unwrap();
        assert!(PlayerNbtSnapshot::new(NbtCompound::new(), &session).is_err());
        assert!(
            server_data
                .storage
                .save_player_data(&uuid, NbtCompound::new())
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), corrupt);
    }

    #[test]
    fn statistics_errors_do_not_prevent_player_nbt_save_and_both_errors_are_reported() {
        use crate::entity::player::statistics::{CustomStatistic, Statistics, StatisticsDataError};
        let temp = tempdir().unwrap();
        let players = temp.path().join("players");
        let storage = PlayerDataStorage::new(players.join("data"), true);
        let uuid = Uuid::new_v4();
        let session = Arc::new(PlayerSaveState::default()).begin_session();
        let mut statistics = Statistics::default();
        statistics.load(&players, uuid).unwrap();
        statistics.increment_custom(CustomStatistic::Jump, 1);
        // Fail the real statistics writer by occupying its directory with a file.
        std::fs::write(players.join("stats"), "blocked directory").unwrap();
        let mut nbt = NbtCompound::new();
        nbt.put_int("XpLevel", 42);
        let error = ServerPlayerData::save_snapshot(
            &storage,
            uuid,
            PlayerNbtSnapshot::new(nbt, &session),
            statistics.snapshot(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("player NBT saved"));
        let (loaded, saved) = storage.load_player_data(&uuid).unwrap();
        assert!(loaded);
        assert_eq!(saved.get_int("XpLevel"), Some(42));

        let mut nbt = NbtCompound::new();
        nbt.put_int("XpLevel", 43);
        let error = ServerPlayerData::save_snapshot(
            &storage,
            uuid,
            PlayerNbtSnapshot::new(nbt, &session),
            Err(StatisticsDataError::UnknownNumeric(8, -1)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("unknown numeric statistic"));
        assert_eq!(
            storage
                .load_player_data(&uuid)
                .unwrap()
                .1
                .get_int("XpLevel"),
            Some(43)
        );

        let unavailable_data = temp.path().join("unavailable");
        std::fs::write(&unavailable_data, "blocked directory").unwrap();
        let unavailable = PlayerDataStorage::new(unavailable_data, true);
        let error = ServerPlayerData::save_snapshot(
            &unavailable,
            uuid,
            PlayerNbtSnapshot::new(NbtCompound::new(), &session),
            Err(StatisticsDataError::UnknownNumeric(8, -1)),
        )
        .unwrap_err();
        assert!(error.to_string().contains("statistics save failed"));
        assert!(error.to_string().contains("player NBT save failed"));
    }

    #[tokio::test]
    async fn player_data_storage_new() {
        // Create a temporary directory for testing
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let storage = PlayerDataStorage::new(path.clone(), true);

        assert_eq!(storage.get_data_path().as_path(), path.as_path());
        // Note: save_enabled might be configured differently in your actual code
    }

    #[tokio::test]
    async fn player_data_storage_get_player_data_path() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let storage = PlayerDataStorage::new(path.clone(), true);

        let uuid = Uuid::new_v4();
        let expected_path = path.join(format!("{uuid}.dat"));

        assert_eq!(storage.get_player_data_path(&uuid), expected_path);
    }

    #[tokio::test]
    async fn player_data_storage_save_and_load() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let storage = PlayerDataStorage::new(path, true); // Ensure saving is enabled for this test

        let uuid = Uuid::new_v4();

        // Create test data
        let mut nbt = NbtCompound::new();
        nbt.put_string("TestKey", "TestValue".to_string());
        nbt.put_int("TestInt", 42);

        // Save the data
        storage.save_player_data(&uuid, nbt).unwrap();

        // Load the data
        let (load_success, loaded_nbt) = storage.load_player_data(&uuid).unwrap();

        assert!(load_success);
        assert_eq!(loaded_nbt.get_string("TestKey").unwrap(), "TestValue");
        assert_eq!(loaded_nbt.get_int("TestInt").unwrap(), 42);
    }

    #[tokio::test]
    async fn player_data_storage_load_nonexistent() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let storage = PlayerDataStorage::new(path, true); // Ensure saving is enabled for this test

        let uuid = Uuid::new_v4();

        // Try to load non-existent data
        let (load_success, empty_nbt) = storage.load_player_data(&uuid).unwrap();

        assert!(!load_success);
        assert_eq!(empty_nbt.child_tags.len(), 0);
    }

    #[tokio::test]
    async fn player_data_storage_disabled() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let storage = PlayerDataStorage::new(path, false);

        let uuid = Uuid::new_v4();
        let mut nbt = NbtCompound::new();
        nbt.put_string("TestKey", "TestValue".to_string());

        // Save should succeed but do nothing
        let save_result = storage.save_player_data(&uuid, nbt);
        assert!(save_result.is_ok());

        // Load should return empty data
        let (load_success, empty_nbt) = storage.load_player_data(&uuid).unwrap();
        assert!(!load_success);
        assert_eq!(empty_nbt.child_tags.len(), 0);
    }

    #[tokio::test]
    async fn server_player_data_new() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();
        let save_interval = Duration::from_mins(5);

        let player_data = ServerPlayerData::new(path, save_interval, true);

        assert_eq!(player_data.save_interval, save_interval);
        assert!(
            Instant::now().duration_since(player_data.last_save.load()) < Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn player_data_file_structure() {
        let temp_dir = tempdir().unwrap();
        let path = temp_dir.path().to_path_buf();

        let uuid = Uuid::new_v4();
        let storage = PlayerDataStorage::new(path, true);

        // Create and save player data
        let mut nbt = NbtCompound::new();
        nbt.put_string("name", "TestPlayer".to_string());
        nbt.put_int("level", 42);
        storage.save_player_data(&uuid, nbt).unwrap();

        // Verify the file exists
        let player_data_path = storage.get_player_data_path(&uuid);
        assert!(player_data_path.exists());

        // Load it again and verify content
        let (success, loaded_data) = storage.load_player_data(&uuid).unwrap();
        assert!(success);
        assert_eq!(loaded_data.get_string("name").unwrap(), "TestPlayer");
        assert_eq!(loaded_data.get_int("level").unwrap(), 42);
    }
}
