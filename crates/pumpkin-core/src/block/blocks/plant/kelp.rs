use std::sync::Arc;

use pumpkin_data::block_properties::KelpLikeProperties;
use pumpkin_data::fluid::Fluid;
use pumpkin_data::tag::{self, Taggable};
use pumpkin_data::{Block, BlockDirection, BlockId, BlockStateId};
use pumpkin_util::math::position::BlockPos;
use pumpkin_world::tick::TickPriority;
use pumpkin_world::world::{BlockAccessor, BlockFlags};
use rand::RngExt;

use crate::block::{
    BlockBehaviour, BlockMetadata, BonemealArgs, CanPlaceAtArgs, GetStateForNeighborUpdateArgs,
    OnPlaceArgs, OnScheduledTickArgs, RandomTickArgs,
};
use crate::plugin::api::events::block::block_grow::BlockGrowEvent;
use crate::world::World;

const GROW_PER_TICK_PROBABILITY: f64 = 0.14;
const MAX_AGE: u8 = 25;

pub struct KelpBlock;

impl BlockMetadata for KelpBlock {
    fn ids() -> Box<[BlockId]> {
        [BlockId::KELP, BlockId::KELP_PLANT].into()
    }
}

impl KelpBlock {
    fn can_survive(block_accessor: &dyn BlockAccessor, position: &BlockPos) -> bool {
        let (support, state) = block_accessor.get_block_and_state(&position.down());
        !support.has_tag(&tag::Block::MINECRAFT_CANNOT_SUPPORT_KELP)
            && (support == &Block::KELP
                || support == &Block::KELP_PLANT
                || state.is_side_solid(BlockDirection::Up))
    }

    fn can_place_in_water(world: &World, position: &BlockPos) -> bool {
        let (fluid, state) = world.get_fluid_and_fluid_state(position);
        // FluidState#isFull checks the amount, including full falling water.
        fluid.has_tag(&tag::Fluid::MINECRAFT_WATER) && state.level == 8
    }

    fn get_state_for_placement(
        block_accessor: &dyn BlockAccessor,
        position: &BlockPos,
        random: &mut impl RngExt,
    ) -> BlockStateId {
        if matches!(
            block_accessor.get_block(&position.up()).id,
            BlockId::KELP | BlockId::KELP_PLANT
        ) {
            Block::KELP_PLANT.default_state.id
        } else {
            KelpLikeProperties {
                age: random.random_range(0..MAX_AGE),
            }
            .to_state_id(&Block::KELP)
        }
    }

    fn get_head_pos(block_accessor: &dyn BlockAccessor, position: &BlockPos) -> Option<BlockPos> {
        let mut position = *position;
        while block_accessor.get_block(&position) == &Block::KELP_PLANT {
            position = position.up();
        }
        (block_accessor.get_block(&position) == &Block::KELP).then_some(position)
    }

    fn can_grow_into(world: &World, position: &BlockPos) -> bool {
        world.is_in_height_limit(position.0.y) && world.get_block(position) == &Block::WATER
    }

    fn random_tick(world: &Arc<World>, position: &BlockPos, random: &mut impl RngExt) {
        let (block, state_id) = world.get_block_and_state_id(position);
        if block != &Block::KELP {
            return;
        }
        let age = KelpLikeProperties::from_state_id(state_id).age;
        if age >= MAX_AGE || random.random::<f64>() >= GROW_PER_TICK_PROBABILITY {
            return;
        }

        let growth_pos = position.up();
        if !Self::can_grow_into(world, &growth_pos) {
            return;
        }
        let mut new_state_id = KelpLikeProperties { age: age + 1 }.to_state_id(&Block::KELP);
        if let Some(server) = world.server.upgrade() {
            let (old_block, old_state_id) = world.get_block_and_state_id(&growth_pos);
            let mut event = BlockGrowEvent::new(
                world.clone(),
                old_block,
                old_state_id,
                &Block::KELP,
                new_state_id,
                growth_pos,
            );
            server.plugin_manager.fire_blocking(&server, &mut event);
            if event.cancelled {
                return;
            }
            new_state_id = event.new_state_id;
        }
        world.set_block_state(&growth_pos, new_state_id, BlockFlags::NOTIFY_ALL);
    }
}

