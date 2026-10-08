//! Opt-in, read-only lifecycle evidence for a bounded set of entity UUIDs.
//! Enable the `pumpkin_entity_audit` debug target and provide comma-separated
//! `PUMPKIN_ENTITY_AUDIT_UUIDS` only on a disposable validation server.

use std::sync::{Arc, OnceLock};

use pumpkin_nbt::compound::NbtCompound;
use pumpkin_util::math::vector2::Vector2;
use rustc_hash::FxHashSet;
use tracing::Level;
use uuid::Uuid;

use super::{World, entity_storage};
use crate::entity::{EntityBase, RemovalReason, living::LivingEntity};

const TARGET: &str = "pumpkin_entity_audit";

fn parse_targets(value: &str) -> Result<FxHashSet<Uuid>, String> {
    if value.trim().is_empty() {
        return Ok(FxHashSet::default());
    }
    value
        .split(',')
        .map(|value| Uuid::parse_str(value.trim()))
        .collect::<Result<_, _>>()
        .map_err(|error| format!("Invalid entity audit UUID setting: {error}"))
}

fn targets() -> Result<&'static FxHashSet<Uuid>, &'static String> {
    static UUIDS: OnceLock<Result<FxHashSet<Uuid>, String>> = OnceLock::new();
    UUIDS
        .get_or_init(|| match std::env::var("PUMPKIN_ENTITY_AUDIT_UUIDS") {
            Ok(value) => parse_targets(&value),
            Err(std::env::VarError::NotPresent) => Ok(FxHashSet::default()),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err("Entity audit UUID setting is not Unicode".into())
            }
        })
        .as_ref()
}

pub(super) fn configure() -> Result<(), pumpkin_world::world_info::WorldInfoError> {
    if !tracing::enabled!(target: TARGET, Level::DEBUG) {
        return Ok(());
    }
    targets().map(|_| ()).map_err(|error| {
        pumpkin_world::world_info::WorldInfoError::DeserializationError(error.clone())
    })
}

pub(super) fn enabled() -> bool {
    tracing::enabled!(target: TARGET, Level::DEBUG) && targets().is_ok_and(|set| !set.is_empty())
}

fn selected(uuid: Uuid) -> bool {
    // World::load validates this optional diagnostic configuration before use.
    enabled() && targets().is_ok_and(|set| set.contains(&uuid))
}

pub(super) fn live(world: &World, entity: &Arc<dyn EntityBase>, stage: &str, reason: &str) {
    if !selected(entity.get_entity().entity_uuid) {
        return;
    }
    emit_live(
        world,
        entity.as_ref(),
        stage,
        reason,
        Some(entity_storage::entity_storage_chunk(entity)),
    );
}

fn emit_live(
    world: &World,
    entity: &dyn EntityBase,
    stage: &str,
    reason: &str,
    save_chunk: Option<Vector2<i32>>,
) {
    let base = entity.get_entity();
    if !selected(base.entity_uuid) {
        return;
    }
    let dimensions = base.entity_dimension.load();
    let living = entity.get_living_entity();
    let damage = living.map(LivingEntity::last_damage_snapshot);
    tracing::debug!(target: TARGET,
        stage, reason, uuid = %base.entity_uuid, world = %world.uuid,
        entity_id = base.entity_id, pos = ?base.pos.load(),
        chunk = ?base.chunk_pos.load(), ?save_chunk,
        health = ?living.map(|living| living.health.load()),
        last_damage_type = ?damage.as_ref().and_then(|(kind, _)| kind.as_deref()),
        last_damage_tick = ?damage.as_ref().map(|(_, tick)| *tick),
        world_tick = world.get_world_age(),
        width = dimensions.width, height = dimensions.height,
        eye_height = dimensions.eye_height,
        "entity_audit"
    );
}

pub(super) fn removal(world: &World, entity: &dyn EntityBase, reason: RemovalReason) {
    if !selected(entity.get_entity().entity_uuid) {
        return;
    }
    let reason = match reason {
        RemovalReason::Killed => "Killed",
        RemovalReason::Discarded => "Discarded",
        RemovalReason::UnloadedToChunk => "UnloadedToChunk",
        RemovalReason::UnloadedWithPlayer => "UnloadedWithPlayer",
        RemovalReason::ChangedDimension => "ChangedDimension",
    };
    // Entity::remove receives the base Entity. Resolve its still-live wrapper
    // before membership removal so the observer can read health and damage.
    let entities = world.entities.load();
    let entity = entities
        .iter()
        .find(|candidate| candidate.get_entity().entity_uuid == entity.get_entity().entity_uuid)
        .map_or(entity, |entity| entity.as_ref());
    emit_live(world, entity, "removal", reason, None);
}

pub(super) fn rejected(world: &World, record: &NbtCompound, outcome: &str, detail: &str) {
    if !enabled() {
        return;
    }
    let mut records = vec![record];
    while let Some(record) = records.pop() {
        if let Some(uuid) = record.get_uuid("UUID")
            && selected(uuid)
        {
            tracing::debug!(target: TARGET,
                stage = "admission", reason = outcome, detail, %uuid, world = %world.uuid,
                health = ?record.get_float("Health"), pos = ?record.get_list("Pos"),
                "entity_audit"
            );
        }
        if let Some(passengers) = record.get_list("Passengers") {
            records.extend(passengers.iter().filter_map(|tag| tag.extract_compound()));
        }
    }
}

#[cfg(test)]
mod tests {
    use rustc_hash::FxHashSet;

    use super::parse_targets;

    #[test]
    fn selector_rejects_partial_or_invalid_uuid_configuration() {
        let valid = "00000000-0000-0000-0000-000000000001, 00000000-0000-0000-0000-000000000002";
        assert_eq!(parse_targets(valid).as_ref().map(FxHashSet::len), Ok(2));
        assert_eq!(parse_targets("").as_ref().map(FxHashSet::len), Ok(0));
        assert!(parse_targets(&format!("{valid},invalid")).is_err());
        assert!(parse_targets(&format!("{valid},")).is_err());
    }
}
