use crate::chunk::format::linear::LinearV2File;
use crate::chunk::format::pump::PumpFile;
use crate::chunk_system::{ChunkListener, ChunkLoading, GenerationSchedule, LevelChannel};
use crate::generation::generator::WorldGenerator;
use crate::lighting::DynamicLightEngine;
use crate::{
    chunk::{
        ChunkData, ChunkEntityData, ChunkReadingError, ChunkWritingError,
        format::anvil::AnvilChunkFile,
        io::{
            Dirtiable, FileIO, LoadedData,
            file_manager::{ChunkFileManager, LevelFileIO},
        },
        palette::has_random_ticking_fluid,
    },
    generation::get_world_gen_with_all_settings,
    tick::{OrderedTick, ScheduledTick, TickPriority},
    world::WorldPortalExt,
};
use arc_swap::ArcSwap;
use crossbeam::queue::SegQueue;
use dashmap::{DashMap, Entry};
use pumpkin_config::{chunk::ChunkConfig, lighting::LightingEngineConfig, world::LevelConfig};
use pumpkin_data::biome::Biome;
use pumpkin_data::dimension::Dimension;
use pumpkin_data::{Block, BlockStateId, block_properties::has_random_ticks, fluid::Fluid};
use pumpkin_util::math::{position::BlockPos, vector2::Vector2};
use pumpkin_util::world_seed::Seed;
use rustc_hash::FxHashSet;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
// use tokio::runtime::Handle;
use tokio::{
    sync::{
        mpsc::{self, Receiver},
        oneshot,
    },
    task::JoinHandle,
};
use tokio_util::task::TaskTracker;

pub type SyncChunk = Arc<ChunkData>;
pub type SyncEntityChunk = Arc<ChunkEntityData>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadedChunkChange {
    Loaded(Vector2<i32>),
    Unloaded(Vector2<i32>),
}

pub type ChunkSaver =
    LevelFileIO<LinearV2File<ChunkData>, AnvilChunkFile<ChunkData>, PumpFile<ChunkData>>;

pub type EntitySaver = LevelFileIO<
    LinearV2File<ChunkEntityData>,
    AnvilChunkFile<ChunkEntityData>,
    PumpFile<ChunkEntityData>,
>;

/// The `Level` module provides functionality for working with chunks within or outside a Minecraft world.
///
/// Key features include:
///
/// - **Chunk Loading:** Efficiently loads chunks from disk.
/// - **Chunk Caching:** Stores accessed chunks in memory for faster access.
/// - **Chunk Generation:** Generates new chunks on-demand using a specified `WorldGenerator`.
///
/// For more details on world generation, refer to the `WorldGenerator` module.
pub struct Level {
    pub seed: Seed,
    pub world_portal: ArcSwap<Option<Arc<dyn WorldPortalExt>>>,
    pub level_folder: Arc<LevelFolder>,
    pub lighting_config: LightingEngineConfig,

    /// Counts the number of ticks that have been scheduled for this world
    schedule_tick_counts: AtomicU64,

    // Chunks that are paired with chunk watchers. When a chunk is no longer watched, it is removed
    // from the loaded chunks map and sent to the underlying ChunkIO
    pub loaded_chunks: Arc<DashMap<Vector2<i32>, SyncChunk>>,
    pub(crate) loaded_chunk_changes: Arc<SegQueue<LoadedChunkChange>>,
    loaded_entity_chunks: Arc<DashMap<Vector2<i32>, SyncEntityChunk>>,
    pub chunks_with_scheduled_ticks: Arc<dashmap::DashSet<Vector2<i32>>>,
    pub chunk_loading: Mutex<ChunkLoading>,
    /// Covers block-entity tick batches and the final snapshot before terrain eviction.
    pub block_entity_tick_lock: Mutex<()>,

    chunk_watchers: Arc<DashMap<Vector2<i32>, usize>>,

    pub chunk_saver: Arc<ChunkSaver>,
    entity_saver: Arc<EntitySaver>,

    pub world_gen: ArcSwap<WorldGenerator>,

    /// Handles runtime lighting updates
    pub light_engine: DynamicLightEngine,

    /// Tracks tasks associated with this world instance
    tasks: TaskTracker,
    pub chunk_system_tasks: TaskTracker,
    /// Notification that interrupts tasks for shutdown
    pub cancel_token: CancellationToken,

    pub shut_down_chunk_system: AtomicBool,
    pub should_save: AtomicBool,
    pub should_unload: AtomicBool,
    /// Whether periodic autosaving is enabled. Toggled by `/save-off` and `/save-on`;
    /// a manual `/save-all` still saves while this is `false`.
    pub save_enabled: AtomicBool,
    /// Number of ticks between autosave checks. If 0, autosave is disabled.
    pub autosave_ticks: u64,

    pending_entity_loads: DashMap<Vector2<i32>, Arc<tokio::sync::OnceCell<SyncEntityChunk>>>,
    pending_entity_generations: Arc<DashMap<Vector2<i32>, Vec<oneshot::Sender<SyncEntityChunk>>>>,

    pub level_channel: Arc<LevelChannel>,
    pub thread_tracker: Mutex<Vec<thread::JoinHandle<()>>>,
    pub chunk_listener: Arc<ChunkListener>,
}

pub struct TickData {
    pub block_ticks: Vec<OrderedTick<&'static Block>>,
    pub fluid_ticks: Vec<OrderedTick<&'static Fluid>>,
    pub random_ticks: Vec<RandomTickSample>,
}

#[derive(Clone, Copy)]
pub struct RandomTickSample {
    pub position: BlockPos,
    pub tick_block: bool,
    pub tick_fluid: bool,
}

pub struct LevelFolder {
    pub root_folder: PathBuf,
    pub dim_folder: PathBuf,
    pub region_folder: PathBuf,
    pub entities_folder: PathBuf,
    pub poi_folder: PathBuf,
}

