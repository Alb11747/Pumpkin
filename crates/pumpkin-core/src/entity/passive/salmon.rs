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

pub struct SalmonEntity {
    pub mob_entity: MobEntity,
    pub from_bucket: AtomicBool,
    pub variant: AtomicI32,
}

impl SalmonEntity {
    pub fn new(entity: Entity) -> Arc<Self> {
        let mob_entity = MobEntity::new(entity);
        let salmon = Self {
            mob_entity,
            from_bucket: AtomicBool::new(false),
            variant: AtomicI32::new(1),
        };
        let mob_arc = Arc::new(salmon);
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

impl Mob for SalmonEntity {
    fn requires_custom_persistence(&self) -> bool {
        self.from_bucket.load(Ordering::Relaxed)
    }

    fn remove_when_far_away(&self, _distance_sq: f64) -> bool {
        !self.requires_custom_persistence() && (**self.get_entity().custom_name.load()).is_none()
    }

    fn mob_write_nbt(&self, nbt: &mut NbtCompound) {
        nbt.put_bool("FromBucket", self.from_bucket.load(Ordering::Relaxed));
        let name = match self.variant.load(Ordering::Relaxed) {
            0 => "small",
            2 => "large",
            _ => "medium",
        };
        nbt.put_string("type", name.into());
    }

    fn mob_read_nbt(&self, nbt: &NbtCompound) {
        if let Some(name) = nbt.get_string("type") {
            let variant = match name {
                "small" => 0,
                "large" => 2,
                _ => 1,
            };
            self.variant.store(variant, Ordering::Relaxed);
            self.get_entity().set_synced_data(
                pumpkin_data::tracked_data::salmon::DATA_TYPE,
                VarInt(variant),
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
