use std::{collections::BTreeMap, sync::Mutex};

use pumpkin_util::math::position::BlockPos;
use rustc_hash::FxHashSet;

use crate::tick::{OrderedTick, ScheduledTick};

pub struct ChunkTickScheduler<T> {
    inner: Mutex<Option<Box<ChunkTickSchedulerInner<T>>>>,
}

struct ChunkTickSchedulerInner<T> {
    // Signed deadlines preserve overdue ticks until the first step, and do not
    // wrap long delays through a fixed-size wheel. Time shares the queue's lock.
    elapsed: i64,
    tick_queue: BTreeMap<i64, Vec<OrderedTick<T>>>,
    queued_ticks: FxHashSet<(BlockPos, T)>,
}

impl<'a, T: std::hash::Hash + Eq> ChunkTickScheduler<&'a T> {
    pub fn step_tick(&self) -> Vec<OrderedTick<&'a T>> {
        let mut inner_guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(inner) = inner_guard.as_mut() else {
            return Vec::new();
        };

        let mut res = Vec::new();
        while let Some(entry) = inner.tick_queue.first_entry() {
            if *entry.key() > inner.elapsed {
                break;
            }
            res.append(&mut entry.remove());
        }
        inner.elapsed += 1;

        if !res.is_empty() {
            for next_tick in &res {
                inner
                    .queued_ticks
                    .remove(&(next_tick.position, next_tick.value));
            }
            if inner.queued_ticks.is_empty() {
                *inner_guard = None;
            }
        }
        res
    }

    pub fn schedule_tick(&self, tick: &ScheduledTick<&'a T>, sub_tick_order: u64) {
        let mut inner_guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let inner = inner_guard.get_or_insert_with(|| {
            Box::new(ChunkTickSchedulerInner {
                elapsed: 0,
                tick_queue: BTreeMap::new(),
                queued_ticks: FxHashSet::default(),
            })
        });

        if inner.queued_ticks.insert((tick.position, tick.value)) {
            let deadline = inner.elapsed + i64::from(tick.delay);
            inner
                .tick_queue
                .entry(deadline)
                .or_default()
                .push(OrderedTick {
                    priority: tick.priority,
                    sub_tick_order,
                    position: tick.position,
                    value: tick.value,
                });
        }
    }

    pub fn is_scheduled(&self, pos: BlockPos, value: &T) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|inner| inner.queued_ticks.contains(&(pos, value)))
    }

    pub fn clear_area(&self, min: &BlockPos, max: &BlockPos) {
        let mut inner_guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(inner) = inner_guard.as_mut() else {
            return;
        };

        let contains = |position: &BlockPos| {
            position.0.x >= min.0.x
                && position.0.x < max.0.x
                && position.0.y >= min.0.y
                && position.0.y < max.0.y
                && position.0.z >= min.0.z
                && position.0.z < max.0.z
        };

        inner.tick_queue.retain(|_, queue| {
            queue.retain(|tick| !contains(&tick.position));
            !queue.is_empty()
        });
        inner
            .queued_ticks
            .retain(|(position, _)| !contains(position));
        let became_empty = inner.queued_ticks.is_empty();

        if became_empty {
            *inner_guard = None;
        }
    }

    pub fn has_ticks(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|inner| !inner.queued_ticks.is_empty())
    }

    #[must_use]
    pub fn to_vec(&self) -> Vec<ScheduledTick<&'a T>> {
        let inner_guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(inner) = inner_guard.as_ref() else {
            return Vec::new();
        };

        let mut res = Vec::new();

        for (&deadline, queue) in &inner.tick_queue {
            res.extend(queue.iter().map(|x| ScheduledTick {
                // Matches ScheduledTick.toSavedTick's signed int conversion.
                delay: (deadline - inner.elapsed) as i32,
                priority: x.priority,
                position: x.position,
                value: x.value,
            }));
        }
        res
    }
}

impl<'a, T: std::hash::Hash + Eq + 'static> FromIterator<ScheduledTick<&'a T>>
    for ChunkTickScheduler<&'a T>
{
    fn from_iter<I: IntoIterator<Item = ScheduledTick<&'a T>>>(iter: I) -> Self {
        let scheduler = Self::default();
        let iter = iter.into_iter();

        let (lower, _) = iter.size_hint();
        if lower > 0 {
            let mut inner_guard = scheduler
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let inner = inner_guard.get_or_insert_with(|| {
                Box::new(ChunkTickSchedulerInner {
                    elapsed: 0,
                    tick_queue: BTreeMap::new(),
                    queued_ticks: FxHashSet::default(),
                })
            });
            inner.queued_ticks.reserve(lower);
        }

        for tick in iter {
            scheduler.schedule_tick(&tick, 0);
        }
        scheduler
    }
}