impl Level {
    #[expect(clippy::too_many_lines)]
    pub fn from_root_folder(
        level_config: &LevelConfig,
        root_folder: PathBuf,
        seed: i64,
        dimension: Dimension,
    ) -> Result<Arc<Self>, crate::world_info::WorldInfoError> {
        let (namespace, name) = match dimension.minecraft_name.split_once(':') {
            Some((ns, n)) => (ns, n),
            None => ("minecraft", dimension.minecraft_name),
        };

        // 26.2 canonical layout: root_folder/dimensions/<namespace>/<name>
        let canonical_dim_folder = root_folder.join("dimensions").join(namespace).join(name);

        // Check if canonical 26.2 folder exists, or fall back to pre-26.2 legacy folders
        let dim_folder = if canonical_dim_folder.exists() {
            canonical_dim_folder
        } else if dimension.minecraft_name == Dimension::OVERWORLD.minecraft_name
            && root_folder.join("region").exists()
        {
            root_folder.clone()
        } else if dimension.minecraft_name == Dimension::THE_NETHER.minecraft_name
            && root_folder.join("DIM-1").join("region").exists()
        {
            root_folder.join("DIM-1")
        } else if dimension.minecraft_name == Dimension::THE_END.minecraft_name
            && root_folder.join("DIM1").join("region").exists()
        {
            root_folder.join("DIM1")
        } else {
            canonical_dim_folder
        };

        let region_folder = dim_folder.join("region");
        let entities_folder = dim_folder.join("entities");
        let poi_folder = dim_folder.join("poi");

        let _ = std::fs::create_dir_all(&region_folder);
        let _ = std::fs::create_dir_all(&entities_folder);
        let _ = std::fs::create_dir_all(&poi_folder);

        let level_folder = Arc::new(LevelFolder {
            root_folder,
            dim_folder,
            region_folder,
            entities_folder,
            poi_folder,
        });

        let mut is_flat = false;
        let mut flat_layers = Vec::new();
        let mut flat_biome = "minecraft:plains".to_string();
        let mut generator_settings_name: Option<String> = None;
        let mut biome_source: Option<crate::world_info::BiomeSource> = None;
        let mut structure_overrides: Option<Vec<String>> = None;

        let world_gen_settings = crate::world_info::data_files::read_dimension_world_gen_settings(
            &level_folder.root_folder,
            &level_folder.dim_folder,
            dimension.minecraft_name,
        )?;
        let seed = world_gen_settings
            .as_ref()
            .map_or(seed, |settings| settings.seed);
        let generate_structures = world_gen_settings
            .as_ref()
            .is_none_or(|settings| settings.generate_structures);
        if let Some(wgs) = world_gen_settings
            && let Some(dim_settings) = wgs.dimensions.get(dimension.minecraft_name)
        {
            biome_source.clone_from(&dim_settings.generator.biome_source);

            if dim_settings.generator.generator_type == "minecraft:flat" {
                is_flat = true;
                let flat_settings = dim_settings
                    .generator
                    .settings
                    .as_ref()
                    .and_then(crate::world_info::GeneratorSettings::as_flat_settings);
                if let Some(flat_settings) = flat_settings {
                    flat_layers = flat_settings.to_flat_layers();
                    structure_overrides = flat_settings.structure_overrides_vec();
                    flat_biome = flat_settings.biome;
                }
            } else if let Some(crate::world_info::GeneratorSettings::Reference(s)) =
                &dim_settings.generator.settings
            {
                generator_settings_name = Some(s.clone());
            }
        }

        if !generate_structures {
            structure_overrides = Some(Vec::new());
        }

        let dim_min_y = dimension.min_y;
        let dim_height = dimension.height;
        let seed = Seed(seed as u64);
        let world_gen: Arc<WorldGenerator> = Arc::from(get_world_gen_with_all_settings(
            seed,
            dimension,
            is_flat,
            flat_layers,
            flat_biome,
            generator_settings_name.as_deref(),
            biome_source.as_ref(),
            structure_overrides.as_deref(),
        ));

        let chunk_saver = match &level_config.chunk {
            ChunkConfig::Linear => Arc::new(ChunkSaver::Linear(ChunkFileManager::new(()))),
            ChunkConfig::Anvil(config) => {
                Arc::new(ChunkSaver::Anvil(ChunkFileManager::new(config.clone())))
            }
            ChunkConfig::Pump => Arc::new(ChunkSaver::Pump(ChunkFileManager::new(()))),
        };
        let entity_saver = match &level_config.chunk {
            ChunkConfig::Linear => Arc::new(EntitySaver::Linear(ChunkFileManager::new(()))),
            ChunkConfig::Anvil(config) => {
                Arc::new(EntitySaver::Anvil(ChunkFileManager::new(config.clone())))
            }
            ChunkConfig::Pump => Arc::new(EntitySaver::Pump(ChunkFileManager::new(()))),
        };

        let pending_entity_generations = Arc::new(DashMap::new());
        let level_channel = Arc::new(LevelChannel::new());
        let thread_tracker = Mutex::new(Vec::new());
        let listener = Arc::new(ChunkListener::new());

        let level_ref = Arc::new(Self {
            seed,
            world_portal: ArcSwap::new(Arc::new(None)),
            world_gen: ArcSwap::new(world_gen),
            level_folder,
            lighting_config: level_config.lighting,
            light_engine: DynamicLightEngine::new(dim_min_y, dim_min_y + dim_height),
            chunk_saver,
            entity_saver,
            schedule_tick_counts: AtomicU64::new(0),
            loaded_chunks: Arc::new(DashMap::new()),
            loaded_chunk_changes: Arc::new(SegQueue::new()),
            loaded_entity_chunks: Arc::new(DashMap::new()),
            chunks_with_scheduled_ticks: Arc::new(dashmap::DashSet::new()),
            chunk_loading: Mutex::new(ChunkLoading::new(level_channel.clone())),
            block_entity_tick_lock: Mutex::new(()),
            chunk_watchers: Arc::new(DashMap::new()),
            tasks: TaskTracker::new(),
            chunk_system_tasks: TaskTracker::new(),
            cancel_token: CancellationToken::new(),
            shut_down_chunk_system: AtomicBool::new(false),
            should_save: AtomicBool::new(false),
            should_unload: AtomicBool::new(false),
            save_enabled: AtomicBool::new(true),
            autosave_ticks: level_config.autosave_ticks,
            pending_entity_loads: DashMap::new(),
            pending_entity_generations,
            level_channel: level_channel.clone(),
            thread_tracker,
            chunk_listener: listener.clone(),
        });

        GenerationSchedule::create(
            4,
            level_ref.clone(),
            level_channel,
            listener,
            level_ref
                .thread_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_mut(),
        );

        Ok(level_ref)
    }

