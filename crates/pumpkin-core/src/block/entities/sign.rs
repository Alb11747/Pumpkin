use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicI8, Ordering},
};

use super::BlockEntity;
use pumpkin_nbt::{compound::NbtCompound, tag::NbtTag};
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::text::TextComponent;
use pumpkin_util::text::click::ClickEvent;
use pumpkin_util::version::JavaMinecraftVersion;

pub use pumpkin_data::dye_color::DyeColor;

pub struct SignBlockEntity {
    pub front_text: SignText,
    pub back_text: SignText,
    pub is_waxed: AtomicBool,
    pub allow_op_features: AtomicBool,
    position: BlockPos,
    pub currently_editing_player: Arc<Mutex<Option<uuid::Uuid>>>,
}

pub type Text = SignText;

#[derive(Clone)]
struct PreservedSignMessage {
    message: Box<str>,
    nbt: NbtTag,
}

pub struct SignText {
    pub has_glowing_text: AtomicBool,
    color: AtomicI8,
    pub messages: Arc<Mutex<[Box<str>; 4]>>,
    pub filtered_messages: Arc<Mutex<[Box<str>; 4]>>,
    // Keep original components, including fields the runtime decoder cannot
    // represent, until the corresponding line is edited.
    preserved_messages: [Option<PreservedSignMessage>; 4],
    preserved_filtered_messages: [Option<PreservedSignMessage>; 4],
}

impl Clone for SignText {
    fn clone(&self) -> Self {
        Self {
            has_glowing_text: AtomicBool::new(self.has_glowing_text.load(Ordering::Relaxed)),
            color: AtomicI8::new(self.color.load(Ordering::Relaxed)),
            messages: self.messages.clone(),
            filtered_messages: self.filtered_messages.clone(),
            preserved_messages: self.preserved_messages.clone(),
            preserved_filtered_messages: self.preserved_filtered_messages.clone(),
        }
    }
}

impl Default for SignText {
    fn default() -> Self {
        Self {
            has_glowing_text: AtomicBool::new(false),
            color: AtomicI8::new(DyeColor::Black as i8),
            messages: Arc::new(Mutex::new(Self::empty_messages())),
            filtered_messages: Arc::new(Mutex::new(Self::empty_messages())),
            preserved_messages: std::array::from_fn(|_| None),
            preserved_filtered_messages: std::array::from_fn(|_| None),
        }
    }
}

#[allow(clippy::fallible_impl_from)]
impl From<SignText> for NbtTag {
    fn from(value: SignText) -> Self {
        let mut nbt = NbtCompound::new();
        nbt.put_bool(
            "has_glowing_text",
            value.has_glowing_text.load(Ordering::Relaxed),
        );
        nbt.put_string("color", value.get_color().name().to_string());

        let messages = value
            .messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let messages_nbt = SignText::messages_to_nbt(&messages, &value.preserved_messages);

        let filtered_messages = value
            .filtered_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let filtered_nbt =
            SignText::messages_to_nbt(&filtered_messages, &value.preserved_filtered_messages);
        if filtered_nbt != messages_nbt {
            nbt.put_list("filtered_messages", filtered_nbt);
        }
        nbt.put_list("messages", messages_nbt);

        Self::Compound(nbt)
    }
}

impl From<NbtTag> for SignText {
    fn from(tag: NbtTag) -> Self {
        let Some(nbt) = tag.extract_compound() else {
            return Self::default();
        };
        let has_glowing_text = nbt.get_bool("has_glowing_text").unwrap_or(false);
        let color = nbt.get_string("color").unwrap_or("black");
        let (parsed_messages, preserved_messages) =
            Self::messages_from_nbt(nbt.get_list("messages"));
        let (parsed_filtered, preserved_filtered_messages) =
            nbt.get_list("filtered_messages").map_or_else(
                || (parsed_messages.clone(), preserved_messages.clone()),
                |filtered| Self::messages_from_nbt(Some(filtered)),
            );

        Self {
            has_glowing_text: AtomicBool::new(has_glowing_text),
            color: AtomicI8::new(DyeColor::by_name(color).unwrap_or(DyeColor::Black).id() as i8),
            messages: Arc::new(Mutex::new(parsed_messages)),
            filtered_messages: Arc::new(Mutex::new(parsed_filtered)),
            preserved_messages,
            preserved_filtered_messages,
        }
    }
}

