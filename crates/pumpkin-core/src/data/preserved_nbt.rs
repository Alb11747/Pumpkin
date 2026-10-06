use std::collections::HashMap;

use pumpkin_nbt::{NbtCompound, tag::NbtTag};

#[derive(Default)]
struct ModeledKeys(HashMap<Box<str>, Option<Self>>);

impl ModeledKeys {
    fn from_compound(compound: &NbtCompound) -> Self {
        Self(
            compound
                .child_tags
                .iter()
                .map(|(key, value)| {
                    let children = match value {
                        NbtTag::Compound(compound) => Some(Self::from_compound(compound)),
                        _ => None,
                    };
                    (key.clone(), children)
                })
                .collect(),
        )
    }
}

/// Keeps fields the live model does not own, including nested compound fields.
/// Lists are managed as a whole: their owning codecs must retain element metadata.
pub struct PreservedNbt {
    raw: NbtCompound,
    modeled: ModeledKeys,
}

impl PreservedNbt {
    pub(crate) fn new(raw: &NbtCompound, modeled: &NbtCompound) -> Self {
        Self {
            raw: raw.clone(),
            modeled: ModeledKeys::from_compound(modeled),
        }
    }

    pub(crate) fn discard(&mut self, names: &[&str]) {
        for name in names {
            self.raw.child_tags.remove(*name);
        }
    }

    pub(crate) fn discard_aliases(&mut self, current: &NbtCompound, aliases: &[(&str, &str)]) {
        for (alias, canonical) in aliases {
            if current.get(canonical).is_some() || self.modeled.0.contains_key(*canonical) {
                self.raw.child_tags.remove(*alias);
            }
        }
    }

    pub(crate) fn discard_removed_dependencies(
        &mut self,
        current: &NbtCompound,
        dependencies: &[(&str, &str)],
    ) {
        for (dependent, owner) in dependencies {
            if self.modeled.0.contains_key(*owner) && current.get(owner).is_none() {
                self.raw.child_tags.remove(*dependent);
            }
        }
    }

    pub(crate) fn snapshot(&mut self, current: &mut NbtCompound) {
        let next_keys = ModeledKeys::from_compound(current);
        merge_compound(&mut self.raw, &self.modeled, current);
        *current = self.raw.clone();
        self.modeled = next_keys;
    }
}

fn merge_compound(raw: &mut NbtCompound, modeled: &ModeledKeys, current: &NbtCompound) {
    // Omitted modeled branches were cleared. Unmanaged branches remain intact.
    raw.child_tags
        .retain(|key, _| !modeled.0.contains_key(key) || current.child_tags.contains_key(key));
    for (key, value) in &current.child_tags {
        if let NbtTag::Compound(current_child) = value
            && let Some(NbtTag::Compound(raw_child)) = raw.child_tags.get_mut(key)
        {
            match modeled.0.get(key) {
                Some(None) => *raw_child = current_child.clone(),
                Some(Some(owned_child)) => merge_compound(raw_child, owned_child, current_child),
                None => merge_compound(raw_child, &ModeledKeys::default(), current_child),
            }
        } else {
            raw.child_tags.insert(key.clone(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_compound_fields_survive_while_modeled_deletions_stay_deleted() {
        let mut raw_child = NbtCompound::new();
        raw_child.put_string("id", "minecraft:pig".into());
        raw_child.put_int("unmanaged", 42);
        let mut raw = NbtCompound::new();
        raw.put_compound("SpawnData", raw_child);
        raw.put_string("CustomName", "old".into());
        raw.put_string("opaque", "retain".into());
        let mut child = NbtCompound::new();
        child.put_string("id", "minecraft:pig".into());
        let mut initial = NbtCompound::new();
        initial.put_compound("SpawnData", child.clone());
        initial.put_string("CustomName", "old".into());
        let mut preserved = PreservedNbt::new(&raw, &initial);

        child.put_string("id", "minecraft:cow".into());
        let mut current = NbtCompound::new();
        current.put_compound("SpawnData", child);
        preserved.snapshot(&mut current);
        assert!(current.get("CustomName").is_none());
        assert_eq!(current.get_string("opaque"), Some("retain"));
        assert_eq!(
            current
                .get_compound("SpawnData")
                .unwrap()
                .get_int("unmanaged"),
            Some(42)
        );
        assert_eq!(
            current.get_compound("SpawnData").unwrap().get_string("id"),
            Some("minecraft:cow")
        );

        let mut cleared = NbtCompound::new();
        preserved.snapshot(&mut cleared);
        assert!(cleared.get("SpawnData").is_none());
        assert!(cleared.get("CustomName").is_none());
        let mut readded = NbtCompound::new();
        readded.put_compound("SpawnData", NbtCompound::new());
        preserved.snapshot(&mut readded);
        assert!(readded.get_compound("SpawnData").unwrap().is_empty());
    }

    #[test]
    fn newly_modeled_fields_and_legacy_aliases_do_not_resurrect() {
        let mut raw = NbtCompound::new();
        raw.put_string("future", "old opaque value".into());
        raw.put_string("Legacy", "old".into());
        let mut preserved = PreservedNbt::new(&raw, &NbtCompound::new());
        let mut first = NbtCompound::new();
        first.put_string("future", "now modeled".into());
        first.put_string("canonical", "current".into());
        preserved.discard_aliases(&first, &[("Legacy", "canonical")]);
        preserved.snapshot(&mut first);
        assert!(first.get("Legacy").is_none());
        let mut second = NbtCompound::new();
        preserved.snapshot(&mut second);
        assert!(second.is_empty());
    }
}
