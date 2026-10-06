use std::sync::{Arc, Weak};

use arc_swap::ArcSwap;
use pumpkin_config::world::LevelConfig;
use pumpkin_data::{damage::DamageType, dimension::Dimension, entity::EntityType, item::Item};
use pumpkin_util::{math::vector3::Vector3, text::TextComponent, world_seed::Seed};
use pumpkin_world::{level::Level, world_info::LevelData};

use crate::{
    block::registry::BlockRegistry,
    entity::{Entity, EntityBase, item::ItemEntity, ride_tick, r#type::from_type},
    world::World,
};

pub fn test_world(path: &std::path::Path) -> Arc<World> {
    let dimension = Dimension::THE_END;
    let level = Level::from_root_folder(
        &LevelConfig::default(),
        path.to_path_buf(),
        0,
        dimension.clone(),
    )
    .unwrap();
    Arc::new(
        World::load(
            level,
            Arc::new(ArcSwap::from_pointee(LevelData::default(Seed(0)))),
            dimension,
            Arc::new(BlockRegistry::default()),
            Weak::new(),
        )
        .unwrap(),
    )
}

#[tokio::test]
async fn mounted_teleport_cannot_be_undone_by_active_or_retired_vehicle() {
    let directory = tempfile::tempdir().unwrap();
    let world = test_world(directory.path());
    for retired in [false, true] {
        let vehicle = from_type(
            &EntityType::MINECART,
            Vector3::new(0.5, 64.0, 0.5),
            &world,
            uuid::Uuid::new_v4(),
        );
        let passenger = from_type(
            &EntityType::ENDERMITE,
            Vector3::default(),
            &world,
            uuid::Uuid::new_v4(),
        );
        vehicle
            .get_entity()
            .add_passenger(vehicle.clone(), passenger.clone());
        vehicle.position_passenger(passenger.as_ref());
        if retired {
            vehicle.get_entity().remove();
        }
        let destination = Vector3::new(96.5, 80.0, -32.5);
        passenger.teleport(destination, Some(45.0), Some(12.0), world.clone());
        assert!(!passenger.get_entity().has_vehicle());
        assert!(!vehicle.get_entity().has_passengers());
        // Both attachment paths must leave an accepted destination alone.
        vehicle.position_passenger(passenger.as_ref());
        ride_tick(passenger.as_ref(), || {});
        assert_eq!(passenger.get_entity().pos.load(), destination);
        assert!((passenger.get_entity().yaw.load() - 45.0).abs() < f32::EPSILON);
        assert!((passenger.get_entity().pitch.load() - 12.0).abs() < f32::EPSILON);
    }
    world.level.shutdown().await;
}

#[tokio::test]
async fn teleport_detachment_clears_missing_parent_entry_without_repositioning() {
    let directory = tempfile::tempdir().unwrap();
    let source = test_world(&directory.path().join("source"));
    let destination_world = test_world(&directory.path().join("destination"));
    for retired in [false, true] {
        let vehicle = from_type(
            &EntityType::OAK_BOAT,
            Vector3::new(2.5, 64.0, 2.5),
            &source,
            uuid::Uuid::new_v4(),
        );
        let passenger = from_type(
            &EntityType::ENDERMITE,
            Vector3::default(),
            &source,
            uuid::Uuid::new_v4(),
        );
        vehicle
            .get_entity()
            .add_passenger(vehicle.clone(), passenger.clone());
        vehicle.position_passenger(passenger.as_ref());
        if retired {
            vehicle.get_entity().remove();
        }
        // Source retirement can discard its list before the passenger transfers.
        vehicle.get_entity().passengers.lock().unwrap().clear();
        let entity = passenger.get_entity();
        let before = entity.pos.load();
        assert!(entity.has_vehicle());
        assert!(entity.dismount_before_teleport());
        assert_eq!(entity.pos.load(), before);
        assert!(!entity.has_vehicle());
        assert!(!vehicle.get_entity().has_passengers());
        entity.set_world(destination_world.clone());
        let destination = Vector3::new(-48.5, 92.0, 128.5);
        entity.set_pos(destination);
        vehicle.position_passenger(passenger.as_ref());
        ride_tick(passenger.as_ref(), || {});
        assert_eq!(entity.pos.load(), destination);
        assert!(Arc::ptr_eq(&entity.world.load_full(), &destination_world));
    }
    source.level.shutdown().await;
    destination_world.level.shutdown().await;
}

#[tokio::test]
async fn vehicle_attachment_offsets_apply_to_nonplayers_and_survive_player_crouching() {
    let directory = tempfile::tempdir().unwrap();
    let world = test_world(directory.path());
    let cart = from_type(
        &EntityType::MINECART,
        Vector3::new(0.5, 64.0, 0.5),
        &world,
        uuid::Uuid::new_v4(),
    );
    for (entity_type, expected_y) in [
        (&EntityType::SKELETON, 63.487_500_011_920_93),
        (&EntityType::PLAYER, 63.5875),
    ] {
        let passenger: Arc<dyn EntityBase> =
            Arc::new(Entity::new(world.clone(), Vector3::default(), entity_type));
        cart.get_entity()
            .add_passenger(cart.clone(), passenger.clone());
        cart.position_passenger(passenger.as_ref());
        assert!((passenger.get_entity().pos.load().y - expected_y).abs() < 1.0e-10);
        if entity_type == &EntityType::PLAYER {
            let mut dimensions = passenger.get_entity().entity_dimension.load();
            dimensions.height = 1.5;
            passenger.get_entity().entity_dimension.store(dimensions);
            cart.position_passenger(passenger.as_ref());
            assert!((passenger.get_entity().pos.load().y - expected_y).abs() < 1.0e-10);
        }
        cart.get_entity()
            .remove_passenger_sync(passenger.get_entity().entity_id);
    }
    world.level.shutdown().await;
}

#[tokio::test]
async fn survival_vehicle_break_drops_one_named_item_and_dismounts() {
    for (entity_type, item) in [
        (&EntityType::MINECART, &Item::MINECART),
        (&EntityType::OAK_BOAT, &Item::OAK_BOAT),
        (&EntityType::BAMBOO_RAFT, &Item::BAMBOO_RAFT),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let vehicle = from_type(
            entity_type,
            Vector3::new(0.5, 64.0, 0.5),
            &world,
            uuid::Uuid::new_v4(),
        );
        let passenger: Arc<dyn EntityBase> = Arc::new(Entity::new(
            world.clone(),
            Vector3::default(),
            &EntityType::ITEM,
        ));
        vehicle
            .get_entity()
            .set_custom_name(TextComponent::text("Farm transport"));
        vehicle
            .get_entity()
            .add_passenger(vehicle.clone(), passenger.clone());
        world.add_entity_silent(vehicle.clone());
        world.add_entity_silent(passenger.clone());
        assert!(vehicle.damage(vehicle.as_ref(), 5.0, DamageType::GENERIC));
        assert!(vehicle.get_entity().is_removed());
        assert!(!passenger.get_entity().has_vehicle());
        assert!(!vehicle.get_entity().has_passengers());
        // Hitting the removed vehicle must not produce another item.
        vehicle.damage(vehicle.as_ref(), 5.0, DamageType::GENERIC);
        let drops: Vec<_> = world
            .entities
            .load()
            .iter()
            .filter_map(|entity| {
                entity
                    .cast_any()
                    .downcast_ref::<ItemEntity>()
                    .map(|drop| drop.get_item_stack().lock().unwrap().clone())
            })
            .collect();
        assert_eq!(drops.len(), 1);
        assert_eq!(drops[0].item, item);
        assert_eq!(drops[0].item_count, 1);
        assert_eq!(
            drops[0].get_custom_name(),
            Some(&TextComponent::text("Farm transport"))
        );
        world.level.shutdown().await;
    }
}

#[tokio::test]
async fn disabled_entity_drops_suppresses_vehicle_item() {
    let directory = tempfile::tempdir().unwrap();
    let world = test_world(directory.path());
    let mut level_data = (**world.level_info.load()).clone();
    level_data.game_rules.entity_drops = false;
    world.level_info.store(Arc::new(level_data));
    for entity_type in [&EntityType::MINECART, &EntityType::BIRCH_BOAT] {
        let vehicle = from_type(
            entity_type,
            Vector3::new(0.5, 64.0, 0.5),
            &world,
            uuid::Uuid::new_v4(),
        );
        world.add_entity_silent(vehicle.clone());
        vehicle.damage(vehicle.as_ref(), 5.0, DamageType::GENERIC);
        assert!(vehicle.get_entity().is_removed());
    }
    assert!(world.entities.load().is_empty());
    world.level.shutdown().await;
}