impl SignText {
    pub const LINES: usize = 4;

    fn messages_from_nbt(
        list: Option<&[NbtTag]>,
    ) -> ([Box<str>; 4], [Option<PreservedSignMessage>; 4]) {
        let mut messages = Self::empty_messages();
        let preserved = std::array::from_fn(|index| {
            let tag = list?.get(index)?;
            let component_tag = Self::list_element(tag);
            let message = match TextComponent::try_from_nbt(component_tag) {
                Ok(component) => Self::component_message(&component),
                Err(error) => {
                    tracing::warn!(index, %error, "Unable to decode sign message; retaining original NBT");
                    Box::from("")
                }
            };
            messages[index] = message.clone();
            Some(PreservedSignMessage {
                message,
                nbt: tag.clone(),
            })
        });
        (messages, preserved)
    }

    fn messages_to_nbt(
        messages: &[Box<str>; 4],
        preserved: &[Option<PreservedSignMessage>; 4],
    ) -> Vec<NbtTag> {
        let mut result: Vec<_> = messages
            .iter()
            .zip(preserved)
            .map(|(message, original)| {
                if let Some(original) = original
                    && original.message == *message
                {
                    return original.nbt.clone();
                }
                Self::message_component(message)
                    .0
                    .to_nbt_tag_for_version(&JavaMinecraftVersion::V_26_3)
            })
            .collect();
        // An edited compact string beside retained raw wrapper compounds must
        // use the same compound list type, so disk writers do not escape the
        // retained wrappers while converting a mixed semantic list.
        if result.iter().any(|tag| matches!(tag, NbtTag::Compound(_))) {
            for tag in &mut result {
                if !matches!(tag, NbtTag::Compound(_)) {
                    let mut wrapper = NbtCompound::new();
                    wrapper.put("", tag.clone());
                    *tag = NbtTag::Compound(wrapper);
                }
            }
        }
        result
    }

    fn component_message(component: &TextComponent) -> Box<str> {
        component
            .0
            .to_json_value_for_version(&JavaMinecraftVersion::V_26_3)
            .to_string()
            .into_boxed_str()
    }

    fn message_component(message: &str) -> TextComponent {
        serde_json::from_str(message).unwrap_or_else(|_| TextComponent::text(message.to_string()))
    }

    pub(crate) fn get_components(&self, should_filter: bool) -> [TextComponent; 4] {
        self.get_messages(should_filter)
            .map(|message| Self::message_component(&message))
    }

    fn list_element(tag: &NbtTag) -> &NbtTag {
        if let NbtTag::Compound(compound) = tag
            && compound.child_tags.len() == 1
        {
            compound.get("").unwrap_or(tag)
        } else {
            tag
        }
    }

    pub(crate) fn network_nbt(&self) -> NbtTag {
        // Disk components retain ListTag wrappers. Ordinary packet writers need
        // the semantic tree or they escape those wrappers a second time.
        fn semantic(tag: &NbtTag) -> NbtTag {
            match tag {
                NbtTag::List(list) => NbtTag::List(
                    list.iter()
                        .map(|tag| semantic(SignText::list_element(tag)))
                        .collect(),
                ),
                NbtTag::Compound(compound) => {
                    let mut result = NbtCompound::new();
                    for (name, value) in &compound.child_tags {
                        result.put(name, semantic(value));
                    }
                    NbtTag::Compound(result)
                }
                tag => tag.clone(),
            }
        }
        semantic(&NbtTag::from(self.clone()))
    }

    #[must_use]
    pub fn empty_messages() -> [Box<str>; 4] {
        [Box::from(""), Box::from(""), Box::from(""), Box::from("")]
    }

