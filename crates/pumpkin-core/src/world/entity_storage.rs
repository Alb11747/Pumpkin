use std::sync::Arc;

use pumpkin_data::entity::EntityType;
use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};
use pumpkin_util::math::{vector2::Vector2, vector3::Vector3};
use rustc_hash::FxHashSet;
use tracing::warn;
use uuid::Uuid;

use super::World;
use crate::entity::{EntityBase, r#type::from_type};

#[derive(Debug)]
pub(super) enum EntityLoadError {
    Unsupported(&'static str),
    AlreadyLive,
}

impl From<&'static str> for EntityLoadError {
    fn from(reason: &'static str) -> Self {
        Self::Unsupported(reason)
    }
}

pub(super) fn load_entity_tree(
    nbt: &NbtCompound,
    world: &Arc<World>,
) -> Result<Vec<Arc<dyn EntityBase>>, EntityLoadError> {
    // Validate the whole tree before creating any live entities. An unsupported
    // passenger must not disappear when its otherwise supported vehicle is saved.
    let mut pending = vec![(nbt, None, 0)];
    let mut records = Vec::new();
    let mut uuids = FxHashSet::default();
    while let Some((nbt, parent, depth)) = pending.pop() {
        if depth > pumpkin_nbt::MAX_NBT_DEPTH {
            return Err("passenger nesting is too deep".into());
        }
        let id = nbt.get_string("id").ok_or("missing entity ID")?;
        let entity_type = EntityType::from_name(id.strip_prefix("minecraft:").unwrap_or(id))
            .ok_or("unknown entity ID")?;
        if entity_type == &EntityType::PLAYER {
            return Err("player in entity chunk".into());
        }
        let uuid = nbt
            .get_uuid("UUID")
            .ok_or("missing or invalid entity UUID")?;
        if !uuids.insert(uuid) {
            return Err("duplicate UUID in passenger tree".into());
        }
        let position = nbt.get_list("Pos").ok_or("missing entity position")?;
        if position.len() != 3
            || position
                .iter()
                .any(|coordinate| !coordinate.extract_double().is_some_and(f64::is_finite))
        {
            return Err("invalid entity position".into());
        }
        let index = records.len();
        records.push((nbt, entity_type, uuid, parent));
        if let Some(passengers) = nbt.get("Passengers") {
            let passengers = passengers.extract_list().ok_or("invalid passenger list")?;
            for passenger in passengers.iter().rev() {
                pending.push((
                    passenger
                        .extract_compound()
                        .ok_or("invalid passenger record")?,
                    Some(index),
                    depth + 1,
                ));
            }
        }
    }

    // Structural validation wins over deduplication: a genuinely unsupported
    // original tree stays preserved even if another record shares its UUID.
    if world
        .entities
        .load()
        .iter()
        .any(|entity| uuids.contains(&entity.get_entity().entity_uuid))
    {
        return Err(EntityLoadError::AlreadyLive);
    }

    let mut entities: Vec<Arc<dyn EntityBase>> = Vec::with_capacity(records.len());
    for (nbt, entity_type, uuid, parent) in records {
        let entity = from_type(entity_type, Vector3::default(), world, uuid);
        entity.read_nbt_non_mut(nbt);
        entity.init_data_tracker();
        let base = entity.get_entity();
        base.last_sent_velocity.store(base.velocity.load());
        if let Some(parent) = parent {
            let vehicle = entities[parent].clone();
            // Restore the saved relationship before tracker pairing. This is
            // loading existing state, not a new gameplay mount or spawn event.
            *entity
                .get_entity()
                .vehicle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(vehicle.clone());
            vehicle
                .get_entity()
                .passengers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(entity.clone());
        }
        entities.push(entity);
    }
    Ok(entities)
}

fn saved_vehicle(entity: &Arc<dyn EntityBase>) -> Option<Arc<dyn EntityBase>> {
    entity.get_entity().get_vehicle().filter(|vehicle| {
        !vehicle.get_entity().is_removed()
            && vehicle.get_player().is_none()
            && vehicle.get_entity().world.load().uuid == entity.get_entity().world.load().uuid
    })
}

pub(super) fn entity_storage_chunk(entity: &Arc<dyn EntityBase>) -> Vector2<i32> {
    let mut root = entity.clone();
    let mut seen = FxHashSet::default();
    while seen.insert(root.get_entity().entity_uuid) {
        let Some(vehicle) = saved_vehicle(&root) else {
            break;
        };
        root = vehicle;
    }
    root.get_entity().chunk_pos.load()
}

pub(super) fn save_entity_tree(entity: &Arc<dyn EntityBase>) -> Option<NbtCompound> {
    if entity.get_entity().is_removed()
        || saved_vehicle(entity).is_some()
        || entity.get_player().is_some()
    {
        return None;
    }
    save_as_passenger(entity, &mut FxHashSet::default(), 0)
}

fn save_as_passenger(
    entity: &Arc<dyn EntityBase>,
    seen: &mut FxHashSet<Uuid>,
    depth: usize,
) -> Option<NbtCompound> {
    let base = entity.get_entity();
    if base.is_removed() || entity.get_player().is_some() {
        return None;
    }
    if depth > pumpkin_nbt::MAX_NBT_DEPTH || !seen.insert(base.entity_uuid) {
        warn!(
            "Skipping cyclic or excessively nested passenger {}",
            base.entity_uuid
        );
        return None;
    }
    let mut nbt = NbtCompound::new();
    entity.write_nbt(&mut nbt);
    if let Some(vehicle) = saved_vehicle(entity) {
        let position = base.pos.load();
        let vehicle_position = vehicle.get_entity().pos.load();
        nbt.put(
            "Pos",
            NbtTag::List(vec![
                vehicle_position.x.into(),
                position.y.into(),
                vehicle_position.z.into(),
            ]),
        );
    }
    let passengers = base
        .passengers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let mut saved_passengers = Vec::new();
    for passenger in passengers {
        if saved_vehicle(&passenger)
            .is_none_or(|vehicle| vehicle.get_entity().entity_uuid != base.entity_uuid)
        {
            continue;
        }
        if let Some(nbt) = save_as_passenger(&passenger, seen, depth + 1) {
            saved_passengers.push(NbtTag::Compound(nbt));
        }
    }
    if !saved_passengers.is_empty() {
        nbt.put("Passengers", NbtTag::List(saved_passengers));
    }
    Some(nbt)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{block::registry::BlockRegistry, entity::mob::Mob};
    use arc_swap::ArcSwap;
    use pumpkin_config::world::LevelConfig;
    use pumpkin_data::dimension::Dimension;
    use pumpkin_nbt::tag::NbtTag;
    use pumpkin_util::world_seed::Seed;
    use pumpkin_world::{level::Level, world_info::LevelData};
    use std::sync::{Weak, atomic::Ordering};

    fn test_world(path: &std::path::Path) -> Arc<World> {
        let dimension = Dimension::THE_END;
        let level = Level::from_root_folder(
            &LevelConfig::default(),
            path.to_path_buf(),
            0,
            dimension.clone(),
        );
        Arc::new(World::load(
            level,
            Arc::new(ArcSwap::from_pointee(LevelData::default(Seed(0)))),
            dimension,
            Arc::new(BlockRegistry::default()),
            Weak::new(),
        ))
    }

    // Vanilla Entity.saveWithoutId/EntityType.loadPassengersRecursive shape,
    // including the persistent named endermite used in the migrated End farm.
    fn minecart_fixture() -> NbtCompound {
        let mut passenger = NbtCompound::new();
        passenger.put_string("id", "minecraft:endermite".to_string());
        passenger.put_uuid("UUID", Uuid::from_u128(2));
        passenger.put(
            "Pos",
            NbtTag::List(vec![315.57.into(), 46.1875.into(), 0.4246.into()]),
        );
        passenger.put(
            "Motion",
            NbtTag::List(vec![0.125.into(), 0.0.into(), 0.0.into()]),
        );
        passenger.put_string("CustomName", "Farm mite".to_string());
        passenger.put_bool("PersistenceRequired", true);
        passenger.put_int("Lifetime", 2399);
        passenger.put_float("Health", 7.0);
        let mut root = NbtCompound::new();
        root.put_string("id", "minecraft:minecart".to_string());
        root.put_uuid("UUID", Uuid::from_u128(1));
        root.put(
            "Pos",
            NbtTag::List(vec![315.57.into(), 46.0.into(), 0.4246.into()]),
        );
        root.put(
            "Passengers",
            NbtTag::List(vec![NbtTag::Compound(passenger)]),
        );
        root
    }

    #[tokio::test]
    async fn persistent_endermite_keeps_saved_lifetime() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let entity = crate::entity::mob::endermite::EndermiteEntity::new(
            crate::entity::Entity::new(world.clone(), Vector3::default(), &EntityType::ENDERMITE),
        );
        let fixture = minecart_fixture();
        let nbt = fixture.get_list("Passengers").unwrap()[0]
            .extract_compound()
            .unwrap();
        entity.read_nbt_non_mut(nbt);
        for _ in 0..2401 {
            entity.mob_tick(entity.as_ref());
        }
        assert!(!entity.get_entity().is_removed());
        assert_eq!(entity.lifetime.load(Ordering::Relaxed), 2399);
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn passenger_tree_survives_save_reload_and_persistent_ticks() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let loaded = load_entity_tree(&minecart_fixture(), &world).unwrap();
        assert_eq!(loaded.len(), 2, "both vehicle and passenger must be live");
        let vehicle = &loaded[0];
        let passenger = &loaded[1];
        assert_eq!(
            passenger
                .get_entity()
                .get_vehicle()
                .unwrap()
                .get_entity()
                .entity_uuid,
            Uuid::from_u128(1)
        );
        assert!(
            vehicle
                .get_entity()
                .has_passenger(passenger.get_entity().entity_id)
        );
        assert_eq!(passenger.get_entity().velocity.load().x, 0.125);
        let mob = passenger.get_mob().unwrap();
        for _ in 0..2401 {
            mob.mob_tick(passenger.as_ref());
        }
        assert!(!passenger.get_entity().is_removed());
        assert!(
            mob.get_mob_entity()
                .persistence_required
                .load(Ordering::Relaxed)
        );
        let saved = save_entity_tree(vehicle).unwrap();
        let nested = saved.get_list("Passengers").unwrap()[0]
            .extract_compound()
            .unwrap();
        assert_eq!(nested.get_uuid("UUID"), Some(Uuid::from_u128(2)));
        assert_eq!(nested.get_int("Lifetime"), Some(2399));
        assert_eq!(nested.get_bool("PersistenceRequired"), Some(true));
        assert_eq!(nested.get_string("CustomName"), Some("Farm mite"));
        assert_eq!(nested.get_float("Health"), Some(7.0));
        let reloaded = load_entity_tree(&saved, &world).unwrap();
        assert_eq!(reloaded.len(), 2);
        assert_eq!(
            reloaded[1]
                .get_entity()
                .get_vehicle()
                .unwrap()
                .get_entity()
                .entity_uuid,
            Uuid::from_u128(1)
        );
        for entity in loaded.iter().chain(&reloaded) {
            entity.get_entity().passengers.lock().unwrap().clear();
            entity.get_entity().vehicle.lock().unwrap().take();
        }
        world.level.shutdown().await;
    }
    #[tokio::test]
    async fn endermite_expiry_matches_vanilla_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        for (persistent, lifetime, removed, expected_lifetime) in [
            (false, 2398, false, 2399),
            (false, 2399, true, 2400),
            (true, 2399, false, 2399),
            (true, 2400, true, 2400),
        ] {
            let entity =
                crate::entity::mob::endermite::EndermiteEntity::new(crate::entity::Entity::new(
                    world.clone(),
                    Vector3::default(),
                    &EntityType::ENDERMITE,
                ));
            entity
                .mob_entity
                .persistence_required
                .store(persistent, Ordering::Relaxed);
            entity.lifetime.store(lifetime, Ordering::Relaxed);
            entity.mob_tick(entity.as_ref());
            assert_eq!(entity.get_entity().is_removed(), removed);
            assert_eq!(entity.lifetime.load(Ordering::Relaxed), expected_lifetime);
        }
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn nested_passengers_are_saved_once_with_their_original_order() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let mut fixture = minecart_fixture();
        let mut passenger = fixture.get_list("Passengers").unwrap()[0]
            .extract_compound()
            .unwrap()
            .clone();
        let mut child = passenger.clone();
        child.put_uuid("UUID", Uuid::from_u128(3));
        let mut sibling = child.clone();
        sibling.put_uuid("UUID", Uuid::from_u128(4));
        passenger.put(
            "Passengers",
            NbtTag::List(vec![child.into(), sibling.into()]),
        );
        fixture.put("Passengers", NbtTag::List(vec![passenger.into()]));
        let loaded = load_entity_tree(&fixture, &world).unwrap();
        assert_eq!(loaded.len(), 4);
        let roots: Vec<_> = loaded.iter().filter_map(save_entity_tree).collect();
        assert_eq!(roots.len(), 1);
        let passengers = roots[0].get_list("Passengers").unwrap();
        assert_eq!(passengers.len(), 1);
        let descendants = passengers[0]
            .extract_compound()
            .unwrap()
            .get_list("Passengers")
            .unwrap();
        let uuids: Vec<_> = descendants
            .iter()
            .map(|tag| tag.extract_compound().unwrap().get_uuid("UUID").unwrap())
            .collect();
        assert_eq!(uuids, [Uuid::from_u128(3), Uuid::from_u128(4)]);
        for entity in &loaded {
            entity.get_entity().passengers.lock().unwrap().clear();
            entity.get_entity().vehicle.lock().unwrap().take();
        }
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn passenger_chunk_does_not_unload_its_watched_vehicle_tree() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let root_pos = Vector2::new(19, 0);
        let passenger_pos = Vector2::new(20, 0);
        let root_chunk = world.level.get_entity_chunk(root_pos).await;
        root_chunk.live.store(true, Ordering::Relaxed);
        let passenger_chunk = world.level.get_entity_chunk(passenger_pos).await;
        passenger_chunk.live.store(true, Ordering::Relaxed);
        let loaded = world.load_entity_records(root_pos, vec![minecart_fixture()]);
        loaded[1]
            .get_entity()
            .set_pos(Vector3::new(335.0, 46.1875, 0.4246));
        assert_eq!(entity_storage_chunk(&loaded[1]), root_pos);
        world.remove_entities_in_chunks([passenger_pos]).await;
        assert_eq!(world.entities.load().len(), 2);
        assert!(passenger_chunk.data.lock().unwrap().is_empty());
        world.remove_entities_in_chunks([root_pos]).await;
        assert!(world.entities.load().is_empty());
        let records = root_chunk.data.lock().unwrap().clone();
        assert_eq!(records.len(), 1);
        let saved_passenger = records[0].get_list("Passengers").unwrap()[0]
            .extract_compound()
            .unwrap();
        assert_eq!(saved_passenger.get_uuid("UUID"), Some(Uuid::from_u128(2)));
        assert_eq!(
            saved_passenger.get_list("Pos").unwrap()[0].extract_double(),
            Some(315.57)
        );
        assert!(!loaded[1].get_entity().has_vehicle());
        root_chunk.live.store(true, Ordering::Relaxed);
        let reloaded = world.load_entity_records(root_pos, records);
        assert_eq!(reloaded.len(), 2);
        assert!(reloaded[1].get_entity().has_vehicle());
        world.remove_entities_in_chunks([root_pos]).await;
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn dismounted_passenger_saves_in_its_own_chunk() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let root_pos = Vector2::new(19, 0);
        let passenger_pos = Vector2::new(20, 0);
        let loaded = world.load_entity_records(root_pos, vec![minecart_fixture()]);
        loaded[0]
            .get_entity()
            .remove_passenger_on_disconnect(loaded[1].get_entity().entity_id);
        loaded[1]
            .get_entity()
            .set_pos(Vector3::new(335.0, 46.1875, 0.4246));
        assert_eq!(entity_storage_chunk(&loaded[1]), passenger_pos);
        assert!(
            save_entity_tree(&loaded[0])
                .unwrap()
                .get_list("Passengers")
                .is_none()
        );
        assert!(save_entity_tree(&loaded[1]).is_some());
        world.remove_entities_in_chunks([root_pos]).await;
        assert_eq!(world.entities.load().len(), 1);
        assert_eq!(
            world.entities.load()[0].get_entity().entity_uuid,
            Uuid::from_u128(2)
        );
        let root_records = world
            .level
            .get_entity_chunk(root_pos)
            .await
            .data
            .lock()
            .unwrap()
            .clone();
        assert_eq!(root_records.len(), 1);
        assert!(root_records[0].get_list("Passengers").is_none());
        world.remove_entities_in_chunks([passenger_pos]).await;
        let passenger_records = world
            .level
            .get_entity_chunk(passenger_pos)
            .await
            .data
            .lock()
            .unwrap()
            .clone();
        assert_eq!(passenger_records.len(), 1);
        assert_eq!(
            passenger_records[0].get_uuid("UUID"),
            Some(Uuid::from_u128(2))
        );
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn malformed_or_unknown_passenger_tree_is_preserved_across_repeated_saves() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let position = Vector2::new(19, 0);
        let chunk = world.level.get_entity_chunk(position).await;
        let mut unknown = minecart_fixture();
        let mut passenger = unknown.get_list("Passengers").unwrap()[0]
            .extract_compound()
            .unwrap()
            .clone();
        passenger.put_string("id", "custom:unknown_mob".to_string());
        passenger.put_compound("OpaquePluginState", minecart_fixture());
        unknown.put("Passengers", NbtTag::List(vec![passenger.into()]));
        let mut malformed = minecart_fixture();
        malformed.put("Passengers", NbtTag::String("invalid".into()));
        let mut missing_uuid = minecart_fixture();
        missing_uuid.child_tags.remove("UUID");
        let original = vec![unknown, malformed, missing_uuid];
        *chunk.data.lock().unwrap() = original.clone();
        assert!(world.load_entity_chunk(&chunk).unwrap().is_empty());
        assert!(world.load_entity_chunk(&chunk).is_none());
        assert!(world.entities.load().is_empty());
        for _ in 0..3 {
            world.save_entities_by_chunk(&[], [position]).await;
            assert_eq!(*chunk.data.lock().unwrap(), original);
        }
        world.remove_entities_in_chunks([position]).await;
        assert!(world.preserved_entity_records.get(&position).is_none());
        world.save_entities_by_chunk(&[], [position]).await;
        assert_eq!(*chunk.data.lock().unwrap(), original);
        assert!(world.load_entity_chunk(&chunk).unwrap().is_empty());
        world.save_entities_by_chunk(&[], [position]).await;
        assert_eq!(*chunk.data.lock().unwrap(), original);
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn removed_vehicle_does_not_hide_its_surviving_passenger() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let loaded = world.load_entity_records(Vector2::new(19, 0), vec![minecart_fixture()]);
        loaded[0].get_entity().remove();
        assert!(save_entity_tree(&loaded[0]).is_none());
        assert!(save_entity_tree(&loaded[1]).is_some());
        world.remove_entities_in_chunks([Vector2::new(19, 0)]).await;
        let records = world
            .level
            .get_entity_chunk(Vector2::new(19, 0))
            .await
            .data
            .lock()
            .unwrap()
            .clone();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].get_uuid("UUID"), Some(Uuid::from_u128(2)));
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn dormant_cached_entity_chunk_loads_once_and_survives_repeated_unloads() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let position = Vector2::new(19, 0);
        let chunk = world.level.get_entity_chunk(position).await;
        assert!(!chunk.live.load(Ordering::Relaxed));
        *chunk.data.lock().unwrap() = vec![minecart_fixture()];
        world.save_entities_by_chunk(&[], [position]).await;
        assert_eq!(chunk.data.lock().unwrap().len(), 1);
        for _ in 0..3 {
            let loaded = world.load_entity_chunk(&chunk).unwrap();
            assert_eq!(loaded.len(), 2);
            assert_eq!(world.entities.load().len(), 2);
            assert!(chunk.data.lock().unwrap().is_empty());
            assert!(world.load_entity_chunk(&chunk).is_none());
            for _ in 0..2 {
                let entities = world.entities.load_full();
                world.save_entities_by_chunk(&entities, [position]).await;
                assert_eq!(chunk.data.lock().unwrap().len(), 1);
            }
            world.remove_entities_in_chunks([position]).await;
            assert!(!chunk.live.load(Ordering::Relaxed));
            assert!(world.entities.load().is_empty());
            world.save_entities_by_chunk(&[], [position]).await;
            let records = chunk.data.lock().unwrap();
            assert_eq!(records.len(), 1);
            let passenger = records[0].get_list("Passengers").unwrap()[0]
                .extract_compound()
                .unwrap();
            assert_eq!(passenger.get_uuid("UUID"), Some(Uuid::from_u128(2)));
        }
        world.level.shutdown().await;
    }
    #[tokio::test]
    async fn mounting_clears_old_standalone_snapshot_without_losing_other_chunk_records() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let root_position = Vector2::new(19, 0);
        let passenger_position = Vector2::new(20, 0);
        let root_chunk = world.level.get_entity_chunk(root_position).await;
        root_chunk.live.store(true, Ordering::Relaxed);
        let passenger_chunk = world.level.get_entity_chunk(passenger_position).await;
        passenger_chunk.live.store(true, Ordering::Relaxed);
        let loaded = world.load_entity_records(root_position, vec![minecart_fixture()]);
        loaded[0]
            .get_entity()
            .remove_passenger_on_disconnect(loaded[1].get_entity().entity_id);
        loaded[1]
            .get_entity()
            .set_pos(Vector3::new(335.0, 46.1875, 0.4246));
        let fixture = minecart_fixture();
        let mut other = fixture.get_list("Passengers").unwrap()[0]
            .extract_compound()
            .unwrap()
            .clone();
        other.put_uuid("UUID", Uuid::from_u128(3));
        other.put(
            "Pos",
            NbtTag::List(vec![334.0.into(), 46.1875.into(), 0.4246.into()]),
        );
        let mut unknown = minecart_fixture();
        unknown.put_string("id", "custom:unknown_vehicle".to_string());
        unknown.put_uuid("UUID", Uuid::from_u128(90));
        unknown.child_tags.remove("Passengers");
        world.load_entity_records(passenger_position, vec![other, unknown.clone()]);
        let entities = world.entities.load_full();
        world
            .save_entities_by_chunk(&entities, [root_position, passenger_position])
            .await;
        assert_eq!(passenger_chunk.data.lock().unwrap().len(), 3);
        loaded[0]
            .get_entity()
            .add_passenger(loaded[0].clone(), loaded[1].clone());
        world.remove_entities_in_chunks([root_position]).await;
        assert_eq!(world.entities.load().len(), 1);
        assert_eq!(
            world.entities.load()[0].get_entity().entity_uuid,
            Uuid::from_u128(3)
        );
        let records = passenger_chunk.data.lock().unwrap().clone();
        assert_eq!(records.len(), 2);
        assert!(
            !records
                .iter()
                .any(|record| record.get_uuid("UUID") == Some(Uuid::from_u128(2)))
        );
        assert!(
            records
                .iter()
                .any(|record| record.get_uuid("UUID") == Some(Uuid::from_u128(3)))
        );
        assert!(records.contains(&unknown));
        assert_eq!(root_chunk.data.lock().unwrap().len(), 1);
        assert!(passenger_chunk.live.load(Ordering::Relaxed));
        world.remove_entities_in_chunks([passenger_position]).await;
        world.level.shutdown().await;
    }
    #[tokio::test]
    async fn concurrent_uncached_load_cannot_resurrect_a_killed_passenger() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let position = Vector2::new(19, 0);
        let original = minecart_fixture();
        let stored = Arc::new(pumpkin_world::chunk::ChunkEntityData {
            x: position.x,
            z: position.y,
            data: std::sync::Mutex::new(vec![original]),
            live: std::sync::atomic::AtomicBool::new(false),
            dirty: std::sync::atomic::AtomicBool::new(true),
        });
        world
            .level
            .write_entity_chunks(vec![(position, stored)])
            .await;
        assert!(world.level.get_entity_chunk_sync(&position).is_none());

        // Hold every Rayon decoder until both requests have reached their
        // uncached path. Dropping the senders also releases workers on panic.
        let (started, mut workers) = tokio::sync::mpsc::unbounded_channel();
        let mut release_workers = Vec::new();
        let worker_count = rayon::current_num_threads();
        for _ in 0..worker_count {
            let (release, waiting) = std::sync::mpsc::channel::<()>();
            release_workers.push(release);
            let started = started.clone();
            rayon::spawn(move || {
                let _ = started.send(());
                let _ = waiting.recv();
            });
        }
        for _ in 0..worker_count {
            workers.recv().await.unwrap();
        }
        let mut first = Box::pin(world.level.get_entity_chunk(position));
        let mut second = Box::pin(world.level.get_entity_chunk(position));
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        assert!(world.level.get_entity_chunk_sync(&position).is_none());
        drop(release_workers);
        let (first, second) = tokio::join!(first, second);
        assert!(
            Arc::ptr_eq(&first, &second),
            "concurrent decodes must publish one canonical chunk"
        );

        let loaded = world.load_entity_chunk(&first).unwrap();
        assert_eq!(loaded.len(), 2);
        loaded[0]
            .get_entity()
            .remove_passenger_on_disconnect(loaded[1].get_entity().entity_id);
        loaded[0]
            .get_entity()
            .set_custom_name(pumpkin_util::text::TextComponent::text("Changed cart"));
        loaded[1].get_entity().remove();
        assert!(world.load_entity_chunk(&second).is_none());
        assert!(world.preserved_entity_records.get(&position).is_none());
        world.remove_entities_in_chunks([position]).await;
        let saved = first.data.lock().unwrap().clone();
        assert_eq!(saved.len(), 1);
        assert!(saved[0].get_list("Passengers").is_none());
        assert_eq!(saved[0].get_string("CustomName"), Some("Changed cart"));
        world
            .level
            .write_entity_chunks(vec![(position, first.clone())])
            .await;
        world.level.clean_entity_chunks([position]);
        let mut stream_a = world.level.receive_entity_chunks(vec![position]);
        let mut stream_b = world.level.receive_entity_chunks(vec![position]);
        let (a, b) = tokio::join!(stream_a.recv(), stream_b.recv());
        let a = a.unwrap().0.upgrade().unwrap();
        let b = b.unwrap().0.upgrade().unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        let reloaded = world.load_entity_chunk(&a).unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].get_entity().entity_uuid, Uuid::from_u128(1));
        assert!(world.load_entity_chunk(&b).is_none());
        assert!(
            !world
                .entities
                .load()
                .iter()
                .any(|entity| entity.get_entity().entity_uuid == Uuid::from_u128(2))
        );
        world.remove_entities_in_chunks([position]).await;
        world.level.shutdown().await;
    }
    #[tokio::test]
    async fn duplicate_delivery_is_not_preserved_as_a_replayable_entity_tree() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let position = Vector2::new(19, 0);
        let chunk = world.level.get_entity_chunk(position).await;
        chunk.live.store(true, Ordering::Relaxed);
        let stale = minecart_fixture();
        let mut unknown = NbtCompound::new();
        unknown.put_string("id", "custom:unknown_entity".to_string());
        unknown.put_uuid("UUID", Uuid::from_u128(90));
        let loaded = world.load_entity_records(position, vec![stale.clone(), unknown.clone()]);
        loaded[0]
            .get_entity()
            .set_custom_name(pumpkin_util::text::TextComponent::text("Changed cart"));
        loaded[0]
            .get_entity()
            .remove_passenger_on_disconnect(loaded[1].get_entity().entity_id);
        loaded[1].get_entity().remove();
        assert!(world.load_entity_records(position, vec![stale]).is_empty());
        assert_eq!(
            world
                .preserved_entity_records
                .get(&position)
                .unwrap()
                .as_slice(),
            &[unknown.clone()]
        );
        world.remove_entities_in_chunks([position]).await;
        let records = chunk.data.lock().unwrap().clone();
        assert_eq!(records.len(), 2);
        assert!(records.contains(&unknown));
        assert!(
            records
                .iter()
                .all(|record| record.get_list("Passengers").is_none())
        );
        let loaded = world.load_entity_chunk(&chunk).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].get_entity().entity_uuid, Uuid::from_u128(1));
        let mut saved = NbtCompound::new();
        loaded[0].write_nbt(&mut saved);
        assert_eq!(saved.get_string("CustomName"), Some("Changed cart"));
        assert!(
            !world
                .entities
                .load()
                .iter()
                .any(|entity| entity.get_entity().entity_uuid == Uuid::from_u128(2))
        );
        world.remove_entities_in_chunks([position]).await;
        world.level.shutdown().await;
    }
}
