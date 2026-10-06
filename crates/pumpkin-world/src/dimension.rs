use std::{path::PathBuf, sync::Arc};

use pumpkin_config::world::LevelConfig;
use pumpkin_data::dimension::Dimension;

use crate::level::Level;

pub fn into_level(
    dimension: Dimension,
    level_config: &LevelConfig,
    base_directory: PathBuf,
    seed: i64,
) -> Result<Arc<Level>, crate::world_info::WorldInfoError> {
    Level::from_root_folder(level_config, base_directory, seed, dimension)
}
