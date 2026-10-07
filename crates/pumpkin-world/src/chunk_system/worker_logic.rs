use super::chunk_state::{Chunk, StagedChunkEnum};
use super::generation_cache::Cache;
use super::{ChunkPos, IOLock};
use crate::ProtoChunk;
use crate::chunk::format::LightContainer;
use crate::chunk::io::LoadedData::Loaded;
use crate::chunk::io::{FileIO, LoadedData, run_blocking};
use crate::level::Level;
use pumpkin_config::lighting::LightingEngineConfig;
use pumpkin_data::chunk::ChunkStatus;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use tracing::{Instrument, debug, debug_span, error, warn};

pub enum RecvChunk {
    IO(Chunk),
    ReadFailure {
        error: String,
    },
    Generation(Cache),
    GenerationFailure {
        pos: ChunkPos,
        stage: StagedChunkEnum,
        error: String,
    },
}

fn read_failure(level: &Level, pos: ChunkPos, cause: impl std::fmt::Display) -> RecvChunk {
    let error = format!("Failed to read chunk {pos:?}: {cause}");
    level.fail_chunk_system(error.clone());
    RecvChunk::ReadFailure { error }
}

/// Checks if a chunk needs relighting based on the current lighting configuration
/// Returns true if the chunk has uniform lighting (from full/dark mode) but the server
/// is now running in default mode (which needs proper lighting calculation)
fn needs_relighting(chunk: &crate::chunk::ChunkData, config: LightingEngineConfig) -> bool {
    if config != LightingEngineConfig::Default {
        return false;
    }

    // If the chunk says it's already lit, believe it.
    if chunk.light_populated.load(Relaxed) {
        return false;
    }

    let engine = chunk
        .light_engine
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // Scan for any complex lighting data
    let has_complex_light = engine.sky_light.iter().any(|lc| match lc {
        LightContainer::Full(data) => data.iter().any(|&b| b != 0x00 && b != 0xFF),
        LightContainer::Empty(val) => *val != 0 && *val != 15,
    }) || engine.block_light.iter().any(|lc| match lc {
        LightContainer::Full(data) => data.iter().any(|&b| b != 0x00 && b != 0xFF),
        LightContainer::Empty(val) => *val != 0 && *val != 15,
    });

    // If it has complex light, we don't need to relight.
    !has_complex_light
}

fn load_proto_chunk(chunk: &Arc<crate::chunk::ChunkData>, level: &Level) -> ProtoChunk {
    let mut proto = ProtoChunk::from_chunk_data(chunk, &level.world_gen.load());
    proto.original_chunk = Some(chunk.clone());
    proto
}

fn process_loaded_chunk(chunk: Arc<crate::chunk::ChunkData>, level: &Level) -> Chunk {
    let pos = ChunkPos::new(chunk.x, chunk.z);
    if chunk.status == ChunkStatus::Full {
        let needs_relight = needs_relighting(&chunk, level.lighting_config);
        if needs_relight {
            debug!(
                "Chunk {pos:?} has uniform lighting, downgrading to Features stage for relighting"
            );

            let mut proto = load_proto_chunk(&chunk, level);

            // Clear all lighting data
            let section_count = proto.light.sky_light.len();
            proto.light.sky_light = (0..section_count)
                .map(|_| LightContainer::new_empty(15))
                .collect();
            proto.light.block_light = (0..section_count)
                .map(|_| LightContainer::new_empty(0))
                .collect();
            proto.stage = StagedChunkEnum::Features;
            Chunk::Proto(Box::new(proto))
        } else {
            Chunk::Level(chunk)
        }
    } else {
        let proto = load_proto_chunk(&chunk, level);
        Chunk::Proto(Box::new(proto))
    }
}

