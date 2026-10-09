use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, AtomicI32, Ordering},
};

use pumpkin_data::entity::EntityType;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_protocol::codec::var_int::VarInt;

use crate::entity::{
    Entity, EntityBase,
    ai::goal::{
        look_around::RandomLookAroundGoal, look_at_entity::LookAtEntityGoal, swim::SwimGoal,
        wander_around::WanderAroundGoal,
    },
    mob::{Mob, MobEntity},
};

/// Represents a Pufferfish, a passive aquatic mob that can inflate when threatened.
///
/// Wiki: <https://minecraft.wiki/w/Pufferfish>
pub struct PufferfishEntity {
    pub mob_entity: MobEntity,
    pub from_bucket: AtomicBool,
    pub puff_state: AtomicI32,
}

impl PufferfishEntity {
    pub fn new(entity: Entity) -> Arc<Self> {
        let mob_entity = MobEntity::new(entity);
        let pufferfish = Self {
            mob_entity,
            from_bucket: AtomicBool::new(false),
            puff_state: AtomicI32::new(0),
        };
        let mob_arc = Arc::new(pufferfish);
        let mob_weak: Weak<dyn Mob> = {
            let mob_arc: Arc<dyn Mob> = mob_arc.clone();
            Arc::downgrade(&mob_arc)
        };

        {
            let mut goal_selector = mob_arc
                .mob_entity
                .goals_selector
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            goal_selector.add_goal(0, Box::new(SwimGoal::default()));
            goal_selector.add_goal(1, Box::new(WanderAroundGoal::new(1.0)));
            goal_selector.add_goal(
                2,
                LookAtEntityGoal::with_default(mob_weak, &EntityType::PLAYER, 6.0),
            );
            goal_selector.add_goal(3, Box::new(RandomLookAroundGoal::default()));
        };

        mob_arc
    }
}

impl Mob for PufferfishEntity {
    fn requires_custom_persistence(&self) -> bool {
        self.from_bucket.load(Ordering::Relaxed)
    }

    fn remove_when_far_away(&self, _distance_sq: f64) -> bool {
        !self.requires_custom_persistence() && (**self.get_entity().custom_name.load()).is_none()
    }

    fn mob_write_nbt(&self, nbt: &mut NbtCompound) {
        nbt.put_bool("FromBucket", self.from_bucket.load(Ordering::Relaxed));
        nbt.put_int("PuffState", self.puff_state.load(Ordering::Relaxed));
    }

    fn mob_read_nbt(&self, nbt: &NbtCompound) {
        if let Some(puff_state) = nbt.get_int("PuffState") {
            let puff_state = puff_state.clamp(0, 2);
            self.puff_state.store(puff_state, Ordering::Relaxed);
            self.get_entity().set_synced_data(
                pumpkin_data::tracked_data::pufferfish::PUFF_STATE,
                VarInt(puff_state),
            );
        }
        if let Some(from_bucket) = nbt.get_bool("FromBucket") {
            self.from_bucket.store(from_bucket, Ordering::Relaxed);
            self.get_entity().set_synced_data(
                pumpkin_data::tracked_data::abstract_fish::FROM_BUCKET,
                from_bucket,
            );
        }
    }

    fn get_mob_entity(&self) -> &MobEntity {
        &self.mob_entity
    }
}
