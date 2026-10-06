use super::GossipType;
use pumpkin_data::{dimension::Dimension, item_stack::ItemStack};
use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};
use pumpkin_protocol::java::client::play::MerchantOffer;
use pumpkin_util::math::position::BlockPos;
use rustc_hash::FxHashMap;
use uuid::Uuid;

use crate::entity::ai::brain::{
    Brain,
    memory::{GlobalPos, MemoryModuleType, MemoryStatus, PackedMemories, types},
};

pub(super) fn sync_poi_memory(
    brain: &mut Brain,
    key: MemoryModuleType<GlobalPos>,
    position: Option<BlockPos>,
    dimension: &'static Dimension,
) {
    if let Some(position) = position {
        brain.set(key, GlobalPos::new(dimension, position));
    } else if brain
        .get(key)
        .is_none_or(|pos| pos.dimension.id == dimension.id)
    {
        brain.erase(key.id());
    }
}

pub(super) fn make_brain(packed: &PackedMemories) -> Brain {
    let mut brain = Brain::default();
    // Keep persistent memories working while villagers still use the goal-based AI.
    for id in [
        types::HOME.id(),
        types::JOB_SITE.id(),
        types::POTENTIAL_JOB_SITE.id(),
        types::MEETING_POINT.id(),
        types::GOLEM_DETECTED_RECENTLY.id(),
        types::LAST_SLEPT.id(),
        types::LAST_WOKEN.id(),
        types::LAST_WORKED_AT_POI.id(),
    ] {
        brain.register_memory(id);
    }
    brain.load_packed(packed);
    brain
}

pub(super) fn write_brain(mut retained: NbtCompound, brain: &Brain) -> NbtCompound {
    let mut memories = retained
        .get_compound("memories")
        .cloned()
        .unwrap_or_default();
    let saved = memories.clone();
    // Remove modeled entries first, so expired/erased memories cannot resurrect on reload.
    memories.child_tags.retain(|name, tag| {
        let Some(id) = types::from_name(name) else {
            return true;
        };
        if !brain.check(id, MemoryStatus::Registered) {
            return true;
        }
        let Some(codec) = types::codec_of(id) else {
            return true;
        };
        tag.extract_compound()
            .and_then(|entry| entry.get("value"))
            .is_none_or(|value| (codec.decode)(value).is_none())
    });
    let packed = brain.pack().into_nbt();
    if let Some(current) = packed.get_compound("memories") {
        for (name, tag) in &current.child_tags {
            let Some(current_entry) = tag.extract_compound() else {
                continue;
            };
            let mut entry = saved.get_compound(name).cloned().unwrap_or_default();
            entry.child_tags.remove("ttl");
            entry.child_tags.extend(current_entry.child_tags.clone());
            memories.put_compound(name, entry);
        }
    }
    retained.put_compound("memories", memories);
    retained
}

pub(super) fn write_gossips(
    gossips: &FxHashMap<Uuid, FxHashMap<GossipType, i32>>,
    retained: Option<&[NbtTag]>,
) -> Vec<NbtTag> {
    let mut result = Vec::new();
    let mut pending = gossips.clone();
    if let Some(retained) = retained {
        for tag in retained {
            let Some(entry) = tag.extract_compound() else {
                result.push(tag.clone());
                continue;
            };
            let kind = entry
                .get_string("Type")
                .and_then(GossipType::from_name)
                .or_else(|| entry.get_int("Type").and_then(GossipType::from_legacy_id));
            if let (Some(target), Some(kind), Some(_)) =
                (entry.get_uuid("Target"), kind, entry.get_int("Value"))
            {
                if let Some(value) = pending
                    .get_mut(&target)
                    .and_then(|entries| entries.remove(&kind))
                {
                    let mut entry = entry.clone();
                    entry.put_int("Value", value);
                    entry.put_string("Type", kind.name().to_owned());
                    result.push(NbtTag::Compound(entry));
                }
            } else {
                result.push(tag.clone());
            }
        }
    }
    for (target, entries) in pending {
        for (kind, value) in entries {
            let mut entry = NbtCompound::new();
            entry.put_uuid("Target", target);
            entry.put_string("Type", kind.name().to_owned());
            entry.put_int("Value", value);
            result.push(NbtTag::Compound(entry));
        }
    }
    result
}

pub(super) fn read_gossips(entries: &[NbtTag]) -> FxHashMap<Uuid, FxHashMap<GossipType, i32>> {
    let mut gossips: FxHashMap<Uuid, FxHashMap<GossipType, i32>> = FxHashMap::default();
    for tag in entries {
        if let Some(entry) = tag.extract_compound() {
            let kind = entry
                .get_string("Type")
                .and_then(GossipType::from_name)
                .or_else(|| entry.get_int("Type").and_then(GossipType::from_legacy_id));
            if let (Some(target), Some(kind), Some(value)) =
                (entry.get_uuid("Target"), kind, entry.get_int("Value"))
            {
                gossips.entry(target).or_default().insert(kind, value);
            }
        }
    }
    gossips
}

