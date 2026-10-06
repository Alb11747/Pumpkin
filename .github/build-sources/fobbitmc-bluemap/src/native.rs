use crate::snapshot::{
    MAX_PLAYERS, PlayerSnapshot, Position, Rotation, Snapshot, run_exporter, write_snapshot,
};
use crate::visibility::{Visibility, java_canonical_path};
use pumpkin::{
    entity::EntityBase,
    plugin::{Context, Plugin, PluginFuture, PluginMetadata},
    server::Server,
    world::World,
};
use pumpkin_util::math::position::BlockPos;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex, atomic::Ordering, mpsc},
    thread::{self, JoinHandle},
    time::Duration,
};

// These are the three symbols consumed by Pumpkin's NativePluginLoader.
#[unsafe(no_mangle)]
pub static PUMPKIN_API_VERSION: u32 = pumpkin::plugin::PLUGIN_API_VERSION;

#[unsafe(no_mangle)]
pub static METADATA: LazyLock<PluginMetadata> = LazyLock::new(|| PluginMetadata {
    name: env!("CARGO_PKG_NAME").into(),
    version: env!("CARGO_PKG_VERSION").into(),
    authors: vec!["FobbitMC Contributors".into()],
    description: env!("CARGO_PKG_DESCRIPTION").into(),
    dependencies: Vec::new(),
    permissions: Vec::new(),
});

#[unsafe(no_mangle)]
pub fn plugin() -> Box<dyn Plugin> {
    Box::new(BlueMapExporter {
        worker: Mutex::new(None),
    })
}

struct Worker {
    stop: mpsc::Sender<()>,
    thread: JoinHandle<Result<(), String>>,
}

struct BlueMapExporter {
    worker: Mutex<Option<Worker>>,
}

impl Plugin for BlueMapExporter {
    fn on_load(&self, context: Arc<Context>) -> PluginFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let mut worker = self
                .worker
                .lock()
                .map_err(|e| format!("worker lock: {e}"))?;
            if worker.is_some() {
                return Err("BlueMap exporter is already loaded".into());
            }
            let (stop_tx, stop_rx) = mpsc::channel();
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let folder = context.get_data_folder();
            let thread = thread::Builder::new()
                .name("bluemap-player-export".into())
                .spawn(move || {
                    // A scoped subscriber survives reload without setting a global singleton.
                    let subscriber = tracing_subscriber::fmt()
                        .with_target(false)
                        .with_max_level(tracing::Level::INFO)
                        .finish();
                    let _logging = tracing::subscriber::set_default(subscriber);
                    let initialize = || {
                        fs::create_dir_all(&folder)
                            .map_err(|e| format!("create {}: {e}", folder.display()))?;
                        write_snapshot(&folder, &Snapshot::now(Vec::new())?)?;
                        Visibility::load(&folder)
                    };
                    let initialized = initialize();
                    ready_tx
                        .send(initialized.as_ref().map(|_| ()).map_err(Clone::clone))
                        .map_err(|e| format!("report worker startup: {e}"))?;
                    let visibility = initialized?;
                    tracing::info!(path = %folder.display(), "BlueMap player exporter started");
                    let result = run_exporter(&folder, stop_rx, Duration::from_secs(1), || {
                        capture(&context.server, &visibility)
                    });
                    if let Err(error) = &result {
                        tracing::error!(%error, "BlueMap player exporter stopped with an error");
                    }
                    result
                })
                .map_err(|e| format!("start BlueMap exporter thread: {e}"))?;
            match ready_rx.recv() {
                Ok(Ok(())) => {
                    *worker = Some(Worker {
                        stop: stop_tx,
                        thread,
                    });
                    Ok(())
                }
                Ok(Err(error)) => {
                    join(thread)?;
                    Err(error)
                }
                Err(error) => {
                    join(thread)?;
                    Err(format!("BlueMap exporter startup failed: {error}"))
                }
            }
        })
    }

    fn on_unload(&self, _context: Arc<Context>) -> PluginFuture<'_, Result<(), String>> {
        Box::pin(async move {
            let worker = self
                .worker
                .lock()
                .map_err(|e| format!("worker lock: {e}"))?
                .take();
            if let Some(worker) = worker {
                // The receiver also exits on disconnect. Always join, even if it already failed.
                drop(worker.stop);
                join(worker.thread)?;
            }
            Ok(())
        })
    }
}

