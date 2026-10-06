use std::sync::Arc;

use pumpkin_util::math::vector2::Vector2;
use pumpkin_world::{chunk_system::ChunkLoading, level::Level};
use tokio_util::sync::CancellationToken;

use super::{ARENA_SIZE_CHUNKS, DragonFight};
use crate::world::World;

impl DragonFight {
    pub(super) fn arena_entity_chunks(&self) -> Vec<Vector2<i32>> {
        let center = self.origin.chunk_position();
        (-1..=1)
            .flat_map(|dx| (-1..=1).map(move |dz| Vector2::new(center.x + dx, center.y + dz)))
            .collect()
    }

    pub(super) fn update_arena_loading(&mut self, world: &Arc<World>, has_players: bool) {
        if has_players == self.arena_load_cancel.is_some() {
            return;
        }
        if !has_players {
            self.stop_arena_loading(&world.level);
            return;
        }
        let center = self.origin.chunk_position();
        // Translate the arena radius through Pumpkin's levels rather than vanilla's level 9.
        let ticket_level =
            ChunkLoading::get_level_from_simulation_distance(ARENA_SIZE_CHUNKS as u8);
        let mut loading = world
            .level
            .chunk_loading
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loading.add_ticket(center, ticket_level);
        loading.send_change();
        drop(loading);
        let cancel = CancellationToken::new();
        self.arena_load_cancel = Some(cancel.clone());
        let chunks = self.arena_entity_chunks();
        let world = world.clone();
        let level = world.level.clone();
        level.clone().spawn_task(async move {
            {
                let _guard = world.entity_storage_lock.lock().await;
                if cancel.is_cancelled() || level.cancel_token.is_cancelled() { return; }
                level.mark_chunks_as_newly_watched(&chunks).await;
            };
            let mut receiver = level.receive_entity_chunks(chunks.clone());
            loop {
                let received = tokio::select! {
                    () = cancel.cancelled() => break,
                    () = level.cancel_token.cancelled() => break,
                    received = receiver.recv() => received,
                };
                let Some((chunk, _)) = received else { break; };
                if let Some(chunk) = chunk.upgrade() {
                    let _guard = world.entity_storage_lock.lock().await;
                    if cancel.is_cancelled() || level.cancel_token.is_cancelled() { break; }
                    world.load_entity_chunk(&chunk, None);
                }
            }
            if !cancel.is_cancelled() && !level.cancel_token.is_cancelled() {
                tokio::select! { () = cancel.cancelled() => {}, () = level.cancel_token.cancelled() => {} }
            }
            let unwatched = {
                let _guard = world.entity_storage_lock.lock().await;
                level.mark_chunks_as_not_watched(&chunks).await
            };
            world.remove_unwatched_entities_in_chunks(&unwatched).await;
            level.clean_entity_chunks(&unwatched);
        });
    }
    pub(in crate::world) fn stop_arena_loading(&mut self, level: &Level) {
        let Some(cancel) = self.arena_load_cancel.take() else {
            return;
        };
        cancel.cancel();
        let ticket_level =
            ChunkLoading::get_level_from_simulation_distance(ARENA_SIZE_CHUNKS as u8);
        let mut loading = level
            .chunk_loading
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loading.remove_ticket(self.origin.chunk_position(), ticket_level);
        loading.send_change();
    }
}
