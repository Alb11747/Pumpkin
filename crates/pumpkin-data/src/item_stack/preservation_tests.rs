use super::*;
use crate::data_component_impl::{ContainerImpl, ItemNameImpl, LoreImpl};
use pumpkin_nbt::deserializer::NbtReadHelperJava;
use pumpkin_nbt::serializer::NbtWriteHelperJava;
use std::io::Cursor;

fn encoded_round_trip(compound: &NbtCompound) -> NbtCompound {
    let mut bytes = Vec::new();
    compound
        .clone()
        .serialize_content(&mut NbtWriteHelperJava::new(&mut bytes))
        .expect("fixture must serialize");
    NbtCompound::deserialize_content(&mut NbtReadHelperJava::new(Cursor::new(bytes)))
        .expect("fixture must deserialize")
}

fn saved(stack: &ItemStack) -> NbtCompound {
    let mut compound = NbtCompound::new();
    stack.write_item_stack(&mut compound);
    encoded_round_trip(&compound)
}

#[test]
fn pot_decorations_keep_modern_side_components_and_upgrade_legacy_lists() {
    use crate::data_component_impl::PotDecorationsImpl;
    let mut side = NbtCompound::new();
    side.put_string("id", "minecraft:angler_pottery_sherd".into());
    let mut custom = NbtCompound::new();
    custom.put_long("marker", 17);
    let mut components = NbtCompound::new();
    components.put_compound("example:side_data", custom);
    side.put_compound("components", components);
    side.put_string("example:unknown", "retained".into());
    let mut decorations = NbtCompound::new();
    decorations.put_compound("back", side);
    decorations.put_int("example:future", 5);
    let mut components = NbtCompound::new();
    components.put_compound("minecraft:pot_decorations", decorations.clone());
    let mut item = NbtCompound::new();
    item.put_string("id", "minecraft:decorated_pot".into());
    item.put_int("count", 1);
    item.put_compound("components", components);
    let stack = ItemStack::read_item_stack(&encoded_round_trip(&item)).unwrap();
    assert_eq!(
        stack
            .get_data_component::<PotDecorationsImpl>()
            .unwrap()
            .write_data(),
        NbtTag::Compound(decorations)
    );
    assert_eq!(saved(&stack), item);
    let mut components = NbtCompound::new();
    components.put_list(
        "minecraft:pot_decorations",
        vec![NbtTag::String("minecraft:angler_pottery_sherd".into())],
    );
    item.put_compound("components", components);
    let stack = ItemStack::read_item_stack(&item).unwrap();
    let upgraded = saved(&stack);
    let sides = upgraded
        .get_compound("components")
        .unwrap()
        .get_compound("minecraft:pot_decorations")
        .unwrap();
    assert_eq!(
        sides.get_compound("back").unwrap().get_string("id"),
        Some("minecraft:angler_pottery_sherd")
    );
    assert_eq!(
        sides.get_compound("front").unwrap().get_string("id"),
        Some("minecraft:brick")
    );
    assert_eq!(
        saved(&ItemStack::read_item_stack(&upgraded).unwrap()),
        upgraded
    );
}

// ItemStack.MAP_CODEC and DataComponentPatch.PatchKey in vanilla 26.2/26.3 use
// these id/count/components fields and ! removal keys. The extension payload is
// synthetic: its different NBT types must survive an unavailable component codec.
fn fixture() -> NbtCompound {
    let mut payload = NbtCompound::new();
    payload.put("byte", NbtTag::Byte(-128));
    payload.put("short", NbtTag::Short(-1234));
    payload.put_int("int", 123456);
    payload.put_long("long", i64::MAX);
    payload.put_float("float", 1.25);
    payload.put("double", NbtTag::Double(-2.5));
    payload.put_string("string", "preserved".into());
    payload.put("bytes", NbtTag::ByteArray(vec![0, -1, 127].into()));
    payload.put("ints", NbtTag::IntArray(vec![i32::MIN, i32::MAX]));
    payload.put("longs", NbtTag::LongArray(vec![i64::MIN, i64::MAX]));
    payload.put(
        "list",
        NbtTag::List(vec![NbtTag::Short(1), NbtTag::Short(2)]),
    );
    payload.put(
        "nested",
        NbtTag::List(vec![NbtTag::Compound(NbtCompound::new())]),
    );

    let mut name = NbtCompound::new();
    name.put_string("text", "Atlas".into());
    name.put_string("color", "gold".into());
    name.put("italic", NbtTag::Byte(0));

    let mut enchantments = NbtCompound::new();
    enchantments.put_int("minecraft:unbreaking", 3);
    let mut components = NbtCompound::new();
    components.put("example:extension", payload);
    components.put("!example:removed", NbtCompound::new());
    components.put("minecraft:interact_animation", NbtCompound::new());
    let mut modifier = NbtCompound::new();
    modifier.put_string("type", "minecraft:attack_damage".into());
    modifier.put_string("id", "example:bonus".into());
    modifier.put("amount", NbtTag::Double(2.0));
    modifier.put_string("operation", "add_value".into());
    modifier.put_string("slot", "mainhand".into());
    components.put(
        "minecraft:attribute_modifiers",
        NbtTag::List(vec![NbtTag::Compound(modifier)]),
    );
    components.put("minecraft:custom_name", name.clone());
    components.put(
        "minecraft:lore",
        NbtTag::List(vec![NbtTag::Compound(name.clone())]),
    );
    components.put("minecraft:item_name", name);
    components.put_int("minecraft:damage", 7);
    components.put("minecraft:enchantments", enchantments);
    components.put("!minecraft:enchantable", NbtCompound::new());
    let mut compound = NbtCompound::new();
    compound.put_string("id", "minecraft:diamond_sword".into());
    compound.put_int("count", 1);
    compound.put("components", components);
    encoded_round_trip(&compound)
}

