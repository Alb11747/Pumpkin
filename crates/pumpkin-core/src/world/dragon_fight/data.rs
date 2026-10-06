use std::{
    fs::{self, File},
    io::ErrorKind,
    path::Path,
};

use pumpkin_nbt::{
    compound::NbtCompound,
    nbt_compress::{read_gzip_compound_tag, write_gzip_compound_tag},
    tag::NbtTag,
};
use pumpkin_util::math::position::BlockPos;
use pumpkin_world::{
    level::LevelFolder,
    world_info::{
        MAXIMUM_SUPPORTED_WORLD_DATA_VERSION, WorldInfoError, data_files::minecraft_data_dir,
    },
};
use uuid::Uuid;

use super::{DragonFight, DragonRespawnStage};

fn read_compound(path: &Path) -> Result<Option<NbtCompound>, WorldInfoError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    read_gzip_compound_tag(file).map(Some).map_err(|error| {
        WorldInfoError::DeserializationError(format!("{}: {error}", path.display()))
    })
}

fn legacy_fight_data(root: &NbtCompound) -> Option<&NbtCompound> {
    let data = root.get_compound("Data")?;
    data.get_compound("DragonFight").or_else(|| {
        data.get_compound("DimensionData")?
            .get_compound("1")?
            .get_compound("DragonFight")
    })
}

fn malformed(field: &str) -> WorldInfoError {
    WorldInfoError::DeserializationError(format!("Invalid dragon fight {field}"))
}

fn bool_field(
    data: &NbtCompound,
    modern: &str,
    legacy: &str,
    default: bool,
) -> Result<bool, WorldInfoError> {
    let key = if data.get(modern).is_some() {
        modern
    } else {
        legacy
    };
    if data.get(key).is_none() {
        return Ok(default);
    }
    data.get_bool(key).ok_or_else(|| malformed(key))
}

fn uuid_from_tag(tag: &NbtTag) -> Result<Uuid, WorldInfoError> {
    let mut compound = NbtCompound::new();
    compound.put("uuid", tag.clone());
    compound.get_uuid("uuid").ok_or_else(|| malformed("UUID"))
}

impl DragonRespawnStage {
    const fn serialized_name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::PreparingToSummonPillars => "preparing_to_summon_pillars",
            Self::SummoningPillars => "summoning_pillars",
            Self::SummoningDragon => "summoning_dragon",
            Self::End => "end",
        }
    }

    fn from_serialized_name(name: &str) -> Result<Self, WorldInfoError> {
        match name {
            "start" => Ok(Self::Start),
            "preparing_to_summon_pillars" => Ok(Self::PreparingToSummonPillars),
            "summoning_pillars" => Ok(Self::SummoningPillars),
            "summoning_dragon" => Ok(Self::SummoningDragon),
            "end" => Ok(Self::End),
            _ => Err(malformed("respawn_stage")),
        }
    }
}

impl DragonFight {
    pub fn load(folder: &LevelFolder, seed: u64) -> Result<Self, WorldInfoError> {
        let path = minecraft_data_dir(&folder.dim_folder).join("ender_dragon_fight.dat");
        if let Some(root) = read_compound(&path)? {
            let data = root
                .get_compound("data")
                .ok_or_else(|| malformed("data compound"))?;
            let mut fight = Self::from_saved_data(data)?;
            fight.saved_data = root;
            fight.init(seed, BlockPos::new(0, 0, 0));
            return Ok(fight);
        }
        for folder in [&folder.dim_folder, &folder.root_folder] {
            if let Some(root) = read_compound(&folder.join("level.dat"))?
                && let Some(data) = legacy_fight_data(&root)
            {
                let mut fight = Self::from_saved_data(data)?;
                let mut saved = NbtCompound::new();
                saved.put_compound("data", data.clone());
                fight.saved_data = saved;
                fight.init(seed, BlockPos::new(0, 0, 0));
                return Ok(fight);
            }
        }
        Ok(Self::new_with_seed(seed, BlockPos::new(0, 0, 0)))
    }

