use std::sync::{Arc, Weak, atomic::Ordering};

use pumpkin_data::{
    entity::EntityPose,
    sound::{Sound, SoundCategory},
};
use uuid::Uuid;

use super::{Controls, Goal};
use crate::entity::{
    EntityBase, RemovalReason, ai::target_predicate::TargetPredicate, mob::Mob,
    passive::frog::FrogEntity,
};

const TIME_OUT_DURATION: i32 = 100;
const CATCH_ANIMATION_DURATION: i32 = 6;
const TONGUE_ANIMATION_DURATION: i32 = 10;
const EATING_DISTANCE: f64 = 1.75;
const EATING_MOVEMENT_FACTOR: f64 = 0.75;
const UNREACHABLE_TONGUE_TARGETS_COOLDOWN_DURATION: i64 = 100;
const MAX_UNREACHBLE_TONGUE_TARGETS_IN_MEMORY: usize = 5;
const TARGET_DETECTION_DISTANCE: f64 = 10.0;
const DEFAULT_SCAN_RATE: i64 = 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    MoveToTarget,
    CatchAnimation(i32),
    EatAnimation(i32),
    Done,
}

impl State {
    /// Returns true only on the transition that bites, including vanilla's post-increment timer.
    const fn tick_animation(&mut self) -> bool {
        match *self {
            Self::CatchAnimation(timer) if timer >= CATCH_ANIMATION_DURATION => {
                *self = Self::EatAnimation(timer + 1);
                true
            }
            Self::CatchAnimation(timer) => {
                *self = Self::CatchAnimation(timer + 1);
                false
            }
            Self::EatAnimation(timer) if timer >= TONGUE_ANIMATION_DURATION => {
                *self = Self::Done;
                false
            }
            Self::EatAnimation(timer) => {
                *self = Self::EatAnimation(timer + 1);
                false
            }
            Self::MoveToTarget | Self::Done => false,
        }
    }
}

/// Goal-system equivalent of vanilla `ShootTongue` and `FrogAttackablesSensor`.
pub struct FrogShootTongueGoal {
    frog: Weak<FrogEntity>,
    target: Option<Arc<dyn EntityBase>>,
    state: State,
    ticks: i32,
    calculate_path_counter: i32,
    next_scan: i64,
    unreachable_targets: Vec<Uuid>,
    unreachable_until: i64,
}

impl FrogShootTongueGoal {
    #[must_use]
    pub fn new(frog: Weak<FrogEntity>) -> Self {
        Self {
            frog,
            target: None,
            state: State::Done,
            ticks: 0,
            calculate_path_counter: 0,
            next_scan: 0,
            unreachable_targets: Vec::new(),
            unreachable_until: 0,
        }
    }

    fn target_is_alive(target: &dyn EntityBase) -> bool {
        target.get_entity().is_alive()
            && target.get_living_entity().is_some_and(|living| {
                !living.dead.load(Ordering::Relaxed) && living.health.load() > 0.0
            })
    }

    fn add_unreachable_target(&mut self, uuid: Uuid, now: i64) {
        if !self.unreachable_targets.contains(&uuid) {
            if self.unreachable_targets.len() == MAX_UNREACHBLE_TONGUE_TARGETS_IN_MEMORY {
                self.unreachable_targets.remove(0);
            }
            self.unreachable_targets.push(uuid);
        }
        self.unreachable_until = now + UNREACHABLE_TONGUE_TARGETS_COOLDOWN_DURATION;
    }

    fn eat_entity(frog: &FrogEntity, target: &dyn EntityBase) {
        let entity = frog.get_entity();
        let world = entity.world.load();
        world.play_sound_fine(
            Sound::EntityFrogEat,
            SoundCategory::Neutral,
            &entity.pos.load(),
            2.0,
            1.0,
        );
        if Self::target_is_alive(target) && FrogEntity::can_eat(target) {
            frog.mob_entity.try_attack(frog, target);
            if !Self::target_is_alive(target) {
                // Damage owns death events and loot; removal only ends the death animation early.
                world.remove_entity_with_reason(target, RemovalReason::Killed);
            }
        }
    }
}

