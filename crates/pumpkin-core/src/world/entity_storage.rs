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
        entity.init_data_tracker_on_load();
        let mut modeled = NbtCompound::new();
        entity.write_nbt(&mut modeled);
        let mut preserved = crate::data::preserved_nbt::PreservedNbt::new(nbt, &modeled);
        preserved.discard(&["Passengers"]);
        preserved.discard_aliases(&modeled, &[("BukkitValues", "PumpkinCustomData")]);
        preserved.discard_aliases(&modeled, entity.nbt_aliases());
        let base = entity.get_entity();
        *base
            .preserved_nbt
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(preserved);
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

pub(super) struct LoadedPlayerVehicle {
    pub vehicle: Arc<dyn EntityBase>,
    pub admitted: Vec<Arc<dyn EntityBase>>,
}

pub(super) fn load_player_vehicle(
    root: &NbtCompound,
    world: &Arc<World>,
) -> Result<LoadedPlayerVehicle, EntityLoadError> {
    let attach = root.get_uuid("Attach").ok_or("invalid attachment UUID")?;
    if let Some(vehicle) = world.get_entity_by_uuid(attach) {
        if !supports_player_vehicle(&vehicle) {
            return Err("unsupported attached vehicle".into());
        }
        return Ok(LoadedPlayerVehicle {
            vehicle,
            admitted: Vec::new(),
        });
    }
    let entities = load_entity_tree(
        root.get_compound("Entity")
            .ok_or("missing vehicle snapshot")?,
        world,
    )?;
    let result = (|| {
        if entities
            .iter()
            .any(|entity| !supports_player_vehicle(entity))
        {
            return Err("unsupported player vehicle tree".into());
        }
        let vehicle = entities
            .iter()
            .find(|entity| entity.get_entity().entity_uuid == attach)
            .cloned()
            .ok_or("attachment is outside the saved vehicle tree")?;
        if world.add_entity_tree_silent(&entities) {
            Ok(LoadedPlayerVehicle {
                vehicle,
                admitted: entities.clone(),
            })
        } else {
            Err(EntityLoadError::AlreadyLive)
        }
    })();
    if result.is_err() {
        // Unpublished trees still have strong parent/passenger links.
        for entity in entities {
            entity
                .get_entity()
                .vehicle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            entity
                .get_entity()
                .passengers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        }
    }
    result
}

pub fn supports_player_vehicle(entity: &Arc<dyn EntityBase>) -> bool {
    // The factory's generic fallback preserves NBT but has no vehicle behaviour.
    entity.get_player().is_none()
        && entity.get_entity().entity_type.saveable
        && !entity.cast_any().is::<crate::entity::Entity>()
        && !entity
            .cast_any()
            .is::<crate::entity::living::LivingEntity>()
        && !entity.get_entity().is_removed()
}