pub async fn io_read_work(
    recv: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Vec<ChunkPos>>>>,
    send: crossbeam::channel::Sender<(ChunkPos, RecvChunk)>,
    level: Arc<Level>,
    lock: IOLock,
) {
    debug!("io read thread start");

    // Cleaner loop and async recv
    loop {
        let batch = {
            let mut lock_rx = recv.lock().await;
            lock_rx.recv().await
        };
        let Some(batch) = batch else {
            break;
        };
        for pos in &batch {
            // Lock handling
            loop {
                let notified = lock.1.notified();
                if !lock
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(pos)
                {
                    break;
                }
                tokio::select! {
                    () = notified => {},
                    () = level.cancel_token.cancelled() => return,
                }
            }
        }

        let (t_send, mut t_recv) = tokio::sync::mpsc::channel(1000);

        let batch_len = batch.len();
        let mut pending: super::HashSetType<_> = batch.iter().copied().collect();
        let level_clone = level.clone();

        let fetch_task = tokio::spawn(async move {
            level_clone
                .chunk_saver
                .fetch_chunks(&level_clone.level_folder, &batch, t_send)
                .await;
        });

        let mut receiver_closed = false;
        for _ in 0..batch_len {
            let Some(data) = t_recv.recv().await else {
                break;
            };

            let received_pos = match &data {
                Loaded(chunk) => ChunkPos::new(chunk.x, chunk.z),
                LoadedData::Missing(pos) | LoadedData::Error((pos, _)) => *pos,
            };
            pending.remove(&received_pos);
            let (pos, received) = match data {
                LoadedData::Error((pos, cause)) => (pos, read_failure(&level, pos, cause)),
                Loaded(chunk) => {
                    let pos = ChunkPos::new(chunk.x, chunk.z);
                    let level_for_processing = level.clone();
                    let result =
                        run_blocking(move || process_loaded_chunk(chunk, &level_for_processing))
                            .await;
                    let received = match result {
                        Ok(processed) => RecvChunk::IO(processed),
                        Err(err) => read_failure(&level, pos, err),
                    };
                    (pos, received)
                }
                LoadedData::Missing(pos) => (
                    pos,
                    RecvChunk::IO(Chunk::Proto(Box::new(ProtoChunk::new(
                        pos.x,
                        pos.y,
                        &level.world_gen.load(),
                    )))),
                ),
            };
            if send.send((pos, received)).is_err() {
                receiver_closed = true;
                break;
            }
        }
        // Let bounded serializer forwarding unwind if the scheduler has stopped.
        drop(t_recv);
        let fetch_result = fetch_task.await;
        if receiver_closed {
            break;
        }
        for pos in pending {
            let cause = match &fetch_result {
                Ok(()) => "Chunk loader closed without returning a result".to_string(),
                Err(err) => format!("Chunk loader task failed: {err}"),
            };
            if send.send((pos, read_failure(&level, pos, cause))).is_err() {
                break;
            }
        }
    }
    debug!("io read thread stop");
}

pub async fn io_write_work(
    mut recv: tokio::sync::mpsc::Receiver<Vec<(ChunkPos, Chunk)>>,
    level: Arc<Level>,
    lock: IOLock,
) {
    let mut batch = 0u64;
    loop {
        // Don't check cancel_token here (keep saving chunks)
        let Some(data) = recv.recv().await else {
            debug!(
                target: "pumpkin_save_lineage", kind = "writer_closed",
                world = %level.level_folder.dim_folder.display(),
                dimension = level.world_gen.load().dimension().minecraft_name,
                "save_lineage"
            );
            break;
        };
        batch += 1;
        debug!(
            target: "pumpkin_save_lineage", kind = "receive",
            world = %level.level_folder.dim_folder.display(),
            dimension = level.world_gen.load().dimension().minecraft_name,
            batch, chunk_count = data.len(), "save_lineage"
        );
        let save_span = debug_span!(
            target: "pumpkin_save_lineage", "terrain_batch",
            world = %level.level_folder.dim_folder.display(),
            dimension = level.world_gen.load().dimension().minecraft_name,
            batch
        );
        // debug!("io write thread receive chunks size {}", data.len());
        let positions = data.iter().map(|(pos, _)| *pos).collect::<Vec<_>>();
        let level_for_upgrade = level.clone();
        let upgrade_result = run_blocking(move || {
            let mut vec = Vec::with_capacity(data.len());
            for (pos, chunk) in data {
                match chunk {
                    Chunk::Level(chunk) => vec.push((pos, chunk)),
                    Chunk::Proto(chunk) => {
                        let mut temp = Chunk::Proto(chunk);
                        temp.upgrade_to_level_chunk(
                            level_for_upgrade.world_gen.load().dimension(),
                            &level_for_upgrade.lighting_config,
                        );
                        let Chunk::Level(chunk) = temp else { panic!() };
                        vec.push((pos, chunk));
                    }
                }
            }
            vec
        })
        .await;
        let chunks = match upgrade_result {
            Ok(chunks) => chunks,
            Err(cause) => {
                debug!(
                    target: "pumpkin_save_lineage", kind = "upgrade_failed",
                    world = %level.level_folder.dim_folder.display(),
                    dimension = level.world_gen.load().dimension().minecraft_name,
                    batch, error = %cause, "save_lineage"
                );
                level.fail_chunk_system(format!("Failed to upgrade chunks for saving: {cause}"));
                // Keep the read barrier: disk does not contain this version.
                break;
            }
        };
        let mut retry_delay = std::time::Duration::from_secs(1);
        while let Err(cause) = level
            .chunk_saver
            .save_chunks(&level.level_folder, chunks.clone())
            .instrument(save_span.clone())
            .await
        {
            debug!(
                target: "pumpkin_save_lineage", kind = "write_failed",
                world = %level.level_folder.dim_folder.display(),
                dimension = level.world_gen.load().dimension().minecraft_name,
                batch, error = %cause, retry_seconds = retry_delay.as_secs(), "save_lineage"
            );
            error!(
                "Failed to save chunks: {cause}; retaining data and retrying in {}s",
                retry_delay.as_secs()
            );
            // Keep both the canonical instances and read barriers until commit,
            // including during shutdown. Cancelling here would discard world data.
            tokio::time::sleep(retry_delay).await;
            retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(30));
            debug!(
                target: "pumpkin_save_lineage", kind = "retry",
                world = %level.level_folder.dim_folder.display(),
                dimension = level.world_gen.load().dimension().minecraft_name,
                batch, "save_lineage"
            );
        }

        release_saved_chunk_barriers(&lock, positions);
        debug!(
            target: "pumpkin_save_lineage", kind = "complete",
            world = %level.level_folder.dim_folder.display(),
            dimension = level.world_gen.load().dimension().minecraft_name,
            batch, "save_lineage"
        );
    }
}