impl BlockBehaviour for KelpBlock {
    fn can_place_at(&self, args: CanPlaceAtArgs<'_>) -> bool {
        Self::can_survive(args.block_accessor, args.position)
            && (args.use_item_on.is_none()
                || args
                    .world
                    .is_some_and(|world| Self::can_place_in_water(world, args.position)))
    }

    fn on_place(&self, args: OnPlaceArgs<'_>) -> BlockStateId {
        Self::get_state_for_placement(args.world, args.position, &mut rand::rng())
    }

    fn get_state_for_neighbor_update(
        &self,
        args: GetStateForNeighborUpdateArgs<'_>,
    ) -> BlockStateId {
        let can_survive = Self::can_survive(args.world, args.position);
        if args.direction == BlockDirection::Down && !can_survive {
            args.world
                .schedule_block_tick(args.block, *args.position, 1, TickPriority::Normal);
        }

        let neighbor_is_kelp = matches!(
            args.neighbor_state_id.to_block_id(),
            BlockId::KELP | BlockId::KELP_PLANT
        );
        if args.block == &Block::KELP {
            if (args.direction == BlockDirection::Up && neighbor_is_kelp)
                || (args.direction == BlockDirection::Down
                    && can_survive
                    && matches!(
                        args.world.get_block(&args.position.up()).id,
                        BlockId::KELP | BlockId::KELP_PLANT
                    ))
            {
                return Block::KELP_PLANT.default_state.id;
            }
        } else if args.direction == BlockDirection::Up && !neighbor_is_kelp {
            // Harvesting exposes a new head with a fresh age, rather than age zero.
            return KelpLikeProperties {
                age: rand::rng().random_range(0..MAX_AGE),
            }
            .to_state_id(&Block::KELP);
        }

        args.world.schedule_fluid_tick(
            &Fluid::WATER,
            *args.position,
            Fluid::WATER.flow_speed as u8,
            TickPriority::Normal,
        );
        args.state_id
    }

    fn on_scheduled_tick(&self, args: OnScheduledTickArgs<'_>) {
        if !Self::can_survive(args.world.as_ref(), args.position) {
            args.world
                .break_block(args.position, None, BlockFlags::NOTIFY_ALL);
        }
    }

    fn random_tick(&self, args: RandomTickArgs<'_>) {
        Self::random_tick(args.world, args.position, &mut rand::rng());
    }

    fn is_valid_bonemeal_target(&self, args: BonemealArgs<'_>) -> bool {
        Self::get_head_pos(args.world.as_ref(), args.position)
            .is_some_and(|position| Self::can_grow_into(args.world, &position.up()))
    }

