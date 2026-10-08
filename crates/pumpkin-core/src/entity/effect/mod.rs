pub mod hunger;
pub mod infested;
pub mod oozing;
pub mod poison;
pub mod raid_omen;
pub mod regeneration;
pub mod saturation;
pub mod weaving;
pub mod wind_charged;
pub mod wither;

use pumpkin_data::damage::DamageType;
use pumpkin_data::effect::StatusEffect;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_nbt::tag::NbtTag;
use tracing::warn;

use crate::entity::living::LivingEntity;
use crate::entity::{NBTStorage, NBTStorageInit};

pub trait MobEffect: Send + Sync {
    /// Returns true if `apply_effect_tick` should be called for the current tick and duration.
    fn should_apply_effect_tick(&self, _duration: i32, _amplifier: u8) -> bool {
        false
    }

    /// Applies periodic/tick-based effect logic on a living entity.
    fn apply_effect_tick(&self, _living: &LivingEntity, _amplifier: u8) {}

    /// Called when an entity carrying this effect is hurt.
    fn on_mob_hurt(
        &self,
        _living: &LivingEntity,
        _amplifier: u8,
        _damage_type: &DamageType,
        _damage_amount: f32,
    ) {
    }

    /// Called when an entity carrying this effect dies.
    fn on_mob_death(&self, _living: &LivingEntity, _amplifier: u8, _damage_type: &DamageType) {}
}

pub static REGENERATION: regeneration::RegenerationMobEffect = regeneration::RegenerationMobEffect;
pub static POISON: poison::PoisonMobEffect = poison::PoisonMobEffect;
pub static WITHER: wither::WitherMobEffect = wither::WitherMobEffect;
pub static HUNGER: hunger::HungerMobEffect = hunger::HungerMobEffect;
pub static SATURATION: saturation::SaturationMobEffect = saturation::SaturationMobEffect;
pub static RAID_OMEN: raid_omen::RaidOmenMobEffect = raid_omen::RaidOmenMobEffect;
pub static INFESTED: infested::InfestedMobEffect = infested::InfestedMobEffect;
pub static OOZING: oozing::OozingMobEffect = oozing::OozingMobEffect;
pub static WEAVING: weaving::WeavingMobEffect = weaving::WeavingMobEffect;
pub static WIND_CHARGED: wind_charged::WindChargedMobEffect = wind_charged::WindChargedMobEffect;

