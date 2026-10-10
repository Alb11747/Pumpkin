#[allow(clippy::wildcard_imports)]
use super::*;
use pumpkin_data::attributes::Attributes;
use pumpkin_protocol::java::server::play::SSpectatorAction;

impl JavaClient {
    pub fn handle_spectator_action(&self, player: &Arc<Player>, packet: &SSpectatorAction) {
        if !player.has_client_loaded() || player.gamemode.load() != GameMode::Spectator {
            return;
        }
        player.update_last_action_time();

        // Vanilla's absent target is an attack with no entity hit, not a camera exit.
        let Some(target_id) = packet.spectate_entity_id else {
            return;
        };
        let world = player.world();
        let Some(target) = world.get_entity_by_id(target_id) else {
            return;
        };
        let target_entity = target.get_entity();
        if target_entity.is_removed()
            || target.is_spectator()
            || (target.get_living_entity().is_none() && !target.can_hit())
        {
            return;
        }

        let target_block = target_entity.block_pos.load();
        if !world
            .worldborder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(f64::from(target_block.0.x), f64::from(target_block.0.z))
        {
            return;
        }

        let range = player
            .living_entity
            .get_attribute_value(&Attributes::ENTITY_INTERACTION_RANGE)
            + 3.0;
        let in_range = target_entity
            .bounding_box
            .load()
            .squared_magnitude(player.get_eye_pos())
            < range * range;
        if !in_range {
            return;
        }
        if player.get_camera_entity_id() == target_id {
            return;
        }

        // Vanilla setCamera teleports with the player's rotation, then refreshes chunk tracking.
        let player_entity = player.get_entity();
        player.request_teleport(
            target_entity.pos.load(),
            player_entity.yaw.load(),
            player_entity.pitch.load(),
        );
        chunker::update_position(player);
        player.set_camera_entity_id(target_id);
    }
}
