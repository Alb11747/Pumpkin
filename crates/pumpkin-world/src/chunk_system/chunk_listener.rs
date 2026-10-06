use super::ChunkPos;
use crate::level::SyncChunk;
use crossbeam::channel::{Receiver, Sender};
use std::sync::Arc;
use std::sync::{Mutex, Weak};
use tokio::sync::oneshot;

#[derive(Debug, thiserror::Error)]
#[error("Chunk system stopped: {0}")]
pub struct ChunkSystemFailure(pub String);

type ChunkResult = Result<SyncChunk, Arc<ChunkSystemFailure>>;

struct SingleChunkListeners {
    waiters: Vec<(ChunkPos, oneshot::Sender<ChunkResult>)>,
    failure: Option<Arc<ChunkSystemFailure>>,
}

#[expect(clippy::type_complexity)]
pub struct ChunkListener {
    single: Mutex<SingleChunkListeners>,
    global: Mutex<Vec<Sender<(ChunkPos, Weak<crate::chunk::ChunkData>)>>>,
}

impl Default for ChunkListener {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkListener {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            single: Mutex::new(SingleChunkListeners {
                waiters: Vec::new(),
                failure: None,
            }),
            global: Mutex::new(Vec::new()),
        }
    }

    pub fn add_single_chunk_listener(&self, pos: ChunkPos) -> oneshot::Receiver<ChunkResult> {
        let (tx, rx) = oneshot::channel();
        let mut single = self
            .single
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(failure) = &single.failure {
            let _ = tx.send(Err(failure.clone()));
        } else {
            single.waiters.retain(|(_, sender)| !sender.is_closed());
            single.waiters.push((pos, tx));
        }
        rx
    }

    /// Reject pending and future chunk requests when the world cannot safely continue.
    pub(crate) fn stop(&self, cause: String) {
        let mut single = self
            .single
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let failure = single
            .failure
            .get_or_insert_with(|| Arc::new(ChunkSystemFailure(cause)))
            .clone();
        for (_, sender) in single.waiters.drain(..) {
            let _ = sender.send(Err(failure.clone()));
        }
    }

    pub fn add_global_chunk_listener(&self) -> Receiver<(ChunkPos, Weak<crate::chunk::ChunkData>)> {
        let (tx, rx) = crossbeam::channel::unbounded();
        self.global
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(tx);
        rx
    }

    pub fn process_new_chunk(&self, pos: ChunkPos, chunk: &SyncChunk) {
        {
            let mut single = self
                .single
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if single.failure.is_some() {
                return;
            }
            let mut i = 0;
            let mut len = single.waiters.len();
            while i < len {
                if single.waiters[i].0 == pos {
                    let (_, send) = single.waiters.remove(i);
                    let _ = send.send(Ok(chunk.clone()));
                    // log::debug!("single listener {i} send {pos:?}");
                    len -= 1;
                    continue;
                }
                i += 1;
            }
        }
        {
            let weak = Arc::downgrade(chunk);
            let mut global = self
                .global
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut i = 0;
            let mut len = global.len();
            while i < len {
                if matches!(global[i].send((pos, weak.clone())), Ok(())) {
                    // log::debug!("global listener {i} send {pos:?}");
                } else {
                    // log::debug!("one global listener dropped");
                    global.remove(i);
                    len -= 1;
                    continue;
                }
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn stopped_world_completes_cancelled_pending_and_future_waiters() {
        let listener = ChunkListener::new();
        let pos = ChunkPos::new(3, -2);
        drop(listener.add_single_chunk_listener(pos));
        let pending = listener.add_single_chunk_listener(pos);
        let neighbor = listener.add_single_chunk_listener(ChunkPos::new(4, -2));
        listener.stop("Failed to read chunk (3, -2): invalid header".to_string());
        listener.stop("shutdown".to_string());
        let future = listener.add_single_chunk_listener(pos);
        for waiter in [pending, neighbor, future] {
            let cause = tokio::time::timeout(Duration::from_secs(1), waiter)
                .await
                .unwrap()
                .unwrap()
                .err()
                .unwrap();
            assert!(cause.to_string().contains("(3, -2): invalid header"));
        }
        assert!(listener.single.lock().unwrap().waiters.is_empty());
    }
}