    fn from_saved_data(data: &NbtCompound) -> Result<Self, WorldInfoError> {
        let mut fight = Self::new_with_seed(0, BlockPos::new(0, 0, 0));
        fight.needs_state_scanning =
            bool_field(data, "needs_state_scanning", "NeedsStateScanning", true)?;
        fight.dragon_killed = bool_field(data, "dragon_killed", "DragonKilled", false)?;
        fight.previously_killed = bool_field(data, "previously_killed", "PreviouslyKilled", false)?;
        if let Some(tag) = data.get("dragon_uuid").or_else(|| data.get("Dragon")) {
            fight.dragon_uuid = Some(uuid_from_tag(tag)?);
        }
        if let Some(tag) = data
            .get("exit_portal_location")
            .or_else(|| data.get("ExitPortalLocation"))
        {
            fight.exit_portal_location = Some(match tag {
                NbtTag::IntArray(values) if values.len() == 3 => {
                    BlockPos::new(values[0], values[1], values[2])
                }
                NbtTag::Compound(pos) => BlockPos::new(
                    pos.get_int("X")
                        .ok_or_else(|| malformed("ExitPortalLocation.X"))?,
                    pos.get_int("Y")
                        .ok_or_else(|| malformed("ExitPortalLocation.Y"))?,
                    pos.get_int("Z")
                        .ok_or_else(|| malformed("ExitPortalLocation.Z"))?,
                ),
                _ => return Err(malformed("exit_portal_location")),
            });
        }
        fight.gateways = match data.get("gateways").or_else(|| data.get("Gateways")) {
            Some(NbtTag::IntArray(values)) => values.clone(),
            Some(NbtTag::List(values)) => values
                .iter()
                .map(|tag| match tag {
                    NbtTag::Int(value) => Ok(*value),
                    _ => Err(malformed("gateways")),
                })
                .collect::<Result<Vec<_>, _>>()?,
            None => Vec::new(),
            _ => return Err(malformed("gateways")),
        };
        if fight.gateways.len() > super::GATEWAY_COUNT
            || fight
                .gateways
                .iter()
                .any(|&gateway| !(0..super::GATEWAY_COUNT as i32).contains(&gateway))
        {
            return Err(malformed("gateways"));
        }
        if data.get("respawn_stage").is_some() {
            fight.respawn_stage = Some(DragonRespawnStage::from_serialized_name(
                data.get_string("respawn_stage")
                    .ok_or_else(|| malformed("respawn_stage"))?,
            )?);
        } else if bool_field(data, "is_respawning", "IsRespawning", false)? {
            fight.respawn_stage = Some(DragonRespawnStage::Start);
            fight.legacy_respawn_pending = true;
        }
        fight.respawn_time = if data.get("respawn_time").is_some() {
            data.get_int("respawn_time")
                .ok_or_else(|| malformed("respawn_time"))?
        } else {
            0
        };
        if let Some(tag) = data.get("respawn_crystals") {
            let NbtTag::List(crystals) = tag else {
                return Err(malformed("respawn_crystals"));
            };
            fight.respawn_crystals = crystals
                .iter()
                .map(uuid_from_tag)
                .collect::<Result<Vec<_>, _>>()?;
        }
        fight.saved_data.put_compound("data", data.clone());
        Ok(fight)
    }

    pub fn saved_data(&self) -> NbtCompound {
        let mut root = self.saved_data.clone();
        let mut data = root.get_compound("data").cloned().unwrap_or_default();
        data.put_bool("needs_state_scanning", self.needs_state_scanning);
        data.put_bool("dragon_killed", self.dragon_killed);
        data.put_bool("previously_killed", self.previously_killed);
        data.put_int("respawn_time", self.respawn_time);
        data.put_list(
            "gateways",
            self.gateways.iter().copied().map(NbtTag::Int).collect(),
        );
        // Remove modeled legacy aliases so an absent optional modern field cannot resurrect old state.
        for key in [
            "Dragon",
            "ExitPortalLocation",
            "IsRespawning",
            "is_respawning",
            "NeedsStateScanning",
            "DragonKilled",
            "PreviouslyKilled",
            "Gateways",
            "dragon_uuid",
            "exit_portal_location",
            "respawn_stage",
        ] {
            data.child_tags.remove(key);
        }
        if let Some(uuid) = self.dragon_uuid {
            data.put_uuid("dragon_uuid", uuid);
        }
        if let Some(pos) = self.exit_portal_location {
            data.put(
                "exit_portal_location",
                NbtTag::IntArray(vec![pos.0.x, pos.0.y, pos.0.z]),
            );
        }
        if self.legacy_respawn_pending && self.respawn_stage.is_some() {
            // Keep legacy recovery pending across saves until ritual entities are admitted.
            data.put_bool("IsRespawning", true);
        } else if let Some(stage) = self.respawn_stage {
            data.put_string("respawn_stage", stage.serialized_name().to_string());
        }
        data.put_list(
            "respawn_crystals",
            self.respawn_crystals
                .iter()
                .map(|uuid| {
                    let value = uuid.as_u128();
                    NbtTag::IntArray(vec![
                        (value >> 96) as i32,
                        (value >> 64) as i32,
                        (value >> 32) as i32,
                        value as i32,
                    ])
                })
                .collect(),
        );
        root.put_int("DataVersion", MAXIMUM_SUPPORTED_WORLD_DATA_VERSION);
        root.put_compound("data", data);
        root
    }

