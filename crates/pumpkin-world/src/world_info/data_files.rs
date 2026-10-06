use std::{
    fs::{self, File},
    io::BufWriter,
    path::{Path, PathBuf},
};

use pumpkin_data::game_rules::{GameRule, GameRuleRegistry, GameRuleValue};
use pumpkin_nbt::{compound::NbtCompound, nbt_compress::read_gzip_compound_tag, tag::NbtTag};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::world_info::{WorldGenSettings, WorldInfoError};

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct DataFileRoot<T> {
    #[serde(rename = "data")]
    pub data: T,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct WeatherData {
    #[serde(rename = "rain_time", default)]
    pub rain_time: i32,
    #[serde(rename = "raining", default)]
    pub raining: bool,
    #[serde(rename = "thundering", default)]
    pub thundering: bool,
    #[serde(rename = "thunder_time", default)]
    pub thunder_time: i32,
    #[serde(rename = "clear_weather_time", default)]
    pub clear_weather_time: i32,
    #[serde(rename = "DataVersion", default)]
    pub data_version: i32,
}

impl Default for WeatherData {
    fn default() -> Self {
        Self {
            rain_time: 0,
            raining: false,
            thundering: false,
            thunder_time: 0,
            clear_weather_time: -1,
            data_version: 0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct WorldGenSettingsData {
    #[serde(flatten)]
    pub settings: WorldGenSettings,
    #[serde(rename = "DataVersion", default)]
    pub data_version: i32,
}

impl WorldGenSettingsData {
    #[must_use]
    pub const fn new(settings: WorldGenSettings, data_version: i32) -> Self {
        Self {
            settings,
            data_version,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DimensionClock {
    pub total_ticks: i64,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct WorldClocksData {
    pub clocks: std::collections::HashMap<String, DimensionClock>,
    pub data_version: i32,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct WanderingTraderData {
    #[serde(rename = "spawn_delay", default = "default_wandering_trader_delay")]
    pub spawn_delay: i32,
    #[serde(rename = "spawn_chance", default = "default_wandering_trader_chance")]
    pub spawn_chance: i32,
    #[serde(rename = "DataVersion", default)]
    pub data_version: i32,
}

const fn default_wandering_trader_delay() -> i32 {
    24_000
}
const fn default_wandering_trader_chance() -> i32 {
    25
}

impl Default for WanderingTraderData {
    fn default() -> Self {
        Self {
            spawn_delay: default_wandering_trader_delay(),
            spawn_chance: default_wandering_trader_chance(),
            data_version: 0,
        }
    }
}

#[must_use]
pub fn minecraft_data_dir(level_folder: &Path) -> PathBuf {
    level_folder.join("data").join("minecraft")
}

/// Ensures the `<world>/data/minecraft/` directory exists.
pub fn ensure_minecraft_data_dir(level_folder: &Path) -> Result<PathBuf, WorldInfoError> {
    let dir = minecraft_data_dir(level_folder);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn read_weather(level_folder: &Path) -> WeatherData {
    let path = minecraft_data_dir(level_folder).join("weather.dat");
    if !path.exists() {
        return WeatherData::default();
    }
    match File::open(&path) {
        Ok(f) => match read_gzip_compound_tag(f) {
            Ok(compound) => {
                let data_compound = compound.get_compound("data");
                let c = data_compound.as_ref().map_or(&compound, |v| v);
                WeatherData {
                    clear_weather_time: c.get_int("clear_weather_time").unwrap_or(0),
                    rain_time: c.get_int("rain_time").unwrap_or(0),
                    thunder_time: c.get_int("thunder_time").unwrap_or(0),
                    raining: c.get_bool("raining").unwrap_or(false),
                    thundering: c.get_bool("thundering").unwrap_or(false),
                    data_version: c.get_int("DataVersion").unwrap_or(0),
                }
            }
            Err(e) => {
                warn!("Failed to deserialize weather.dat, using defaults: {e}");
                WeatherData::default()
            }
        },
        Err(e) => {
            warn!("Failed to open weather.dat, using defaults: {e}");
            WeatherData::default()
        }
    }
}

pub fn write_weather(level_folder: &Path, data: &WeatherData) -> Result<(), WorldInfoError> {
    let dir = ensure_minecraft_data_dir(level_folder)?;
    let path = dir.join("weather.dat");
    let file = File::create(&path)?;
    let mut data_comp = NbtCompound::new();
    data_comp.put_int("clear_weather_time", data.clear_weather_time);
    data_comp.put_int("rain_time", data.rain_time);
    data_comp.put_int("thunder_time", data.thunder_time);
    data_comp.put_bool("raining", data.raining);
    data_comp.put_bool("thundering", data.thundering);
    let mut root = NbtCompound::new();
    root.put_int("DataVersion", data.data_version);
    root.put_compound("data", data_comp);
    pumpkin_nbt::nbt_compress::write_gzip_compound_tag(root, BufWriter::new(file))
        .map_err(|e| WorldInfoError::SerializationError(e.to_string()))
}

#[must_use]
pub fn json_to_nbt_tag(val: &serde_json::Value) -> NbtTag {
    match val {
        serde_json::Value::Null => NbtTag::End,
        serde_json::Value::Bool(b) => NbtTag::Byte(i8::from(*b)),
        serde_json::Value::Number(n) => n.as_i64().map_or_else(
            || n.as_f64().map_or(NbtTag::End, NbtTag::Double),
            |i| i32::try_from(i).map_or(NbtTag::Long(i), NbtTag::Int),
        ),
        serde_json::Value::String(s) => NbtTag::String(s.clone().into()),
        serde_json::Value::Array(arr) => NbtTag::List(arr.iter().map(json_to_nbt_tag).collect()),
        serde_json::Value::Object(map) => {
            let mut compound = NbtCompound::new();
            for (k, v) in map {
                compound.put(k, json_to_nbt_tag(v));
            }
            NbtTag::Compound(compound)
        }
    }
}

#[must_use]
pub fn nbt_tag_to_json(tag: &NbtTag) -> serde_json::Value {
    match tag {
        NbtTag::Byte(b) => serde_json::Value::Number((*b).into()),
        NbtTag::Short(s) => serde_json::Value::Number((*s).into()),
        NbtTag::Int(i) => serde_json::Value::Number((*i).into()),
        NbtTag::Long(l) => serde_json::Value::Number((*l).into()),
        NbtTag::Float(f) => serde_json::Number::from_f64(*f as f64)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        NbtTag::Double(d) => serde_json::Number::from_f64(*d)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        NbtTag::String(s) => serde_json::Value::String(s.to_string()),
        NbtTag::List(list) => serde_json::Value::Array(list.iter().map(nbt_tag_to_json).collect()),
        NbtTag::Compound(comp) => {
            let mut map = serde_json::Map::new();
            for (k, v) in &comp.child_tags {
                map.insert(k.to_string(), nbt_tag_to_json(v));
            }
            serde_json::Value::Object(map)
        }
        NbtTag::ByteArray(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|b| serde_json::Value::Number((*b).into()))
                .collect(),
        ),
        NbtTag::IntArray(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|i| serde_json::Value::Number((*i).into()))
                .collect(),
        ),
        NbtTag::LongArray(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|l| serde_json::Value::Number((*l).into()))
                .collect(),
        ),
        NbtTag::End => serde_json::Value::Null,
    }
}

/// Reads generation metadata without replacing malformed stored settings with defaults.
pub fn read_world_gen_settings_checked(
    level_folder: &Path,
) -> Result<Option<WorldGenSettings>, WorldInfoError> {
    let paths = [
        minecraft_data_dir(level_folder).join("world_gen_settings.dat"),
        level_folder.join("dimensions/minecraft/overworld/data/minecraft/world_gen_settings.dat"),
    ];
    for path in paths {
        if path.exists() {
            let root = read_gzip_compound_tag(File::open(&path)?).map_err(|e| {
                WorldInfoError::DeserializationError(format!("{}: {e}", path.display()))
            })?;
            let payload =
                world_gen_settings_payload(&root).ok_or(WorldInfoError::MissingWorldSeed)?;
            return world_gen_settings_from_nbt(payload).map(Some);
        }
    }
    let path = level_folder.join("level.dat");
    if !path.exists() {
        return Ok(None);
    }
    let root = read_gzip_compound_tag(File::open(&path)?)
        .map_err(|e| WorldInfoError::DeserializationError(format!("{}: {e}", path.display())))?;
    let settings = root
        .get_compound("Data")
        .and_then(|data| data.get_compound("WorldGenSettings"))
        .ok_or(WorldInfoError::MissingWorldSeed)?;
    world_gen_settings_from_nbt(settings).map(Some)
}

// Codec.LONG reads NumericTag via Number.longValue; encoding still uses LongTag.
pub(super) fn world_gen_settings_seed(settings: &NbtCompound) -> Option<i64> {
    match settings.get("seed")? {
        NbtTag::Byte(seed) => Some(i64::from(*seed)),
        NbtTag::Short(seed) => Some(i64::from(*seed)),
        NbtTag::Int(seed) => Some(i64::from(*seed)),
        NbtTag::Long(seed) => Some(*seed),
        NbtTag::Float(seed) => Some(*seed as i64),
        NbtTag::Double(seed) => Some(*seed as i64),
        _ => None,
    }
}

pub fn world_gen_settings_from_nbt(
    settings: &NbtCompound,
) -> Result<WorldGenSettings, WorldInfoError> {
    let seed = world_gen_settings_seed(settings).ok_or(WorldInfoError::MissingWorldSeed)?;
    let mut settings = settings.clone();
    settings.put_long("seed", seed);
    serde_json::from_value(nbt_tag_to_json(&NbtTag::Compound(settings)))
        .map_err(|e| WorldInfoError::DeserializationError(e.to_string()))
}

#[must_use]
pub fn read_world_gen_settings(level_folder: &Path) -> Option<WorldGenSettings> {
    match read_world_gen_settings_checked(level_folder) {
        Ok(settings) => settings,
        Err(error) => {
            warn!("Failed to read world generation settings: {error}");
            None
        }
    }
}

/// Selects explicit dimension metadata before the root world's generation settings.
pub fn read_dimension_world_gen_settings(
    root_folder: &Path,
    dim_folder: &Path,
    dimension: &str,
) -> Result<Option<WorldGenSettings>, WorldInfoError> {
    let (settings, dimension_override) = if dim_folder == root_folder {
        (read_world_gen_settings_checked(root_folder)?, false)
    } else {
        match read_world_gen_settings_checked(dim_folder)? {
            Some(settings) => (Some(settings), true),
            None => (read_world_gen_settings_checked(root_folder)?, false),
        }
    };
    if let Some(settings) = &settings {
        if (dimension_override || !settings.dimensions.is_empty())
            && !settings.dimensions.contains_key(dimension)
        {
            return Err(WorldInfoError::DeserializationError(format!(
                "World generation settings have no generator for {dimension}"
            )));
        }
        if let Some(dim) = settings.dimensions.get(dimension) {
            if dim.dimension_type != dimension {
                return Err(WorldInfoError::DeserializationError(format!(
                    "Unsupported stored dimension type {} for {dimension}",
                    dim.dimension_type
                )));
            }
            match dim.generator.generator_type.as_str() {
                "minecraft:flat"
                    if dim
                        .generator
                        .settings
                        .as_ref()
                        .and_then(crate::world_info::GeneratorSettings::as_flat_settings)
                        .is_some() =>
                {
                    let flat = dim
                        .generator
                        .settings
                        .as_ref()
                        .and_then(crate::world_info::GeneratorSettings::as_flat_settings)
                        .ok_or_else(|| {
                            WorldInfoError::DeserializationError(
                                "Invalid flat generator settings".to_string(),
                            )
                        })?;
                    validate_flat_generator(&flat, dimension, settings.generate_structures)?;
                }
                "minecraft:noise"
                    if matches!(&dim.generator.settings,
                    Some(crate::world_info::GeneratorSettings::Reference(name))
                    if pumpkin_data::noise_settings::NoiseSettings::from_name(name).is_some()) => {}
                _ => {
                    return Err(WorldInfoError::DeserializationError(format!(
                        "Unsupported stored generator settings for {dimension}"
                    )));
                }
            }
            if dim.generator.generator_type == "minecraft:noise" {
                validate_noise_biome_source(dim.generator.biome_source.as_ref(), dimension)?;
            }
        }
    }
    Ok(settings)
}

fn validate_noise_biome_source(
    source: Option<&crate::world_info::BiomeSource>,
    dimension: &str,
) -> Result<(), WorldInfoError> {
    let supported_biome_source = match source {
        Some(crate::world_info::BiomeSource::WithPreset { preset, biome_type }) => {
            biome_type == "minecraft:multi_noise"
                && matches!(preset.as_str(), "minecraft:overworld" | "minecraft:nether")
        }
        Some(crate::world_info::BiomeSource::Fixed { biome, biome_type }) => {
            biome_type == "minecraft:fixed"
                && pumpkin_data::biome::Biome::from_name(
                    biome.strip_prefix("minecraft:").unwrap_or(biome),
                )
                .is_some()
        }
        Some(crate::world_info::BiomeSource::Simple { biome_type }) => {
            biome_type == "minecraft:the_end"
        }
        None => false,
    };
    if !supported_biome_source {
        return Err(WorldInfoError::DeserializationError(format!(
            "Unsupported stored biome source for {dimension}"
        )));
    }
    Ok(())
}

fn validate_flat_generator(
    flat: &crate::world_info::FlatPresetSettings,
    dimension: &str,
    generate_structures: bool,
) -> Result<(), WorldInfoError> {
    let dimension_height = match dimension {
        "minecraft:overworld" => pumpkin_data::dimension::Dimension::OVERWORLD.height,
        "minecraft:the_nether" => pumpkin_data::dimension::Dimension::THE_NETHER.height,
        "minecraft:the_end" => pumpkin_data::dimension::Dimension::THE_END.height,
        _ => {
            return Err(WorldInfoError::DeserializationError(
                "Unsupported flat dimension".to_string(),
            ));
        }
    };
    let invalid_biome = pumpkin_data::biome::Biome::from_name(
        flat.biome.strip_prefix("minecraft:").unwrap_or(&flat.biome),
    )
    .is_none();
    let invalid_layer = flat
        .layers
        .iter()
        .any(|layer| layer.height < 0 || pumpkin_data::Block::from_name(&layer.block).is_none());
    let height: i64 = flat
        .layers
        .iter()
        .map(|layer| i64::from(layer.height))
        .sum();
    let unsupported_structures = generate_structures
        && flat
            .structure_overrides_vec()
            .is_some_and(|structures| !structures.is_empty());
    if invalid_biome
        || invalid_layer
        || height > i64::from(dimension_height)
        || flat.features
        || flat.lakes
        || unsupported_structures
    {
        return Err(WorldInfoError::DeserializationError(format!(
            "Unsupported stored flat generator settings for {dimension}"
        )));
    }
    Ok(())
}

fn world_gen_settings_payload(mut compound: &NbtCompound) -> Option<&NbtCompound> {
    loop {
        if world_gen_settings_seed(compound).is_some() {
            return Some(compound);
        }

        compound = compound
            .get_compound("data")
            .or_else(|| compound.get_compound("Data"))?;
    }
}

pub fn write_world_gen_settings(
    level_folder: &Path,
    settings: &WorldGenSettings,
    data_version: i32,
) -> Result<(), WorldInfoError> {
    let dir = ensure_minecraft_data_dir(level_folder)?;
    let path = dir.join("world_gen_settings.dat");
    let mut root = if path.exists() {
        read_gzip_compound_tag(File::open(&path)?)
            .map_err(|e| WorldInfoError::DeserializationError(e.to_string()))?
    } else {
        NbtCompound::new()
    };
    let mut inner = if path.exists() {
        world_gen_settings_payload(&root)
            .ok_or(WorldInfoError::MissingWorldSeed)?
            .clone()
    } else {
        let legacy_path = level_folder.join("level.dat");
        if legacy_path.exists() {
            let legacy = read_gzip_compound_tag(File::open(legacy_path)?)
                .map_err(|e| WorldInfoError::DeserializationError(e.to_string()))?;
            legacy
                .get_compound("Data")
                .and_then(|data| data.get_compound("WorldGenSettings"))
                .cloned()
                .unwrap_or_default()
        } else {
            NbtCompound::new()
        }
    };
    inner = update_world_gen_settings_nbt(settings, inner);
    inner.put_int("DataVersion", data_version);

    root.put_compound("data", inner);
    let path_new = path.with_extension("dat_new");
    pumpkin_nbt::nbt_compress::write_gzip_compound_tag(
        root,
        BufWriter::new(File::create(&path_new)?),
    )
    .map_err(|e| WorldInfoError::SerializationError(e.to_string()))?;
    fs::rename(path_new, path)?;
    Ok(())
}

/// Updates modeled generation fields while retaining unmodeled NBT tags.
#[must_use]
pub fn update_world_gen_settings_nbt(
    settings: &WorldGenSettings,
    mut inner: NbtCompound,
) -> NbtCompound {
    inner.put_long("seed", settings.seed);
    inner.child_tags.remove("generate_features");
    inner.put_bool("generate_structures", settings.generate_structures);
    inner.put_bool("bonus_chest", settings.bonus_chest);

    let old_dimensions = inner
        .get_compound("dimensions")
        .cloned()
        .unwrap_or_default();
    let mut dims_comp = NbtCompound::new();
    for (dim_name, dim) in &settings.dimensions {
        let mut dim_comp = old_dimensions
            .get_compound(dim_name)
            .cloned()
            .unwrap_or_default();
        dim_comp.put_string("type", dim.dimension_type.clone());

        let mut gen_comp = dim_comp
            .get_compound("generator")
            .cloned()
            .unwrap_or_default();
        gen_comp.put_string("type", dim.generator.generator_type.clone());
        if let Some(s) = &dim.generator.settings {
            match s {
                crate::world_info::GeneratorSettings::Reference(r) => {
                    gen_comp.put_string("settings", r.clone());
                }
                crate::world_info::GeneratorSettings::Compound(json_val) => {
                    if gen_comp
                        .get("settings")
                        .is_none_or(|stored| nbt_tag_to_json(stored) != *json_val)
                    {
                        gen_comp.put("settings", json_to_nbt_tag(json_val));
                    }
                }
            }
        } else {
            gen_comp.child_tags.remove("settings");
        }
        if let Some(bs) = &dim.generator.biome_source {
            let mut bs_comp = gen_comp
                .get_compound("biome_source")
                .cloned()
                .unwrap_or_default();
            bs_comp.child_tags.remove("preset");
            bs_comp.child_tags.remove("biome");
            match bs {
                crate::world_info::BiomeSource::WithPreset { preset, biome_type } => {
                    bs_comp.put_string("preset", preset.clone());
                    bs_comp.put_string("type", biome_type.clone());
                }
                crate::world_info::BiomeSource::Fixed { biome, biome_type } => {
                    bs_comp.put_string("biome", biome.clone());
                    bs_comp.put_string("type", biome_type.clone());
                }
                crate::world_info::BiomeSource::Simple { biome_type } => {
                    bs_comp.put_string("type", biome_type.clone());
                }
            }
            gen_comp.put_compound("biome_source", bs_comp);
        } else {
            gen_comp.child_tags.remove("biome_source");
        }
        dim_comp.put_compound("generator", gen_comp);
        dims_comp.put_compound(dim_name, dim_comp);
    }
    inner.put_compound("dimensions", dims_comp);

    inner
}

#[must_use]
pub fn game_rules_to_nbt(rules: &GameRuleRegistry, data_version: i32) -> NbtCompound {
    let mut inner = NbtCompound::new();
    for rule in GameRule::all() {
        let key = format!("minecraft:{rule}");
        match rules.get(rule) {
            GameRuleValue::Bool(b) => inner.put(&key, NbtTag::Byte(i8::from(*b))),
            GameRuleValue::Int(i) => inner.put(&key, NbtTag::Int(*i as i32)),
        }
    }
    inner.put_int("DataVersion", data_version);

    let mut root = NbtCompound::new();
    root.put_compound("data", inner);
    root
}

pub fn game_rules_from_nbt(root: &NbtCompound) -> GameRuleRegistry {
    let mut registry = GameRuleRegistry::default();

    let Some(inner) = root.get_compound("data") else {
        warn!("game_rules.dat missing 'data' compound, using defaults");
        return registry;
    };

    for rule in GameRule::all() {
        let key = format!("minecraft:{rule}");
        match registry.get_mut(rule) {
            GameRuleValue::Bool(b) => {
                if let Some(v) = inner.get_byte(&key) {
                    *b = v != 0;
                }
            }
            GameRuleValue::Int(i) => {
                if let Some(v) = inner.get_int(&key) {
                    *i = i64::from(v);
                }
            }
        }
    }

    registry
}

pub fn read_game_rules(level_folder: &Path) -> GameRuleRegistry {
    let path = minecraft_data_dir(level_folder).join("game_rules.dat");
    if !path.exists() {
        return GameRuleRegistry::default();
    }

    match File::open(&path) {
        Ok(f) => match read_gzip_compound_tag(f) {
            Ok(compound) => game_rules_from_nbt(&compound),
            Err(e) => {
                warn!("Failed to parse game_rules.dat: {e}");
                GameRuleRegistry::default()
            }
        },
        Err(e) => {
            warn!("Failed to open game_rules.dat: {e}");
            GameRuleRegistry::default()
        }
    }
}

pub fn write_game_rules(
    level_folder: &Path,
    rules: &GameRuleRegistry,
    data_version: i32,
) -> Result<(), WorldInfoError> {
    let dir = ensure_minecraft_data_dir(level_folder)?;
    let path = dir.join("game_rules.dat");

    let compound = game_rules_to_nbt(rules, data_version);
    let file = File::create(&path)?;

    pumpkin_nbt::nbt_compress::write_gzip_compound_tag(compound, file)
        .map_err(|e| WorldInfoError::SerializationError(e.to_string()))
}

pub fn read_world_clocks(level_folder: &Path) -> WorldClocksData {
    let path = minecraft_data_dir(level_folder).join("world_clocks.dat");
    if !path.exists() {
        return WorldClocksData::default();
    }

    match File::open(&path) {
        Ok(f) => match read_gzip_compound_tag(f) {
            Ok(compound) => world_clocks_from_nbt(&compound),
            Err(e) => {
                warn!("Failed to parse world_clocks.dat: {e}");
                WorldClocksData::default()
            }
        },
        Err(e) => {
            warn!("Failed to open world_clocks.dat: {e}");
            WorldClocksData::default()
        }
    }
}

fn world_clocks_from_nbt(root: &NbtCompound) -> WorldClocksData {
    let mut result = WorldClocksData::default();

    let Some(inner) = root.get_compound("data") else {
        return result;
    };

    result.data_version = inner.get_int("DataVersion").unwrap_or(0);

    for (key, tag) in &inner.child_tags {
        if key.as_ref() == "DataVersion" {
            continue;
        }
        if let NbtTag::Compound(dim_compound) = tag {
            let total_ticks = dim_compound.get_long("total_ticks").unwrap_or(0);
            result
                .clocks
                .insert(key.to_string(), DimensionClock { total_ticks });
        }
    }

    result
}

pub fn write_world_clocks(
    level_folder: &Path,
    clocks: &WorldClocksData,
) -> Result<(), WorldInfoError> {
    let dir = ensure_minecraft_data_dir(level_folder)?;
    let path = dir.join("world_clocks.dat");

    let mut inner = NbtCompound::new();
    for (dim_name, clock) in &clocks.clocks {
        let mut dim_compound = NbtCompound::new();
        dim_compound.put_long("total_ticks", clock.total_ticks);
        inner.put_compound(dim_name, dim_compound);
    }
    inner.put_int("DataVersion", clocks.data_version);

    let mut root = NbtCompound::new();
    root.put_compound("data", inner);

    let file = File::create(&path)?;

    pumpkin_nbt::nbt_compress::write_gzip_compound_tag(root, file)
        .map_err(|e| WorldInfoError::SerializationError(e.to_string()))
}

pub fn read_wandering_trader(level_folder: &Path) -> WanderingTraderData {
    let path = minecraft_data_dir(level_folder).join("wandering_trader.dat");
    if !path.exists() {
        return WanderingTraderData::default();
    }
    match File::open(&path) {
        Ok(f) => match read_gzip_compound_tag(f) {
            Ok(compound) => {
                let data_compound = compound.get_compound("data");
                let c = data_compound.as_ref().map_or(&compound, |v| v);
                let data_version = compound
                    .get_int("DataVersion")
                    .or_else(|| c.get_int("DataVersion"))
                    .unwrap_or(0);
                WanderingTraderData {
                    spawn_delay: c
                        .get_int("spawn_delay")
                        .or_else(|| c.get_int("WanderingTraderSpawnDelay"))
                        .unwrap_or(24_000),
                    spawn_chance: c
                        .get_int("spawn_chance")
                        .or_else(|| c.get_int("WanderingTraderSpawnChance"))
                        .unwrap_or(25),
                    data_version,
                }
            }
            Err(e) => {
                warn!("Failed to deserialize wandering_trader.dat, using defaults: {e}");
                WanderingTraderData::default()
            }
        },
        Err(e) => {
            warn!("Failed to open wandering_trader.dat, using defaults: {e}");
            WanderingTraderData::default()
        }
    }
}

pub fn write_wandering_trader(
    level_folder: &Path,
    data: &WanderingTraderData,
) -> Result<(), WorldInfoError> {
    let dir = ensure_minecraft_data_dir(level_folder)?;
    let path = dir.join("wandering_trader.dat");
    let file = File::create(&path)?;
    let mut data_comp = NbtCompound::new();
    data_comp.put_int("spawn_delay", data.spawn_delay);
    data_comp.put_int("spawn_chance", data.spawn_chance);
    let mut root = NbtCompound::new();
    root.put_int("DataVersion", data.data_version);
    root.put_compound("data", data_comp);
    pumpkin_nbt::nbt_compress::write_gzip_compound_tag(root, BufWriter::new(file))
        .map_err(|e| WorldInfoError::SerializationError(e.to_string()))
}

pub fn write_custom_boss_events_stub(
    level_folder: &Path,
    data_version: i32,
) -> Result<(), WorldInfoError> {
    let dir = ensure_minecraft_data_dir(level_folder)?;
    let path = dir.join("custom_boss_events.dat");
    if path.exists() {
        return Ok(());
    }

    let mut root = NbtCompound::new();
    root.put_int("DataVersion", data_version);
    root.put_compound("data", NbtCompound::new());

    let file = File::create(&path)?;
    pumpkin_nbt::nbt_compress::write_gzip_compound_tag(root, file)
        .map_err(|e| WorldInfoError::SerializationError(e.to_string()))
}

pub fn write_scheduled_events_stub(
    level_folder: &Path,
    data_version: i32,
) -> Result<(), WorldInfoError> {
    let dir = ensure_minecraft_data_dir(level_folder)?;
    let path = dir.join("scheduled_events.dat");
    if path.exists() {
        return Ok(());
    }

    let mut inner = NbtCompound::new();
    inner.put("events", NbtTag::List(vec![]));
    let mut root = NbtCompound::new();
    root.put_int("DataVersion", data_version);
    root.put_compound("data", inner);

    let file = File::create(&path)?;
    pumpkin_nbt::nbt_compress::write_gzip_compound_tag(root, file)
        .map_err(|e| WorldInfoError::SerializationError(e.to_string()))
}

pub fn write_random_sequences_stub(
    level_folder: &Path,
    data_version: i32,
) -> Result<(), WorldInfoError> {
    let dir = ensure_minecraft_data_dir(level_folder)?;
    let path = dir.join("random_sequences.dat");
    if path.exists() {
        return Ok(());
    }

    let mut inner = NbtCompound::new();
    inner.put_int("salt", 0);
    inner.put_compound("sequences", NbtCompound::new());
    let mut root = NbtCompound::new();
    root.put_int("DataVersion", data_version);
    root.put_compound("data", inner);

    let file = File::create(&path)?;
    pumpkin_nbt::nbt_compress::write_gzip_compound_tag(root, file)
        .map_err(|e| WorldInfoError::SerializationError(e.to_string()))
}

pub fn write_scoreboard_stub(level_folder: &Path, data_version: i32) -> Result<(), WorldInfoError> {
    let dir = ensure_minecraft_data_dir(level_folder)?;
    let path = dir.join("scoreboard.dat");
    if path.exists() {
        return Ok(());
    }

    let mut root = NbtCompound::new();
    root.put_int("DataVersion", data_version);
    root.put_compound("data", NbtCompound::new());

    let file = File::create(&path)?;
    pumpkin_nbt::nbt_compress::write_gzip_compound_tag(root, file)
        .map_err(|e| WorldInfoError::SerializationError(e.to_string()))
}

pub fn write_stopwatches_stub(
    level_folder: &Path,
    data_version: i32,
) -> Result<(), WorldInfoError> {
    let dir = ensure_minecraft_data_dir(level_folder)?;
    let path = dir.join("stopwatches.dat");
    if path.exists() {
        return Ok(());
    }

    let mut inner = NbtCompound::new();
    inner.put_compound("stopwatches", NbtCompound::new());
    let mut root = NbtCompound::new();
    root.put_int("DataVersion", data_version);
    root.put_compound("data", inner);

    let file = File::create(&path)?;
    pumpkin_nbt::nbt_compress::write_gzip_compound_tag(root, file)
        .map_err(|e| WorldInfoError::SerializationError(e.to_string()))
}

#[cfg(test)]
mod generation_metadata_tests {
    use super::*;
    use pumpkin_util::world_seed::Seed;
    use tempfile::TempDir;

    #[test]
    fn generation_metadata_rewrite_keeps_opaque_configuration() {
        let directory = TempDir::new().unwrap();
        let path = minecraft_data_dir(directory.path()).join("world_gen_settings.dat");
        let mut settings = WorldGenSettings::new(Seed(42));
        write_world_gen_settings(directory.path(), &settings, 4903).unwrap();
        let mut root = read_gzip_compound_tag(File::open(&path).unwrap()).unwrap();
        root.put_string("unmodeled_root", "retained".to_string());
        let mut data = root.get_compound("data").unwrap().clone();
        data.put_bool("generate_structures", false);
        data.put_bool("bonus_chest", true);
        let mut dimensions = data.get_compound("dimensions").unwrap().clone();
        let mut end = dimensions
            .get_compound("minecraft:the_end")
            .unwrap()
            .clone();
        let mut generator = end.get_compound("generator").unwrap().clone();
        generator.put_long("unmodeled_generator", 99);
        end.put_compound("generator", generator);
        dimensions.put_compound("minecraft:the_end", end);
        data.put_compound("dimensions", dimensions);
        root.put_compound("data", data);
        pumpkin_nbt::nbt_compress::write_gzip_compound_tag(root, File::create(&path).unwrap())
            .unwrap();

        settings = read_world_gen_settings_checked(directory.path())
            .unwrap()
            .unwrap();
        settings.seed = 84;
        write_world_gen_settings(directory.path(), &settings, 4903).unwrap();
        let stored = read_gzip_compound_tag(File::open(&path).unwrap()).unwrap();
        assert_eq!(stored.get_string("unmodeled_root"), Some("retained"));
        let data = stored.get_compound("data").unwrap();
        assert_eq!(data.get_bool("generate_structures"), Some(false));
        assert_eq!(data.get_bool("bonus_chest"), Some(true));
        assert_eq!(
            data.get_compound("dimensions")
                .unwrap()
                .get_compound("minecraft:the_end")
                .unwrap()
                .get_compound("generator")
                .unwrap()
                .get_long("unmodeled_generator"),
            Some(99)
        );
        assert_eq!(
            read_world_gen_settings_checked(directory.path())
                .unwrap()
                .unwrap()
                .seed,
            84
        );
    }

    #[test]
    fn damaged_dimension_override_cannot_fall_back_to_root_seed() {
        let directory = TempDir::new().unwrap();
        write_world_gen_settings(directory.path(), &WorldGenSettings::new(Seed(42)), 4903).unwrap();
        let end = directory.path().join("dimensions/minecraft/the_end");
        let data = ensure_minecraft_data_dir(&end).unwrap();
        fs::write(
            data.join("world_gen_settings.dat"),
            b"damaged retained metadata",
        )
        .unwrap();
        assert!(
            read_dimension_world_gen_settings(directory.path(), &end, "minecraft:the_end").is_err()
        );
    }
    #[test]
    fn flat_metadata_cannot_substitute_unknown_ids_or_ignore_unsupported_options() {
        let directory = TempDir::new().unwrap();
        let end = directory.path().join("dimensions/minecraft/the_end");
        let base = serde_json::json!({"biome": "minecraft:the_end", "layers": [{"block": "minecraft:end_stone", "height": 7}]});
        let mut variants = Vec::new();
        let mut unknown_biome = base.clone();
        unknown_biome["biome"] = "test:missing_biome".into();
        variants.push(unknown_biome);
        let mut unknown_block = base.clone();
        unknown_block["layers"][0]["block"] = "test:missing_block".into();
        variants.push(unknown_block);
        let mut features = base.clone();
        features["features"] = true.into();
        variants.push(features);
        let mut lakes = base.clone();
        lakes["lakes"] = true.into();
        variants.push(lakes);
        let mut structures = base.clone();
        structures["structure_overrides"] = serde_json::json!(["minecraft:villages"]);
        variants.push(structures);
        let mut negative = base.clone();
        negative["layers"][0]["height"] = (-1).into();
        variants.push(negative);
        let mut too_tall = base;
        too_tall["layers"][0]["height"] = 9999.into();
        variants.push(too_tall);
        for flat in variants {
            let mut settings = WorldGenSettings::new(Seed(42));
            let generator = &mut settings
                .dimensions
                .get_mut("minecraft:the_end")
                .unwrap()
                .generator;
            generator.generator_type = "minecraft:flat".to_string();
            generator.settings = Some(crate::world_info::GeneratorSettings::Compound(flat));
            write_world_gen_settings(&end, &settings, 4903).unwrap();
            let path = minecraft_data_dir(&end).join("world_gen_settings.dat");
            let before = fs::read(&path).unwrap();
            assert!(
                read_dimension_world_gen_settings(directory.path(), &end, "minecraft:the_end")
                    .is_err()
            );
            assert_eq!(fs::read(path).unwrap(), before);
        }
    }

    #[test]
    fn numeric_seed_loading_matches_vanilla_long_codec_and_writes_long_tags() {
        let directory = TempDir::new().unwrap();
        let path = ensure_minecraft_data_dir(directory.path())
            .unwrap()
            .join("world_gen_settings.dat");
        // Expected results from official 26.3 Codec.LONG.parse(NbtOps.INSTANCE, tag).
        for (tag, expected) in [
            (NbtTag::Byte(123), 123),
            (NbtTag::Short(123), 123),
            (NbtTag::Int(123), 123),
            (NbtTag::Long(123), 123),
            (NbtTag::Float(123.75), 123),
            (NbtTag::Double(-123.75), -123),
            (NbtTag::Double(f64::NAN), 0),
            (NbtTag::Double(f64::INFINITY), i64::MAX),
        ] {
            let settings = WorldGenSettings::new(Seed(0));
            let mut payload = update_world_gen_settings_nbt(&settings, NbtCompound::new());
            payload.put("seed", tag);
            let mut inner = NbtCompound::new();
            inner.put_compound("data", payload);
            let mut root = NbtCompound::new();
            root.put_compound("Data", inner);
            pumpkin_nbt::nbt_compress::write_gzip_compound_tag(root, File::create(&path).unwrap())
                .unwrap();
            let loaded = read_world_gen_settings_checked(directory.path())
                .unwrap()
                .unwrap();
            assert_eq!(loaded.seed, expected);
            write_world_gen_settings(
                directory.path(),
                &loaded,
                crate::world_info::MAXIMUM_SUPPORTED_WORLD_DATA_VERSION,
            )
            .unwrap();
            let written = read_gzip_compound_tag(File::open(&path).unwrap()).unwrap();
            assert_eq!(
                written.get_compound("data").unwrap().get("seed"),
                Some(&NbtTag::Long(expected))
            );
        }
        let mut invalid = NbtCompound::new();
        invalid.put_string("seed", "123".to_string());
        assert!(matches!(
            world_gen_settings_from_nbt(&invalid),
            Err(WorldInfoError::MissingWorldSeed)
        ));
    }
}
