use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

/// Modern vanilla stores progress under world/players; older worlds keep the
/// same files directly under world. Keep writing whichever existing file was read.
pub fn existing_progress_path(path: &Path) -> io::Result<PathBuf> {
    if path.try_exists()? {
        return Ok(path.to_owned());
    }
    if let Some(directory) = path.parent()
        && let Some(players) = directory.parent()
        && players.file_name().is_some_and(|name| name == "players")
        && let (Some(world), Some(kind), Some(filename)) =
            (players.parent(), directory.file_name(), path.file_name())
    {
        let legacy = world.join(kind).join(filename);
        if legacy.try_exists()? {
            return Ok(legacy);
        }
    }
    Ok(path.to_owned())
}

pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    pumpkin_util::atomic_file::atomic_write(path, |file| file.write_all(contents))
}

// Atomic publication performs synchronous writes and fsyncs; keep it off Tokio workers.
pub async fn atomic_write_async(path: PathBuf, contents: Vec<u8>) -> io::Result<()> {
    tokio::task::spawn_blocking(move || atomic_write(&path, &contents))
        .await
        .map_err(io::Error::other)?
}

// Sessions and snapshots share one writer for each UUID, including reconnects.
// Capturing an order ticket only locks the short counter, never disk IO.
#[derive(Default)]
pub struct PlayerSaveState {
    order: std::sync::Mutex<PlayerSaveOrder>,
    writer: std::sync::Mutex<()>,
    admission: std::sync::Arc<tokio::sync::Mutex<()>>,
}

#[derive(Default)]
struct PlayerSaveOrder {
    generation: u64,
    session: u64,
    nbt_generation: u64,
    statistics_generation: u64,
    load_failed: bool,
    snapshots_closed: bool,
}

#[derive(Clone)]
pub struct PlayerSaveSession {
    state: std::sync::Arc<PlayerSaveState>,
    id: u64,
}

#[derive(Clone, Copy)]
pub enum PlayerSaveChannel {
    Nbt,
    Statistics,
}

#[derive(Debug, thiserror::Error)]
pub enum PlayerSaveError {
    #[error("replaced player session")]
    ReplacedSession,
    #[error("player data load failed; saving disabled")]
    FailedLoad,
    #[error("player disconnect snapshot is final; further snapshots disabled")]
    FinishedSession,
}

pub struct PlayerSaveTicket {
    session: PlayerSaveSession,
    generation: u64,
    channel: PlayerSaveChannel,
}

impl PlayerSaveState {
    pub async fn admission_guard(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.admission.clone().lock_owned().await
    }

    pub fn writer(&self) -> std::sync::MutexGuard<'_, ()> {
        self.writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    // The caller holds writer while beginning a session and loading its files.
    pub fn begin_session(self: &std::sync::Arc<Self>) -> PlayerSaveSession {
        let mut order = self
            .order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        order.generation += 1;
        order.session = order.generation;
        order.load_failed = false;
        order.snapshots_closed = false;
        PlayerSaveSession {
            state: self.clone(),
            id: order.session,
        }
    }
}

impl PlayerSaveSession {
    // Reserve both final tickets together and close ordinary capture. A tick
    // retaining an old player list cannot supersede either disconnect snapshot.
    pub fn final_tickets(&self) -> Result<(PlayerSaveTicket, PlayerSaveTicket), PlayerSaveError> {
        let mut order = self
            .state
            .order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if order.session != self.id {
            return Err(PlayerSaveError::ReplacedSession);
        }
        if order.load_failed {
            return Err(PlayerSaveError::FailedLoad);
        }
        if order.snapshots_closed {
            return Err(PlayerSaveError::FinishedSession);
        }
        order.snapshots_closed = true;
        order.generation += 1;
        order.nbt_generation = order.generation;
        let nbt = PlayerSaveTicket {
            session: self.clone(),
            generation: order.nbt_generation,
            channel: PlayerSaveChannel::Nbt,
        };
        order.generation += 1;
        order.statistics_generation = order.generation;
        let statistics = PlayerSaveTicket {
            session: self.clone(),
            generation: order.statistics_generation,
            channel: PlayerSaveChannel::Statistics,
        };
        Ok((nbt, statistics))
    }

    #[must_use]
    pub fn snapshots_closed(&self) -> bool {
        let order = self
            .state
            .order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        order.session != self.id || order.snapshots_closed
    }

    pub fn writer(&self) -> std::sync::MutexGuard<'_, ()> {
        self.state.writer()
    }
    pub fn state(&self) -> std::sync::Arc<PlayerSaveState> {
        self.state.clone()
    }

    pub fn set_load_failed(&self, failed: bool) {
        let mut order = self
            .state
            .order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if order.session == self.id {
            order.load_failed = failed;
        }
    }

    pub fn ticket(&self, channel: PlayerSaveChannel) -> Result<PlayerSaveTicket, PlayerSaveError> {
        let mut order = self
            .state
            .order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if order.session != self.id {
            return Err(PlayerSaveError::ReplacedSession);
        }
        if order.load_failed {
            return Err(PlayerSaveError::FailedLoad);
        }
        if order.snapshots_closed {
            return Err(PlayerSaveError::FinishedSession);
        }
        order.generation += 1;
        let generation = order.generation;
        match channel {
            PlayerSaveChannel::Nbt => order.nbt_generation = generation,
            PlayerSaveChannel::Statistics => order.statistics_generation = generation,
        }
        Ok(PlayerSaveTicket {
            session: self.clone(),
            generation,
            channel,
        })
    }
}

impl PlayerSaveTicket {
    pub fn writer(&self) -> std::sync::MutexGuard<'_, ()> {
        self.session.writer()
    }
    pub fn is_current(&self) -> bool {
        let order = self
            .session
            .state
            .order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.session.id == order.session
            && !order.load_failed
            && self.generation
                == match self.channel {
                    PlayerSaveChannel::Nbt => order.nbt_generation,
                    PlayerSaveChannel::Statistics => order.statistics_generation,
                }
    }
}