    #[must_use]
    pub fn new(
        messages: [Box<str>; 4],
        filtered_messages: Option<[Box<str>; 4]>,
        color: DyeColor,
        has_glowing_text: bool,
    ) -> Self {
        let messages = messages
            .map(|message| Self::component_message(&TextComponent::text(message.into_string())));
        let filtered = filtered_messages.map_or_else(
            || messages.clone(),
            |messages| {
                messages.map(|message| {
                    Self::component_message(&TextComponent::text(message.into_string()))
                })
            },
        );
        Self {
            has_glowing_text: AtomicBool::new(has_glowing_text),
            color: AtomicI8::new(color.id() as i8),
            messages: Arc::new(Mutex::new(messages)),
            filtered_messages: Arc::new(Mutex::new(filtered)),
            preserved_messages: std::array::from_fn(|_| None),
            preserved_filtered_messages: std::array::from_fn(|_| None),
        }
    }

    #[must_use]
    pub fn from_messages(messages: [Box<str>; 4]) -> Self {
        Self::new(messages, None, DyeColor::Black, false)
    }

    #[must_use]
    pub fn has_glowing_text(&self) -> bool {
        self.has_glowing_text.load(Ordering::Relaxed)
    }

    pub fn set_has_glowing_text(&self, has_glowing_text: bool) {
        self.has_glowing_text
            .store(has_glowing_text, Ordering::Relaxed);
    }

    #[must_use]
    pub fn get_color(&self) -> DyeColor {
        let c = self.color.load(Ordering::Relaxed);
        if c >= 0 {
            DyeColor::by_id(c as u8).unwrap_or(DyeColor::Black)
        } else {
            DyeColor::Black
        }
    }

    pub fn set_color(&self, color: DyeColor) {
        self.color.store(color.id() as i8, Ordering::Relaxed);
    }

    #[must_use]
    pub fn get_message(&self, index: usize, should_filter: bool) -> Box<str> {
        if index >= Self::LINES {
            return Box::from("");
        }
        let lock = if should_filter {
            self.filtered_messages.lock()
        } else {
            self.messages.lock()
        };
        lock.unwrap_or_else(std::sync::PoisonError::into_inner)[index].clone()
    }

    pub fn set_message(&self, index: usize, message: Box<str>, filtered_message: Option<Box<str>>) {
        self.set_message_with_filter(index, message, filtered_message, false);
    }

    pub(crate) fn set_message_with_filter(
        &self,
        index: usize,
        message: Box<str>,
        filtered_message: Option<Box<str>>,
        should_filter: bool,
    ) {
        if index >= Self::LINES {
            return;
        }
        let filtered = filtered_message.unwrap_or_else(|| message.clone());
        let style = self.get_components(should_filter)[index].0.style.clone();
        let literal = |message: Box<str>| {
            let mut component = TextComponent::text(message.into_string());
            component.0.style.clone_from(&style);
            Self::component_message(&component)
        };
        self.messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)[index] = literal(message);
        self.filtered_messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)[index] = literal(filtered);
    }

    #[must_use]
    pub fn get_messages(&self, should_filter: bool) -> [Box<str>; 4] {
        let lock = if should_filter {
            self.filtered_messages.lock()
        } else {
            self.messages.lock()
        };
        lock.unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn has_message(&self, should_filter: bool) -> bool {
        self.get_components(should_filter)
            .into_iter()
            .any(|component| !component.get_text().is_empty())
    }

    #[must_use]
    pub fn has_any_click_commands(&self, should_filter: bool) -> bool {
        self.get_components(should_filter).iter().any(|component| {
            matches!(
                component.0.style.click_event.as_ref(),
                Some(ClickEvent::RunCommand { .. })
            )
        })
    }
}