#[test]
fn unavailable_item_components_round_trip_without_deleting_stack() {
    let original = fixture();
    let stack =
        ItemStack::read_item_stack(&original).expect("valid item must survive missing codecs");
    assert_eq!(stack.item.id, Item::DIAMOND_SWORD.id);
    assert_eq!(stack.get_damage(), 7);
    assert_eq!(stack.get_enchantment_level(&Enchantment::UNBREAKING), 3);
    assert!(!stack.has_data_component(DataComponent::AttributeModifiers));
    assert_eq!(
        stack
            .get_custom_name()
            .expect("styled name is modeled")
            .clone()
            .get_text(),
        "Atlas"
    );
    assert_eq!(
        stack
            .get_data_component::<LoreImpl>()
            .expect("styled lore is modeled")
            .lines
            .len(),
        1
    );
    assert_eq!(saved(&stack), original);
}

#[test]
fn malformed_component_keeps_other_components_and_original_value() {
    let mut original = fixture();
    let components = original
        .child_tags
        .get_mut("components")
        .expect("components");
    let NbtTag::Compound(components) = components else {
        panic!("components must be compound")
    };
    components.put_string("minecraft:damage", "not an integer".into());
    let mut stack =
        ItemStack::read_item_stack(&original).expect("malformed component is preserved");
    assert_eq!(stack.get_enchantment_level(&Enchantment::UNBREAKING), 3);
    assert!(stack.get_data_component::<DamageImpl>().is_none());
    assert_eq!(saved(&stack), original);

    stack.set_damage(0);
    assert!(
        !saved(&stack)
            .get_compound("components")
            .expect("components")
            .child_tags
            .contains_key("minecraft:damage")
    );
    stack.set_damage(12);
    assert_eq!(
        saved(&stack)
            .get_compound("components")
            .expect("components")
            .get_int("minecraft:damage"),
        Some(12)
    );
}

#[test]
fn edits_removals_and_clear_do_not_restore_preserved_components() {
    let mut stack = ItemStack::read_item_stack(&fixture()).expect("valid fixture");
    let mut mutated = stack.clone();
    mutated
        .get_data_component_mut::<ItemNameImpl>()
        .expect("modeled name")
        .name = Cow::Borrowed("mutated");
    assert_eq!(
        saved(&mutated)
            .get_compound("components")
            .expect("components")
            .get_compound("minecraft:item_name")
            .expect("mutated name")
            .get_string("translate"),
        Some("mutated")
    );
    stack.set_data_component(ItemNameImpl {
        name: Cow::Borrowed("replacement"),
    });
    stack.remove_data_component(DataComponent::InteractAnimation);
    let edited = saved(&stack);
    let components = edited.get_compound("components").expect("components");
    assert_eq!(
        components
            .get_compound("minecraft:item_name")
            .expect("replacement name")
            .get_string("translate"),
        Some("replacement")
    );
    assert!(
        !components
            .child_tags
            .contains_key("minecraft:interact_animation")
    );
    assert!(
        components
            .child_tags
            .contains_key("!minecraft:interact_animation")
    );
    assert!(components.child_tags.contains_key("example:extension"));

    let mut without_overrides = stack.clone();
    without_overrides.clear_components();
    assert!(
        saved(&without_overrides)
            .get_compound("components")
            .expect("components")
            .is_empty()
    );

    stack.patch.retain(|(id, _)| *id != DataComponent::ItemName);
    assert!(
        !saved(&stack)
            .get_compound("components")
            .expect("components")
            .child_tags
            .contains_key("minecraft:item_name")
    );
    stack.clear();
    assert!(
        saved(&stack)
            .get_compound("components")
            .expect("components")
            .is_empty()
    );
}