    fn perform_bonemeal(&self, args: BonemealArgs<'_>) {
        let Some(head_pos) = Self::get_head_pos(args.world.as_ref(), args.position) else {
            return;
        };
        let growth_pos = head_pos.up();
        if !Self::can_grow_into(args.world, &growth_pos) {
            return;
        }
        let age = KelpLikeProperties::from_state_id(args.world.get_block_state_id(&head_pos)).age;
        args.world.set_block_state(
            &growth_pos,
            KelpLikeProperties {
                age: (age + 1).min(MAX_AGE),
            }
            .to_state_id(&Block::KELP),
            BlockFlags::NOTIFY_ALL,
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Weak;

    use arc_swap::ArcSwap;
    use pumpkin_config::world::LevelConfig;
    use pumpkin_data::block_properties::{OakTrapdoorLikeProperties, WaterLikeProperties};
    use pumpkin_data::dimension::Dimension;
    use pumpkin_data::entity::EntityType;
    use pumpkin_util::math::vector2::Vector2;
    use pumpkin_util::world_seed::Seed;
    use pumpkin_world::{chunk::ChunkData, level::Level, world_info::LevelData};
    use rand::{SeedableRng, rngs::StdRng};

    use super::*;
    use crate::block::registry::BlockRegistry;

    fn test_world(path: &std::path::Path) -> Arc<World> {
        let dimension = Dimension::OVERWORLD;
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            path.to_path_buf(),
            0,
            dimension.clone(),
        )
        .unwrap();
        level
            .loaded_chunks
            .insert(Vector2::new(0, 0), ChunkData::empty_sync(0, 0));
        let mut registry = BlockRegistry::default();
        registry.register(KelpBlock);
        Arc::new(
            World::load(
                level,
                Arc::new(ArcSwap::from_pointee(LevelData::default(Seed(0)))),
                dimension,
                Arc::new(registry),
                Weak::new(),
            )
            .unwrap(),
        )
    }

    fn set_fixture(world: &Arc<World>, position: &BlockPos, state: BlockStateId) {
        world.set_block_state(
            position,
            state,
            BlockFlags::UPDATE_KNOWN_SHAPE | BlockFlags::SKIP_BLOCK_ADDED_CALLBACK,
        );
    }

    #[tokio::test]
    async fn random_growth_converts_head_and_stops_at_max_age() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let position = BlockPos::new(4, 64, 4);
        let mut random = StdRng::seed_from_u64(0);
        set_fixture(&world, &position.down(), Block::STONE.default_state.id);
        set_fixture(
            &world,
            &position,
            KelpLikeProperties { age: 24 }.to_state_id(&Block::KELP),
        );
        set_fixture(
            &world,
            &position.up(),
            WaterLikeProperties { level: 7 }.to_state_id(&Block::WATER),
        );
        set_fixture(
            &world,
            &position.up_height(2),
            Block::WATER.default_state.id,
        );

        for _ in 0..100 {
            KelpBlock::random_tick(&world, &position, &mut random);
        }
        assert_eq!(world.get_block(&position), &Block::KELP_PLANT);
        assert_eq!(world.get_block(&position.up()), &Block::KELP);
        assert_eq!(
            KelpLikeProperties::from_state_id(world.get_block_state_id(&position.up())).age,
            25
        );
        for _ in 0..100 {
            KelpBlock::random_tick(&world, &position.up(), &mut random);
        }
        assert_eq!(world.get_block(&position.up_height(2)), &Block::WATER);
        world.shutdown().await;
    }

    #[tokio::test]
    async fn bonemeal_follows_body_to_mature_head_and_respects_obstructions() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let position = BlockPos::new(4, 64, 4);
        set_fixture(&world, &position.down(), Block::STONE.default_state.id);
        set_fixture(&world, &position, Block::KELP_PLANT.default_state.id);
        set_fixture(
            &world,
            &position.up(),
            KelpLikeProperties { age: 25 }.to_state_id(&Block::KELP),
        );
        set_fixture(
            &world,
            &position.up_height(2),
            Block::WATER.default_state.id,
        );
        assert!(world.block_registry.bone_meal(
            &Block::KELP_PLANT,
            &world,
            &position,
            Block::KELP_PLANT.default_state.id,
        ));
        assert_eq!(world.get_block(&position.up()), &Block::KELP_PLANT);
        assert_eq!(world.get_block(&position.up_height(2)), &Block::KELP);
        assert_eq!(
            KelpLikeProperties::from_state_id(world.get_block_state_id(&position.up_height(2))).age,
            25
        );

        let mut trapdoor = OakTrapdoorLikeProperties::default(&Block::OAK_TRAPDOOR);
        trapdoor.waterlogged = true;
        for obstruction in [
            Block::AIR.default_state.id,
            Block::STONE.default_state.id,
            trapdoor.to_state_id(&Block::OAK_TRAPDOOR),
        ] {
            set_fixture(&world, &position.up_height(3), obstruction);
            assert!(!world.block_registry.bone_meal(
                &Block::KELP_PLANT,
                &world,
                &position,
                Block::KELP_PLANT.default_state.id,
            ));
            assert_eq!(
                world.get_block_state_id(&position.up_height(3)),
                obstruction
            );
        }
        set_fixture(&world, &position.up(), Block::WATER.default_state.id);
        assert!(!KelpBlock.is_valid_bonemeal_target(BonemealArgs {
            world: &world,
            block: &Block::KELP_PLANT,
            position: &position,
            state_id: Block::KELP_PLANT.default_state.id,
        }));