impl BlockEntity for SignBlockEntity {
    fn resource_location(&self) -> &'static str {
        Self::ID
    }

    fn get_position(&self) -> BlockPos {
        self.position
    }

    fn from_nbt(nbt: &pumpkin_nbt::compound::NbtCompound, position: BlockPos) -> Self
    where
        Self: Sized,
    {
        let front_text = nbt
            .get("front_text")
            .cloned()
            .map(SignText::from)
            .unwrap_or_default();
        let back_text = nbt
            .get("back_text")
            .cloned()
            .map(SignText::from)
            .unwrap_or_default();
        let is_waxed = nbt.get_bool("is_waxed").unwrap_or(false);
        Self {
            position,
            front_text,
            back_text,
            is_waxed: AtomicBool::new(is_waxed),
            allow_op_features: AtomicBool::new(nbt.get_bool("allow_op_features").unwrap_or(false)),
            currently_editing_player: Arc::new(Mutex::new(None)),
        }
    }

    fn write_nbt(&self, nbt: &mut NbtCompound) {
        nbt.put("front_text", self.front_text.clone());
        nbt.put("back_text", self.back_text.clone());
        nbt.put_bool("is_waxed", self.is_waxed.load(Ordering::Relaxed));
        nbt.put_bool(
            "allow_op_features",
            self.allow_op_features.load(Ordering::Relaxed),
        );
    }

    fn chunk_data_nbt(&self) -> Option<NbtCompound> {
        let mut nbt = NbtCompound::new();
        nbt.put("front_text", self.front_text.network_nbt());
        nbt.put("back_text", self.back_text.network_nbt());
        nbt.put_bool("is_waxed", self.is_waxed.load(Ordering::Relaxed));
        nbt.put_bool(
            "allow_op_features",
            self.allow_op_features.load(Ordering::Relaxed),
        );
        Some(nbt)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl SignBlockEntity {
    pub const ID: &'static str = "minecraft:sign";

    #[must_use]
    pub fn new(position: BlockPos, is_front: bool, messages: [Box<str>; 4]) -> Self {
        Self {
            position,
            is_waxed: AtomicBool::new(false),
            allow_op_features: AtomicBool::new(false),
            front_text: if is_front {
                SignText::from_messages(messages.clone())
            } else {
                SignText::default()
            },
            back_text: if is_front {
                SignText::default()
            } else {
                SignText::from_messages(messages)
            },
            currently_editing_player: Arc::new(Mutex::new(None)),
        }
    }

    #[must_use]
    pub fn empty(position: BlockPos) -> Self {
        Self {
            position,
            is_waxed: AtomicBool::new(false),
            allow_op_features: AtomicBool::new(false),
            front_text: SignText::default(),
            back_text: SignText::default(),
            currently_editing_player: Arc::new(Mutex::new(None)),
        }
    }
}

pub enum SignEntityRef<'a> {
    Sign(&'a SignBlockEntity),
    Hanging(&'a super::hanging_sign::HangingSignBlockEntity),
}

impl<'a> SignEntityRef<'a> {
    pub fn from_block_entity(entity: &'a dyn BlockEntity) -> Option<Self> {
        entity
            .as_any()
            .downcast_ref::<SignBlockEntity>()
            .map(Self::Sign)
            .or_else(|| {
                entity
                    .as_any()
                    .downcast_ref::<super::hanging_sign::HangingSignBlockEntity>()
                    .map(Self::Hanging)
            })
    }

    #[must_use]
    pub const fn front_text(&self) -> &'a SignText {
        match self {
            Self::Sign(s) => &s.front_text,
            Self::Hanging(s) => &s.front_text,
        }
    }

    #[must_use]
    pub const fn back_text(&self) -> &'a SignText {
        match self {
            Self::Sign(s) => &s.back_text,
            Self::Hanging(s) => &s.back_text,
        }
    }

    #[must_use]
    pub const fn get_text(&self, is_front: bool) -> &'a SignText {
        if is_front {
            self.front_text()
        } else {
            self.back_text()
        }
    }

    #[must_use]
    pub fn is_waxed(&self) -> bool {
        match self {
            Self::Sign(s) => s.is_waxed.load(Ordering::Relaxed),
            Self::Hanging(s) => s.is_waxed.load(Ordering::Relaxed),
        }
    }

    #[must_use]
    pub fn allow_op_features(&self) -> bool {
        match self {
            Self::Sign(s) => s.allow_op_features.load(Ordering::Relaxed),
            Self::Hanging(s) => s.allow_op_features.load(Ordering::Relaxed),
        }
    }

    pub fn set_waxed(&self, waxed: bool) {
        match self {
            Self::Sign(s) => s.is_waxed.store(waxed, Ordering::Relaxed),
            Self::Hanging(s) => s.is_waxed.store(waxed, Ordering::Relaxed),
        }
    }

    #[must_use]
    pub const fn currently_editing_player(&self) -> &'a Arc<Mutex<Option<uuid::Uuid>>> {
        match self {
            Self::Sign(s) => &s.currently_editing_player,
            Self::Hanging(s) => &s.currently_editing_player,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_nbt::{Nbt, deserializer::NbtReadHelperJava};
    use std::io::Cursor;

    fn disk_roundtrip(root: NbtCompound) -> NbtCompound {
        let bytes = Nbt::from(root).try_write_preserving().unwrap();
        let mut reader = NbtReadHelperJava::new_preserving(Cursor::new(bytes.as_ref()));
        Nbt::read_complete(&mut reader).unwrap().root_tag
    }

    fn hover_fixtures() -> [pumpkin_util::text::hover::HoverEvent; 6] {
        use pumpkin_util::text::hover::HoverEvent;

        [
            HoverEvent::show_text(TextComponent::text("tip")),
            HoverEvent::show_text(TextComponent::text("tip").bold()),
            HoverEvent::show_entity(
                "00000000-0000-0000-0000-000000000123",
                "minecraft:pig",
                Some(TextComponent::text("Pig")),
            ),
            HoverEvent::show_entity(
                "00000000-0000-0000-0000-000000000123",
                "minecraft:pig",
                Some(TextComponent::text("Pig").italic()),
            ),
            HoverEvent::ShowText {
                value: vec![
                    TextComponent::text("first").0,
                    TextComponent::text("second").bold().0,
                ],
            },
            HoverEvent::ShowEntity {
                id: "minecraft:pig".into(),
                uuid: "00000000-0000-0000-0000-000000000123".into(),
                name: Some(vec![
                    TextComponent::text("first").0,
                    TextComponent::text("second").bold().0,
                ]),
            },
        ]
    }

    #[test]
    fn compact_hover_signs_keep_root_commands_and_styles_after_literal_edits() {
        for hover in hover_fixtures() {
            let expected = TextComponent::text("Run")
                .bold()
                .color_rgb(pumpkin_util::text::color::RGBColor::new(0x12, 0x34, 0x56))
                .hover_event(hover)
                .click_event(ClickEvent::RunCommand {
                    command: "say actual".into(),
                });
            let mut text = NbtCompound::new();
            text.put_list(
                "messages",
                vec![
                    expected
                        .0
                        .to_nbt_tag_for_version(&JavaMinecraftVersion::V_26_3),
                    "".into(),
                    "".into(),
                    "".into(),
                ],
            );
            let mut container = NbtCompound::new();
            container.put_compound("text", text);
            let mut raw = disk_roundtrip(container);
            let mut edited = expected.clone();
            let literal = r#"{"text":"a literal edit"}"#;
            edited.0.content = TextComponent::text(literal).0.content;

            for cycle in 0..2 {
                let sign = SignText::from(raw.get("text").unwrap().clone());
                assert_eq!(
                    sign.get_components(false)[0],
                    if cycle == 0 {
                        expected.clone()
                    } else {
                        edited.clone()
                    }
                );
                assert!(sign.has_any_click_commands(false));
                sign.set_message(0, Box::from(literal), None);
                assert_eq!(sign.get_components(false)[0], edited);
                assert_eq!(sign.get_components(true)[0], edited);
                assert!(sign.has_any_click_commands(false));
                assert!(sign.has_any_click_commands(true));

                let mut saved = NbtCompound::new();
                saved.put("text", sign.clone());
                raw = disk_roundtrip(saved);
                let list = raw
                    .get_compound("text")
                    .unwrap()
                    .get_list("messages")
                    .unwrap();
                assert_eq!(
                    TextComponent::try_from_nbt(SignText::list_element(&list[0])).unwrap(),
                    edited
                );
                let mut network = NbtCompound::new();
                network.put("text", sign.network_nbt());
                let bytes = Nbt::from(network).try_write_unnamed().unwrap();
                let mut reader = NbtReadHelperJava::new_preserving(Cursor::new(bytes.as_ref()));
                let network = Nbt::read_unnamed_complete(&mut reader).unwrap();
                let list = network
                    .get_compound("text")
                    .unwrap()
                    .get_list("messages")
                    .unwrap();
                assert_eq!(
                    TextComponent::try_from_nbt(SignText::list_element(&list[0])).unwrap(),
                    edited
                );
            }
        }
    }

    fn mixed_sign_fixture(literal_json: &str) -> NbtCompound {
        let mut styled = NbtCompound::new();
        styled.put_string("text", "Styled".to_string());
        styled.put_bool("bold", true);
        styled.put_string("color", "gold".to_string());
        styled.put_string("font", "minecraft:uniform".to_string());
        let mut collision = NbtCompound::new();
        collision.put_string("text", "hello".to_string());
        let mut nested = NbtCompound::new();
        nested.put_string("translate", "chat.type.text".to_string());
        nested.put_list(
            "with",
            vec!["argument".into(), NbtTag::Compound(styled.clone())],
        );
        nested.put_list(
            "extra",
            vec!["sibling".into(), NbtTag::Compound(styled.clone())],
        );
        nested.put_int("future_text_meta", 7);
        let mut escaped = NbtCompound::new();
        escaped.put_string("", "opaque".to_string());
        let mut wrapper = NbtCompound::new();
        wrapper.put_compound("", escaped);
        nested.put_list("future_cells", vec![NbtTag::Compound(wrapper)]);
        let mut text = NbtCompound::new();
        text.put_list(
            "messages",
            vec![
                literal_json.into(),
                NbtTag::Compound(styled),
                NbtTag::Compound(collision),
                NbtTag::Compound(nested),
            ],
        );
        text.put_string("color", "blue".to_string());
        text.put_bool("has_glowing_text", true);
        let mut fixture = NbtCompound::new();
        fixture.put_compound("front_text", text.clone());
        fixture.put_compound("back_text", text);
        fixture.put_bool("is_waxed", false);
        fixture.put_bool("allow_op_features", true);
        disk_roundtrip(fixture)
    }

    #[test]
    fn mixed_sign_components_survive_disk_edits_and_ordinary_network_writers() {
        use crate::block::entities::hanging_sign::HangingSignBlockEntity;

        let literal_json =
            r#"{"text":"literal","click_event":{"action":"run_command","command":"say wrong"}}"#;
        let original = mixed_sign_fixture(literal_json);
        let position = BlockPos::new(0, 64, 0);

        for hanging in [false, true] {
            let mut raw = original.clone();
            for cycle in 0..2 {
                let entity: Box<dyn BlockEntity> = if hanging {
                    Box::new(HangingSignBlockEntity::from_nbt(&raw, position))
                } else {
                    Box::new(SignBlockEntity::from_nbt(&raw, position))
                };
                let sign = SignEntityRef::from_block_entity(entity.as_ref()).unwrap();
                if cycle == 0 {
                    let mut unchanged = NbtCompound::new();
                    entity.write_nbt(&mut unchanged);
                    assert_eq!(disk_roundtrip(unchanged), original);
                }
                for face in [sign.front_text(), sign.back_text()] {
                    assert!(!face.has_any_click_commands(false));
                    assert_eq!(
                        face.get_components(false)[0],
                        TextComponent::text(literal_json)
                    );
                    if cycle == 0 {
                        face.set_message(1, Box::from("Updated"), None);
                        face.set_message(2, Box::from(r#"{"text":"hello"}"#), None);
                    }
                    let edited = face.get_components(false);
                    assert_eq!(edited[1].clone().get_text(), "Updated");
                    let style: serde_json::Value =
                        serde_json::from_str(&face.get_message(1, false)).unwrap();
                    assert_eq!(style["bold"], true);
                    assert_eq!(style["color"], "gold");
                    assert_eq!(style["font"], "minecraft:uniform");
                    assert_eq!(edited[2], TextComponent::text(r#"{"text":"hello"}"#));
                }
                let mut saved = NbtCompound::new();
                entity.write_nbt(&mut saved);
                raw = disk_roundtrip(saved);

                // This is the ordinary unnamed writer used by client NBT, followed
                // by the preserving disk reader so an extra wrapper stays visible.
                let packet = Nbt::from(entity.chunk_data_nbt().unwrap())
                    .try_write_unnamed()
                    .unwrap();
                let mut reader = NbtReadHelperJava::new_preserving(Cursor::new(packet.as_ref()));
                let network = Nbt::read_unnamed_complete(&mut reader).unwrap().root_tag;
                for face in ["front_text", "back_text"] {
                    let messages = network
                        .get_compound(face)
                        .unwrap()
                        .get_list("messages")
                        .unwrap();
                    assert_eq!(
                        TextComponent::try_from_nbt(SignText::list_element(&messages[0])).unwrap(),
                        TextComponent::text(literal_json)
                    );
                    assert_eq!(
                        TextComponent::try_from_nbt(SignText::list_element(&messages[2])).unwrap(),
                        TextComponent::text(r#"{"text":"hello"}"#)
                    );
                    let expected = original
                        .get_compound(face)
                        .unwrap()
                        .get_list("messages")
                        .unwrap();
                    assert_eq!(messages[0], expected[0]);
                    assert_eq!(messages[3], expected[3]);
                    assert_eq!(
                        raw.get_compound(face)
                            .unwrap()
                            .get_list("messages")
                            .unwrap()[3],
                        expected[3]
                    );
                    assert!(
                        messages[3]
                            .extract_compound()
                            .unwrap()
                            .get_int("future_text_meta")
                            .is_some()
                    );
                }
            }
        }
    }

    #[test]
    fn signs_and_hanging_signs_preserve_component_nbt_on_both_faces() {
        use crate::block::entities::hanging_sign::HangingSignBlockEntity;

        let mut literal = NbtCompound::new();
        literal.put_string("text", "Farm entrance".to_string());
        let mut styled = NbtCompound::new();
        styled.put_string("text", "Gold".to_string());
        styled.put_string("color", "gold".to_string());
        styled.put_bool("bold", true);
        let mut translated = NbtCompound::new();
        translated.put_string("translate", "block.minecraft.chest".to_string());
        translated.put_list("with", vec![NbtTag::Compound(literal.clone())]);
        translated.put_list("extra", vec![NbtTag::Compound(styled.clone())]);
        let mut empty = NbtCompound::new();
        empty.put_string("text", String::new());
        let messages = vec![
            NbtTag::Compound(literal),
            NbtTag::Compound(styled),
            NbtTag::Compound(translated),
            NbtTag::Compound(empty),
        ];
        let mut text = NbtCompound::new();
        text.put_list("messages", messages.clone());
        let mut filtered = messages.clone();
        filtered.swap(0, 1);
        text.put_list("filtered_messages", filtered.clone());
        text.put_string("color", "blue".to_string());
        text.put_bool("has_glowing_text", true);
        let mut fixture = NbtCompound::new();
        fixture.put_compound("front_text", text.clone());
        fixture.put_compound("back_text", text);
        let position = BlockPos::new(0, 64, 0);

        for hanging in [false, true] {
            let mut raw = fixture.clone();
            for _ in 0..2 {
                let entity: Box<dyn BlockEntity> = if hanging {
                    Box::new(HangingSignBlockEntity::from_nbt(&raw, position))
                } else {
                    Box::new(SignBlockEntity::from_nbt(&raw, position))
                };
                let sign = SignEntityRef::from_block_entity(entity.as_ref()).unwrap();
                for face in [sign.front_text(), sign.back_text()] {
                    assert!(face.has_glowing_text());
                    assert_eq!(face.get_color(), DyeColor::Blue);
                    let styled: serde_json::Value =
                        serde_json::from_str(&face.get_message(1, false)).unwrap();
                    assert_eq!(styled["text"], "Gold");
                    assert_eq!(styled["color"], "gold");
                    assert_eq!(styled["bold"], true);
                    let translated: serde_json::Value =
                        serde_json::from_str(&face.get_message(2, false)).unwrap();
                    assert_eq!(translated["translate"], "block.minecraft.chest");
                    assert_eq!(translated["with"][0], "Farm entrance");
                    for (should_filter, expected) in [(false, &messages), (true, &filtered)] {
                        for (index, tag) in expected.iter().enumerate() {
                            let decoded: TextComponent =
                                serde_json::from_str(&face.get_message(index, should_filter))
                                    .unwrap();
                            assert_eq!(decoded, TextComponent::try_from_nbt(tag).unwrap());
                        }
                    }
                }
                let mut saved = NbtCompound::new();
                entity.write_nbt(&mut saved);
                for face in ["front_text", "back_text"] {
                    let saved_text = saved.get_compound(face).unwrap();
                    assert_eq!(saved_text.get_list("messages"), Some(messages.as_slice()));
                    assert_eq!(
                        saved_text.get_list("filtered_messages"),
                        Some(filtered.as_slice())
                    );
                }
                raw = saved;
            }
        }
    }

    #[test]
    fn unreadable_sign_line_is_retained_without_shifting_or_erasing_other_lines() {
        let mut unsupported = NbtCompound::new();
        unsupported.put_string("future_component", "retain me".to_string());
        let mut literal = NbtCompound::new();
        literal.put_string("text", "Visible second line".to_string());
        let mut text = NbtCompound::new();
        let messages = vec![
            NbtTag::Compound(unsupported),
            NbtTag::Compound(literal),
            NbtTag::String(Box::from("third")),
            NbtTag::String(Box::from("")),
        ];
        text.put_list("messages", messages);
        let mut container = NbtCompound::new();
        container.put_compound("text", text);
        let container = disk_roundtrip(container);
        let mut raw = container.get("text").unwrap().clone();
        let messages = raw
            .extract_compound()
            .unwrap()
            .get_list("messages")
            .unwrap()
            .to_vec();
        for _ in 0..2 {
            let sign = SignText::from(raw);
            let second: TextComponent = serde_json::from_str(&sign.get_message(1, false)).unwrap();
            assert_eq!(second, TextComponent::text("Visible second line"));
            assert_eq!(sign.get_components(false)[2], TextComponent::text("third"));
            raw = sign.into();
            assert_eq!(
                raw.extract_compound().unwrap().get_list("messages"),
                Some(messages.as_slice())
            );
        }
        let sign = SignText::from(raw);
        sign.set_message(1, Box::from("Edited"), None);
        let saved: NbtTag = sign.into();
        let saved_messages = saved
            .extract_compound()
            .unwrap()
            .get_list("messages")
            .unwrap();
        assert_eq!(saved_messages[0], messages[0]);
        assert_eq!(
            SignText::list_element(&saved_messages[1]).extract_string(),
            Some("Edited")
        );
        assert_eq!(saved_messages[2], messages[2]);
    }

    #[test]
    fn signs_preserve_explicit_op_feature_permission_with_safe_default() {
        use crate::block::entities::hanging_sign::HangingSignBlockEntity;
        let position = BlockPos::new(0, 64, 0);
        let mut nbt = NbtCompound::new();
        for allow in [false, true] {
            nbt.put_bool("allow_op_features", allow);
            let sign = SignBlockEntity::from_nbt(&nbt, position);
            let hanging = HangingSignBlockEntity::from_nbt(&nbt, position);
            assert_eq!(SignEntityRef::Sign(&sign).allow_op_features(), allow);
            assert_eq!(SignEntityRef::Hanging(&hanging).allow_op_features(), allow);
            for entity in [&sign as &dyn BlockEntity, &hanging as &dyn BlockEntity] {
                let mut saved = NbtCompound::new();
                entity.write_nbt(&mut saved);
                assert_eq!(saved.get_bool("allow_op_features"), Some(allow));
                assert_eq!(
                    entity
                        .chunk_data_nbt()
                        .unwrap()
                        .get_bool("allow_op_features"),
                    Some(allow)
                );
            }
        }
        assert!(
            !SignEntityRef::Sign(&SignBlockEntity::from_nbt(&NbtCompound::new(), position))
                .allow_op_features()
        );
        assert!(
            !SignEntityRef::Hanging(&HangingSignBlockEntity::empty(position)).allow_op_features()
        );
    }
}
