use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, AtomicI32, Ordering},
};

use pumpkin_data::entity::EntityType;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_protocol::codec::var_int::VarInt;
use pumpkin_util::math::boundingbox::{BoundingBox, EntityDimensions};

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
    const STATE_SMALL: i32 = 0;
    const STATE_MID: i32 = 1;
    const STATE_FULL: i32 = 2;

    pub fn new(entity: Entity) -> Arc<Self> {
        let mob_entity = MobEntity::new(entity);
        let pufferfish = Self {
            mob_entity,
            from_bucket: AtomicBool::new(false),
            puff_state: AtomicI32::new(Self::STATE_SMALL),
        };
        pufferfish.refresh_dimensions();
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

    /// Updates puff state and refreshes the body and eye dimensions.
    pub fn set_puff_state(&self, puff_state: i32) {
        let puff_state = puff_state.clamp(Self::STATE_SMALL, Self::STATE_FULL);
        self.puff_state.store(puff_state, Ordering::Relaxed);
        self.get_entity().set_synced_data(
            pumpkin_data::tracked_data::pufferfish::PUFF_STATE,
            VarInt(puff_state),
        );
        self.refresh_dimensions();
    }

    const fn get_scale(puff_state: i32) -> f32 {
        match puff_state {
            Self::STATE_SMALL => 0.5,
            Self::STATE_MID => 0.7,
            _ => 1.0,
        }
    }

    fn refresh_dimensions(&self) {
        let entity = self.get_entity();
        let scale = Self::get_scale(self.puff_state.load(Ordering::Relaxed));
        // Always scale the type defaults so repeated state changes don't compound.
        let base = Entity::type_dimensions(entity.entity_type);
        let dimensions = EntityDimensions::new(
            base.width * scale,
            base.height * scale,
            base.eye_height * scale,
        );
        entity.entity_dimension.store(dimensions);
        let pos = entity.pos.load();
        entity
            .bounding_box
            .store(BoundingBox::new_from_pos(pos.x, pos.y, pos.z, &dimensions));
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
            self.set_puff_state(puff_state);
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