pub(super) fn read_offer(recipe: &NbtCompound) -> Option<MerchantOffer> {
    let buy = recipe
        .get_compound("buy")
        .and_then(ItemStack::read_item_stack)?;
    let output = recipe
        .get_compound("sell")
        .and_then(ItemStack::read_item_stack)?;
    let cost_b = match recipe.get("buyB") {
        Some(tag) => {
            let compound = tag.extract_compound()?;
            if compound.child_tags.is_empty() {
                None
            } else {
                let stack = ItemStack::read_item_stack(compound)?;
                (!stack.is_empty()).then_some(stack)
            }
        }
        None => None,
    };
    if buy.is_empty() || output.is_empty() {
        return None;
    }
    Some(MerchantOffer {
        base_cost_a: buy.into(),
        output: output.into(),
        cost_b: cost_b.map(Into::into),
        uses: recipe.get_int("uses").unwrap_or(0),
        max_uses: recipe.get_int("maxUses").unwrap_or(4),
        reward_exp: recipe.get_bool("rewardExp").unwrap_or(true),
        xp: recipe.get_int("xp").unwrap_or(1),
        price_multiplier: recipe.get_float("priceMultiplier").unwrap_or(0.0),
        special_price: recipe.get_int("specialPrice").unwrap_or(0),
        demand: recipe.get_int("demand").unwrap_or(0),
    })
}

fn write_offer(offer: &MerchantOffer, mut retained: NbtCompound) -> NbtCompound {
    for (key, stack) in [
        ("buy", Some(offer.base_cost_a.0.as_ref())),
        ("sell", Some(offer.output.0.as_ref())),
        ("buyB", offer.cost_b.as_ref().map(|cost| cost.0.as_ref())),
    ] {
        if let Some(stack) = stack {
            let mut item = retained.get_compound(key).cloned().unwrap_or_default();
            stack.write_item_stack(&mut item);
            retained.put_compound(key, item);
        } else {
            retained.child_tags.remove(key);
        }
    }
    retained.put_int("uses", offer.uses);
    retained.put_int("maxUses", offer.max_uses);
    retained.put_bool("rewardExp", offer.reward_exp);
    retained.put_int("xp", offer.xp);
    retained.put_float("priceMultiplier", offer.price_multiplier);
    retained.put_int("specialPrice", offer.special_price);
    retained.put_int("demand", offer.demand);
    retained
}

