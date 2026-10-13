use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Weak};

use pumpkin_data::entity::EntityType;
use pumpkin_nbt::compound::NbtCompound;

use crate::entity::{
    Entity, EntityBase,
    ai::goal::{
        active_target::ActiveTargetGoal, look_around::RandomLookAroundGoal,
        look_at_entity::LookAtEntityGoal, melee_attack::MeleeAttackGoal, swim::SwimGoal,
        wander_around::WanderAroundGoal,
    },
    mob::{Mob, MobEntity},
};

pub struct EndermiteEntity {
    pub mob_entity: MobEntity,
    pub lifetime: AtomicI32,
}

impl EndermiteEntity {
    const MAX_LIFE: i32 = 2400;

    pub fn new(entity: Entity) -> Arc<Self> {
        let mob_entity = MobEntity::new(entity);
        let endermite = Self {
            mob_entity,
            lifetime: AtomicI32::new(0),
        };
        let mob_arc = Arc::new(endermite);
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
            goal_selector.add_goal(4, Box::new(MeleeAttackGoal::new(1.0, true)));
            goal_selector.add_goal(5, Box::new(WanderAroundGoal::new(1.0)));
            goal_selector.add_goal(
                6,
                LookAtEntityGoal::with_default(mob_weak.clone(), &EntityType::PLAYER, 8.0),
            );
            goal_selector.add_goal(7, Box::new(RandomLookAroundGoal::default()));

            let mut target_selector = mob_arc
                .mob_entity
                .target_selector
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            target_selector.add_goal(
                1,
                ActiveTargetGoal::with_default(&mob_arc.mob_entity, &EntityType::PLAYER, true),
            );
        };

        mob_arc
    }
}

impl Mob for EndermiteEntity {
    fn get_mob_entity(&self) -> &MobEntity {
        &self.mob_entity
    }

    fn mob_write_nbt(&self, nbt: &mut NbtCompound) {
        nbt.put_int("Lifetime", self.lifetime.load(Ordering::Relaxed));
    }

    fn mob_read_nbt(&self, nbt: &NbtCompound) {
        if let Some(life) = nbt.get_int("Lifetime") {
            self.lifetime.store(life, Ordering::Relaxed);
        }
    }

    fn mob_tick(&self, _caller: &dyn EntityBase) {
        let entity = &self.mob_entity.living_entity.entity;
        if !entity.is_alive() {
            return;
        }

        let life = if self.mob_entity.persistence_required.load(Ordering::Relaxed) {
            self.lifetime.load(Ordering::Relaxed)
        } else {
            self.lifetime.fetch_add(1, Ordering::Relaxed) + 1
        };
        if life >= Self::MAX_LIFE {
            entity.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{data::VanillaData, server::Server};
    use pumpkin_config::{AdvancedConfiguration, BasicConfiguration, TelemetryConfig};
    use pumpkin_data::dimension::Dimension;
    use pumpkin_util::math::vector3::Vector3;
    use std::sync::RwLock;

    #[tokio::test]
    async fn persistent_endermites_do_not_age_out() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let basic = BasicConfiguration {
            default_level_name: directory
                .path()
                .to_str()
                .ok_or("non-UTF8 test path")?
                .into(),
            allow_nether: false,
            allow_end: false,
            use_favicon: false,
            ..Default::default()
        };
        let mut advanced = AdvancedConfiguration::default();
        advanced.networking.bedrock.online_mode = false;
        let server = Server::new(
            basic,
            advanced,
            TelemetryConfig::default(),
            VanillaData {
                banned_ip_list: RwLock::default(),
                banned_player_list: RwLock::default(),
                operator_config: RwLock::default(),
                user_cache: RwLock::default(),
                whitelist_config: RwLock::default(),
            },
            Vec::new(),
        )
        .await?;
        let world = server.get_world_from_dimension(&Dimension::OVERWORLD);
        let mut observed = Vec::new();
        for (kind, persistent, lifetime) in [
            ("ordinary", false, 2399),
            ("PersistenceRequired", true, 2399),
            ("already expired", true, 2400),
        ] {
            let mob = EndermiteEntity::new(Entity::new(
                world.clone(),
                Vector3::new(0.0, 100.0, 0.0),
                &EntityType::ENDERMITE,
            ));
            let entity = mob.get_entity();
            mob.mob_entity
                .persistence_required
                .store(persistent, Ordering::Relaxed);
            world.add_entity_silent(mob.clone());
            mob.lifetime.store(lifetime, Ordering::Relaxed);
            mob.mob_tick(mob.as_ref());
            observed.push((
                kind,
                entity.is_removed(),
                mob.lifetime.load(Ordering::Relaxed),
            ));
            entity.remove();
        }
        server.shutdown().await;
        world.level.world_portal.store(Arc::new(None));
        assert_eq!(
            observed,
            [
                ("ordinary", true, 2400),
                ("PersistenceRequired", false, 2399),
                ("already expired", true, 2400)
            ],
            "persistent endermites must retain their lifetime and remain alive"
        );
        Ok(())
    }
}
