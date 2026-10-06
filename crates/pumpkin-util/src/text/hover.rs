use std::{borrow::Cow, vec};

use serde::{Deserialize, Deserializer, Serialize};

use super::{TextComponent, TextComponentBase};

#[derive(Deserialize)]
#[serde(untagged)]
enum HoverComponents {
    Array(Vec<TextComponent>),
    Single(TextComponent),
}

// ComponentSerialization.CODEC accepts a compact string/compound or an array;
// retain the existing vector representation for callers and serialization.
impl HoverComponents {
    fn into_components(self) -> Vec<TextComponentBase> {
        match self {
            Self::Array(components) => components
                .into_iter()
                .map(|component| component.0)
                .collect(),
            Self::Single(component) => vec![component.0],
        }
    }
}

fn deserialize_components<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<TextComponentBase>, D::Error> {
    HoverComponents::deserialize(deserializer).map(HoverComponents::into_components)
}

fn deserialize_optional_components<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<TextComponentBase>>, D::Error> {
    Option::<HoverComponents>::deserialize(deserializer)
        .map(|value| value.map(HoverComponents::into_components))
}

/// Represents the hover event action in a chat component.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HoverEvent {
    /// Displays a tooltip with the given text.
    ShowText {
        #[serde(deserialize_with = "deserialize_components")]
        value: Vec<TextComponentBase>,
    },
    /// Shows an item.
    ShowItem {
        /// Resource identifier of the item.
        id: Cow<'static, str>,
        /// Number of the items in the stack.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<i32>,
        // #[serde(default, skip_serializing_if = "Option::is_none")]
        // components: Option<Cow<'static, str>>,
    },
    /// Shows an entity.
    ShowEntity {
        /// The entity's ID Entity Type.
        id: Cow<'static, str>,
        /// The entity's UUID
        /// The UUID cannot use `uuid::Uuid` because its serialization parses it into bytes, so its double bytes serialized.
        uuid: Cow<'static, str>,
        /// Optional custom name for the entity.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_optional_components"
        )]
        name: Option<Vec<TextComponentBase>>,
    },
}

impl HoverEvent {
    /// Creates a new hover event that displays text.
    ///
    /// # Arguments
    /// - `text` – The text component to display in the tooltip.
    ///
    /// # Returns
    /// A `HoverEvent::ShowText` variant containing the provided text.
    #[must_use]
    pub fn show_text(text: TextComponent) -> Self {
        Self::ShowText {
            value: vec![text.0],
        }
    }

    /// Creates a new hover event that displays entity information.
    ///
    /// # Arguments
    /// - `uuid` – The entity's UUID as a string.
    /// - `kind` – The entity type identifier (e.g., "minecraft:pig").
    /// - `name` – Optional custom name for the entity.
    ///
    /// # Returns
    /// A `HoverEvent::ShowEntity` variant containing the entity information.
    pub fn show_entity<P: Into<Cow<'static, str>>>(
        uuid: P,
        kind: P,
        name: Option<TextComponent>,
    ) -> Self {
        Self::ShowEntity {
            id: kind.into(),
            uuid: uuid.into(),
            name: match name {
                Some(name) => Some(vec![name.0]),
                None => None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::JavaMinecraftVersion;
    use pumpkin_nbt::{Nbt, NbtCompound, deserializer::NbtReadHelperJava};
    use std::io::Cursor;

    #[test]
    fn hover_components_accept_compact_and_array_json_and_nbt() {
        let version = JavaMinecraftVersion::V_26_3;
        for components in [
            vec![TextComponent::text("tip").0],
            vec![TextComponent::text("tip").bold().0],
            vec![
                TextComponent::text("first").0,
                TextComponent::text("second").0,
            ],
            vec![
                TextComponent::text("first").0,
                TextComponent::text("second").bold().0,
            ],
        ] {
            for hover in [
                HoverEvent::ShowText {
                    value: components.clone(),
                },
                HoverEvent::ShowEntity {
                    id: Cow::Borrowed("minecraft:pig"),
                    uuid: Cow::Borrowed("00000000-0000-0000-0000-000000000123"),
                    name: Some(components.clone()),
                },
            ] {
                let expected = TextComponent::text("Hover").hover_event(hover);
                let mut component = expected.clone();
                for _ in 0..2 {
                    let json = component.0.to_json_value_for_version(&version).to_string();
                    component = serde_json::from_str(&json).unwrap();
                    assert_eq!(component, expected);
                    let mut root = NbtCompound::new();
                    root.put("text", component.0.to_nbt_tag_for_version(&version));
                    let bytes = Nbt::from(root).try_write_preserving().unwrap();
                    let mut reader = NbtReadHelperJava::new_preserving(Cursor::new(bytes.as_ref()));
                    let raw = Nbt::read_complete(&mut reader).unwrap();
                    if components.len() == 2 && components[1].style.bold == Some(true) {
                        let hover = raw
                            .get_compound("text")
                            .unwrap()
                            .get_compound("hover_event")
                            .unwrap();
                        let field = if hover.get_string("action") == Some("show_text") {
                            "value"
                        } else {
                            "name"
                        };
                        let list = hover.get_list(field).unwrap();
                        assert_eq!(
                            list[0].extract_compound().unwrap().get_string(""),
                            Some("first")
                        );
                        assert_eq!(
                            list[1].extract_compound().unwrap().get_bool("bold"),
                            Some(true)
                        );
                    }
                    component = TextComponent::try_from_nbt(raw.get("text").unwrap()).unwrap();
                    assert_eq!(component, expected);
                }
            }
        }
        for json in [
            r#"{"action":"show_entity","id":"minecraft:pig","uuid":"test"}"#,
            r#"{"action":"show_entity","id":"minecraft:pig","uuid":"test","name":null}"#,
        ] {
            assert!(matches!(
                serde_json::from_str::<HoverEvent>(json).unwrap(),
                HoverEvent::ShowEntity { name: None, .. }
            ));
        }
        assert!(
            serde_json::from_str::<HoverEvent>(r#"{"action":"show_text","value":false}"#).is_err()
        );
    }
}
