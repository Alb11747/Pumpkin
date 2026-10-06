use serde::Deserialize;
use std::{collections::HashSet, fs::File, io::Read, path::Path};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    version: u8,
    hidden_players: Vec<String>,
    vanish_source: String,
}

pub struct Visibility {
    pub hidden_players: HashSet<String>,
}

impl Visibility {
    pub fn load(folder: &Path) -> Result<Self, String> {
        let path = folder.join("visibility.json");
        let file = File::open(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
        let mut bytes = Vec::new();
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err("visibility.json exceeds 1 MiB".into());
        }
        Self::parse(&bytes).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let config: Config =
            serde_json::from_slice(bytes).map_err(|e| format!("invalid JSON: {e}"))?;
        if config.version != 1 || config.vanish_source != "pumpkin-observer-hides" {
            return Err("requires version 1 and vanishSource pumpkin-observer-hides".into());
        }
        let mut hidden_players = HashSet::new();
        for uuid in config.hidden_players {
            if !canonical_uuid(&uuid) || !hidden_players.insert(uuid) {
                return Err(
                    "hiddenPlayers requires unique canonical lowercase UUID strings".into(),
                );
            }
        }
        Ok(Self { hidden_players })
    }
}

pub fn canonical_uuid(text: &str) -> bool {
    let uuid = text.as_bytes();
    uuid.len() == 36
        && uuid.iter().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                *c == b'-'
            } else {
                c.is_ascii_digit() || (b'a'..=b'f').contains(c)
            }
        })
}

/// Matches Java's Windows toRealPath spelling without changing the resolved location.
pub fn java_canonical_path(path: &str) -> String {
    if let Some(unc) = path.strip_prefix("\\\\?\\UNC\\") {
        return format!("\\\\{unc}");
    }
    if let Some(drive) = path.strip_prefix("\\\\?\\") {
        let bytes = drive.as_bytes();
        if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1..3] == *b":\\" {
            return drive.into();
        }
    }
    path.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_config_requires_explicit_supported_policy_and_canonical_unique_uuids() {
        let valid = br#"{"version":1,"hiddenPlayers":["00000000-0000-0000-0000-000000000001"],"vanishSource":"pumpkin-observer-hides"}"#;
        let visibility = Visibility::parse(valid).unwrap();
        assert!(
            visibility
                .hidden_players
                .contains("00000000-0000-0000-0000-000000000001")
        );
        for invalid in [
            r#"{"version":1,"vanishSource":"pumpkin-observer-hides"}"#,
            r#"{"version":1,"hiddenPlayers":[],"vanishSource":"bukkit-metadata"}"#,
            r#"{"version":1,"version":1,"hiddenPlayers":[],"vanishSource":"pumpkin-observer-hides"}"#,
            r#"{"version":1,"hiddenPlayers":["00000000-0000-0000-0000-00000000000A"],"vanishSource":"pumpkin-observer-hides"}"#,
            r#"{"version":1,"hiddenPlayers":["00000000-0000-0000-0000-000000000001","00000000-0000-0000-0000-000000000001"],"vanishSource":"pumpkin-observer-hides"}"#,
        ] {
            assert!(Visibility::parse(invalid.as_bytes()).is_err());
        }
        let folder = tempfile::tempdir().unwrap();
        assert!(Visibility::load(folder.path()).is_err());
    }

    #[test]
    fn windows_verbatim_paths_match_java_without_remapping_other_paths() {
        assert_eq!(java_canonical_path(r"\\?\C:\world"), r"C:\world");
        assert_eq!(
            java_canonical_path(r"\\?\UNC\server\share\world"),
            r"\\server\share\world"
        );
        assert_eq!(java_canonical_path("/srv/world"), "/srv/world");
        assert_eq!(
            java_canonical_path(r"\\?\Volume{synthetic}\world"),
            r"\\?\Volume{synthetic}\world"
        );
    }
}
