use pumpkin_data::packet::serverbound::play::SPECTATOR_ACTION;
use pumpkin_macros::java_packet;
use pumpkin_util::version::JavaMinecraftVersion;

use crate::{
    ClientPacket, ServerPacket, VarInt,
    ser::{NetworkReadExt, NetworkWriteExt, ReadingError, WritingError},
};

#[java_packet(SPECTATOR_ACTION)]
#[derive(Debug, PartialEq, Eq)]
pub struct SSpectatorAction {
    pub spectate_entity_id: Option<i32>,
}

impl<'a> ServerPacket<'a> for SSpectatorAction {
    fn read(bytebuf: &mut &'a [u8], _version: &JavaMinecraftVersion) -> Result<Self, ReadingError> {
        let encoded = bytebuf.get_var_int()?.0;
        // Vanilla ByteBufCodecs.OPTIONAL_VAR_INT reserves zero for no action.
        Ok(Self {
            spectate_entity_id: (encoded != 0).then(|| encoded.wrapping_sub(1)),
        })
    }
}

impl ClientPacket for SSpectatorAction {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        _version: &JavaMinecraftVersion,
    ) -> Result<(), WritingError> {
        write.write_var_int(&VarInt(
            self.spectate_entity_id.map_or(0, |id| id.wrapping_add(1)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MultiVersionJavaPacket;
    use crate::java::server::play::SSpectateEntity;

    #[test]
    fn spectator_action_26_3_known_bytes() {
        // Official 26.3 OPTIONAL_VAR_INT fixtures, including the first two-byte target.
        for (bytes, target) in [
            (&[0x00][..], None),
            (&[0x01][..], Some(0)),
            (&[0x7f][..], Some(126)),
            (&[0x80, 0x01][..], Some(127)),
        ] {
            let mut payload = bytes;
            let packet = SSpectatorAction::read(&mut payload, &JavaMinecraftVersion::V_26_3)
                .expect("valid official spectator action");
            assert_eq!(packet.spectate_entity_id, target);
            assert!(
                payload.is_empty(),
                "must consume exactly the action payload"
            );

            let mut encoded = Vec::new();
            packet
                .write_packet_data(&mut encoded, &JavaMinecraftVersion::V_26_3)
                .expect("encode spectator action");
            assert_eq!(encoded, bytes);
        }
    }

    #[test]
    fn spectator_action_rejects_empty_and_truncated_varint() {
        for mut bytes in [&[][..], &[0x80][..]] {
            assert!(
                SSpectatorAction::read(&mut bytes, &JavaMinecraftVersion::V_26_3).is_err(),
                "missing target VarInt must remain a decode error"
            );
        }
    }

    #[test]
    fn legacy_uuid_spectate_does_not_claim_native_action() {
        // Generated Wasm adapters still expose the legacy UUID type: it must not decode 63.
        assert_eq!(SSpectateEntity::to_id(JavaMinecraftVersion::V_26_3), -1);
        assert_ne!(
            SSpectateEntity::to_id(JavaMinecraftVersion::V_26_3),
            SSpectatorAction::to_id(JavaMinecraftVersion::V_26_3)
        );
    }
}
