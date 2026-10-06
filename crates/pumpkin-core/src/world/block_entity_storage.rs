use std::sync::{Arc, Weak};

use pumpkin_nbt::NbtCompound;

use crate::{block::entities::BlockEntity, data::preserved_nbt::PreservedNbt};

use super::World;

pub(super) struct BlockEntityNbt {
    owner: Weak<dyn BlockEntity>,
    preserved: PreservedNbt,
}

impl BlockEntityNbt {
    fn capture(entity: &Arc<dyn BlockEntity>, source: &NbtCompound, modeled: &NbtCompound) -> Self {
        let mut preserved = PreservedNbt::new(source, modeled);
        preserved.discard_aliases(modeled, &[("BukkitValues", "PumpkinCustomData")]);
        preserved.discard_aliases(modeled, entity.nbt_aliases());
        Self {
            owner: Arc::downgrade(entity),
            preserved,
        }
    }

    fn apply(&mut self, entity: &Arc<dyn BlockEntity>, modeled: &mut NbtCompound) {
        // Position alone is not identity: placing another block here must not inherit data.
        if !self.owner.ptr_eq(&Arc::downgrade(entity)) {
            return;
        }
        self.preserved
            .discard_aliases(modeled, &[("BukkitValues", "PumpkinCustomData")]);
        self.preserved
            .discard_aliases(modeled, entity.nbt_aliases());
        self.preserved
            .discard_removed_dependencies(modeled, entity.nbt_dependencies());
        self.preserved.snapshot(modeled);
    }
}

impl World {
    pub(super) fn retire_chunk_block_entities(
        &self,
        position: pumpkin_util::math::vector2::Vector2<i32>,
    ) {
        if let dashmap::mapref::entry::Entry::Occupied(entry) = self.block_entities.entry(position)
        {
            // Keep the instance map locked until its metadata is gone; a replacement
            // at the same position must not be retired by an older unload.
            for position in entry.get().keys() {
                self.preserved_block_entity_nbt.remove(position);
                self.custom_block_entity_data.remove(position);
            }
            entry.remove();
        }
    }

    fn modeled_block_entity_nbt(&self, entity: &Arc<dyn BlockEntity>) -> NbtCompound {
        let mut modeled = NbtCompound::new();
        entity.write_internal(&mut modeled);
        if let Some(custom) = self.custom_block_entity_data.get(&entity.get_position())
            && !custom.is_empty()
        {
            modeled.put_compound("PumpkinCustomData", custom.clone());
        }
        modeled
    }

    pub(super) fn capture_block_entity_nbt(
        &self,
        entity: &Arc<dyn BlockEntity>,
        source: &NbtCompound,
    ) {
        let modeled = self.modeled_block_entity_nbt(entity);
        self.preserved_block_entity_nbt.insert(
            entity.get_position(),
            BlockEntityNbt::capture(entity, source, &modeled),
        );
    }