pub(super) fn write_offers(offers: &[MerchantOffer], mut retained: NbtCompound) -> NbtCompound {
    let mut current = offers.iter();
    let mut recipes = Vec::new();
    if let Some(saved) = retained.get_list("Recipes") {
        for tag in saved {
            if let Some(recipe) = tag.extract_compound()
                && read_offer(recipe).is_some()
            {
                if let Some(offer) = current.next() {
                    recipes.push(NbtTag::Compound(write_offer(offer, recipe.clone())));
                }
            } else {
                // An unsupported recipe must survive even though it cannot be offered yet.
                recipes.push(tag.clone());
            }
        }
    }
    recipes.extend(current.map(|offer| NbtTag::Compound(write_offer(offer, NbtCompound::new()))));
    retained.put_list("Recipes", recipes);
    retained
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::entity::ai::brain::memory::GlobalPos;
    use pumpkin_data::dimension::Dimension;
    use pumpkin_util::math::position::BlockPos;

    fn item(id: &str, count: i32) -> NbtCompound {
        let mut item = NbtCompound::new();
        item.put_string("id", id.to_owned());
        item.put_int("count", count);
        item
    }

    #[test]
    fn imported_offer_keeps_its_trade_state_and_unknown_fields() {
        let mut recipe = NbtCompound::new();
        recipe.put_compound("buy", item("minecraft:emerald", 17));
        recipe.put_compound("buyB", item("minecraft:book", 1));
        recipe.put_compound("sell", item("minecraft:enchanted_book", 1));
        recipe.put_int("uses", 7);
        recipe.put_int("maxUses", 12);
        recipe.put_int("xp", 30);
        recipe.put_int("specialPrice", -3);
        recipe.put_int("demand", 8);
        recipe.put_float("priceMultiplier", 0.2);
        recipe.put_bool("rewardExp", false);
        recipe.put_string("future_field", "retained".to_owned());
        let mut offer = read_offer(&recipe).unwrap();
        assert_eq!(offer.base_cost_a.0.item_count, 17);
        assert_eq!(offer.cost_b.as_ref().unwrap().0.item_count, 1);
        assert_eq!(
            (
                offer.uses,
                offer.max_uses,
                offer.xp,
                offer.special_price,
                offer.demand
            ),
            (7, 12, 30, -3, 8)
        );
        assert!(!offer.reward_exp);
        assert_eq!(offer.price_multiplier, 0.2);
        offer.uses += 1;
        let mut imported = NbtCompound::new();
        imported.put_list("Recipes", vec![NbtTag::Compound(recipe)]);
        let written = write_offers(&[offer], imported);
        let recipe = written.get_list("Recipes").unwrap()[0]
            .extract_compound()
            .unwrap();
        assert_eq!(recipe.get_int("uses"), Some(8));
        assert_eq!(recipe.get_string("future_field"), Some("retained"));
        assert_eq!(recipe.get_int("specialPrice"), Some(-3));
    }

    #[test]
    fn vanilla_optional_offer_defaults_and_unsupported_recipe_survive() {
        let mut recipe = NbtCompound::new();
        recipe.put_compound("buy", item("minecraft:emerald", 1));
        recipe.put_compound("sell", item("minecraft:bread", 6));
        let offer = read_offer(&recipe).unwrap();
        assert_eq!(
            (offer.max_uses, offer.xp, offer.price_multiplier),
            (4, 1, 0.0)
        );
        let mut legacy = recipe.clone();
        legacy.put_compound("buyB", NbtCompound::new());
        assert!(read_offer(&legacy).unwrap().cost_b.is_none());
        let mut unsupported = recipe.clone();
        unsupported.put_compound("buyB", item("future:item", 1));
        assert!(read_offer(&unsupported).is_none());
        let mut imported = NbtCompound::new();
        imported.put_list(
            "Recipes",
            vec![
                NbtTag::Compound(unsupported.clone()),
                NbtTag::Compound(recipe),
            ],
        );
        let written = write_offers(&[offer], imported);
        assert_eq!(
            written.get_list("Recipes").unwrap()[0],
            NbtTag::Compound(unsupported)
        );
        assert_eq!(written.get_list("Recipes").unwrap().len(), 2);
    }

    #[test]
    fn brain_preserves_pois_ttl_and_unknown_memories_without_resurrecting_erased_values() {
        let mut job = NbtCompound::new();
        job.put_string("dimension", "minecraft:overworld".to_owned());
        job.put("pos", NbtTag::IntArray(vec![23, 70, -9]));
        let mut entry = NbtCompound::new();
        entry.put_compound("value", job);
        entry.put_string("future_field", "retained".to_owned());
        let mut expiring = NbtCompound::new();
        expiring.put_bool("value", true);
        expiring.put_long("ttl", 37);
        let mut memories = NbtCompound::new();
        memories.put_compound("minecraft:job_site", entry);
        memories.put_compound("minecraft:golem_detected_recently", expiring);
        memories.put_string("future:memory", "retained".to_owned());
        let mut imported = NbtCompound::new();
        imported.put_compound("memories", memories);
        let mut brain = make_brain(&PackedMemories::from_nbt(&imported));
        assert_eq!(
            brain.get(types::JOB_SITE).unwrap().pos,
            BlockPos::new(23, 70, -9)
        );
        let written = write_brain(imported.clone(), &brain);
        let memories = written.get_compound("memories").unwrap();
        assert_eq!(
            memories
                .get_compound("minecraft:golem_detected_recently")
                .unwrap()
                .get_long("ttl"),
            Some(37)
        );
        assert_eq!(
            memories
                .get_compound("minecraft:job_site")
                .unwrap()
                .get_string("future_field"),
            Some("retained")
        );
        brain.erase(types::GOLEM_DETECTED_RECENTLY.id());
        let written = write_brain(imported, &brain);
        let memories = written.get_compound("memories").unwrap();
        assert!(memories.get("minecraft:golem_detected_recently").is_none());
        assert_eq!(memories.get_string("future:memory"), Some("retained"));
        brain.set(
            types::HOME,
            GlobalPos::new(&Dimension::THE_NETHER, BlockPos::new(4, 60, 4)),
        );
        sync_poi_memory(&mut brain, types::HOME, None, &Dimension::OVERWORLD);
        assert_eq!(
            brain.get(types::HOME).unwrap().dimension.id,
            Dimension::THE_NETHER.id
        );
    }

    #[test]
    fn gossip_import_preserves_signed_uuid_words_and_rejects_short_arrays() {
        let target = uuid::Uuid::from_u128(0x12345678_9abcdef0_12345678_fedcba98);
        let mut entry = NbtCompound::new();
        entry.put(
            "Target",
            NbtTag::IntArray(vec![
                0x12345678,
                0x9abcdef0u32 as i32,
                0x12345678,
                0xfedcba98u32 as i32,
            ]),
        );
        entry.put_string("Type", "major_positive".to_owned());
        entry.put_int("Value", 17);
        let mut malformed = entry.clone();
        malformed.put("Target", NbtTag::IntArray(vec![1, 2]));
        let entries = vec![NbtTag::Compound(entry), NbtTag::Compound(malformed.clone())];
        let gossips = read_gossips(&entries);
        assert_eq!(gossips.len(), 1);
        assert_eq!(
            gossips[&target][&super::super::GossipType::MajorPositive],
            17
        );
        let written = write_gossips(&gossips, Some(&entries));
        assert_eq!(read_gossips(&written), gossips);
        assert!(written.contains(&NbtTag::Compound(malformed)));
    }
}