impl Goal for FrogShootTongueGoal {
    fn can_start(&mut self, mob: &dyn Mob) -> bool {
        let entity = mob.get_entity();
        let world = entity.world.load();
        let now = world.get_world_age();
        if now < self.next_scan
            || mob.is_panicking()
            || mob.get_mob_entity().is_in_love()
            || matches!(
                entity.pose.load(),
                EntityPose::Croaking | EntityPose::LongJumping
            )
        {
            return false;
        }
        self.next_scan = now + DEFAULT_SCAN_RATE;
        if now >= self.unreachable_until {
            self.unreachable_targets.clear();
        }
        let predicate =
            TargetPredicate::create_attackable().set_base_max_distance(TARGET_DETECTION_DISTANCE);
        let pos = entity.pos.load();
        let target = world
            .get_entities_at_box(&entity.bounding_box.load().expand(
                TARGET_DETECTION_DISTANCE,
                TARGET_DETECTION_DISTANCE,
                TARGET_DETECTION_DISTANCE,
            ))
            .into_iter()
            .filter(|target| {
                FrogEntity::can_eat(target.as_ref())
                    && Self::target_is_alive(target.as_ref())
                    && !self
                        .unreachable_targets
                        .contains(&target.get_entity().entity_uuid)
                    && pos.squared_distance_to_vec(&target.get_entity().pos.load())
                        < TARGET_DETECTION_DISTANCE * TARGET_DETECTION_DISTANCE
                    && predicate.test(&world, Some(mob), target.as_ref())
            })
            .min_by(|a, b| {
                pos.squared_distance_to_vec(&a.get_entity().pos.load())
                    .total_cmp(&pos.squared_distance_to_vec(&b.get_entity().pos.load()))
            });
        let Some(target) = target else {
            return false;
        };
        let path = mob
            .get_mob_entity()
            .navigator
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .create_path(
                &mob.get_mob_entity().living_entity,
                target.get_entity().pos.load(),
                0,
            );
        if !path.is_some_and(|path| f64::from(path.get_dist_to_target()) < EATING_DISTANCE) {
            self.add_unreachable_target(target.get_entity().entity_uuid, now);
            return false;
        }
        self.target = Some(target);
        true
    }

    fn start(&mut self, mob: &dyn Mob) {
        self.state = State::MoveToTarget;
        self.ticks = 0;
        self.calculate_path_counter = 0;
        if let (Some(frog), Some(target)) = (self.frog.upgrade(), &self.target) {
            frog.set_tongue_target(Some(target.get_entity().entity_id));
            mob.get_mob_entity().set_target(Some(target.clone()));
        }
    }

    fn should_continue(&mut self, mob: &dyn Mob) -> bool {
        self.state != State::Done
            && self.ticks < TIME_OUT_DURATION
            && !mob.is_panicking()
            && self.target.as_ref().is_some_and(|target| {
                // A successful bite removes the target but the frog still finishes its animation.
                matches!(self.state, State::EatAnimation(_))
                    || Self::target_is_alive(target.as_ref())
            })
    }

    fn tick(&mut self, mob: &dyn Mob) {
        self.ticks += 1;
        let (Some(frog), Some(target)) = (self.frog.upgrade(), &self.target) else {
            return;
        };
        let entity = frog.get_entity();
        let target_entity = target.get_entity();
        mob.get_mob_entity()
            .look_control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .look_at_entity_with_range(target, 45.0, 90.0);
        if self.state == State::MoveToTarget {
            if entity
                .pos
                .load()
                .squared_distance_to_vec(&target_entity.pos.load())
                < EATING_DISTANCE * EATING_DISTANCE
            {
                entity.world.load().play_sound_fine(
                    Sound::EntityFrogTongue,
                    SoundCategory::Neutral,
                    &entity.pos.load(),
                    2.0,
                    1.0,
                );
                entity.set_pose(EntityPose::UsingTongue);
                target_entity.set_velocity(
                    (entity.pos.load() - target_entity.pos.load()).normalize()
                        * EATING_MOVEMENT_FACTOR,
                );
                mob.get_mob_entity()
                    .navigator
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .stop();
                self.state = State::CatchAnimation(0);
            } else if self.calculate_path_counter <= 0 {
                mob.get_mob_entity()
                    .navigator
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .move_to_coords(
                        target_entity.pos.load().x,
                        target_entity.pos.load().y,
                        target_entity.pos.load().z,
                        2.0,
                        &mob.get_mob_entity().living_entity,
                    );
                self.calculate_path_counter = 10;
            } else {
                self.calculate_path_counter -= 1;
            }
        } else if self.state.tick_animation() {
            Self::eat_entity(&frog, target.as_ref());
        }
    }

    fn stop(&mut self, mob: &dyn Mob) {
        self.target = None;
        self.state = State::Done;
        mob.get_mob_entity().set_target(None);
        mob.get_mob_entity()
            .navigator
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .stop();
        if let Some(frog) = self.frog.upgrade() {
            frog.set_tongue_target(None);
            frog.get_entity().set_pose(EntityPose::Standing);
        }
    }

    fn should_run_every_tick(&self) -> bool {
        true
    }
    fn controls(&self) -> Controls {
        Controls::MOVE | Controls::LOOK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tongue_bites_once_and_finishes_after_the_bite() {
        let mut state = State::CatchAnimation(0);
        for _ in 0..6 {
            assert!(!state.tick_animation());
        }
        assert!(state.tick_animation());
        assert!(matches!(state, State::EatAnimation(_)));
        let mut bites = 1;
        for _ in 0..100 {
            bites += i32::from(state.tick_animation());
        }
        assert_eq!(bites, 1);
        assert!(state == State::Done);
    }

    #[test]
    fn unreachable_memory_evicts_oldest_without_duplicating_targets() {
        let mut goal = FrogShootTongueGoal::new(Weak::new());
        for id in 1..=6 {
            goal.add_unreachable_target(Uuid::from_u128(id), 0);
        }
        assert!(!goal.unreachable_targets.contains(&Uuid::from_u128(1)));
        goal.add_unreachable_target(Uuid::from_u128(6), 40);
        assert_eq!(goal.unreachable_targets.len(), 5);
        assert_eq!(goal.unreachable_until, 140);
    }
}