#[must_use]
pub fn get_mob_effect(effect: &'static StatusEffect) -> Option<&'static dyn MobEffect> {
    if effect == &StatusEffect::REGENERATION {
        Some(&REGENERATION)
    } else if effect == &StatusEffect::POISON {
        Some(&POISON)
    } else if effect == &StatusEffect::WITHER {
        Some(&WITHER)
    } else if effect == &StatusEffect::HUNGER {
        Some(&HUNGER)
    } else if effect == &StatusEffect::SATURATION {
        Some(&SATURATION)
    } else if effect == &StatusEffect::RAID_OMEN {
        Some(&RAID_OMEN)
    } else if effect == &StatusEffect::INFESTED {
        Some(&INFESTED)
    } else if effect == &StatusEffect::OOZING {
        Some(&OOZING)
    } else if effect == &StatusEffect::WEAVING {
        Some(&WEAVING)
    } else if effect == &StatusEffect::WIND_CHARGED {
        Some(&WIND_CHARGED)
    } else {
        None
    }
}

impl NBTStorage for pumpkin_data::potion::Effect {
    fn write_nbt(&self, nbt: &mut NbtCompound) {
        nbt.put("id", self.effect_type.minecraft_name);
        if self.amplifier > 0 {
            // Vanilla ExtraCodecs.UNSIGNED_BYTE uses the signed NBT byte's raw bits.
            nbt.put("amplifier", NbtTag::Byte(self.amplifier as i8));
        }
        nbt.put("duration", NbtTag::Int(self.duration));
        if self.ambient {
            nbt.put("ambient", NbtTag::Byte(1));
        }
        if !self.show_particles {
            nbt.put("show_particles", NbtTag::Byte(0));
        }
        let show_icon: i8 = i8::from(self.show_icon);
        nbt.put("show_icon", NbtTag::Byte(show_icon));
    }
}

impl NBTStorageInit for pumpkin_data::potion::Effect {
    fn create_from_nbt(nbt: &mut NbtCompound) -> Option<Self> {
        let Some(effect_id) = nbt.get_string("id") else {
            warn!("Unable to read effect. Effect id is not present");
            return None;
        };
        let Some(effect_type) = StatusEffect::from_minecraft_name(effect_id) else {
            warn!("Unable to read effect. Unknown effect type: {effect_id}");
            return None;
        };
        let amplifier = match nbt.get("amplifier") {
            None => 0,
            Some(NbtTag::Byte(value)) => *value as u8,
            // Accept older Pumpkin saves, which incorrectly wrote an int.
            Some(NbtTag::Int(value)) => match u8::try_from(*value) {
                Ok(value) => value,
                Err(_) => {
                    warn!("Unable to read effect. Amplifier is outside the unsigned byte range");
                    return None;
                }
            },
            Some(_) => {
                warn!("Unable to read effect. Amplifier has an unsupported NBT type");
                return None;
            }
        };
        let duration = nbt.get_int("duration").unwrap_or(0);
        let ambient = nbt.get_byte("ambient").unwrap_or(0) == 1;
        let show_particles = nbt.get_byte("show_particles").unwrap_or(1) == 1;
        let show_icon = nbt
            .get_byte("show_icon")
            .map_or(show_particles, |value| value == 1);
        Some(Self {
            effect_type,
            duration,
            amplifier,
            ambient,
            show_particles,
            show_icon,
            blend: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_data::potion::Effect;

    #[test]
    fn amplifier_reads_unsigned_bytes_and_legacy_ints_and_writes_bytes() {
        for amplifier in [0_u8, 1, 127, 128, 255] {
            for tag in [
                NbtTag::Byte(amplifier as i8),
                NbtTag::Int(i32::from(amplifier)),
            ] {
                let mut input = NbtCompound::new();
                input.put_string("id", "minecraft:speed".to_owned());
                input.put("amplifier", tag);
                let effect = Effect::create_from_nbt(&mut input).unwrap();
                assert_eq!(effect.amplifier, amplifier);
                let mut saved = NbtCompound::new();
                effect.write_nbt(&mut saved);
                if amplifier == 0 {
                    assert!(saved.get("amplifier").is_none());
                } else {
                    assert_eq!(saved.get_byte("amplifier"), Some(amplifier as i8));
                    assert!(saved.get_int("amplifier").is_none());
                }
                assert_eq!(
                    Effect::create_from_nbt(&mut saved).unwrap().amplifier,
                    amplifier
                );
            }
        }
    }

    #[test]
    fn missing_show_icon_defaults_to_particles_and_explicit_icon_wins() {
        for particles in [false, true] {
            let mut input = NbtCompound::new();
            input.put_string("id", "minecraft:speed".to_owned());
            input.put_bool("show_particles", particles);
            assert_eq!(
                Effect::create_from_nbt(&mut input).unwrap().show_icon,
                particles
            );
            input.put_bool("show_icon", !particles);
            assert_eq!(
                Effect::create_from_nbt(&mut input).unwrap().show_icon,
                !particles
            );
        }
    }

    #[test]
    fn unsupported_amplifiers_do_not_wrap_or_default() {
        for tag in [NbtTag::Int(-1), NbtTag::Int(256), NbtTag::Short(1)] {
            let mut input = NbtCompound::new();
            input.put_string("id", "minecraft:speed".to_owned());
            input.put("amplifier", tag);
            assert!(Effect::create_from_nbt(&mut input).is_none());
        }
    }
}
