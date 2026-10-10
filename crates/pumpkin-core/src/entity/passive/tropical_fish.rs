use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, AtomicI32, Ordering},
};

use pumpkin_data::{
    data_component_impl::{
        CustomNameImpl, TropicalFishBaseColorImpl, TropicalFishPatternColorImpl,
        TropicalFishPatternImpl,
    },
    dye_color::DyeColor,
    entity::EntityType,
    item_stack::ItemStack,
};
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

pub struct TropicalFishEntity {
    pub mob_entity: MobEntity,
    pub from_bucket: AtomicBool,
    pub variant: AtomicI32,
}

impl TropicalFishEntity {
    pub(crate) fn save_to_bucket_tag(&self, stack: &mut ItemStack) {
        let variant = self.variant.load(Ordering::Relaxed);
        stack.set_data_component(TropicalFishPatternImpl::from_packed_id(variant & 0xffff));
        stack.set_data_component(TropicalFishBaseColorImpl {
            value: DyeColor::by_id(((variant >> 16) & 0xff) as u8)
                .unwrap_or(DyeColor::White)
                .name()
                .into(),
        });
        stack.set_data_component(TropicalFishPatternColorImpl {
            value: DyeColor::by_id(((variant >> 24) & 0xff) as u8)
                .unwrap_or(DyeColor::White)
                .name()
                .into(),
        });
        if let Some(name) = &**self.get_entity().custom_name.load() {
            stack.set_data_component(CustomNameImpl { name: name.clone() });
        }
    }

    pub(crate) fn load_from_bucket_stack(&self, stack: &ItemStack) {
        let mut variant = self.variant.load(Ordering::Relaxed);
        if let Some(pattern) = stack.get_data_component::<TropicalFishPatternImpl>()
            && let Some(id) = pattern.packed_id()
        {
            variant = (variant & !0xffff) | id;
        }
        if let Some(color) = stack.get_data_component::<TropicalFishBaseColorImpl>()
            && let Some(color) = DyeColor::by_name(&color.value)
        {
            variant = (variant & !(0xff << 16)) | (i32::from(color.id()) << 16);
        }
        if let Some(color) = stack.get_data_component::<TropicalFishPatternColorImpl>()
            && let Some(color) = DyeColor::by_name(&color.value)
        {
            variant = (variant & !(0xff << 24)) | (i32::from(color.id()) << 24);
        }
        let mut nbt = NbtCompound::new();
        nbt.put_int("Variant", variant);
        self.mob_read_nbt(&nbt);
        if let Some(name) = stack.get_custom_name() {
            self.get_entity().set_custom_name(name.clone());
        }
    }

    pub fn new(entity: Entity) -> Arc<Self> {
        let mob_entity = MobEntity::new(entity);
        let tropical_fish = Self {
            mob_entity,
            from_bucket: AtomicBool::new(false),
            variant: AtomicI32::new(0),
        };
        let mob_arc = Arc::new(tropical_fish);
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

impl Mob for TropicalFishEntity {
    fn requires_custom_persistence(&self) -> bool {
        self.from_bucket.load(Ordering::Relaxed)
    }

    fn remove_when_far_away(&self, _distance_sq: f64) -> bool {
        !self.requires_custom_persistence() && (**self.get_entity().custom_name.load()).is_none()
    }

    fn mob_write_nbt(&self, nbt: &mut NbtCompound) {
        nbt.put_bool("FromBucket", self.from_bucket.load(Ordering::Relaxed));
        nbt.put_int("Variant", self.variant.load(Ordering::Relaxed));
    }

    fn mob_read_nbt(&self, nbt: &NbtCompound) {
        if let Some(from_bucket) = nbt.get_bool("FromBucket") {
            self.from_bucket.store(from_bucket, Ordering::Relaxed);
            self.get_entity().set_synced_data(
                pumpkin_data::tracked_data::abstract_fish::FROM_BUCKET,
                from_bucket,
            );
        }
        if let Some(variant) = nbt.get_int("Variant") {
            self.variant.store(variant, Ordering::Relaxed);
            self.get_entity().set_synced_data(
                pumpkin_data::tracked_data::tropical_fish::DATA_ID_TYPE_VARIANT,
                VarInt(variant),
            );
        }
    }

    fn get_mob_entity(&self) -> &MobEntity {
        &self.mob_entity
    }
}