    pub fn set_world_gen(&self, generator: Arc<WorldGenerator>) {
        self.world_gen.store(generator);
    }

    #[must_use]
    pub fn world_gen(&self) -> Arc<WorldGenerator> {
        self.world_gen.load_full()
    }

    pub(crate) fn fail_chunk_system(&self, cause: String) {
        error!("{cause}; stopping this world's chunk system");
        self.shut_down_chunk_system.store(true, Ordering::Relaxed);
        self.chunk_listener.stop(cause);
        self.level_channel.notify();
    }

    pub fn spawn_entity_generation(self: &Arc<Self>, pos: Vector2<i32>) {
        let level = self.clone();
        rayon::spawn(move || {
            let arc_chunk = Arc::new(ChunkEntityData {
                x: pos.x,
                z: pos.y,
                data: std::sync::Mutex::new(Vec::new()),
                preserved_tags: std::sync::Mutex::new(pumpkin_nbt::NbtCompound::new()),
                live: AtomicBool::new(false),
                dirty: AtomicBool::new(false),
            });

            let arc_chunk = level
                .loaded_entity_chunks
                .entry(pos)
                .or_insert(arc_chunk)
                .clone();

            if let Some((_, waiters)) = level.pending_entity_generations.remove(&pos) {
                for tx in waiters {
                    let _ = tx.send(arc_chunk.clone());
                }
            }
        });
    }

    /// Spawns a task associated with this world. All tasks spawned with this method are awaited
    /// when the client. This means tasks should complete in a reasonable (no looping) amount of time.
    pub fn spawn_task<F>(&self, task: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.tasks.spawn(task)
    }

    pub async fn shutdown(&self) {
        let world_id = self.level_folder.root_folder.display();
        info!("Saving level ({})...", world_id);
        self.cancel_token.cancel();
        self.shut_down_chunk_system.store(true, Ordering::Relaxed);
        self.level_channel.notify();

        self.tasks.close();
        self.chunk_system_tasks.close();

        let handles = {
            let mut lock = self
                .thread_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            lock.drain(..).collect::<Vec<_>>()
        };

        let handle_count = handles.len();
        info!("Joining {} threads for {}...", handle_count, world_id);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = std::thread::Builder::new()
            .name("Thread-Joiner".into())
            .spawn(move || {
                let mut failed_count = 0;
                for handle in handles {
                    if handle.join().is_err() {
                        failed_count += 1;
                    }
                }
                let _ = tx.send(failed_count);
            });

        match timeout(Duration::from_secs(3), rx).await {
            Ok(Ok(failed_count)) => {
                if failed_count > 0 {
                    warn!(
                        "{} threads failed to join properly for {}.",
                        failed_count, world_id
                    );
                }
            }
            Ok(Err(_)) => {
                warn!("Thread join task panicked for {}.", world_id);
            }
            Err(_) => {
                warn!("Timed out waiting for threads to join for {}.", world_id);
            }
        }

        self.tasks.wait().await;
        self.chunk_system_tasks.wait().await;

        info!("Flushing chunk data to disk for {}...", world_id);
        self.chunk_saver.block_and_await_ongoing_tasks().await;
        info!("Flushing entity data to disk for {}...", world_id);
        self.entity_saver.block_and_await_ongoing_tasks().await;

        // save all chunks currently in memory
        let chunks_to_write = self
            .loaded_entity_chunks
            .iter()
            .map(|chunk| (*chunk.key(), chunk.value().clone()))
            .collect::<Vec<_>>();
        self.entity_saver.clear_watched_chunks().await;
        let mut retry_delay = Duration::from_secs(1);
        while self
            .write_entity_chunks(chunks_to_write.clone())
            .await
            .is_err()
        {
            error!(
                "Entity chunks remain unsaved; retrying in {}s",
                retry_delay.as_secs()
            );
            tokio::time::sleep(retry_delay).await;
            retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
        }
        self.loaded_entity_chunks.clear();
        self.pending_entity_loads.clear();
    }

    pub fn loaded_chunk_count(&self) -> usize {
        self.loaded_chunks.len()
    }

    pub fn list_cached(&self) {
        for entry in self.loaded_chunks.iter() {
            debug!("In map: {:?}", entry.key());
        }
    }

    /// Marks chunks as "watched" by a unique player. When no players are watching a chunk,
    /// it is removed from memory. Should only be called on chunks the player was not watching
    /// before
    pub async fn mark_chunks_as_newly_watched(&self, chunks: &[Vector2<i32>]) {
        for chunk in chunks {
            self.chunk_watchers
                .entry(*chunk)
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(1);
        }

        self.entity_saver
            .watch_chunks(&self.level_folder, chunks)
            .await;
    }

    /// Marks chunks no longer "watched" by a unique player. When no players are watching a chunk,
    /// it is removed from memory. Should only be called on chunks the player was watching before
    pub async fn mark_chunks_as_not_watched(
        &self,
        chunks: impl IntoIterator<Item = impl std::borrow::Borrow<Vector2<i32>>>,
    ) -> Vec<Vector2<i32>> {
        let mut chunks_to_clean = Vec::new();
        let chunks_vec: Vec<Vector2<i32>> = chunks.into_iter().map(|c| *c.borrow()).collect();

        for chunk in &chunks_vec {
            if let Entry::Occupied(mut entry) = self.chunk_watchers.entry(*chunk) {
                *entry.get_mut() = entry.get().saturating_sub(1);
                if *entry.get() == 0 {
                    entry.remove();
                    chunks_to_clean.push(*chunk);
                }
            }
        }

        self.entity_saver
            .unwatch_chunks(&self.level_folder, &chunks_vec)
            .await;
        chunks_to_clean
    }

    /// Returns whether the chunk should be removed from memory
    #[inline]
    pub async fn mark_chunk_as_not_watched(&self, chunk: Vector2<i32>) -> bool {
        !self.mark_chunks_as_not_watched([chunk]).await.is_empty()
    }

    pub fn clean_entity_chunks(
        self: &Arc<Self>,
        chunks: impl IntoIterator<Item = impl std::borrow::Borrow<Vector2<i32>>>,
    ) {
        let positions: Vec<_> = chunks.into_iter().map(|pos| *pos.borrow()).collect();
        let level = self.clone();
        self.spawn_task(async move {
            level.save_and_clean_entity_chunks(positions).await;
        });
    }

