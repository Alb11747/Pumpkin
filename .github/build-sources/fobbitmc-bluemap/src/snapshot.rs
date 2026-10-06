use serde::Serialize;
use std::{
    collections::HashSet,
    fs::{self, File},
    io::Write,
    path::Path,
    sync::mpsc::{Receiver, RecvTimeoutError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const OUTPUT_FILE: &str = "live-players.json";
pub const MAX_PLAYERS: usize = 1024;
const MAX_BYTES: usize = 1024 * 1024;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    version: u8,
    generated_at: u64,
    pub players: Vec<PlayerSnapshot>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerSnapshot {
    pub uuid: String,
    pub name: String,
    pub world_path: String,
    pub dimension: String,
    pub position: Position,
    pub rotation: Rotation,
    pub gamemode: String,
    pub invisible: Option<bool>,
    pub sneaking: Option<bool>,
    pub vanished: Option<bool>,
    pub hidden: Option<bool>,
    pub sky_light: Option<u8>,
    pub block_light: Option<u8>,
}

#[derive(Serialize)]
pub struct Position {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

#[derive(Serialize)]
pub struct Rotation {
    pub pitch: f32,
    pub yaw: f32,
}

impl Snapshot {
    pub fn now(players: Vec<PlayerSnapshot>) -> Result<Self, String> {
        let generated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| format!("system clock precedes Unix epoch: {e}"))?
            .as_millis()
            .try_into()
            .map_err(|e| format!("snapshot timestamp overflows: {e}"))?;
        Ok(Self {
            version: 1,
            generated_at,
            players,
        })
    }

    fn encode(&self) -> Result<Vec<u8>, String> {
        if self.players.len() > MAX_PLAYERS {
            return Err(format!("snapshot exceeds {MAX_PLAYERS} players"));
        }
        let mut uuids = HashSet::new();
        for player in &self.players {
            player.validate()?;
            if !uuids.insert(&player.uuid) {
                return Err("snapshot contains duplicate player UUIDs".into());
            }
        }
        let bytes = serde_json::to_vec(self).map_err(|e| format!("encode snapshot: {e}"))?;
        if bytes.len() > MAX_BYTES {
            return Err(format!("snapshot exceeds {MAX_BYTES} bytes"));
        }
        Ok(bytes)
    }
}

impl PlayerSnapshot {
    fn validate(&self) -> Result<(), String> {
        if !crate::visibility::canonical_uuid(&self.uuid) {
            return Err("player UUID is not canonical lowercase UUID text".into());
        }
        if self.name.is_empty()
            || self.name.len() > 256
            || self.name.chars().any(char::is_control)
            || self.world_path.len() > 4096
            || !Path::new(&self.world_path).is_absolute()
        {
            return Err("player name or canonical world path is invalid".into());
        }
        if !matches!(
            self.dimension.as_str(),
            "minecraft:overworld" | "minecraft:the_nether" | "minecraft:the_end"
        ) || !matches!(
            self.gamemode.as_str(),
            "survival" | "creative" | "adventure" | "spectator"
        ) {
            return Err("unsupported player dimension or game mode".into());
        }
        if [self.position.x, self.position.y, self.position.z]
            .into_iter()
            .any(|value| !value.is_finite() || value.abs() > 60_000_000.0)
            || !self.rotation.pitch.is_finite()
            || self.rotation.pitch.abs() > 90.0
            || !self.rotation.yaw.is_finite()
            || self.rotation.yaw.abs() > 360_000.0
            || self.sky_light.is_some_and(|value| value > 15)
            || self.block_light.is_some_and(|value| value > 15)
        {
            return Err("player coordinates, rotation or light are invalid".into());
        }
        Ok(())
    }
}

pub fn write_snapshot(folder: &Path, snapshot: &Snapshot) -> Result<(), String> {
    // A single owned writer replaces a sibling file, so readers never see partial JSON.
    let bytes = snapshot.encode()?;
    let temporary = folder.join(format!("{OUTPUT_FILE}.tmp"));
    let destination = folder.join(OUTPUT_FILE);
    let mut file =
        File::create(&temporary).map_err(|e| format!("create {}: {e}", temporary.display()))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("write {}: {e}", temporary.display()))?;
    drop(file);
    fs::rename(&temporary, &destination)
        .map_err(|e| format!("replace {}: {e}", destination.display()))
}

pub fn run_exporter(
    folder: &Path,
    stop: Receiver<()>,
    interval: Duration,
    mut capture: impl FnMut() -> Result<Snapshot, String>,
) -> Result<(), String> {
    let mut next = Instant::now();
    loop {
        match stop.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                let snapshot = match capture() {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        tracing::error!(%error, "BlueMap player capture failed; clearing snapshot");
                        Snapshot::now(Vec::new())?
                    }
                };
                if let Err(error) = write_snapshot(folder, &snapshot) {
                    tracing::error!(%error, "BlueMap player snapshot write failed");
                    // Invalid sampled state must not preserve the previous visible players.
                    write_snapshot(folder, &Snapshot::now(Vec::new())?).map_err(|clear_error| {
                        format!("snapshot failed ({error}); clearing failed ({clear_error})")
                    })?;
                }
                next += interval;
                if next <= Instant::now() {
                    next = Instant::now() + interval;
                }
            }
        }
    }
    write_snapshot(folder, &Snapshot::now(Vec::new())?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread};

    fn player(folder: &Path) -> PlayerSnapshot {
        PlayerSnapshot {
            uuid: "00000000-0000-0000-0000-000000000001".into(),
            name: "SyntheticPlayer".into(),
            world_path: folder.to_str().unwrap().into(),
            dimension: "minecraft:overworld".into(),
            position: Position {
                x: -1.5,
                y: 65.0,
                z: 32.0,
            },
            rotation: Rotation {
                pitch: 10.0,
                yaw: -30.0,
            },
            gamemode: "survival".into(),
            invisible: Some(false),
            sneaking: Some(true),
            vanished: None,
            hidden: None,
            sky_light: None,
            block_light: Some(4),
        }
    }

    #[test]
    fn unknown_fields_are_required_null_and_invalid_state_preserves_last_file() {
        let folder = tempfile::tempdir().unwrap();
        let snapshot = Snapshot::now(vec![player(folder.path())]).unwrap();
        write_snapshot(folder.path(), &snapshot).unwrap();
        let before = fs::read(folder.path().join(OUTPUT_FILE)).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&before).unwrap();
        assert_eq!(json["version"], 1);
        let exported = &json["players"][0];
        for field in ["vanished", "hidden", "skyLight"] {
            assert!(exported.as_object().unwrap().contains_key(field));
            assert!(exported[field].is_null());
        }
        assert_eq!(exported["sneaking"], true);
        assert_eq!(exported["blockLight"], 4);
        let mut invalid = Snapshot::now(vec![player(folder.path())]).unwrap();
        invalid.players[0].position.x = f64::NAN;
        assert!(write_snapshot(folder.path(), &invalid).is_err());
        assert_eq!(fs::read(folder.path().join(OUTPUT_FILE)).unwrap(), before);
        assert!(!folder.path().join(format!("{OUTPUT_FILE}.tmp")).exists());
    }

    #[test]
    fn stop_joins_after_empty_snapshot() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().to_owned();
        let (stop_tx, stop_rx) = mpsc::channel();
        let (sample_tx, sample_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            run_exporter(&path, stop_rx, Duration::from_millis(10), || {
                sample_tx.send(()).unwrap();
                Snapshot::now(vec![player(&path)])
            })
        });
        sample_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        stop_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(folder.path().join(OUTPUT_FILE)).unwrap()).unwrap();
        assert_eq!(json["players"], serde_json::json!([]));
    }

    #[test]
    fn failed_capture_clears_previous_players_before_unload() {
        let folder = tempfile::tempdir().unwrap();
        write_snapshot(
            folder.path(),
            &Snapshot::now(vec![player(folder.path())]).unwrap(),
        )
        .unwrap();
        let path = folder.path().to_owned();
        let (stop_tx, stop_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            run_exporter(&path, stop_rx, Duration::from_secs(60), || {
                Err("synthetic capture failure".into())
            })
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let json: serde_json::Value =
                serde_json::from_slice(&fs::read(folder.path().join(OUTPUT_FILE)).unwrap())
                    .unwrap();
            if json["players"] == serde_json::json!([]) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "capture failure did not clear players"
            );
            thread::sleep(Duration::from_millis(1));
        }
        stop_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn duplicate_uuid_and_size_limit_are_rejected() {
        let folder = tempfile::tempdir().unwrap();
        let duplicate = Snapshot::now(vec![player(folder.path()), player(folder.path())]).unwrap();
        assert!(duplicate.encode().unwrap_err().contains("duplicate"));
        let mut players = Vec::new();
        for index in 0..=MAX_PLAYERS {
            let mut item = player(folder.path());
            item.uuid = format!("00000000-0000-0000-0000-{index:012x}");
            players.push(item);
        }
        assert!(
            Snapshot::now(players)
                .unwrap()
                .encode()
                .unwrap_err()
                .contains("players")
        );
        let mut players = Vec::new();
        for index in 0..300 {
            let mut item = player(folder.path());
            item.uuid = format!("00000000-0000-0000-0000-{index:012x}");
            item.world_path = folder
                .path()
                .join("w".repeat(3900))
                .to_str()
                .unwrap()
                .into();
            players.push(item);
        }
        assert!(
            Snapshot::now(players)
                .unwrap()
                .encode()
                .unwrap_err()
                .contains("bytes")
        );
    }
}