fn join(thread: JoinHandle<Result<(), String>>) -> Result<(), String> {
    thread
        .join()
        .map_err(|_| "BlueMap exporter thread panicked".to_string())?
}

fn capture(server: &Server, visibility: &Visibility) -> Result<Snapshot, String> {
    let mut paths = HashMap::<PathBuf, String>::new();
    let mut seen = HashSet::new();
    let mut players = Vec::new();
    let connected: Vec<_> = server
        .get_all_players()
        .into_iter()
        .filter(|player| !player.client.closed())
        .collect();
    let mut vanished = HashSet::new();
    for observer in &connected {
        let hidden = observer
            .hidden_players
            .lock()
            .map_err(|e| format!("observer hide lock: {e}"))?;
        vanished.extend(hidden.iter().copied());
    }
    for player in connected {
        if player.client.closed() || !seen.insert(player.gameprofile.id) {
            continue;
        }
        if players.len() == MAX_PLAYERS {
            return Err(format!("more than {MAX_PLAYERS} connected players"));
        }
        let entity = player.get_entity();
        let world = entity.world.load_full();
        // Dimension changes publish the world before positioning and tracking the
        // arriving player. Omit that interval rather than mix two worlds' state.
        if !world.entity_tracker.has_entity_with_id(entity.entity_id) {
            continue;
        }
        let pos = entity.pos.load();
        let rotation = Rotation {
            pitch: entity.pitch.load(),
            yaw: entity.yaw.load(),
        };
        let root = &world.level.level_folder.root_folder;
        let world_path = if let Some(path) = paths.get(root) {
            path.clone()
        } else {
            let canonical = fs::canonicalize(root)
                .map_err(|e| format!("canonicalize saved-world root {}: {e}", root.display()))?;
            let text = java_canonical_path(
                canonical
                    .to_str()
                    .ok_or_else(|| "saved-world root is not UTF-8".to_string())?,
            );
            paths.insert(root.clone(), text.clone());
            text
        };
        let (sky_light, block_light) = raw_light(&world, BlockPos::containing_vec(pos))?;
        // A player moving between world lists can appear twice or change worlds mid-sample.
        if player.client.closed()
            || !Arc::ptr_eq(&world, &entity.world.load_full())
            || !world.entity_tracker.has_entity_with_id(entity.entity_id)
        {
            continue;
        }
        let uuid = player.gameprofile.id.hyphenated().to_string();
        let hidden = visibility.hidden_players.contains(&uuid);
        players.push(PlayerSnapshot {
            uuid,
            name: player.gameprofile.name.clone(),
            world_path,
            dimension: world.dimension.minecraft_name.into(),
            position: Position {
                x: pos.x,
                y: pos.y,
                z: pos.z,
            },
            rotation,
            gamemode: player.gamemode.load().name().into(),
            invisible: Some(entity.invisible.load(Ordering::Relaxed)),
            sneaking: Some(entity.is_sneaking()),
            vanished: Some(vanished.contains(&player.gameprofile.id)),
            hidden: Some(hidden),
            sky_light,
            block_light,
        });
    }
    Snapshot::now(players)
}

fn raw_light(world: &World, position: BlockPos) -> Result<(Option<u8>, Option<u8>), String> {
    let (coordinate, relative) = position.chunk_and_chunk_relative_position();
    world
        .level
        .read_chunk_sync(&coordinate, |chunk| {
            if !chunk.light_populated.load(Ordering::Relaxed) {
                return Ok((None, None));
            }
            let Some(y) = relative
                .y
                .checked_sub(chunk.section.min_y)
                .filter(|y| *y >= 0)
            else {
                return Ok((None, None));
            };
            let light = chunk
                .light_engine
                .lock()
                .map_err(|e| format!("chunk light lock: {e}"))?;
            let section = y as usize / 16;
            let x = relative.x as usize;
            let z = relative.z as usize;
            let y = y as usize % 16;
            Ok((
                light
                    .sky_light
                    .get(section)
                    .map(|values| values.get(x, y, z)),
                light
                    .block_light
                    .get(section)
                    .map(|values| values.get(x, y, z)),
            ))
        })
        .unwrap_or(Ok((None, None)))
}