#[test]
fn opaque_components_prevent_plain_stack_merge_and_survive_split() {
    let original = fixture();
    let mut stack = ItemStack::read_item_stack(&original).expect("valid fixture");
    let plain = ItemStack::new_with_component(1, stack.item, stack.patch.clone());
    assert!(!stack.are_items_and_components_equal(&plain));
    assert!(!plain.are_items_and_components_equal(&stack));
    let copy = stack.copy_with_count(2);
    assert!(stack.are_items_and_components_equal(&copy));
    assert!(!stack.are_equal(&copy));
    let split = stack.split_off(1);
    assert_eq!(saved(&split), original);
    assert_eq!(
        saved(&copy).get_compound("components"),
        original.get_compound("components")
    );
}

#[test]
fn unknown_nested_item_preserves_entire_container_component() {
    let mut nested = NbtCompound::new();
    nested.put_string("id", "example:unknown_item".into());
    nested.put_int("count", 2);
    let mut entry = NbtCompound::new();
    entry.put_int("slot", 0);
    entry.put("item", nested);
    let mut diamonds = NbtCompound::new();
    ItemStack::new(64, &Item::DIAMOND).write_item_stack(&mut diamonds);
    let mut sibling = NbtCompound::new();
    sibling.put_int("slot", 1);
    sibling.put_compound("item", diamonds);
    let mut components = NbtCompound::new();
    components.put(
        "minecraft:container",
        NbtTag::List(vec![NbtTag::Compound(entry), NbtTag::Compound(sibling)]),
    );
    let mut original = NbtCompound::new();
    original.put_string("id", "minecraft:shulker_box".into());
    original.put_int("count", 1);
    original.put("components", components);
    let stack = ItemStack::read_item_stack(&original).expect("known container item must survive");
    assert!(stack.get_data_component::<ContainerImpl>().is_none());
    assert!(stack.has_opaque_container());
    assert_eq!(saved(&stack), original);
    let mut reloaded = ItemStack::read_item_stack(&saved(&stack)).expect("preserved item reloads");
    assert!(reloaded.has_opaque_container());
    assert_eq!(saved(&reloaded), original);

    reloaded.set_data_component(ContainerImpl {
        items: vec![(1, ItemStack::new(64, &Item::DIAMOND))],
    });
    assert!(!reloaded.has_opaque_container());
}

#[test]
fn absent_and_removed_containers_are_not_opaque() {
    let stack = ItemStack::new(1, &Item::SHULKER_BOX);
    assert!(!stack.has_opaque_container());
    let mut removed = stack;
    removed.remove_data_component(DataComponent::Container);
    let reloaded = ItemStack::read_item_stack(&saved(&removed)).expect("removal reloads");
    assert!(!reloaded.has_opaque_container());
}

#[test]
fn unknown_item_and_unrepresentable_count_are_refused() {
    let mut compound = NbtCompound::new();
    compound.put_string("id", "example:unknown_item".into());
    compound.put_int("count", 1);
    assert!(ItemStack::read_item_stack(&compound).is_none());
    compound.put_string("id", "minecraft:stone".into());
    compound.put_int("count", 256);
    assert!(ItemStack::read_item_stack(&compound).is_none());
}

#[test]
fn conflicting_component_aliases_stay_opaque_until_replaced() {
    let mut components = NbtCompound::new();
    components.put_int("damage", 3);
    components.put_int("minecraft:damage", 4);
    components.put("!minecraft:damage", NbtCompound::new());
    let mut original = NbtCompound::new();
    original.put_string("id", "minecraft:diamond_sword".into());
    original.put_int("count", 1);
    original.put("components", components);
    let mut stack = ItemStack::read_item_stack(&original).expect("conflicting values must survive");
    assert_eq!(stack.patch.len(), 1);
    assert!(stack.get_data_component::<DamageImpl>().is_none());
    assert_eq!(saved(&stack), original);
    stack.set_damage(8);
    let updated = saved(&stack);
    let components = updated.get_compound("components").expect("components");
    assert_eq!(components.child_tags.len(), 1);
    assert_eq!(components.get_int("minecraft:damage"), Some(8));
}