pub fn save_player_vehicle(vehicle: &Arc<dyn EntityBase>) -> Option<NbtCompound> {
    let mut root = vehicle.clone();
    let mut seen = FxHashSet::default();
    loop {
        if !seen.insert(root.get_entity().entity_uuid) || !supports_player_vehicle(&root) {
            return None;
        }
        let Some(parent) = saved_vehicle(&root) else {
            break;
        };
        root = parent;
    }
    // ServerPlayer.saveParentVehicle only owns trees with exactly one player.
    let mut pending = vec![root.clone()];
    let mut players = 0;
    seen.clear();
    while let Some(entity) = pending.pop() {
        if !seen.insert(entity.get_entity().entity_uuid) {
            return None;
        }
        if entity.get_player().is_some() {
            players += 1;
        }
        pending.extend(
            entity
                .get_entity()
                .passengers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .cloned(),
        );
    }
    if players != 1 {
        return None;
    }
    let snapshot = save_entity_tree(&root)?;
    let mut wrapper = NbtCompound::new();
    wrapper.put_uuid("Attach", vehicle.get_entity().entity_uuid);
    wrapper.put_compound("Entity", snapshot);
    Some(wrapper)
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
    if let Some(preserved) = base
        .preserved_nbt
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
    {
        preserved.discard_aliases(&nbt, &[("BukkitValues", "PumpkinCustomData")]);
        preserved.discard_aliases(&nbt, entity.nbt_aliases());
        preserved.snapshot(&mut nbt);
    }
    // The live relationship tree is authoritative even after a dismount or death.
    nbt.child_tags.remove("Passengers");
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
    use pumpkin_util::{math::position::BlockPos, world_seed::Seed};
    use pumpkin_world::{chunk::ChunkData, level::Level, world_info::LevelData};
    use std::sync::{Weak, atomic::Ordering};

    fn test_world(path: &std::path::Path) -> Arc<World> {
        test_world_in_dimension(path, Dimension::THE_END)
    }

    fn test_world_in_dimension(path: &std::path::Path, dimension: Dimension) -> Arc<World> {
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
    async fn loaded_farm_mobs_keep_vanilla_distance_policy_and_survive_without_players() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        assert!(world.players.load().is_empty());
        let mut outcomes = Vec::new();
        for (id, uuid, from_bucket, expected_far_despawn) in [
            ("villager", 701, false, false),
            ("frog", 702, false, false),
            ("cow", 703, false, false),
            ("zombie", 704, false, true),
            ("axolotl", 705, false, true),
            ("axolotl", 706, true, false),
        ] {
            let mut fixture = NbtCompound::new();
            fixture.put_string("id", format!("minecraft:{id}"));
            fixture.put_uuid("UUID", Uuid::from_u128(uuid));
            fixture.put_list("Pos", vec![0.5.into(), 64.0.into(), 0.5.into()]);
            fixture.put_bool("PersistenceRequired", false);
            fixture.put_bool("FromBucket", from_bucket);
            let loaded = load_entity_tree(&fixture, &world).unwrap();
            let mob = loaded[0].get_mob().unwrap();
            let actual_far_despawn = mob.remove_when_far_away(130.0 * 130.0);
            mob.get_mob_entity().check_despawn(mob);
            let saved_uuid = save_entity_tree(&loaded[0]).and_then(|saved| saved.get_uuid("UUID"));
            outcomes.push((
                id,
                uuid,
                expected_far_despawn,
                actual_far_despawn,
                loaded[0].get_entity().is_removed(),
                saved_uuid,
            ));
        }
        world.level.shutdown().await;
        for (id, uuid, expected_far_despawn, actual_far_despawn, removed, saved_uuid) in outcomes {
            assert_eq!(
                actual_far_despawn, expected_far_despawn,
                "{id} far-away policy"
            );
            assert!(!removed, "unattended {id}");
            assert_eq!(
                saved_uuid,
                Some(Uuid::from_u128(uuid)),
                "{id} must remain saveable"
            );
        }
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
    async fn loaded_mobs_preserve_equipment_counts_and_never_receive_spawn_equipment() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        for (id, uuid) in [("piglin", 101), ("skeleton", 102)] {
            let mut fixture = NbtCompound::new();
            fixture.put_string("id", format!("minecraft:{id}"));
            fixture.put_uuid("UUID", Uuid::from_u128(uuid));
            fixture.put_list("Pos", vec![0.5.into(), 64.0.into(), 0.5.into()]);
            fixture.put_bool("CanPickUpLoot", true);
            let mut equipment = NbtCompound::new();
            for (slot, item, count) in [
                ("mainhand", "minecraft:gold_nugget", 27),
                ("head", "minecraft:carved_pumpkin", 1),
                ("feet", "minecraft:iron_boots", 1),
            ] {
                let mut stack = NbtCompound::new();
                stack.put_string("id", item.to_string());
                stack.put_int("count", count);
                equipment.put_compound(slot, stack);
            }
            fixture.put_compound("equipment", equipment.clone());

            for saved_equipment in [true, false] {
                let mut raw = fixture.clone();
                if !saved_equipment {
                    raw.child_tags.remove("equipment");
                    raw.child_tags.remove("CanPickUpLoot");
                }
                for _ in 0..2 {
                    let loaded = load_entity_tree(&raw, &world).unwrap();
                    let entity = &loaded[0];
                    assert_eq!(
                        entity
                            .get_mob()
                            .unwrap()
                            .get_mob_entity()
                            .can_pick_up_loot(),
                        saved_equipment,
                    );
                    let saved = save_entity_tree(entity).unwrap();
                    if saved_equipment {
                        let saved_equipment = saved.get_compound("equipment").unwrap();
                        assert_eq!(saved_equipment.child_tags.len(), equipment.child_tags.len());
                        for (slot, expected) in &equipment.child_tags {
                            let expected = expected.extract_compound().unwrap();
                            let stack = saved_equipment.get_compound(slot).unwrap();
                            assert_eq!(stack.get_string("id"), expected.get_string("id"));
                            assert_eq!(stack.get_int("count"), expected.get_int("count"));
                        }
                    } else {
                        assert!(
                            entity
                                .get_living_entity()
                                .unwrap()
                                .entity_equipment
                                .lock()
                                .unwrap()
                                .is_empty()
                        );
                        assert!(saved.get("equipment").is_none());
                        assert!(!saved.get_bool("CanPickUpLoot").unwrap_or(false));
                    }
                    raw = saved;
                }
            }
        }

        // The load hook must not remove equipment generation from actual spawns.
        let skeleton = from_type(
            &EntityType::SKELETON,
            Vector3::new(0.5, 64.0, 0.5),
            &world,
            Uuid::from_u128(103),
        );
        skeleton.init_data_tracker();
        let saved = save_entity_tree(&skeleton).unwrap();
        assert_eq!(
            saved
                .get_compound("equipment")
                .unwrap()
                .get_compound("mainhand")
                .unwrap()
                .get_string("id"),
            Some("minecraft:bow"),
        );
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn player_vehicle_snapshot_loads_once_and_rejects_invalid_attachment_without_orphans() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let mut root = NbtCompound::new();
        root.put_uuid("Attach", Uuid::from_u128(1));
        root.put_compound("Entity", minecart_fixture());
        let first = load_player_vehicle(&root, &world).unwrap();
        assert_eq!(first.admitted.len(), 2);
        first
            .vehicle
            .get_entity()
            .set_pos(Vector3::new(320.0, 48.0, 2.0));
        let second = load_player_vehicle(&root, &world).unwrap();
        assert!(second.admitted.is_empty());
        assert!(Arc::ptr_eq(&first.vehicle, &second.vehicle));
        assert_eq!(
            second.vehicle.get_entity().pos.load().x,
            320.0,
            "stale playerdata cannot overwrite a live vehicle"
        );
        assert_eq!(world.entities.load().len(), 2);
        for entity in first.admitted {
            world.remove_entity(entity.as_ref());
        }
        root.put_uuid("Attach", Uuid::from_u128(99));
        assert!(matches!(
            load_player_vehicle(&root, &world),
            Err(EntityLoadError::Unsupported(_))
        ));
        assert!(world.entities.load().is_empty());
        root.put_uuid("Attach", Uuid::from_u128(1));
        let mut unsupported = minecart_fixture();
        unsupported.put_string("id", "minecraft:mannequin".to_owned());
        root.put_compound("Entity", unsupported);
        let before = root.clone();
        assert!(matches!(
            load_player_vehicle(&root, &world),
            Err(EntityLoadError::Unsupported(_))
        ));
        assert_eq!(root, before);
        assert!(world.entities.load().is_empty());
        world.level.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn competing_passenger_tree_admission_never_publishes_duplicate_or_partial_trees() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let first = load_entity_tree(&minecart_fixture(), &world).unwrap();
        let second = load_entity_tree(&minecart_fixture(), &world).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = [first, second]
            .into_iter()
            .map(|entities| {
                let world = world.clone();
                let barrier = barrier.clone();
                tokio::task::spawn_blocking(move || {
                    barrier.wait();
                    world.add_entity_tree_silent(&entities)
                })
            })
            .collect();
        let mut admitted = 0;
        for handle in handles {
            admitted += usize::from(handle.await.unwrap());
        }
        assert_eq!(admitted, 1);
        let entities = world.entities.load();
        assert_eq!(entities.len(), 2);
        assert_ne!(
            entities[0].get_entity().entity_uuid,
            entities[1].get_entity().entity_uuid
        );
        assert!(Arc::ptr_eq(
            &entities[1].get_entity().get_vehicle().unwrap(),
            &entities[0]
        ));
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn passenger_mount_rejects_a_vehicle_left_in_another_world() {
        let first_directory = tempfile::tempdir().unwrap();
        let second_directory = tempfile::tempdir().unwrap();
        let first_world = test_world(first_directory.path());
        let second_world = test_world(second_directory.path());
        let vehicle = from_type(
            &EntityType::MINECART,
            Vector3::default(),
            &first_world,
            Uuid::new_v4(),
        );
        let passenger = from_type(
            &EntityType::ENDERMITE,
            Vector3::default(),
            &second_world,
            Uuid::new_v4(),
        );
        vehicle
            .get_entity()
            .add_passenger(vehicle.clone(), passenger.clone());
        assert!(!passenger.get_entity().has_vehicle());
        assert!(!vehicle.get_entity().has_passengers());

        passenger.get_entity().set_world(first_world.clone());
        vehicle
            .get_entity()
            .add_passenger(vehicle.clone(), passenger.clone());
        assert!(Arc::ptr_eq(
            &passenger.get_entity().get_vehicle().unwrap(),
            &vehicle
        ));
        assert!(
            vehicle
                .get_entity()
                .has_passenger(passenger.get_entity().entity_id)
        );
        vehicle
            .get_entity()
            .remove_passenger_on_disconnect(passenger.get_entity().entity_id);
        *passenger.get_entity().vehicle.lock().unwrap() = None;
        first_world.level.shutdown().await;
        second_world.level.shutdown().await;
    }

    #[tokio::test]
    async fn modern_double_fall_distance_restores_and_overrides_legacy_alias() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let entity = from_type(
            &EntityType::ENDERMITE,
            Vector3::default(),
            &world,
            Uuid::new_v4(),
        );
        let mut nbt = NbtCompound::new();
        nbt.put_double("fall_distance", 12.75);
        nbt.put_float("FallDistance", 1.0);
        entity.read_nbt_non_mut(&nbt);
        let living = entity.get_living_entity().unwrap();
        assert_eq!(living.fall_distance.load(), 12.75);
        living.fall_distance.store(4.5);
        let mut saved = NbtCompound::new();
        entity.write_nbt(&mut saved);
        assert_eq!(saved.get_double("fall_distance"), Some(4.5));
        assert_eq!(saved.get_float("FallDistance"), Some(4.5));
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn live_entity_snapshots_keep_opaque_fields_and_do_not_revive_cleared_state() {
        use pumpkin_nbt::{Nbt, deserializer::NbtReadHelperJava};
        use pumpkin_util::{text::TextComponent, version::JavaMinecraftVersion};
        use std::io::Cursor;

        let directory = tempfile::tempdir().unwrap();
        let world = test_world(&directory.path().join("source"));
        let destination = test_world(&directory.path().join("destination"));
        let name = TextComponent::text("Farm")
            .add_child(TextComponent::text(" plain"))
            .add_child(TextComponent::text(" styled").bold());
        let mut fixture = minecart_fixture();
        fixture.put(
            "CustomName",
            name.to_nbt_tag_for_version(&JavaMinecraftVersion::V_26_3),
        );
        fixture.put_list("Tags", vec![NbtTag::String("old-tag".into())]);
        let mut plugin = NbtCompound::new();
        plugin.put_int("opaque-plugin-key", 42);
        fixture.put_compound("BukkitValues", plugin);
        let mut opaque = NbtCompound::new();
        opaque.put("precise", NbtTag::LongArray(vec![i64::MIN, i64::MAX]));
        opaque.put_list("ordered", vec![NbtTag::Byte(3), NbtTag::Byte(1)]);
        fixture.put_compound("UnmanagedVanillaData", opaque.clone());
        let mut passenger = fixture.get_list("Passengers").unwrap()[0]
            .extract_compound()
            .unwrap()
            .clone();
        passenger.put_compound("UnmanagedPassengerData", opaque.clone());
        fixture.put_list("Passengers", vec![NbtTag::Compound(passenger)]);
        let wire = Nbt::from(fixture).try_write_preserving().unwrap();
        let mut reader = NbtReadHelperJava::new_preserving(Cursor::new(wire.as_ref()));
        let raw = Nbt::read_complete(&mut reader).unwrap();
        let loaded = load_entity_tree(&raw, &world).unwrap();
        assert_eq!(
            loaded[0].get_entity().custom_name.load().as_ref(),
            &Some(name.clone())
        );
        let saved = save_entity_tree(&loaded[0]).unwrap();
        assert_eq!(saved.get_compound("UnmanagedVanillaData"), Some(&opaque));
        let child = saved.get_list("Passengers").unwrap()[0]
            .extract_compound()
            .unwrap();
        assert_eq!(child.get_compound("UnmanagedPassengerData"), Some(&opaque));
        // Fresh snapshots must also be valid disk NBT when the name has mixed children.
        assert!(Nbt::from(saved).try_write_preserving().is_ok());

        let root = loaded[0].get_entity();
        root.custom_name.store(Arc::new(None));
        root.scoreboard_tags.lock().unwrap().clear();
        root.custom_data.lock().unwrap().child_tags.clear();
        root.remove_passenger_on_disconnect(loaded[1].get_entity().entity_id);
        root.set_world(destination.clone());
        root.set_pos(Vector3::new(100.0, 80.0, 200.0));
        for _ in 0..2 {
            let saved = save_entity_tree(&loaded[0]).unwrap();
            for key in [
                "CustomName",
                "Tags",
                "BukkitValues",
                "PumpkinCustomData",
                "Passengers",
            ] {
                assert!(saved.get(key).is_none(), "revived {key}");
            }
            assert_eq!(saved.get_compound("UnmanagedVanillaData"), Some(&opaque));
            assert_eq!(
                saved.get_list("Pos").unwrap()[0].extract_double(),
                Some(100.0)
            );
        }
        let passenger = save_entity_tree(&loaded[1]).unwrap();
        assert_eq!(
            passenger.get_compound("UnmanagedPassengerData"),
            Some(&opaque)
        );
        world.level.shutdown().await;
        destination.level.shutdown().await;
    }

    #[tokio::test]
    async fn clearing_imported_ownerless_tame_state_is_saved() {
        use crate::entity::passive::{tamable::TamableAnimal, wolf::WolfEntity};

        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let mut fixture = minecart_fixture();
        fixture.child_tags.remove("Passengers");
        fixture.put_string("id", "minecraft:wolf".into());
        fixture.put_bool("IsTame", true);
        let loaded = load_entity_tree(&fixture, &world).unwrap();
        let wolf = loaded[0].cast_any().downcast_ref::<WolfEntity>().unwrap();
        assert!(wolf.is_tame());
        wolf.set_tame(false);
        let saved = save_entity_tree(&loaded[0]).unwrap();
        assert_eq!(saved.get_bool("IsTame"), Some(false));
        let reloaded = load_entity_tree(&saved, &world).unwrap();
        assert!(
            !reloaded[0]
                .cast_any()
                .downcast_ref::<WolfEntity>()
                .unwrap()
                .is_tame()
        );
        world.level.shutdown().await;
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
        let loaded = world.load_entity_records(root_pos, vec![minecart_fixture()], None);
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
        let reloaded = world.load_entity_records(root_pos, records, None);
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
        let loaded = world.load_entity_records(root_pos, vec![minecart_fixture()], None);
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
        assert!(world.load_entity_chunk(&chunk, None).unwrap().is_empty());
        assert!(world.load_entity_chunk(&chunk, None).is_none());
        assert!(world.entities.load().is_empty());
        for _ in 0..3 {
            world.save_entities_by_chunk(&[], [position]).await;
            assert_eq!(*chunk.data.lock().unwrap(), original);
        }
        world.remove_entities_in_chunks([position]).await;
        assert!(world.preserved_entity_records.get(&position).is_none());
        world.save_entities_by_chunk(&[], [position]).await;
        assert_eq!(*chunk.data.lock().unwrap(), original);
        assert!(world.load_entity_chunk(&chunk, None).unwrap().is_empty());
        world.save_entities_by_chunk(&[], [position]).await;
        assert_eq!(*chunk.data.lock().unwrap(), original);
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn removed_vehicle_does_not_hide_its_surviving_passenger() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let loaded = world.load_entity_records(Vector2::new(19, 0), vec![minecart_fixture()], None);
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
            let last_saved_snapshot = chunk.data.lock().unwrap().clone();
            let loaded = world.load_entity_chunk(&chunk, None).unwrap();
            assert_eq!(loaded.len(), 2);
            assert_eq!(world.entities.load().len(), 2);
            assert_eq!(*chunk.data.lock().unwrap(), last_saved_snapshot);
            assert!(world.load_entity_chunk(&chunk, None).is_none());
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
        let loaded = world.load_entity_records(root_position, vec![minecart_fixture()], None);
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
        world.load_entity_records(passenger_position, vec![other, unknown.clone()], None);
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
    async fn loaded_removal_fixture(
        world: &World,
        live_pos: BlockPos,
        dormant_pos: BlockPos,
        retained_pos: BlockPos,
    ) -> Arc<ChunkData> {
        use crate::block::entities::{
            BlockEntity, barrel::BarrelBlockEntity, furnace::FurnaceBlockEntity,
        };
        use pumpkin_data::{Block, item::Item, item_stack::ItemStack};
        use pumpkin_inventory::Inventory;
        use pumpkin_world::chunk::io::Dirtiable;

        let position = live_pos.chunk_position();
        let terrain = Arc::new(ChunkData::empty(0, 0));
        let furnace = Arc::new(FurnaceBlockEntity::new(live_pos));
        furnace.set_stack(0, ItemStack::new(7, &Item::IRON_ORE));
        let dormant = Arc::new(BarrelBlockEntity::new(dormant_pos));
        let retained = Arc::new(BarrelBlockEntity::new(retained_pos));
        retained.set_stack(0, ItemStack::new(9, &Item::DIAMOND));
        let fixtures: [(Arc<dyn BlockEntity>, _); 3] = [
            (furnace, Block::FURNACE.default_state.id),
            (dormant, Block::BARREL.default_state.id),
            (retained, Block::BARREL.default_state.id),
        ];
        for (entity, state) in fixtures {
            let block_pos = entity.get_position();
            terrain.set_block_absolute_y(block_pos.0.x as usize, 64, 1, state);
            let mut nbt = NbtCompound::new();
            entity.write_internal(&mut nbt);
            nbt.put_string("opaque", "retained only while this instance exists".into());
            let mut custom = NbtCompound::new();
            custom.put_int("marker", 1);
            nbt.put_compound("PumpkinCustomData", custom);
            terrain
                .pending_block_entities
                .lock()
                .unwrap()
                .insert(block_pos, nbt);
        }
        terrain.mark_dirty(true);
        world
            .level
            .write_chunks(vec![(position, terrain.clone())])
            .await
            .unwrap();
        world.level.loaded_chunks.insert(position, terrain.clone());
        assert!(!terrain.is_dirty());
        terrain
    }

    // A single blocking worker makes filesystem and serialization pauses controlled.
    fn storage_race_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    async fn pause_blocking_worker() -> (std::sync::mpsc::Sender<()>, tokio::task::JoinHandle<()>) {
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let (started, ready) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = started.send(());
            let _ = blocked.recv();
        });
        ready.await.unwrap();
        (release, blocker)
    }

    async fn load_scheduled_terrain(world: &World, position: Vector2<i32>) -> Arc<ChunkData> {
        use pumpkin_world::chunk_system::{ChunkLoading, StagedChunkEnum};

        // Drive the actual scheduler with the center's resolved ticket level;
        // no neighboring world generation is needed for this save regression.
        let loaded = world
            .level
            .chunk_listener
            .add_single_chunk_listener(position);
        world.level.level_channel.set_level((
            std::iter::once((position, (StagedChunkEnum::None, StagedChunkEnum::Full))).collect(),
            std::iter::once((position, ChunkLoading::FULL_CHUNK_LEVEL)).collect(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(10), loaded)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }

    async fn saved_barrel_terrain(
        world: &World,
        position: Vector2<i32>,
        block_pos: BlockPos,
    ) -> Arc<ChunkData> {
        use crate::block::entities::{BlockEntity, barrel::BarrelBlockEntity};
        use pumpkin_data::{Block, item::Item, item_stack::ItemStack};
        use pumpkin_inventory::Inventory;
        use pumpkin_world::chunk::io::Dirtiable;

        let terrain = Arc::new(ChunkData::empty(0, 0));
        terrain.light_populated.store(true, Ordering::Relaxed);
        terrain.set_block_absolute_y(1, 64, 1, Block::BARREL.default_state.id);
        let original = BarrelBlockEntity::new(block_pos);
        original.set_stack(0, ItemStack::new(1, &Item::DIAMOND));
        let mut nbt = NbtCompound::new();
        original.write_internal(&mut nbt);
        terrain
            .pending_block_entities
            .lock()
            .unwrap()
            .insert(block_pos, nbt);
        terrain.mark_dirty(true);
        world
            .level
            .write_chunks(vec![(position, terrain)])
            .await
            .unwrap();

        load_scheduled_terrain(world, position).await
    }

    fn saved_barrel_count(chunk: &ChunkData, block_pos: BlockPos) -> u8 {
        use crate::block::entities::{BlockEntity, barrel::BarrelBlockEntity};
        use pumpkin_inventory::Inventory;

        let nbt = chunk
            .pending_block_entities
            .lock()
            .unwrap()
            .get(&block_pos)
            .unwrap()
            .clone();
        BarrelBlockEntity::from_nbt(&nbt, block_pos)
            .get_stack(0)
            .item_count
    }

    async fn pause_inventory_tick(
        world: Arc<World>,
        position: Vector2<i32>,
        block_pos: BlockPos,
    ) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        use pumpkin_data::{item::Item, item_stack::ItemStack};

        let (finish_tick, finishing) = std::sync::mpsc::channel::<()>();
        let (tick_started, tick_ready) = tokio::sync::oneshot::channel();
        let tick = std::thread::spawn(move || {
            let _ticks = world.level.block_entity_tick_lock.lock().unwrap();
            let entity = world
                .block_entities
                .get(&position)
                .unwrap()
                .get(&block_pos)
                .unwrap()
                .clone();
            let _ = tick_started.send(());
            let _ = finishing.recv();
            entity
                .get_inventory()
                .unwrap()
                .set_stack(0, ItemStack::new(64, &Item::DIAMOND));
        });
        tick_ready.await.unwrap();
        (finish_tick, tick)
    }

    #[tokio::test]
    async fn removed_live_and_dormant_block_entities_stay_removed_after_disk_reload() {
        use pumpkin_util::math::position::BlockPos;
        use pumpkin_world::chunk::io::{Dirtiable, FileIO, LoadedData};

        let directory = tempfile::tempdir().unwrap();
        let world = test_world_in_dimension(directory.path(), Dimension::OVERWORLD);
        let position = Vector2::new(0, 0);
        let live_pos = BlockPos::new(1, 64, 1);
        let dormant_pos = BlockPos::new(2, 64, 1);
        let retained_pos = BlockPos::new(3, 64, 1);
        let terrain = loaded_removal_fixture(&world, live_pos, dormant_pos, retained_pos).await;
        let live = world.get_block_entity(&live_pos).unwrap();
        assert_eq!(live.get_inventory().unwrap().get_stack(0).item_count, 7);
        assert!(world.preserved_block_entity_nbt.contains_key(&live_pos));
        assert!(world.custom_block_entity_data.contains_key(&live_pos));

        world.remove_block_entity(&live_pos);
        world.remove_block_entity(&live_pos);
        world.remove_block_entity(&dormant_pos);
        assert!(world.get_block_entity(&live_pos).is_none());
        assert!(world.get_block_entity(&dormant_pos).is_none());
        assert!(!world.preserved_block_entity_nbt.contains_key(&live_pos));
        assert!(!world.custom_block_entity_data.contains_key(&live_pos));
        assert!(terrain.is_dirty());
        assert_eq!(terrain.pending_block_entities.lock().unwrap().len(), 1);
        world.save_block_entities(position);
        world
            .level
            .write_chunks(vec![(position, terrain)])
            .await
            .unwrap();
        world.level.shutdown().await;

        // A fresh manager forces a disk read instead of satisfying it from the
        // previous region serializer's cache.
        let reloaded_world = test_world_in_dimension(directory.path(), Dimension::OVERWORLD);
        let (send, mut receive) = tokio::sync::mpsc::channel(1);
        reloaded_world
            .level
            .chunk_saver
            .fetch_chunks(&reloaded_world.level.level_folder, &[position], send)
            .await;
        let Some(LoadedData::Loaded(terrain)) = receive.recv().await else {
            panic!("saved terrain must reload successfully");
        };
        assert_eq!(terrain.pending_block_entities.lock().unwrap().len(), 1);
        reloaded_world.level.loaded_chunks.insert(position, terrain);
        assert!(reloaded_world.get_block_entity(&live_pos).is_none());
        assert!(reloaded_world.get_block_entity(&dormant_pos).is_none());
        let retained = reloaded_world.get_block_entity(&retained_pos).unwrap();
        assert_eq!(
            retained
                .clone()
                .get_inventory()
                .unwrap()
                .get_stack(0)
                .item_count,
            9
        );
        assert_eq!(
            reloaded_world
                .snapshot_block_entity_nbt(&retained)
                .get_string("opaque"),
            Some("retained only while this instance exists")
        );
        reloaded_world.level.shutdown().await;
    }

    #[tokio::test]
    async fn cleared_legacy_villager_home_aliases_do_not_return_after_repeated_saves() {
        use crate::entity::{ai::brain::memory::types, passive::villager::VillagerEntity};
        use pumpkin_util::math::position::BlockPos;

        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let mut fixture = minecart_fixture();
        fixture.child_tags.remove("Passengers");
        fixture.put_string("id", "minecraft:villager".into());
        fixture.put_int("BedX", 301);
        fixture.put_int("BedY", 65);
        fixture.put_int("BedZ", 2);
        fixture.put_string("opaque", "keep villager metadata".into());
        let loaded = load_entity_tree(&fixture, &world).unwrap();
        let villager = loaded[0]
            .cast_any()
            .downcast_ref::<VillagerEntity>()
            .unwrap();
        assert_eq!(villager.get_home(), Some(BlockPos::new(301, 65, 2)));
        *villager.home_pos.lock().unwrap() = None;
        villager
            .mob_entity
            .brain
            .lock()
            .unwrap()
            .erase(types::HOME.id());
        for _ in 0..2 {
            let saved = save_entity_tree(&loaded[0]).unwrap();
            for key in ["BedX", "BedY", "BedZ", "HomeX", "HomeY", "HomeZ"] {
                assert!(saved.get(key).is_none(), "cleared home alias {key}");
            }
            assert!(
                saved
                    .get_compound("Brain")
                    .unwrap()
                    .get_compound("memories")
                    .unwrap()
                    .get("minecraft:home")
                    .is_none()
            );
            assert_eq!(saved.get_string("opaque"), Some("keep villager metadata"));
            let reloaded = load_entity_tree(&saved, &world).unwrap();
            assert!(reloaded[0].get_mob().unwrap().get_home().is_none());
        }
        world.level.shutdown().await;
    }

    #[test]
    fn container_snapshot_precedes_terrain_eviction_while_world_save_waits_for_disk() {
        use pumpkin_data::{item::Item, item_stack::ItemStack};
        use pumpkin_util::math::position::BlockPos;
        use pumpkin_world::{chunk::io::Dirtiable, chunk_system::StagedChunkEnum};
        use std::time::Duration;

        // A single blocking worker lets the test pause real async file I/O,
        // while the scheduler and Tokio workers continue processing unloads.
        let runtime = storage_race_runtime();
        runtime.block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let world = test_world_in_dimension(directory.path(), Dimension::OVERWORLD);
            world
                .level
                .world_portal
                .store(Arc::new(Some(Arc::new(super::super::WorldPortal(
                    world.clone(),
                )))));
            let position = Vector2::new(0, 0);
            let block_pos = BlockPos::new(1, 64, 1);
            let loaded = saved_barrel_terrain(&world, position, block_pos).await;
            let container = world.get_block_entity(&block_pos).unwrap();
            let inventory = container.clone().get_inventory().unwrap();
            inventory.set_stack(0, ItemStack::new(32, &Item::DIAMOND));
            let entity_chunk = world.level.get_entity_chunk(position).await;
            entity_chunk.mark_dirty(true);

            let (release, blocker) = pause_blocking_worker().await;
            let mut saving = Box::pin(world.save());
            assert!(futures::poll!(&mut saving).is_pending());
            assert!(
                world.entity_storage_lock.try_lock().is_err(),
                "world save must be paused inside entity disk I/O"
            );
            let mut cleanup = Box::pin(world.remove_unwatched_entities_in_chunks([position]));
            assert!(futures::poll!(&mut cleanup).is_pending());

            // Model the tail of an already-cloned parallel block-entity batch.
            // The production tick takes this fence before cloning any entities
            // and releases it only after every hopper/furnace tick has joined.
            let (finish_tick, tick) =
                pause_inventory_tick(world.clone(), position, block_pos).await;

            world.level.level_channel.set_level((
                std::iter::once((position, (StagedChunkEnum::Full, StagedChunkEnum::None)))
                    .collect(),
                std::collections::HashMap::default(),
            ));
            assert!(
                tokio::time::timeout(Duration::from_secs(2), async {
                    while world.level.is_chunk_loaded(&position) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .is_err(),
                "terrain cannot be retired while an already-cloned block entity can still mutate"
            );
            drop(finish_tick);
            tick.join().unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                while world.level.is_chunk_loaded(&position) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert!(!world.block_entities.contains_key(&position));
            assert!(
                world.get_block_entity(&block_pos).is_none(),
                "an unloaded terrain entry cannot revive its retired live map"
            );
            // The canonical instance must already carry the live inventory,
            // although both World::save and entity cleanup are still blocked.
            assert_eq!(saved_barrel_count(&loaded, block_pos), 64);
            drop(release);
            blocker.await.unwrap();
            saving.await;
            cleanup.await;

            // Reload through the scheduler, whose I/O barrier waits for the
            // queued terrain commit before permitting the read.
            let reloaded = load_scheduled_terrain(&world, position).await;
            assert_eq!(saved_barrel_count(&reloaded, block_pos), 64);
            let revived = world.get_block_entity(&block_pos).unwrap();
            assert!(!Arc::ptr_eq(&container, &revived));
            assert_eq!(revived.get_inventory().unwrap().get_stack(0).item_count, 64);
            world.level.shutdown().await;
            world.level.world_portal.store(Arc::new(None));
        });
    }

    #[tokio::test]
    async fn queued_watch_registration_after_disconnect_cannot_keep_chunks_alive() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let position = Vector2::new(19, 0);
        let desired = std::sync::Mutex::new(std::iter::once(position).collect::<FxHashSet<_>>());
        let saving = world.entity_storage_lock.lock().await;
        let mut movement =
            Box::pin(world.reconcile_entity_chunk_watches(1, || desired.lock().unwrap().clone()));
        assert!(futures::poll!(&mut movement).is_pending());

        // Disconnect removes world membership before awaiting watch release.
        // Its current desired set is therefore empty when the queued task resumes.
        desired.lock().unwrap().clear();
        let mut disconnect = Box::pin(world.reconcile_entity_chunk_watches(1, FxHashSet::default));
        assert!(futures::poll!(&mut disconnect).is_pending());
        drop(saving);
        let (movement, disconnect) = tokio::join!(movement, disconnect);
        assert!(movement.0.is_empty());
        assert!(disconnect.0.is_empty());
        assert!(!world.player_entity_chunk_watches.contains_key(&1));
        assert!(!world.level.is_chunk_watched(&position));
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn reordered_movements_and_world_transfer_balance_only_owned_watches() {
        let old_directory = tempfile::tempdir().unwrap();
        let new_directory = tempfile::tempdir().unwrap();
        let old_world = test_world(old_directory.path());
        let new_world = test_world(new_directory.path());
        let a = Vector2::new(0, 0);
        let b = Vector2::new(1, 0);
        let c = Vector2::new(2, 0);
        let d = Vector2::new(3, 0);
        old_world
            .reconcile_entity_chunk_watches(1, || [a, b].into_iter().collect())
            .await;
        old_world
            .reconcile_entity_chunk_watches(2, || std::iter::once(b).collect())
            .await;
        let desired = std::sync::Mutex::new([b, c].into_iter().collect::<FxHashSet<_>>());
        let first_movement =
            old_world.reconcile_entity_chunk_watches(1, || desired.lock().unwrap().clone());
        *desired.lock().unwrap() = [c, d].into_iter().collect();
        // A later task executes first. The older task must observe the current
        // section, not replay the earlier b/c transition or duplicate counters.
        old_world
            .reconcile_entity_chunk_watches(1, || desired.lock().unwrap().clone())
            .await;
        assert!(first_movement.await.0.is_empty());
        assert!(!old_world.level.is_chunk_watched(&a));
        assert!(old_world.level.is_chunk_watched(&b));
        assert!(old_world.level.is_chunk_watched(&c));
        assert!(old_world.level.is_chunk_watched(&d));

        let saving = old_world.entity_storage_lock.lock().await;
        let mut queued_old_world = Box::pin(
            old_world.reconcile_entity_chunk_watches(1, || desired.lock().unwrap().clone()),
        );
        assert!(futures::poll!(&mut queued_old_world).is_pending());
        // Transfer removes membership in the old world before publishing new
        // watches. Both worlds use the same player ID but own separate references.
        desired.lock().unwrap().clear();
        new_world
            .reconcile_entity_chunk_watches(1, || std::iter::once(a).collect())
            .await;
        let mut release_old =
            Box::pin(old_world.reconcile_entity_chunk_watches(1, FxHashSet::default));
        assert!(futures::poll!(&mut release_old).is_pending());
        drop(saving);
        tokio::join!(queued_old_world, release_old);
        assert!(!old_world.level.is_chunk_watched(&c));
        assert!(!old_world.level.is_chunk_watched(&d));
        assert!(old_world.level.is_chunk_watched(&b));
        assert!(new_world.level.is_chunk_watched(&a));
        // Repeated cleanup cannot decrement the other player's only reference.
        old_world
            .reconcile_entity_chunk_watches(1, FxHashSet::default)
            .await;
        assert!(old_world.level.is_chunk_watched(&b));
        old_world
            .reconcile_entity_chunk_watches(2, FxHashSet::default)
            .await;
        new_world
            .reconcile_entity_chunk_watches(1, FxHashSet::default)
            .await;
        assert!(!old_world.level.is_chunk_watched(&b));
        assert!(!new_world.level.is_chunk_watched(&a));
        assert!(old_world.player_entity_chunk_watches.is_empty());
        assert!(new_world.player_entity_chunk_watches.is_empty());
        old_world.level.shutdown().await;
        new_world.level.shutdown().await;
    }

    #[tokio::test]
    async fn queued_unwatched_cleanup_does_not_remove_rewatched_live_entities() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let position = Vector2::new(19, 0);
        let chunk = world.level.get_entity_chunk(position).await;
        chunk.data.lock().unwrap().push(minecart_fixture());
        let loaded = world.load_entity_chunk(&chunk, None).unwrap();
        assert_eq!(loaded.len(), 2);

        // A revival owns the lifecycle lock while an old unload queues behind it.
        let revival = world.entity_storage_lock.lock().await;
        let mut cleanup = Box::pin(world.remove_unwatched_entities_in_chunks([position]));
        assert!(futures::poll!(&mut cleanup).is_pending());
        world.level.mark_chunks_as_newly_watched(&[position]).await;
        drop(revival);
        cleanup.await;
        assert!(chunk.live.load(Ordering::Relaxed));
        assert_eq!(world.entities.load().len(), 2);

        world.level.mark_chunks_as_not_watched([position]).await;
        world.remove_unwatched_entities_in_chunks([position]).await;
        assert!(!chunk.live.load(Ordering::Relaxed));
        assert!(world.entities.load().is_empty());
        world.level.shutdown().await;
    }

    fn crossing_frog_fixture(uuid: Uuid, x: f64) -> NbtCompound {
        let mut entity = NbtCompound::new();
        entity.put_string("id", "minecraft:frog".into());
        entity.put_uuid("UUID", uuid);
        entity.put("Pos", NbtTag::List(vec![x.into(), 64.0.into(), 0.5.into()]));
        entity.put_float("Health", 10.0);
        entity.put_string("opaque", "retained across unload and reload".into());
        entity
    }

    async fn load_crossing_frog(world: &Arc<World>, uuid: Uuid, x: f64) -> Arc<dyn EntityBase> {
        let position = Vector2::new((x.floor() as i32) >> 4, 0);
        let chunk = world.level.get_entity_chunk(position).await;
        chunk
            .data
            .lock()
            .unwrap()
            .push(crossing_frog_fixture(uuid, x));
        let loaded = world.load_entity_chunk(&chunk, None).unwrap();
        assert_eq!(loaded.len(), 1);
        loaded.into_iter().next().unwrap()
    }

    fn stored_uuids(world: &World, positions: &[Vector2<i32>]) -> Vec<Uuid> {
        let mut uuids = Vec::new();
        for position in positions {
            let chunk = world.level.get_entity_chunk_sync(position).unwrap();
            uuids.extend(
                chunk
                    .data
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|entity| entity.get_uuid("UUID").unwrap()),
            );
        }
        uuids.sort_unstable();
        uuids
    }

    #[tokio::test]
    async fn unload_crossing_frog_keeps_source_and_live_neighbor_records() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let a = Vector2::new(0, 0);
        let b = Vector2::new(1, 0);
        let p = load_crossing_frog(&world, Uuid::from_u128(1001), 15.75).await;
        let q = load_crossing_frog(&world, Uuid::from_u128(1002), 17.5).await;
        let moving = p.clone();
        // Reproduce the old post-detach/pre-grouping position change exactly.
        // The new snapshot must use its frozen source key, not partially replace B.
        *world.entity_unload_snapshot_hook.lock().unwrap() = Some(Box::new(move || {
            moving
                .get_entity()
                .set_pos(pumpkin_util::math::vector3::Vector3::new(16.5, 64.0, 0.5));
        }));
        world.remove_unwatched_entities_in_chunks([a]).await;
        assert_eq!(world.entities.load().len(), 1);
        assert_eq!(
            world.entities.load()[0].get_entity().entity_uuid,
            q.get_entity().entity_uuid
        );
        assert_eq!(
            stored_uuids(&world, &[a, b]),
            vec![Uuid::from_u128(1001), Uuid::from_u128(1002)],
            "unload crossing must retain both root UUIDs without replacing the live neighbor"
        );
        world.save().await;
        assert_eq!(
            stored_uuids(&world, &[a, b]),
            vec![Uuid::from_u128(1001), Uuid::from_u128(1002)]
        );

        // Reload the detached tree, then let a complete save relocate it into B.
        let chunk = world.level.get_entity_chunk(a).await;
        let reloaded = world.load_entity_chunk(&chunk, None).unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(
            reloaded[0].get_entity().entity_uuid,
            p.get_entity().entity_uuid
        );
        world.save().await;
        assert!(
            world
                .level
                .get_entity_chunk(a)
                .await
                .data
                .lock()
                .unwrap()
                .is_empty()
        );
        let destination = world.level.get_entity_chunk(b).await;
        {
            let records = destination.data.lock().unwrap();
            assert_eq!(records.len(), 2);
            assert!(
                records.iter().all(|entity| entity.get_string("opaque")
                    == Some("retained across unload and reload"))
            );
        }
        // A new serializer reads actual committed files, without the world's cache.
        use pumpkin_config::chunk::AnvilChunkConfig;
        use pumpkin_world::chunk::{
            ChunkEntityData,
            format::anvil::AnvilChunkFile,
            io::{FileIO, LoadedData, file_manager::ChunkFileManager},
        };
        let reader =
            ChunkFileManager::<AnvilChunkFile<ChunkEntityData>>::new(AnvilChunkConfig::default());
        let (send, mut stream) = tokio::sync::mpsc::channel(2);
        reader
            .fetch_chunks(&world.level.level_folder, &[a, b], send)
            .await;
        let mut persisted = Vec::new();
        while let Some(result) = stream.recv().await {
            let LoadedData::Loaded(chunk) = result else {
                panic!("saved entity chunks must parse from disk");
            };
            persisted.extend(
                chunk
                    .data
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|entity| entity.get_uuid("UUID").unwrap()),
            );
        }
        persisted.sort_unstable();
        assert_eq!(
            persisted,
            vec![Uuid::from_u128(1001), Uuid::from_u128(1002)]
        );
        world.level.shutdown().await;
    }

    #[test]
    fn unload_waits_for_cloned_tick_before_classifying_crossing_entities() {
        let runtime = storage_race_runtime();
        runtime.block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let world = test_world(directory.path());
            let a = Vector2::new(0, 0);
            let b = Vector2::new(1, 0);
            let p = load_crossing_frog(&world, Uuid::from_u128(1001), 15.75).await;
            load_crossing_frog(&world, Uuid::from_u128(1002), 17.5).await;
            let (finish, finishing) = std::sync::mpsc::channel::<()>();
            let (ready, started) = tokio::sync::oneshot::channel();
            let ticking = world.clone();
            let tick = std::thread::spawn(move || {
                let _fence = ticking.entity_tick_fence.lock();
                let _snapshot = ticking.entities.load_full();
                ready.send(()).unwrap();
                if finishing.recv().is_ok() {
                    p.get_entity()
                        .set_pos(pumpkin_util::math::vector3::Vector3::new(16.5, 64.0, 0.5));
                }
            });
            started.await.unwrap();
            assert!(
                !world.stop_entity_ticks_for_unload(),
                "synchronous unload must not wait on its calling tick"
            );
            let cleanup_world = world.clone();
            let cleanup = tokio::spawn(async move {
                cleanup_world.remove_unwatched_entities_in_chunks([a]).await;
            });
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while world.entity_storage_lock.try_lock().is_ok() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            finish.send(()).unwrap();
            tick.join().unwrap();
            cleanup.await.unwrap();
            assert_eq!(world.entities.load().len(), 2);
            world.save().await;
            assert_eq!(
                stored_uuids(&world, &[a, b]),
                vec![Uuid::from_u128(1001), Uuid::from_u128(1002)]
            );
            assert!(world.stop_entity_ticks_for_unload());
            world.level.shutdown().await;
        });
    }

    #[tokio::test]
    async fn unload_does_not_resurrect_a_legitimately_removed_crossing_frog() {
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let a = Vector2::new(0, 0);
        let b = Vector2::new(1, 0);
        let p = load_crossing_frog(&world, Uuid::from_u128(1001), 15.75).await;
        load_crossing_frog(&world, Uuid::from_u128(1002), 17.5).await;
        world.remove_entity(p.as_ref());
        world.remove_unwatched_entities_in_chunks([a]).await;
        world.save().await;
        assert_eq!(stored_uuids(&world, &[a, b]), vec![Uuid::from_u128(1002)]);
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn snapshot_waits_for_tick_and_includes_new_roots_without_blocking_runtime() {
        let directory = tempfile::tempdir().unwrap();
        // No dragon-fight I/O may return Pending before the actual fence wait.
        let world = test_world_in_dimension(directory.path(), Dimension::OVERWORLD);
        let a = Vector2::new(0, 0);
        let b = Vector2::new(1, 0);
        load_crossing_frog(&world, Uuid::from_u128(1001), 15.75).await;
        world
            .level
            .get_entity_chunk(b)
            .await
            .live
            .store(true, Ordering::Relaxed);
        let new_root =
            load_entity_tree(&crossing_frog_fixture(Uuid::from_u128(1002), 17.5), &world)
                .unwrap()
                .into_iter()
                .next()
                .unwrap();
        let (finish, finishing) = std::sync::mpsc::channel::<()>();
        let (ready, started) = tokio::sync::oneshot::channel();
        let ticking = world.clone();
        let runtime = tokio::runtime::Handle::current();
        let tick = std::thread::spawn(move || {
            let _runtime = runtime.enter();
            let _fence = ticking.entity_tick_fence.lock();
            ready.send(()).unwrap();
            if finishing.recv().is_ok() {
                assert!(ticking.spawn_entity(new_root));
            }
        });
        started.await.unwrap();
        let mut saving = Box::pin(world.save());
        assert!(futures::poll!(&mut saving).is_pending());
        // This single-threaded async executor is still usable while the tick
        // waits for an event. A blocking mutex acquisition here would deadlock.
        finish.send(()).unwrap();
        tick.join().unwrap();
        saving.await;
        assert_eq!(
            stored_uuids(&world, &[a, b]),
            vec![Uuid::from_u128(1001), Uuid::from_u128(1002)]
        );
        world.level.shutdown().await;
    }

    #[tokio::test]
    async fn autosave_during_unload_cannot_replace_detached_entities_with_empty_snapshot() {
        use pumpkin_world::chunk::io::Dirtiable;
        let directory = tempfile::tempdir().unwrap();
        let world = test_world(directory.path());
        let position = Vector2::new(0, 0);
        let chunk = world.level.get_entity_chunk(position).await;
        let records = vec![minecart_fixture()];
        *chunk.data.lock().unwrap() = records.clone();
        chunk.live.store(true, Ordering::Relaxed);
        chunk.mark_dirty(true);

        // This is the unload boundary: the live list has been detached, but the
        // chunk stays live until remove_entities_in_chunks finishes its snapshot.
        let unloading = world.entity_storage_lock.lock().await;
        assert!(world.entities.load().is_empty());
        let mut saving = Box::pin(world.save());
        assert!(futures::poll!(&mut saving).is_pending());
        assert_eq!(
            *chunk.data.lock().unwrap(),
            records,
            "autosave must wait instead of snapshotting the temporarily empty live list"
        );

        chunk.live.store(false, Ordering::Relaxed);
        drop(unloading);
        saving.await;
        assert_eq!(*chunk.data.lock().unwrap(), records);
        assert!(
            !chunk.is_dirty(),
            "manual save must flush entity snapshots to disk"
        );
        let original_path = world.level.level_folder.entities_folder.join("r.0.0.mca");
        assert!(tokio::fs::metadata(original_path).await.unwrap().len() > 8192);
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
            preserved_tags: std::sync::Mutex::new(NbtCompound::new()),
            live: std::sync::atomic::AtomicBool::new(false),
            dirty: std::sync::atomic::AtomicBool::new(true),
        });
        world
            .level
            .write_entity_chunks(vec![(position, stored)])
            .await
            .unwrap();
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

        let loaded = world.load_entity_chunk(&first, None).unwrap();
        assert_eq!(loaded.len(), 2);
        loaded[0]
            .get_entity()
            .remove_passenger_on_disconnect(loaded[1].get_entity().entity_id);
        loaded[0]
            .get_entity()
            .set_custom_name(pumpkin_util::text::TextComponent::text("Changed cart"));
        loaded[1].get_entity().remove();
        assert!(world.load_entity_chunk(&second, None).is_none());
        assert!(world.preserved_entity_records.get(&position).is_none());
        world.remove_entities_in_chunks([position]).await;
        let saved = first.data.lock().unwrap().clone();
        assert_eq!(saved.len(), 1);
        assert!(saved[0].get_list("Passengers").is_none());
        assert_eq!(saved[0].get_string("CustomName"), Some("Changed cart"));
        world
            .level
            .write_entity_chunks(vec![(position, first.clone())])
            .await
            .unwrap();
        world.level.clean_entity_chunks([position]);
        let mut stream_a = world.level.receive_entity_chunks(vec![position]);
        let mut stream_b = world.level.receive_entity_chunks(vec![position]);
        let (a, b) = tokio::join!(stream_a.recv(), stream_b.recv());
        let a = a.unwrap().0.upgrade().unwrap();
        let b = b.unwrap().0.upgrade().unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        let reloaded = world.load_entity_chunk(&a, None).unwrap();
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].get_entity().entity_uuid, Uuid::from_u128(1));
        assert!(world.load_entity_chunk(&b, None).is_none());
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
    #[test]
    fn revival_during_queued_cleanup_retains_the_complete_disk_snapshot() {
        use pumpkin_config::chunk::AnvilChunkConfig;
        use pumpkin_world::chunk::{
            ChunkEntityData,
            format::anvil::AnvilChunkFile,
            io::{Dirtiable, FileIO, LoadedData, file_manager::ChunkFileManager},
        };
        use std::time::Duration;

        let runtime = storage_race_runtime();
        runtime.block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let world = test_world(directory.path());
            let position = Vector2::new(19, 0);
            let chunk = world.level.get_entity_chunk(position).await;
            let mut unsupported = NbtCompound::new();
            unsupported.put_string("id", "custom:unknown_entity".into());
            unsupported.put_uuid("UUID", Uuid::from_u128(90));
            unsupported.put_string("opaque", "keep unsupported entity data".into());
            let records = vec![minecart_fixture(), unsupported.clone()];
            *chunk.data.lock().unwrap() = records.clone();
            chunk.mark_dirty(true);
            world
                .level
                .write_entity_chunks(vec![(position, chunk.clone())])
                .await
                .unwrap();
            chunk.mark_dirty(true);
            assert_eq!(Arc::strong_count(&chunk), 2);

            // Occupy the only blocking worker before cleanup can read its region
            // or serialize the selected chunk. The async cleanup still reaches
            // its write await and owns both selected/save-list references.
            let (release, blocker) = pause_blocking_worker().await;
            world.level.clean_entity_chunks([position]);
            tokio::time::timeout(Duration::from_secs(10), async {
                while Arc::strong_count(&chunk) < 4 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();

            let lifecycle = world.entity_storage_lock.lock().await;
            world.level.mark_chunks_as_newly_watched(&[position]).await;
            let loaded = world.load_entity_chunk(&chunk, None).unwrap();
            assert_eq!(loaded.len(), 2);
            assert!(world.load_entity_chunk(&chunk, None).is_none());
            assert_eq!(world.entities.load().len(), 2);
            assert_eq!(
                world
                    .preserved_entity_records
                    .get(&position)
                    .unwrap()
                    .as_slice(),
                &[unsupported.clone()]
            );
            drop(lifecycle);
            drop(release);
            blocker.await.unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                while chunk.is_dirty() || Arc::strong_count(&chunk) != 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();

            // Read with a fresh serializer before World::save or shutdown can
            // repair the file. This checks committed disk data, not the old cache.
            let reader = ChunkFileManager::<AnvilChunkFile<ChunkEntityData>>::new(
                AnvilChunkConfig::default(),
            );
            let (send, mut receive) = tokio::sync::mpsc::channel(1);
            reader
                .fetch_chunks(&world.level.level_folder, &[position], send)
                .await;
            let Some(LoadedData::Loaded(persisted)) = receive.recv().await else {
                panic!("cleanup must leave a readable entity region");
            };
            assert_eq!(*persisted.data.lock().unwrap(), records);

            // Keeping the old snapshot must not replay a passenger killed while
            // live: the next normal unload replaces it with the live tree.
            loaded[0]
                .get_entity()
                .remove_passenger_on_disconnect(loaded[1].get_entity().entity_id);
            loaded[1].get_entity().remove();
            world.level.mark_chunks_as_not_watched([position]).await;
            world.remove_unwatched_entities_in_chunks([position]).await;
            let updated = chunk.data.lock().unwrap().clone();
            assert_eq!(updated.len(), 2);
            assert!(updated.contains(&unsupported));
            assert!(
                updated
                    .iter()
                    .all(|record| record.get_list("Passengers").is_none())
            );
            let revived = world.load_entity_chunk(&chunk, None).unwrap();
            assert_eq!(revived.len(), 1);
            assert_eq!(revived[0].get_entity().entity_uuid, Uuid::from_u128(1));
            assert!(world.load_entity_chunk(&chunk, None).is_none());
            world.remove_entities_in_chunks([position]).await;
            world.level.shutdown().await;
        });
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
        let loaded =
            world.load_entity_records(position, vec![stale.clone(), unknown.clone()], None);
        loaded[0]
            .get_entity()
            .set_custom_name(pumpkin_util::text::TextComponent::text("Changed cart"));
        loaded[0]
            .get_entity()
            .remove_passenger_on_disconnect(loaded[1].get_entity().entity_id);
        loaded[1].get_entity().remove();
        assert!(
            world
                .load_entity_records(position, vec![stale], None)
                .is_empty()
        );
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
        let loaded = world.load_entity_chunk(&chunk, None).unwrap();
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