impl<T> Default for ChunkTickScheduler<T> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick::TickPriority;

    fn tick(x: i32, delay: i32, priority: TickPriority) -> ScheduledTick<&'static u8> {
        ScheduledTick {
            delay,
            priority,
            position: BlockPos::new(x, 64, 0),
            value: &1,
        }
    }

    #[test]
    fn overdue_ticks_keep_their_signed_delay_until_the_first_step() {
        let scheduler = ChunkTickScheduler::from_iter([
            tick(0, -1_425_489, TickPriority::Normal),
            tick(1, -134, TickPriority::Normal),
            tick(2, i32::MIN, TickPriority::Normal),
            tick(3, 0, TickPriority::Normal),
        ]);
        assert_eq!(
            scheduler
                .to_vec()
                .iter()
                .map(|t| t.delay)
                .collect::<Vec<_>>(),
            [i32::MIN, -1_425_489, -134, 0]
        );
        assert_eq!(scheduler.step_tick().len(), 4);
        assert!(!scheduler.has_ticks());
        assert!(scheduler.to_vec().is_empty());
        assert!(scheduler.step_tick().is_empty());
    }

    #[test]
    fn future_ticks_do_not_wrap_at_byte_or_short_boundaries() {
        let scheduler = ChunkTickScheduler::from_iter([
            tick(0, 255, TickPriority::Normal),
            tick(1, 256, TickPriority::Normal),
            tick(2, 300, TickPriority::Normal),
            tick(3, 70_000, TickPriority::Normal),
            tick(4, i32::MAX, TickPriority::Normal),
        ]);
        // The first step drains game time 0, matching a tick unpacked at time 0.
        for time in 0..=70_000 {
            let positions: Vec<_> = scheduler
                .step_tick()
                .iter()
                .map(|tick| tick.position.0.x)
                .collect();
            let expected: &[i32] = match time {
                255 => &[0],
                256 => &[1],
                300 => &[2],
                70_000 => &[3],
                _ => &[],
            };
            assert_eq!(positions, expected, "unexpected ticks at game time {time}");
        }
        let remaining = scheduler.to_vec();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].delay, 2_147_413_646);
    }

    #[test]
    fn saving_and_reloading_after_time_advances_keeps_remaining_delays() {
        let scheduler = ChunkTickScheduler::from_iter([
            tick(0, 3, TickPriority::Normal),
            tick(1, 300, TickPriority::Normal),
            tick(2, 70_000, TickPriority::Normal),
        ]);
        assert!(scheduler.step_tick().is_empty());
        assert!(scheduler.step_tick().is_empty());
        let saved = scheduler.to_vec();
        assert_eq!(
            saved.iter().map(|tick| tick.delay).collect::<Vec<_>>(),
            [1, 298, 69_998]
        );
        let reloaded = ChunkTickScheduler::from_iter(saved);
        for _ in 0..=298 {
            let original: Vec<_> = scheduler
                .step_tick()
                .iter()
                .map(|tick| tick.position)
                .collect();
            let restored: Vec<_> = reloaded
                .step_tick()
                .iter()
                .map(|tick| tick.position)
                .collect();
            assert_eq!(original, restored);
        }
        assert_eq!(reloaded.to_vec()[0].delay, 69_699);
    }

    #[test]
    fn deduplication_keeps_the_first_deadline_and_allows_rescheduling_after_drain() {
        let scheduler = ChunkTickScheduler::default();
        scheduler.schedule_tick(&tick(0, 300, TickPriority::Low), 10);
        scheduler.schedule_tick(&tick(0, -1, TickPriority::High), 11);
        assert!(scheduler.is_scheduled(BlockPos::new(0, 64, 0), &1));
        let saved = scheduler.to_vec();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].delay, 300);
        assert_eq!(saved[0].priority, TickPriority::Low);
        for _ in 0..300 {
            assert!(scheduler.step_tick().is_empty());
        }
        assert_eq!(scheduler.step_tick()[0].sub_tick_order, 10);
        assert!(!scheduler.is_scheduled(BlockPos::new(0, 64, 0), &1));
        scheduler.schedule_tick(&tick(0, 0, TickPriority::High), 12);
        assert_eq!(scheduler.step_tick()[0].sub_tick_order, 12);
    }

    #[test]
    fn clear_area_removes_overdue_and_long_future_ticks_with_exclusive_maximum() {
        let scheduler = ChunkTickScheduler::from_iter([
            tick(0, -1, TickPriority::Normal),
            tick(1, 300, TickPriority::Normal),
            tick(2, 70_000, TickPriority::Normal),
        ]);
        scheduler.clear_area(&BlockPos::new(0, 64, 0), &BlockPos::new(2, 65, 1));
        assert!(!scheduler.is_scheduled(BlockPos::new(0, 64, 0), &1));
        assert!(!scheduler.is_scheduled(BlockPos::new(1, 64, 0), &1));
        assert_eq!(scheduler.to_vec().len(), 1);
        assert_eq!(scheduler.to_vec()[0].position.0.x, 2);
        assert_eq!(scheduler.to_vec()[0].delay, 70_000);
        scheduler.clear_area(&BlockPos::new(2, 64, 0), &BlockPos::new(3, 65, 1));
        assert!(!scheduler.has_ticks());
        scheduler.schedule_tick(&tick(1, 0, TickPriority::Normal), 0);
        assert_eq!(scheduler.step_tick().len(), 1);
    }

    #[test]
    fn due_ticks_retain_priority_and_sub_tick_order_for_level_sorting() {
        let scheduler = ChunkTickScheduler::default();
        scheduler.schedule_tick(&tick(0, -1, TickPriority::Low), 0);
        scheduler.schedule_tick(&tick(1, 0, TickPriority::High), 7);
        scheduler.schedule_tick(&tick(2, 0, TickPriority::High), 3);
        scheduler.schedule_tick(&tick(3, 1, TickPriority::ExtremelyHigh), 0);
        let mut due = scheduler.step_tick();
        due.sort_unstable();
        assert_eq!(
            due.iter().map(|tick| tick.position.0.x).collect::<Vec<_>>(),
            [2, 1, 0]
        );
        assert_eq!(scheduler.step_tick()[0].position.0.x, 3);
    }
}