fn release_saved_chunk_barriers(lock: &IOLock, positions: Vec<ChunkPos>) {
    {
        let mut data = lock
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for i in positions {
            match data.entry(i) {
                Entry::Occupied(mut entry) => {
                    let rc = entry.get_mut();
                    if *rc <= 1 {
                        entry.remove();
                    } else {
                        *rc -= 1;
                    }
                }
                Entry::Vacant(_) => {
                    warn!(
                        "io_write: attempted to release missing lock entry for {:?}",
                        i
                    );
                }
            }
        }
    }
    lock.1.notify_waiters();
}

pub fn run_generation(
    pos: ChunkPos,
    mut cache: Cache,
    stage: StagedChunkEnum,
    level: &Level,
) -> RecvChunk {
    let portal = level.world_portal.load_full();
    let Some(portal_ref) = portal.as_deref() else {
        error!("Chunk generation FAILED at {pos:?} ({stage:?}): World portal is not initialized");
        return RecvChunk::GenerationFailure {
            pos,
            stage,
            error: "World portal is not initialized".to_string(),
        };
    };
    // Run generation with panic catching
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cache.advance(
            stage,
            &level.world_gen.load(),
            portal_ref,
            &level.lighting_config,
        );
        cache // Return cache on success
    }));

    match result {
        Ok(cache) => RecvChunk::Generation(cache),
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| {
                    payload
                        .downcast_ref::<String>()
                        .map(std::string::String::as_str)
                })
                .unwrap_or("Unknown panic payload");

            error!("Chunk generation FAILED at {pos:?} ({stage:?}): {msg}");

            RecvChunk::GenerationFailure {
                pos,
                stage,
                error: msg.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_config::world::LevelConfig;
    use pumpkin_data::dimension::Dimension;
    use std::time::Duration;

    #[tokio::test]
    async fn saving_imported_chunks_keeps_full_status_and_original_metadata() {
        use crate::chunk::{ChunkData, format::anvil::SingleChunkDataSerializer};
        use crate::tick::{ScheduledTick, TickPriority};
        use pumpkin_data::{Block, fluid::Fluid};
        use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};
        use pumpkin_util::math::{position::BlockPos, vector2::Vector2};

        let directory = tempfile::tempdir().unwrap();
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            directory.path().into(),
            0,
            Dimension::OVERWORLD,
        )
        .unwrap();
        for (status, expected) in [
            (ChunkStatus::Full, "minecraft:full"),
            (ChunkStatus::Light, "minecraft:light"),
            (ChunkStatus::Spawn, "minecraft:spawn"),
        ] {
            let mut imported = ChunkData::empty(0, 0);
            imported.status = status;
            imported.inhabited_time.store(6_992_414, Relaxed);
            let mut original_tags = NbtCompound::new();
            original_tags.put_list(
                "PostProcessing",
                vec![NbtTag::List(vec![NbtTag::Short(42)])],
            );
            original_tags.put_string("example:unknown", "retained".into());
            *imported.preserved_tags.lock().unwrap() = original_tags.clone();
            imported.custom_data.lock().unwrap().put_int("owner", 17);
            let position = BlockPos::new(1, 64, 2);
            imported.block_ticks.schedule_tick(
                &ScheduledTick {
                    delay: -134,
                    priority: TickPriority::Normal,
                    position,
                    value: &Block::STONE,
                },
                0,
            );
            imported.fluid_ticks.schedule_tick(
                &ScheduledTick {
                    delay: 70_000,
                    priority: TickPriority::Normal,
                    position,
                    value: &Fluid::WATER,
                },
                0,
            );
            // A converted full chunk without isLightOn or light arrays takes this
            // same load path. Exercise the codec as well as the relighting/save bridge.
            let bytes = imported.to_bytes().unwrap();
            let decoded = ChunkData::from_bytes(&bytes, Vector2::new(0, 0)).unwrap();
            let mut chunk = process_loaded_chunk(Arc::new(decoded), &level);
            let Chunk::Proto(proto) = &mut chunk else {
                panic!("unlit imported chunks must enter the proto load path");
            };
            if status == ChunkStatus::Full {
                assert_eq!(proto.stage, StagedChunkEnum::Features);
                // Saving a relighting dependency after Spawn must keep its already
                // completed generation status, without promoting incomplete chunks.
                proto.stage = StagedChunkEnum::Spawn;
            }
            chunk.upgrade_to_level_chunk(&Dimension::OVERWORLD, &LightingEngineConfig::Default);
            let Chunk::Level(saved) = chunk else {
                unreachable!()
            };
            let bytes = saved.to_bytes().unwrap();
            let mut cursor = std::io::Cursor::new(bytes.as_ref());
            let mut reader = pumpkin_nbt::deserializer::NbtReadHelperJava::new(&mut cursor);
            let root = pumpkin_nbt::Nbt::read(&mut reader).unwrap().root_tag;
            assert_eq!(root.get_string("Status"), Some(expected));
            assert_eq!(root.get_long("InhabitedTime"), Some(6_992_414));
            for (name, tag) in &original_tags.child_tags {
                assert_eq!(root.get(name), Some(tag));
            }
            let reloaded = ChunkData::from_bytes(&bytes, Vector2::new(0, 0)).unwrap();
            assert_eq!(
                reloaded.custom_data.lock().unwrap().get_int("owner"),
                Some(17)
            );
            assert_eq!(reloaded.block_ticks.to_vec()[0].delay, -134);
            assert_eq!(reloaded.fluid_ticks.to_vec()[0].delay, 70_000);
        }
        tokio::time::timeout(Duration::from_secs(10), level.shutdown())
            .await
            .unwrap();
    }

    async fn read_batch(
        level: &Arc<Level>,
        positions: Vec<ChunkPos>,
    ) -> Vec<(ChunkPos, RecvChunk)> {
        let (request, recv) = tokio::sync::mpsc::channel(1);
        let (send, results) = crossbeam::channel::unbounded();
        let lock = Arc::new((
            std::sync::Mutex::new(super::super::HashMapType::default()),
            tokio::sync::Notify::new(),
        ));
        request.send(positions).await.unwrap();
        drop(request);
        tokio::time::timeout(
            Duration::from_secs(10),
            io_read_work(
                Arc::new(tokio::sync::Mutex::new(recv)),
                send,
                level.clone(),
                lock,
            ),
        )
        .await
        .unwrap();
        results.try_iter().collect()
    }

    #[tokio::test]
    async fn corrupt_region_is_not_generated_or_overwritten() {
        for original in [b"unreadable existing region".as_slice(), b""] {
            let directory = tempfile::tempdir().unwrap();
            let level = Level::from_root_folder(
                &LevelConfig::default(),
                directory.path().to_path_buf(),
                0,
                Dimension::OVERWORLD,
            )
            .unwrap();
            let path = level.level_folder.region_folder.join("r.0.0.mca");
            tokio::fs::write(&path, original).await.unwrap();
            let positions = vec![ChunkPos::new(0, 0), ChunkPos::new(1, 0)];
            let results = read_batch(&level, positions.clone()).await;
            assert_eq!(
                results.len(),
                positions.len(),
                "every request must receive a result"
            );
            for (pos, result) in results {
                assert!(positions.contains(&pos));
                let RecvChunk::ReadFailure { error } = result else {
                    panic!("unreadable existing chunks must never enter generation");
                };
                assert!(error.contains(&format!("{pos:?}")));
                assert!(error.contains("Failed reading region"));
            }
            assert!(level.shut_down_chunk_system.load(Relaxed));
            tokio::time::timeout(Duration::from_secs(10), level.shutdown())
                .await
                .unwrap();
            assert_eq!(tokio::fs::read(&path).await.unwrap(), original);
            assert!(level.loaded_chunks.is_empty());
        }
    }

    #[tokio::test]
    async fn dropped_scheduler_unblocks_a_backpressured_read_batch() {
        let directory = tempfile::tempdir().unwrap();
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            directory.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        )
        .unwrap();
        let (request, recv) = tokio::sync::mpsc::channel(1);
        let (send, results) = crossbeam::channel::unbounded();
        drop(results);
        let lock = Arc::new((
            std::sync::Mutex::new(super::super::HashMapType::default()),
            tokio::sync::Notify::new(),
        ));
        request
            .send((0..2001).map(|x| ChunkPos::new(x, 0)).collect())
            .await
            .unwrap();
        drop(request);
        tokio::time::timeout(
            Duration::from_secs(10),
            io_read_work(
                Arc::new(tokio::sync::Mutex::new(recv)),
                send,
                level.clone(),
                lock,
            ),
        )
        .await
        .unwrap();
        assert!(!level.shut_down_chunk_system.load(Relaxed));
        tokio::time::timeout(Duration::from_secs(10), level.shutdown())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn failed_terrain_write_keeps_reload_barrier_until_retry_commits() {
        use crate::chunk::{ChunkData, io::Dirtiable};
        let directory = tempfile::tempdir().unwrap();
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            directory.path().into(),
            0,
            Dimension::OVERWORLD,
        )
        .unwrap();
        let position = ChunkPos::new(0, 0);
        let chunk = Arc::new(ChunkData::empty(0, 0));
        chunk.mark_dirty(true);
        let temporary = level.level_folder.region_folder.join("r.0.0.tmp");
        tokio::fs::create_dir(&temporary).await.unwrap();
        let lock = Arc::new((
            std::sync::Mutex::new(super::super::HashMapType::default()),
            tokio::sync::Notify::new(),
        ));
        lock.0.lock().unwrap().insert(position, 1);
        let (send, recv) = tokio::sync::mpsc::channel(1);
        send.send(vec![(position, Chunk::Level(chunk.clone()))])
            .await
            .unwrap();
        drop(send);
        let mut writer = tokio::spawn(io_write_work(recv, level.clone(), lock.clone()));
        assert!(
            tokio::time::timeout(Duration::from_secs(2), &mut writer)
                .await
                .is_err(),
            "a failed write must remain pending instead of claiming success"
        );
        assert_eq!(lock.0.lock().unwrap().get(&position), Some(&1));
        assert!(chunk.is_dirty());
        tokio::fs::remove_dir(&temporary).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), writer)
            .await
            .unwrap()
            .unwrap();
        assert!(!lock.0.lock().unwrap().contains_key(&position));
        assert!(!chunk.is_dirty());
        let (send, mut recv) = tokio::sync::mpsc::channel(1);
        level
            .chunk_saver
            .fetch_chunks(&level.level_folder, &[position], send)
            .await;
        assert!(matches!(recv.recv().await, Some(LoadedData::Loaded(_))));
        tokio::time::timeout(Duration::from_secs(10), level.shutdown())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn absent_region_still_starts_chunk_generation() {
        let directory = tempfile::tempdir().unwrap();
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            directory.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        )
        .unwrap();
        let pos = ChunkPos::new(0, 0);
        let results = read_batch(&level, vec![pos]).await;
        assert_eq!(results.len(), 1);
        let (received, result) = results.into_iter().next().unwrap();
        assert_eq!(received, pos);
        let RecvChunk::IO(Chunk::Proto(chunk)) = result else {
            panic!("absent chunks must remain eligible for generation");
        };
        assert_eq!(chunk.stage, StagedChunkEnum::Empty);
        assert!(!level.shut_down_chunk_system.load(Relaxed));
        tokio::time::timeout(Duration::from_secs(10), level.shutdown())
            .await
            .unwrap();
    }
}