        let ceiling = BlockPos::new(8, world.get_top_y(), 8);
        set_fixture(&world, &ceiling, Block::KELP.default_state.id);
        assert!(!KelpBlock.is_valid_bonemeal_target(BonemealArgs {
            world: &world,
            block: &Block::KELP,
            position: &ceiling,
            state_id: Block::KELP.default_state.id,
        }));
        world.shutdown().await;
    }

    #[tokio::test]
    async fn harvesting_resets_head_and_support_loss_drops_the_remaining_column() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let position = BlockPos::new(4, 64, 4);
        set_fixture(&world, &position.down(), Block::STONE.default_state.id);
        set_fixture(&world, &position, Block::KELP_PLANT.default_state.id);
        set_fixture(&world, &position.up(), Block::KELP_PLANT.default_state.id);
        set_fixture(&world, &position.up_height(2), Block::KELP.default_state.id);

        world.break_block(&position.up_height(2), None, BlockFlags::NOTIFY_ALL);
        assert_eq!(world.get_block(&position.up_height(2)), &Block::WATER);
        assert_eq!(world.get_block(&position.up()), &Block::KELP);
        assert!(
            KelpLikeProperties::from_state_id(world.get_block_state_id(&position.up())).age < 25
        );
        world.break_block(
            &position.down(),
            None,
            BlockFlags::NOTIFY_ALL | BlockFlags::SKIP_DROPS,
        );
        assert_eq!(world.get_block(&position), &Block::KELP_PLANT);
        let chunk = world
            .level
            .loaded_chunks
            .get(&Vector2::new(0, 0))
            .unwrap()
            .clone();
        assert!(chunk.block_ticks.is_scheduled(position, &Block::KELP_PLANT));
        for _ in 0..4 {
            for tick in chunk.block_ticks.step_tick() {
                let block = world.get_block(&tick.position);
                if let Some(behavior) = world.block_registry.get_pumpkin_block(block.id) {
                    behavior.on_scheduled_tick(OnScheduledTickArgs {
                        world: &world,
                        block,
                        position: &tick.position,
                    });
                }
            }
        }
        assert_eq!(world.get_block(&position), &Block::WATER);
        assert_eq!(world.get_block(&position.up()), &Block::WATER);
        assert_eq!(
            world
                .entities
                .load()
                .iter()
                .filter(|entity| entity.get_entity().entity_type == &EntityType::ITEM)
                .count(),
            3
        );
        world.shutdown().await;
    }

    #[tokio::test]
    async fn placement_requires_full_water_and_uses_body_when_kelp_is_above() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let position = BlockPos::new(4, 64, 4);
        let mut random = StdRng::seed_from_u64(0);
        set_fixture(&world, &position.down(), Block::STONE.default_state.id);
        for (level, allowed) in [(0, true), (7, false), (8, true), (15, true)] {
            set_fixture(
                &world,
                &position,
                WaterLikeProperties { level }.to_state_id(&Block::WATER),
            );
            assert_eq!(KelpBlock::can_place_in_water(&world, &position), allowed);
        }
        set_fixture(&world, &position.up(), Block::KELP.default_state.id);
        assert_eq!(
            KelpBlock::get_state_for_placement(world.as_ref(), &position, &mut random),
            Block::KELP_PLANT.default_state.id,
        );
        set_fixture(&world, &position.up(), Block::WATER.default_state.id);
        let state = KelpBlock::get_state_for_placement(world.as_ref(), &position, &mut random);
        assert_eq!(state.to_block(), &Block::KELP);
        assert!(KelpLikeProperties::from_state_id(state).age < 25);
        set_fixture(
            &world,
            &position.down(),
            Block::MAGMA_BLOCK.default_state.id,
        );
        assert!(!KelpBlock::can_survive(world.as_ref(), &position));
        world.shutdown().await;
    }
}