    pub fn write_saved_data(folder: &Path, data: NbtCompound) -> Result<(), WorldInfoError> {
        let dir = minecraft_data_dir(folder);
        fs::create_dir_all(&dir)?;
        let path = dir.join("ender_dragon_fight.dat");
        let path_new = dir.join("ender_dragon_fight.dat_new");
        write_gzip_compound_tag(data, File::create(&path_new)?)
            .map_err(|error| WorldInfoError::SerializationError(error.to_string()))?;
        fs::rename(path_new, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn folder(root: &Path) -> LevelFolder {
        let dim = root.join("dimensions/minecraft/the_end");
        LevelFolder {
            root_folder: root.to_path_buf(),
            region_folder: dim.join("region"),
            entities_folder: dim.join("entities"),
            poi_folder: dim.join("poi"),
            dim_folder: dim,
        }
    }

    #[test]
    fn legacy_dragon_state_import_preserves_history_and_unknown_tags() {
        let directory = TempDir::new().unwrap();
        let folder = folder(directory.path());
        fs::create_dir_all(&folder.dim_folder).unwrap();
        let uuid = Uuid::from_u128(0x1234);
        let mut portal = NbtCompound::new();
        portal.put_int("X", 7);
        portal.put_int("Y", 64);
        portal.put_int("Z", -8);
        let mut history = NbtCompound::new();
        history.put_bool("NeedsStateScanning", false);
        history.put_bool("DragonKilled", true);
        history.put_bool("PreviouslyKilled", true);
        history.put_bool("IsRespawning", true);
        history.put_uuid("Dragon", uuid);
        history.put_compound("ExitPortalLocation", portal);
        history.put_list(
            "Gateways",
            vec![NbtTag::Int(18), NbtTag::Int(3), NbtTag::Int(11)],
        );
        history.put_long("plugin_marker", 9876);
        let mut data = NbtCompound::new();
        data.put_compound("DragonFight", history);
        let mut root = NbtCompound::new();
        root.put_compound("Data", data);
        write_gzip_compound_tag(
            root,
            File::create(folder.dim_folder.join("level.dat")).unwrap(),
        )
        .unwrap();

        let mut fight = DragonFight::load(&folder, 1234).unwrap();
        assert!(!fight.needs_state_scanning);
        assert!(fight.dragon_killed && fight.previously_killed);
        assert_eq!(fight.dragon_uuid, Some(uuid));
        assert_eq!(fight.exit_portal_location, Some(BlockPos::new(7, 64, -8)));
        assert_eq!(fight.gateways, [18, 3, 11]);
        assert_eq!(fight.respawn_stage, Some(DragonRespawnStage::Start));
        DragonFight::write_saved_data(&folder.dim_folder, fight.saved_data()).unwrap();
        let pending = DragonFight::load(&folder, 1234).unwrap();
        assert!(pending.legacy_respawn_pending);
        assert_eq!(pending.respawn_stage, Some(DragonRespawnStage::Start));
        fight.respawn_stage = None;
        fight.gateways.pop();
        fight.dragon_uuid = None;
        fight.exit_portal_location = None;
        DragonFight::write_saved_data(&folder.dim_folder, fight.saved_data()).unwrap();
        let reloaded = DragonFight::load(&folder, 9999).unwrap();
        assert_eq!(reloaded.gateways, [18, 3]);
        assert_eq!(reloaded.dragon_uuid, None);
        assert_eq!(reloaded.exit_portal_location, None);
        assert_eq!(reloaded.respawn_stage, None);
        assert_eq!(
            reloaded
                .saved_data()
                .get_compound("data")
                .unwrap()
                .get_long("plugin_marker"),
            Some(9876)
        );
    }

    #[test]
    fn modern_respawn_roundtrip_and_startup_empty_queue_match_vanilla() {
        let directory = TempDir::new().unwrap();
        let folder = folder(directory.path());
        let mut fight = DragonFight::new_with_seed(1234, BlockPos::new(0, 0, 0));
        // Obtained from official 26.3 Util.shuffle and RandomSource.createThreadLocalInstance(1234).
        assert_eq!(
            fight.gateways,
            [
                1, 6, 11, 14, 4, 18, 12, 2, 17, 0, 16, 15, 7, 9, 3, 13, 10, 5, 19, 8
            ]
        );
        fight.respawn_stage = Some(DragonRespawnStage::SummoningPillars);
        fight.respawn_time = 83;
        fight.respawn_crystals = vec![Uuid::from_u128(11), Uuid::from_u128(22)];
        fight.gateways.clear();
        fight.saved_data.put_long("unmodeled_root", 123);
        DragonFight::write_saved_data(&folder.dim_folder, fight.saved_data()).unwrap();
        let reloaded = DragonFight::load(&folder, 1234).unwrap();
        assert_eq!(reloaded.respawn_stage, fight.respawn_stage);
        assert_eq!(reloaded.respawn_time, 83);
        assert_eq!(reloaded.respawn_crystals, fight.respawn_crystals);
        assert_eq!(
            reloaded.gateways,
            [
                1, 6, 11, 14, 4, 18, 12, 2, 17, 0, 16, 15, 7, 9, 3, 13, 10, 5, 19, 8
            ]
        );
        assert_eq!(reloaded.saved_data().get_long("unmodeled_root"), Some(123));
    }

    #[test]
    fn malformed_fight_metadata_does_not_reset_a_historical_fight() {
        let directory = TempDir::new().unwrap();
        let folder = folder(directory.path());
        let mut data = NbtCompound::new();
        data.put_string("gateways", "corrupt".to_string());
        let mut root = NbtCompound::new();
        root.put_compound("data", data);
        DragonFight::write_saved_data(&folder.dim_folder, root).unwrap();
        assert!(matches!(
            DragonFight::load(&folder, 1234),
            Err(WorldInfoError::DeserializationError(_))
        ));
    }
}