    async fn save_and_clean_entity_chunks(&self, positions: Vec<Vector2<i32>>) {
        let chunks: Vec<_> = positions
            .into_iter()
            .filter_map(|pos| {
                if self.is_chunk_watched(&pos) {
                    return None;
                }
                self.get_entity_chunk_sync(&pos).map(|chunk| (pos, chunk))
            })
            .collect();
        if self.write_entity_chunks(chunks.clone()).await.is_err() {
            // The cache remains canonical and dirty; the next cleanup or shutdown retries it.
            return;
        }
        for (pos, chunk) in chunks {
            // remove_if holds the map shard while checking ownership. A loader
            // either clones this exact chunk first or reads its committed successor
            // from disk after removal. Live or borrowed chunks cannot be retired.
            self.loaded_entity_chunks.remove_if(&pos, |_, current| {
                Arc::ptr_eq(current, &chunk)
                    && !self.is_chunk_watched(&pos)
                    && !current.is_dirty()
                    && !current.live.load(Ordering::Relaxed)
                    && Arc::strong_count(current) == 2
            });
        }
    }

    pub fn get_tick_data(
        &self,
        active_chunks: &FxHashSet<Vector2<i32>>,
        random_tick_speed: i64,
    ) -> TickData {
        let samples_per_section = random_tick_speed.max(0);

        let mut ticks = TickData {
            block_ticks: Vec::new(),
            fluid_ticks: Vec::new(),
            random_ticks: Vec::with_capacity(active_chunks.len() * 3),
        };

        // 1. Process active chunks (random ticks, block entities)
        for pos in active_chunks {
            if let Some(chunk) = self.loaded_chunks.get(pos) {
                let chunk = chunk.value();
                let chunk_x_base = chunk.x * 16;
                let chunk_z_base = chunk.z * 16;
                let section_count = chunk.section.count;

                // Use the bitmask to skip sections
                let mask = chunk.section.randomly_ticking_mask.load(Ordering::Relaxed);
                if mask != 0 {
                    let sections = chunk
                        .section
                        .block_sections
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let min_y = chunk.section.min_y;

                    for i in 0..section_count {
                        if (mask & (1 << i)) == 0 {
                            continue;
                        }
                        let y_base = min_y + (i as i32 * 16);
                        for _ in 0..samples_per_section {
                            let r = rand::random::<u32>();
                            let x_offset = (r & 0xF) as usize;
                            let z_offset = (r >> 8 & 0xF) as usize;
                            let y_in_section = ((r >> 4) & 0xF) as usize;

                            let block_state_id = sections[i].get(x_offset, y_in_section, z_offset);
                            let tick_block = has_random_ticks(block_state_id);
                            let tick_fluid = has_random_ticking_fluid(block_state_id);
                            if tick_block || tick_fluid {
                                ticks.random_ticks.push(RandomTickSample {
                                    position: BlockPos::new(
                                        chunk_x_base + x_offset as i32,
                                        y_base + y_in_section as i32,
                                        chunk_z_base + z_offset as i32,
                                    ),
                                    tick_block,
                                    tick_fluid,
                                });
                            }
                        }
                    }
                }
            }
        }

        // 2. Process chunks with scheduled ticks
        // We collect keys first to avoid holding DashSet shard lock while accessing loaded_chunks (deadlock risk)
        let scheduled_chunk_pos: Vec<_> = self
            .chunks_with_scheduled_ticks
            .iter()
            .map(|p| *p)
            .collect();
        for pos in scheduled_chunk_pos {
            if let Some(chunk) = self.loaded_chunks.get(&pos) {
                let chunk = chunk.value();
                ticks.block_ticks.append(&mut chunk.block_ticks.step_tick());
                ticks.fluid_ticks.append(&mut chunk.fluid_ticks.step_tick());

                // Remove from set if it no longer has ticks
                if !chunk.block_ticks.has_ticks() && !chunk.fluid_ticks.has_ticks() {
                    self.chunks_with_scheduled_ticks.remove(&pos);
                }
            } else {
                self.chunks_with_scheduled_ticks.remove(&pos); // Chunk unloaded
            }
        }

        ticks.block_ticks.sort_unstable();
        ticks.fluid_ticks.sort_unstable();

        ticks
    }

    pub fn clean_entity_chunk(self: &Arc<Self>, chunk: &Vector2<i32>) {
        self.clean_entity_chunks([*chunk]);
    }

    pub fn is_chunk_watched(&self, chunk: &Vector2<i32>) -> bool {
        self.chunk_watchers.get(chunk).is_some()
    }

    pub fn clean_memory(self: &Arc<Self>) -> Vec<Vector2<i32>> {
        self.chunk_watchers.retain(|_, watcher| *watcher != 0);

        let entity_chunks_to_remove: Vec<_> = self
            .loaded_entity_chunks
            .iter()
            .filter(|entry| !self.chunk_watchers.contains_key(entry.key()))
            .map(|entry| *entry.key())
            .collect();

        // We do not clean them here because we want the caller to save any active entities in them first.

        // if the difference is too big, we can shrink the loaded chunks
        // (1024 chunks is the equivalent to a 32x32 chunks area)
        if self.chunk_watchers.capacity() - self.chunk_watchers.len() >= 4096 {
            self.chunk_watchers.shrink_to_fit();
        }

        if self.loaded_chunks.capacity() - self.loaded_chunks.len() >= 4096 {
            self.loaded_chunks.shrink_to_fit();
        }

        if self.loaded_entity_chunks.capacity() - self.loaded_entity_chunks.len() >= 4096 {
            self.loaded_entity_chunks.shrink_to_fit();
        }
        entity_chunks_to_remove
    }

    pub async fn get_or_fetch_chunk<R, F: Fn(&SyncChunk) -> R>(
        self: &Arc<Self>,
        pos: Vector2<i32>,
        f: F,
    ) -> R {
        // Check if already in memory
        if let Some(res) = self.read_chunk_sync(&pos, &f) {
            return res;
        }
        let chunk = self.fetch_chunk(pos).await;
        if self.loaded_chunks.insert(pos, chunk.clone()).is_none() {
            self.loaded_chunk_changes
                .push(LoadedChunkChange::Loaded(pos));
        }
        f(&chunk)
    }