    pub(super) fn snapshot_block_entity_nbt(&self, entity: &Arc<dyn BlockEntity>) -> NbtCompound {
        let mut modeled = self.modeled_block_entity_nbt(entity);
        if let Some(mut saved) = self
            .preserved_block_entity_nbt
            .get_mut(&entity.get_position())
        {
            saved.apply(entity, &mut modeled);
        }
        modeled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::entities::end_gateway::EndGatewayBlockEntity;
    use pumpkin_util::math::position::BlockPos;

    #[test]
    fn spawner_snapshots_keep_unmodeled_spawn_data_and_clear_removed_branches() {
        use crate::block::entities::mob_spawner::MobSpawnerBlockEntity;

        let position = BlockPos::new(1, 64, 2);
        let mut mob = NbtCompound::new();
        mob.put_string("id", "minecraft:pig".into());
        mob.put_string("CustomName", "Named spawn".into());
        mob.put_bool("PersistenceRequired", true);
        let mut rules = NbtCompound::new();
        rules.put_int("opaque_rule", 3);
        let mut spawn_data = NbtCompound::new();
        spawn_data.put_compound("entity", mob.clone());
        spawn_data.put_compound("custom_spawn_rules", rules.clone());
        let mut source = NbtCompound::new();
        let mut potential = NbtCompound::new();
        potential.put_int("weight", 7);
        potential.put_compound("data", spawn_data.clone());
        let potentials = vec![pumpkin_nbt::tag::NbtTag::Compound(potential)];
        source.put_list("SpawnPotentials", potentials.clone());
        source.put_compound("SpawnData", spawn_data);
        source.put_string("EntityId", "minecraft:pig".into());
        let entity: Arc<dyn BlockEntity> =
            Arc::new(MobSpawnerBlockEntity::from_nbt(&source, position));
        let spawner = entity
            .as_any()
            .downcast_ref::<MobSpawnerBlockEntity>()
            .unwrap();
        let mut initial = NbtCompound::new();
        entity.write_internal(&mut initial);
        let mut saved = BlockEntityNbt::capture(&entity, &source, &initial);
        spawner
            .delay
            .store(99, std::sync::atomic::Ordering::Relaxed);
        let mut current = NbtCompound::new();
        entity.write_internal(&mut current);
        saved.apply(&entity, &mut current);
        assert_eq!(current.get_short("Delay"), Some(99));
        assert_eq!(
            current.get_list("SpawnPotentials"),
            Some(potentials.as_slice())
        );
        let spawn_data = current.get_compound("SpawnData").unwrap();
        assert_eq!(spawn_data.get_compound("entity"), Some(&mob));
        assert_eq!(spawn_data.get_compound("custom_spawn_rules"), Some(&rules));
        assert!(current.get("EntityId").is_none());

        spawner.entity_type.store(None);
        for _ in 0..2 {
            let mut current = NbtCompound::new();
            entity.write_internal(&mut current);
            saved.apply(&entity, &mut current);
            assert!(current.get("SpawnData").is_none());
            assert!(current.get("SpawnPotentials").is_none());
            assert!(current.get("EntityId").is_none());
            let reloaded = MobSpawnerBlockEntity::from_nbt(&current, position);
            assert!(reloaded.entity_type.load().is_none());
        }
        spawner.set_entity_type(&pumpkin_data::entity::EntityType::COW);
        let mut readded = NbtCompound::new();
        entity.write_internal(&mut readded);
        saved.apply(&entity, &mut readded);
        assert!(readded.get("SpawnPotentials").is_none());
    }

    #[test]
    fn cleared_gateway_aliases_stay_deleted_and_replacements_do_not_inherit_raw_data() {
        let position = BlockPos::new(1, 64, 2);
        let mut portal = NbtCompound::new();
        portal.put_int("X", 100);
        portal.put_int("Y", 80);
        portal.put_int("Z", 200);
        let mut source = NbtCompound::new();
        source.put_compound("ExitPortal", portal);
        source.put_bool("ExactTeleport", true);
        source.put_string("opaque", "keep".into());
        let entity: Arc<dyn BlockEntity> =
            Arc::new(EndGatewayBlockEntity::from_nbt(&source, position));
        let mut initial = NbtCompound::new();
        entity.write_internal(&mut initial);
        let mut saved = BlockEntityNbt::capture(&entity, &source, &initial);
        let gateway = entity
            .as_any()
            .downcast_ref::<EndGatewayBlockEntity>()
            .unwrap();
        *gateway.exit_portal.lock().unwrap() = None;
        *gateway.exact_teleport.lock().unwrap() = false;
        let mut changed = NbtCompound::new();
        entity.write_internal(&mut changed);
        saved.apply(&entity, &mut changed);
        assert_eq!(changed.get_string("opaque"), Some("keep"));
        for removed in ["ExitPortal", "exit_portal", "ExactTeleport"] {
            assert!(changed.get(removed).is_none());
        }
        let reloaded = EndGatewayBlockEntity::from_nbt(&changed, position);
        assert!(reloaded.exit_portal.lock().unwrap().is_none());
        let replacement: Arc<dyn BlockEntity> = Arc::new(EndGatewayBlockEntity::new(position));
        let mut fresh = NbtCompound::new();
        replacement.write_internal(&mut fresh);
        saved.apply(&replacement, &mut fresh);
        assert!(fresh.get("opaque").is_none());
    }
}
