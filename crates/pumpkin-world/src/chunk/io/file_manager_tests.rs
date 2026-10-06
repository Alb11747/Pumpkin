#![expect(
    clippy::unwrap_used,
    clippy::panic,
    reason = "regression test assertions"
)]

use super::*;
use bytes::Bytes;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use tokio::sync::{Notify, Semaphore};

struct TestChunk {
    value: AtomicU64,
    dirty: AtomicBool,
}

impl TestChunk {
    fn new(value: u64) -> Self {
        Self {
            value: AtomicU64::new(value),
            dirty: AtomicBool::new(true),
        }
    }
}

impl Dirtiable for TestChunk {
    fn is_dirty(&self) -> bool {
        self.dirty.load(Relaxed)
    }
    fn mark_dirty(&self, dirty: bool) {
        self.dirty.store(dirty, Relaxed);
    }
    fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Relaxed)
    }
}

impl PathFromLevelFolder for TestChunk {
    fn file_path(folder: &LevelFolder, name: &str) -> PathBuf {
        folder.region_folder.join(name)
    }
}

struct Control {
    fail_update: AtomicBool,
    fail_write: AtomicBool,
    pause_write: AtomicBool,
    entered: Notify,
    resume: Semaphore,
}

impl Default for Control {
    fn default() -> Self {
        Self {
            fail_update: AtomicBool::new(false),
            fail_write: AtomicBool::new(false),
            pause_write: AtomicBool::new(false),
            entered: Notify::new(),
            resume: Semaphore::new(0),
        }
    }
}

#[derive(Default)]
struct TestSerializer {
    value: u64,
    control: Arc<Control>,
}

impl ChunkSerializer for TestSerializer {
    type Data = TestChunk;
    type WriteBackend = PathBuf;
    type ChunkConfig = Arc<Control>;

    fn get_chunk_key(_: &Vector2<i32>) -> String {
        "test.region".into()
    }
    fn should_write(&self, watched: bool) -> bool {
        !watched
    }

    async fn write(&self, path: &PathBuf) -> Result<(), std::io::Error> {
        if self.control.pause_write.load(Relaxed) {
            self.control.entered.notify_one();
            self.control.resume.acquire().await.unwrap().forget();
        }
        if self.control.fail_write.load(Relaxed) {
            return Err(std::io::Error::other("injected write failure"));
        }
        tokio::fs::write(path, self.value.to_be_bytes()).await
    }

    fn read(bytes: Bytes) -> Result<Self, ChunkReadingError> {
        let bytes: [u8; 8] = bytes
            .as_ref()
            .try_into()
            .map_err(|_| ChunkReadingError::InvalidHeader)?;
        Ok(Self {
            value: u64::from_be_bytes(bytes),
            ..Self::default()
        })
    }

    async fn update_chunk(
        &mut self,
        chunk: Arc<TestChunk>,
        control: &Arc<Control>,
    ) -> Result<(), ChunkWritingError> {
        if control.fail_update.load(Relaxed) {
            return Err(ChunkWritingError::IoError(std::io::Error::other(
                "injected serialization failure",
            )));
        }
        self.value = chunk.value.load(Relaxed);
        self.control = control.clone();
        Ok(())
    }

    async fn get_chunks(
        &self,
        positions: Vec<Vector2<i32>>,
        stream: mpsc::Sender<LoadedData<TestChunk, ChunkReadingError>>,
    ) {
        for _ in positions {
            if stream
                .send(LoadedData::Loaded(TestChunk::new(self.value)))
                .await
                .is_err()
            {
                break;
            }
        }
    }
}

fn folder(path: &Path) -> LevelFolder {
    LevelFolder {
        root_folder: path.into(),
        dim_folder: path.into(),
        region_folder: path.into(),
        entities_folder: path.into(),
        poi_folder: path.into(),
    }
}

async fn load(manager: &ChunkFileManager<TestSerializer>, folder: &LevelFolder) -> u64 {
    let (send, mut recv) = mpsc::channel(1);
    manager
        .fetch_chunks(folder, &[Vector2::new(0, 0)], send)
        .await;
    let Some(LoadedData::Loaded(chunk)) = recv.recv().await else {
        panic!("expected saved chunk")
    };
    chunk.value.load(Relaxed)
}

#[tokio::test]
async fn watched_save_is_on_disk_before_unwatch_and_eviction() {
    let directory = tempfile::tempdir().unwrap();
    let folder = folder(directory.path());
    let manager = ChunkFileManager::<TestSerializer>::new(Arc::default());
    let position = Vector2::new(0, 0);
    manager.watch_chunks(&folder, &[position]).await;
    manager
        .save_chunks(&folder, vec![(position, Arc::new(TestChunk::new(42)))])
        .await
        .unwrap();
    assert_eq!(
        tokio::fs::read(folder.region_folder.join("test.region"))
            .await
            .unwrap(),
        42u64.to_be_bytes()
    );
    manager.unwatch_chunks(&folder, &[position]).await;
    assert!(manager.file_locks.read().await.is_empty());
    assert_eq!(load(&manager, &folder).await, 42);
}