    pub fn loaded_chunk_changes(&self) -> impl Iterator<Item = LoadedChunkChange> + '_ {
        std::iter::from_fn(|| self.loaded_chunk_changes.pop())
    }

    async fn fetch_chunk(self: &Arc<Self>, pos: Vector2<i32>) -> SyncChunk {
        let recv = self.chunk_listener.add_single_chunk_listener(pos);

        {
            let mut lock = self
                .chunk_loading
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            lock.add_ticket(pos, ChunkLoading::FULL_CHUNK_LEVEL);
            lock.send_change();
        };

        let chunk = recv.await;

        {
            let mut lock = self
                .chunk_loading
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            lock.remove_ticket(pos, ChunkLoading::FULL_CHUNK_LEVEL);
            lock.send_change();
        };

        // This API is infallible; abort the requesting task rather than publish
        // an empty replacement for an unreadable existing chunk.
        #[expect(
            clippy::panic,
            reason = "infallible fetch must fail closed on chunk load failure"
        )]
        match chunk {
            Ok(Ok(chunk)) => chunk,
            Ok(Err(cause)) => panic!("Cannot fetch chunk {pos:?}: {cause}"),
            Err(cause) => panic!("Chunk listener closed for {pos:?}: {cause}"),
        }
    }

    async fn load_single_entity_chunk(
        &self,
        pos: Vector2<i32>,
    ) -> Result<(SyncEntityChunk, bool), ChunkReadingError> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        self.entity_saver
            .fetch_chunks(&self.level_folder, &[pos], tx)
            .await;

        match rx.recv().await {
            Some(LoadedData::Loaded(chunk)) => Ok((chunk, false)),
            Some(LoadedData::Error((_, err))) => Err(err),
            Some(LoadedData::Missing(_)) => Err(ChunkReadingError::ChunkNotExist),
            None => Err(ChunkReadingError::IoError(std::io::Error::other(format!(
                "Entity chunk loader closed without a result for {pos:?}"
            )))),
        }
    }

    pub fn receive_entity_chunks(
        self: &Arc<Self>,
        chunks: Vec<Vector2<i32>>,
    ) -> Receiver<(Weak<ChunkEntityData>, bool)> {
        use futures::StreamExt;
        const LOAD_CONCURRENCY: usize = 64;
        let (sender, receiver) = mpsc::channel(LOAD_CONCURRENCY);
        let level = self.clone();
        self.spawn_task(async move {
            let fetch = async {
                let mut loading = futures::stream::iter(chunks)
                    .map(|position| {
                        let level = level.clone();
                        async move {
                            let cached = level.get_entity_chunk_sync(&position).is_some();
                            let chunk = level.get_entity_chunk(position).await;
                            (Arc::downgrade(&chunk), !cached)
                        }
                    })
                    .buffer_unordered(LOAD_CONCURRENCY);
                while let Some(chunk) = loading.next().await {
                    if sender.send(chunk).await.is_err() {
                        return;
                    }
                }
            };
            tokio::select! {
                () = level.cancel_token.cancelled() => {},
                () = fetch => {},
            }
        });
        receiver
    }

    pub async fn get_entity_chunk(self: &Arc<Self>, pos: Vector2<i32>) -> SyncEntityChunk {
        if let Some(chunk) = self.loaded_entity_chunks.get(&pos) {
            return chunk.clone();
        }

        // Concurrent uncached callers share the same decode and the same live
        // flag. A second decoded snapshot must never replace an active chunk.
        let pending = self
            .pending_entity_loads
            .entry(pos)
            .or_insert_with(|| Arc::new(tokio::sync::OnceCell::new()))
            .clone();
        let chunk = pending
            .get_or_init(|| async {
                if let Some(chunk) = self.get_entity_chunk_sync(&pos) {
                    return chunk;
                }
                let chunk = self.load_entity_chunk_uncached(pos).await;
                self.loaded_entity_chunks
                    .entry(pos)
                    .or_insert(chunk)
                    .clone()
            })
            .await
            .clone();
        self.pending_entity_loads
            .remove_if(&pos, |_, current| Arc::ptr_eq(current, &pending));
        chunk
    }

    #[expect(
        clippy::panic,
        reason = "infallible entity fetch must fail closed on read errors"
    )]
    async fn load_entity_chunk_uncached(self: &Arc<Self>, pos: Vector2<i32>) -> SyncEntityChunk {
        match self.load_single_entity_chunk(pos).await {
            Ok((chunk, _)) => chunk,
            Err(ChunkReadingError::ChunkNotExist) => {
                let (tx, rx) = oneshot::channel();
                match self.pending_entity_generations.entry(pos) {
                    dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                        entry.get_mut().push(tx);
                    }
                    dashmap::mapref::entry::Entry::Vacant(entry) => {
                        entry.insert(vec![tx]);
                        self.spawn_entity_generation(pos);
                    }
                }
                match rx.await {
                    Ok(chunk) => chunk,
                    Err(cause) => panic!("Entity chunk generation closed for {pos:?}: {cause}"),
                }
            }
            Err(cause) => {
                let error = format!("Failed to read entity chunk {pos:?}: {cause}");
                self.fail_chunk_system(error.clone());
                panic!("{error}");
            }
        }
    }

    pub fn get_block_state(&self, position: &BlockPos) -> BlockStateId {
        let (chunk_coordinate, relative) = position.chunk_and_chunk_relative_position();
        let id = self
            .read_chunk_sync(&chunk_coordinate, |chunk| {
                chunk.section.get_block_absolute_y(
                    relative.x as usize,
                    relative.y,
                    relative.z as usize,
                )
            })
            .flatten();

        id.unwrap_or(Block::VOID_AIR.default_state.id)
    }

    pub fn set_block_state(
        &self,
        position: &BlockPos,
        block_state_id: BlockStateId,
    ) -> BlockStateId {
        let (chunk_coordinate, relative) = position.chunk_and_chunk_relative_position();
        self.read_chunk_sync(&chunk_coordinate, |chunk| {
            let replaced_block_state_id = chunk.set_block_absolute_y(
                relative.x as usize,
                relative.y,
                relative.z as usize,
                block_state_id,
            );
            if replaced_block_state_id != block_state_id {
                chunk.mark_dirty(true);
            }
            replaced_block_state_id
        })
        .unwrap_or(Block::VOID_AIR.default_state.id)
    }

    pub async fn write_chunks(
        &self,
        chunks_to_write: Vec<(Vector2<i32>, SyncChunk)>,
    ) -> Result<(), ChunkWritingError> {
        let result = self
            .chunk_saver
            .save_chunks(&self.level_folder, chunks_to_write)
            .await;
        if let Err(error) = &result {
            error!("Failed writing chunks to disk: {error}");
        }
        result
    }

    /// Persists the entity snapshots currently held by this level.
    pub async fn save_entity_chunks(&self) -> Result<(), ChunkWritingError> {
        let chunks = self
            .loaded_entity_chunks
            .iter()
            .map(|chunk| (*chunk.key(), chunk.value().clone()))
            .collect();
        self.write_entity_chunks(chunks).await
    }

    pub async fn write_entity_chunks(
        &self,
        chunks_to_write: Vec<(Vector2<i32>, SyncEntityChunk)>,
    ) -> Result<(), ChunkWritingError> {
        let result = self
            .entity_saver
            .save_chunks(&self.level_folder, chunks_to_write)
            .await;
        if let Err(error) = &result {
            error!("Failed writing entity chunks to disk: {error}");
        }
        result
    }

    pub fn is_chunk_loaded(&self, coordinates: &Vector2<i32>) -> bool {
        self.loaded_chunks.contains_key(coordinates)
    }

    pub fn read_chunk_sync<R, F: Fn(&SyncChunk) -> R>(
        &self,
        coordinates: &Vector2<i32>,
        f: F,
    ) -> Option<R> {
        self.loaded_chunks.get(coordinates).map(|x| f(x.value()))
    }

    pub fn read_entity_chunk_sync<R, F: Fn(&SyncEntityChunk) -> R>(
        &self,
        coordinates: &Vector2<i32>,
        f: F,
    ) -> Option<R> {
        self.loaded_entity_chunks
            .get(coordinates)
            .map(|x| f(x.value()))
    }

    pub fn get_rough_biome(&self, position: &BlockPos) -> &'static Biome {
        let (chunk_coordinate, relative) = position.chunk_and_chunk_relative_position();
        let id = self.read_chunk_sync(&chunk_coordinate, |chunk| {
            chunk.section.get_rough_biome_absolute_y(
                relative.x as usize,
                relative.y,
                relative.z as usize,
            )
        });
        Biome::from_id(id.flatten().unwrap_or(0)).unwrap_or(&Biome::THE_VOID)
    }

    pub fn get_entity_chunk_sync(&self, pos: &Vector2<i32>) -> Option<SyncEntityChunk> {
        self.loaded_entity_chunks
            .get(pos)
            .map(|x| x.value().clone())
    }

    #[must_use]
    pub fn live_entity_chunk_positions(&self) -> Vec<Vector2<i32>> {
        self.loaded_entity_chunks
            .iter()
            .filter(|entry| entry.value().live.load(Ordering::Relaxed))
            .map(|entry| *entry.key())
            .collect()
    }

    pub async fn get_or_fetch_entity_chunk<R, F: Fn(&SyncEntityChunk) -> R>(
        self: &Arc<Self>,
        pos: Vector2<i32>,
        f: F,
    ) -> R {
        if let Some(res) = self.read_entity_chunk_sync(&pos, &f) {
            return res;
        }
        let chunk = self.get_entity_chunk(pos).await;
        f(&chunk)
    }

    pub fn try_get_entity_chunk(
        &self,
        coordinates: Vector2<i32>,
    ) -> Option<dashmap::mapref::one::Ref<'_, Vector2<i32>, Arc<ChunkEntityData>>> {
        self.loaded_entity_chunks.try_get(&coordinates).try_unwrap()
    }

    pub fn schedule_block_tick(
        &self,
        block: &Block,
        block_pos: BlockPos,
        delay: u8,
        priority: TickPriority,
    ) {
        let tick_order = self.schedule_tick_counts.fetch_add(1, Ordering::Relaxed);
        let scheduled_tick = ScheduledTick {
            delay: i32::from(delay),
            position: block_pos,
            priority,
            // SAFETY: `block` is a valid reference that outlives this function call for scheduling.
            value: unsafe { &*std::ptr::from_ref::<Block>(block) },
        };

        let chunk_pos = block_pos.chunk_position();
        if self
            .read_chunk_sync(&chunk_pos, |chunk| {
                chunk.block_ticks.schedule_tick(&scheduled_tick, tick_order);
            })
            .is_some()
        {
            self.chunks_with_scheduled_ticks.insert(chunk_pos);
        }
    }

    pub fn schedule_fluid_tick(
        &self,
        fluid: &Fluid,
        block_pos: BlockPos,
        delay: u8,
        priority: TickPriority,
    ) {
        let tick_order = self.schedule_tick_counts.fetch_add(1, Ordering::Relaxed);
        let scheduled_tick = ScheduledTick {
            delay: i32::from(delay),
            position: block_pos,
            priority,
            // SAFETY: `fluid` is a valid reference that outlives this function call for scheduling.
            value: unsafe { &*std::ptr::from_ref::<Fluid>(fluid) },
        };

        let chunk_pos = block_pos.chunk_position();
        if self
            .read_chunk_sync(&chunk_pos, |chunk| {
                chunk.fluid_ticks.schedule_tick(&scheduled_tick, tick_order);
            })
            .is_some()
        {
            self.chunks_with_scheduled_ticks.insert(chunk_pos);
        }
    }

    pub fn is_block_tick_scheduled(&self, block_pos: &BlockPos, block: &Block) -> bool {
        self.read_chunk_sync(&block_pos.chunk_position(), |chunk| {
            chunk.block_ticks.is_scheduled(*block_pos, block)
        })
        .unwrap_or(false)
    }

    pub fn is_fluid_tick_scheduled(&self, block_pos: &BlockPos, fluid: &Fluid) -> bool {
        self.read_chunk_sync(&block_pos.chunk_position(), |chunk| {
            chunk.fluid_ticks.is_scheduled(*block_pos, fluid)
        })
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_config::world::LevelConfig;
    use tempfile::TempDir;

    fn region_with_palette_entry(data_version: Option<i32>, block_name: &str) -> Vec<u8> {
        use pumpkin_nbt::{Nbt, compound::NbtCompound, tag::NbtTag};
        const SECTOR_SIZE: usize = 4096;
        let mut entry = NbtCompound::new();
        entry.put_string("Name", block_name.to_owned());
        let mut blocks = NbtCompound::new();
        blocks.put("palette", NbtTag::List(vec![NbtTag::Compound(entry)]));
        let mut section = NbtCompound::new();
        section.put_int("Y", -4);
        section.put_compound("block_states", blocks);
        let mut root = NbtCompound::new();
        if let Some(version) = data_version {
            root.put_int("DataVersion", version);
        }
        root.put_int("xPos", 0);
        root.put_int("zPos", 0);
        root.put_int("yPos", -4);
        root.put_string("Status", "minecraft:full".to_string());
        root.put("sections", NbtTag::List(vec![NbtTag::Compound(section)]));
        let bytes = Nbt::new(String::new(), root).write();
        let data_offset = SECTOR_SIZE * 2;
        let mut region = vec![0; SECTOR_SIZE * 3];
        region[..4].copy_from_slice(&[0, 0, 2, 1]);
        region[data_offset..data_offset + 4]
            .copy_from_slice(&((bytes.len() + 1) as u32).to_be_bytes());
        region[data_offset + 4] = 3; // Anvil's uncompressed NBT format.
        region[data_offset + 5..data_offset + 5 + bytes.len()].copy_from_slice(&bytes);
        region
    }

    #[tokio::test]
    async fn unreadable_chunk_aborts_fetch_and_preserves_storage_after_shutdown() {
        for (original, expected_cause) in [
            (b"existing damaged region".to_vec(), "Failed reading region"),
            (
                region_with_palette_entry(
                    Some(crate::chunk::format::anvil::WORLD_DATA_VERSION),
                    "minecraft:removed_block",
                ),
                "minecraft:removed_block",
            ),
            (
                region_with_palette_entry(Some(3578), "minecraft:stone"),
                "DataVersion 3578",
            ),
            (
                region_with_palette_entry(Some(3465), "minecraft:stone"),
                "DataVersion 3465",
            ),
            (
                region_with_palette_entry(None, "minecraft:stone"),
                "DataVersion missing",
            ),
        ] {
            let directory = TempDir::new().unwrap();
            let level = Level::from_root_folder(
                &LevelConfig::default(),
                directory.path().to_path_buf(),
                0,
                Dimension::OVERWORLD,
            )
            .unwrap();
            let path = level.level_folder.region_folder.join("r.0.0.mca");
            tokio::fs::write(&path, &original).await.unwrap();
            let pos = Vector2::new(0, 0);
            let fetching_level = level.clone();
            let fetch =
                tokio::spawn(
                    async move { fetching_level.get_or_fetch_chunk(pos, Clone::clone).await },
                );
            let failure = timeout(Duration::from_secs(10), fetch)
                .await
                .unwrap()
                .err()
                .unwrap();
            assert!(
                failure.is_panic(),
                "infallible fetch must abort, never return an empty chunk"
            );
            let cause = failure.into_panic();
            let message = cause.downcast_ref::<String>().unwrap();
            assert!(message.contains("Vector2 { x: 0, y: 0 }"));
            assert!(message.contains(expected_cause));
            assert!(level.loaded_chunks.is_empty());
            timeout(Duration::from_secs(10), level.shutdown())
                .await
                .unwrap();
            assert_eq!(tokio::fs::read(path).await.unwrap(), original);
        }
    }

    #[tokio::test]
    async fn unreadable_entity_region_is_not_generated_or_overwritten() {
        for streaming in [false, true] {
            let directory = TempDir::new().unwrap();
            let level = Level::from_root_folder(
                &LevelConfig::default(),
                directory.path().to_path_buf(),
                0,
                Dimension::OVERWORLD,
            )
            .unwrap();
            let path = level.level_folder.entities_folder.join("r.0.0.mca");
            let original = b"unreadable existing entities";
            tokio::fs::write(&path, original).await.unwrap();
            let pos = Vector2::new(0, 0);
            if streaming {
                let mut chunks = level.receive_entity_chunks(vec![pos]);
                assert!(
                    timeout(Duration::from_secs(10), chunks.recv())
                        .await
                        .unwrap()
                        .is_none()
                );
            } else {
                let fetching_level = level.clone();
                let result = timeout(
                    Duration::from_secs(10),
                    tokio::spawn(async move { fetching_level.get_entity_chunk(pos).await }),
                )
                .await
                .unwrap();
                assert!(result.err().unwrap().is_panic());
            }
            assert!(level.shut_down_chunk_system.load(Ordering::Relaxed));
            assert!(level.loaded_entity_chunks.is_empty());
            assert!(level.pending_entity_generations.is_empty());
            timeout(Duration::from_secs(10), level.shutdown())
                .await
                .unwrap();
            assert_eq!(tokio::fs::read(path).await.unwrap(), original);
        }
    }

    #[tokio::test]
    async fn cleanup_keeps_canonical_entity_chunk_available_to_reload() {
        let directory = TempDir::new().unwrap();
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            directory.path().into(),
            0,
            Dimension::OVERWORLD,
        )
        .unwrap();
        let position = Vector2::new(0, 0);
        let chunk = level.get_entity_chunk(position).await;
        chunk.mark_dirty(true);
        level.clean_entity_chunks([position]);
        assert!(
            Arc::ptr_eq(&chunk, &level.get_entity_chunk_sync(&position).unwrap()),
            "cleanup must not remove the canonical instance before its asynchronous save"
        );
        assert!(Arc::ptr_eq(&chunk, &level.get_entity_chunk(position).await));
        level.save_and_clean_entity_chunks(vec![position]).await;
        assert!(
            Arc::ptr_eq(&chunk, &level.get_entity_chunk(position).await),
            "a borrowed instance must remain canonical after commit"
        );
        timeout(Duration::from_secs(10), level.shutdown())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn failed_entity_cleanup_retains_latest_data_until_successful_retry() {
        let directory = TempDir::new().unwrap();
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            directory.path().into(),
            0,
            Dimension::OVERWORLD,
        )
        .unwrap();
        let position = Vector2::new(0, 0);
        let chunk = level.get_entity_chunk(position).await;
        let mut record = pumpkin_nbt::compound::NbtCompound::new();
        record.put_string("id", "minecraft:pig".into());
        chunk.data.lock().unwrap().push(record);
        chunk.mark_dirty(true);
        level
            .write_entity_chunks(vec![(position, chunk.clone())])
            .await
            .unwrap();
        let path = level.level_folder.entities_folder.join("r.0.0.mca");
        let original = tokio::fs::read(&path).await.unwrap();
        let temporary = path.with_extension("tmp");
        tokio::fs::create_dir(&temporary).await.unwrap();
        chunk.data.lock().unwrap().clear();
        chunk.mark_dirty(true);
        level.save_and_clean_entity_chunks(vec![position]).await;
        assert!(chunk.is_dirty());
        assert!(Arc::ptr_eq(&chunk, &level.get_entity_chunk(position).await));
        assert!(chunk.data.lock().unwrap().is_empty());
        assert_eq!(tokio::fs::read(&path).await.unwrap(), original);
        tokio::fs::remove_dir(&temporary).await.unwrap();
        drop(chunk);
        level.save_and_clean_entity_chunks(vec![position]).await;
        assert!(level.get_entity_chunk_sync(&position).is_none());
        let reloaded = level.get_entity_chunk(position).await;
        assert!(
            reloaded.data.lock().unwrap().is_empty(),
            "removed entity must not resurrect after reload"
        );
        timeout(Duration::from_secs(10), level.shutdown())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn absent_entity_chunk_still_generates() {
        let directory = TempDir::new().unwrap();
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            directory.path().to_path_buf(),
            0,
            Dimension::OVERWORLD,
        )
        .unwrap();
        let pos = Vector2::new(0, 0);
        let chunk = timeout(Duration::from_secs(10), level.get_entity_chunk(pos))
            .await
            .unwrap();
        assert_eq!((chunk.x, chunk.z), (0, 0));
        assert!(level.loaded_entity_chunks.contains_key(&pos));
        assert!(!level.shut_down_chunk_system.load(Ordering::Relaxed));
        timeout(Duration::from_secs(10), level.shutdown())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn dimension_paths_26_2() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().to_path_buf();
        let config = LevelConfig::default();

        let overworld_level =
            Level::from_root_folder(&config, root.clone(), 0, Dimension::OVERWORLD).unwrap();
        assert_eq!(
            overworld_level.level_folder.dim_folder,
            root.join("dimensions").join("minecraft").join("overworld")
        );
        assert_eq!(
            overworld_level.level_folder.region_folder,
            root.join("dimensions")
                .join("minecraft")
                .join("overworld")
                .join("region")
        );

        let nether_level =
            Level::from_root_folder(&config, root.clone(), 0, Dimension::THE_NETHER).unwrap();
        assert_eq!(
            nether_level.level_folder.dim_folder,
            root.join("dimensions").join("minecraft").join("the_nether")
        );

        let end_level =
            Level::from_root_folder(&config, root.clone(), 0, Dimension::THE_END).unwrap();
        assert_eq!(
            end_level.level_folder.dim_folder,
            root.join("dimensions").join("minecraft").join("the_end")
        );
    }

    #[tokio::test]
    async fn legacy_dimension_fallback() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path().to_path_buf();
        let config = LevelConfig::default();

        // Create legacy directories
        std::fs::create_dir_all(root.join("region")).unwrap();
        std::fs::create_dir_all(root.join("DIM-1").join("region")).unwrap();
        std::fs::create_dir_all(root.join("DIM1").join("region")).unwrap();

        let overworld_level =
            Level::from_root_folder(&config, root.clone(), 0, Dimension::OVERWORLD).unwrap();
        assert_eq!(overworld_level.level_folder.dim_folder, root);

        let nether_level =
            Level::from_root_folder(&config, root.clone(), 0, Dimension::THE_NETHER).unwrap();
        assert_eq!(nether_level.level_folder.dim_folder, root.join("DIM-1"));

        let end_level =
            Level::from_root_folder(&config, root.clone(), 0, Dimension::THE_END).unwrap();
        assert_eq!(end_level.level_folder.dim_folder, root.join("DIM1"));
    }
    #[tokio::test]
    async fn imported_dimension_metadata_controls_seed_and_generator() {
        use crate::world_info::{
            GeneratorSettings, WorldGenSettings, data_files::write_world_gen_settings,
        };
        let directory = TempDir::new().unwrap();
        let root = directory.path().to_path_buf();
        let config = LevelConfig::default();
        let root_settings = WorldGenSettings::new(Seed(123));
        write_world_gen_settings(&root, &root_settings, 4903).unwrap();
        let nether_folder = root.join("dimensions/minecraft/the_nether");
        let mut nether_settings = WorldGenSettings::new(Seed(456));
        nether_settings.generate_structures = false;
        write_world_gen_settings(&nether_folder, &nether_settings, 4903).unwrap();
        let end_folder = root.join("dimensions/minecraft/the_end");
        let mut end_settings = WorldGenSettings::new(Seed(789));
        let end = end_settings
            .dimensions
            .get_mut("minecraft:the_end")
            .unwrap();
        end.generator.generator_type = "minecraft:flat".to_string();
        end.generator.settings = Some(GeneratorSettings::Compound(serde_json::json!({
            "biome": "minecraft:the_end", "layers": [{"block": "minecraft:end_stone", "height": 7}]
        })));
        write_world_gen_settings(&end_folder, &end_settings, 4903).unwrap();

        let overworld =
            Level::from_root_folder(&config, root.clone(), -1, Dimension::OVERWORLD).unwrap();
        let nether =
            Level::from_root_folder(&config, root.clone(), -1, Dimension::THE_NETHER).unwrap();
        let end = Level::from_root_folder(&config, root, -1, Dimension::THE_END).unwrap();
        assert_eq!(
            (overworld.seed.0, nether.seed.0, end.seed.0),
            (123, 456, 789)
        );
        assert_eq!(nether.world_gen().seed(), 456);
        let nether_generator = nether.world_gen();
        let WorldGenerator::Noise(noise) = &*nether_generator else {
            panic!("historical noise generator replaced");
        };
        assert!(noise.enabled_structure_sets.as_ref().unwrap().is_empty());
        let generator = end.world_gen();
        let WorldGenerator::Flat(flat) = &*generator else {
            panic!("historical flat generator replaced by noise");
        };
        assert_eq!(flat.seed, 789);
        assert_eq!(flat.layers[0].block, "minecraft:end_stone");
        assert_eq!(flat.layers[0].height, 7);
        assert_eq!(flat.biome, "minecraft:the_end");
        overworld.shutdown().await;
        nether.shutdown().await;
        end.shutdown().await;
    }
}