#[tokio::test]
async fn serialization_failure_leaves_chunk_dirty_for_retry() {
    let directory = tempfile::tempdir().unwrap();
    let folder = folder(directory.path());
    let control = Arc::new(Control::default());
    let manager = ChunkFileManager::<TestSerializer>::new(control.clone());
    let chunk = Arc::new(TestChunk::new(42));
    let chunks = vec![(Vector2::new(0, 0), chunk.clone())];
    control.fail_update.store(true, Relaxed);
    assert!(manager.save_chunks(&folder, chunks.clone()).await.is_err());
    assert!(chunk.is_dirty());
    control.fail_update.store(false, Relaxed);
    manager.save_chunks(&folder, chunks).await.unwrap();
    assert!(!chunk.is_dirty());
    assert_eq!(load(&manager, &folder).await, 42);
}

#[tokio::test]
async fn failed_write_survives_unwatch_clear_and_reload_then_retries() {
    let directory = tempfile::tempdir().unwrap();
    let folder = folder(directory.path());
    let control = Arc::new(Control::default());
    let manager = ChunkFileManager::<TestSerializer>::new(control.clone());
    let position = Vector2::new(0, 0);
    let chunk = Arc::new(TestChunk::new(1));
    let chunks = vec![(position, chunk.clone())];
    manager.save_chunks(&folder, chunks.clone()).await.unwrap();
    manager.watch_chunks(&folder, &[position]).await;
    chunk.value.store(42, Relaxed);
    chunk.mark_dirty(true);
    control.fail_write.store(true, Relaxed);
    assert!(manager.save_chunks(&folder, chunks.clone()).await.is_err());
    assert!(chunk.is_dirty());
    manager.unwatch_chunks(&folder, &[position]).await;
    manager.clear_watched_chunks().await;
    assert_eq!(
        load(&manager, &folder).await,
        42,
        "failed region stays canonical"
    );
    assert_eq!(
        tokio::fs::read(folder.region_folder.join("test.region"))
            .await
            .unwrap(),
        1u64.to_be_bytes()
    );
    control.fail_write.store(false, Relaxed);
    manager.save_chunks(&folder, chunks).await.unwrap();
    assert!(!chunk.is_dirty());
    assert!(manager.file_locks.read().await.is_empty());
    assert_eq!(load(&manager, &folder).await, 42);
}

#[tokio::test]
async fn cancelled_write_restores_dirty_and_preserves_region() {
    let directory = tempfile::tempdir().unwrap();
    let folder = Arc::new(folder(directory.path()));
    let control = Arc::new(Control::default());
    control.pause_write.store(true, Relaxed);
    let manager = Arc::new(ChunkFileManager::<TestSerializer>::new(control.clone()));
    let position = Vector2::new(0, 0);
    let chunk = Arc::new(TestChunk::new(42));
    let saving = tokio::spawn({
        let manager = manager.clone();
        let folder = folder.clone();
        let chunk = chunk.clone();
        async move { manager.save_chunks(&folder, vec![(position, chunk)]).await }
    });
    control.entered.notified().await;
    saving.abort();
    assert!(saving.await.unwrap_err().is_cancelled());
    assert!(chunk.is_dirty());
    manager
        .maybe_evict(&folder.region_folder.join("test.region"))
        .await;
    assert_eq!(load(&manager, &folder).await, 42);
    control.pause_write.store(false, Relaxed);
    manager
        .save_chunks(&folder, vec![(position, chunk.clone())])
        .await
        .unwrap();
    assert!(!chunk.is_dirty());
    assert_eq!(load(&manager, &folder).await, 42);
}

#[tokio::test]
async fn concurrent_mutation_during_write_remains_dirty_until_next_save() {
    let directory = tempfile::tempdir().unwrap();
    let folder = Arc::new(folder(directory.path()));
    let control = Arc::new(Control::default());
    control.pause_write.store(true, Relaxed);
    let manager = Arc::new(ChunkFileManager::<TestSerializer>::new(control.clone()));
    let position = Vector2::new(0, 0);
    let chunk = Arc::new(TestChunk::new(1));
    let saving = tokio::spawn({
        let manager = manager.clone();
        let folder = folder.clone();
        let chunk = chunk.clone();
        async move { manager.save_chunks(&folder, vec![(position, chunk)]).await }
    });
    control.entered.notified().await;
    chunk.value.store(42, Relaxed);
    chunk.mark_dirty(true);
    control.pause_write.store(false, Relaxed);
    control.resume.add_permits(1);
    saving.await.unwrap().unwrap();
    assert!(
        chunk.is_dirty(),
        "first save must not acknowledge the later mutation"
    );
    assert_eq!(
        tokio::fs::read(folder.region_folder.join("test.region"))
            .await
            .unwrap(),
        1u64.to_be_bytes()
    );
    manager
        .save_chunks(&folder, vec![(position, chunk.clone())])
        .await
        .unwrap();
    assert!(!chunk.is_dirty());
    assert_eq!(load(&manager, &folder).await, 42);
}
