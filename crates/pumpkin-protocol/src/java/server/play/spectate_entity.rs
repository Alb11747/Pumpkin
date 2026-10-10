use pumpkin_data::packet::serverbound::play::SPECTATE_ENTITY;

use crate::{
    MultiVersionJavaPacket, ServerPacket,
    ser::{NetworkReadExt, ReadingError},
};
use pumpkin_util::version::JavaMinecraftVersion;

pub struct SSpectateEntity {
    pub target: uuid::Uuid,
}

impl MultiVersionJavaPacket for SSpectateEntity {
    fn to_id(version: JavaMinecraftVersion) -> i32 {
        // The legacy UUID payload remains in the public Wasm API, but 26.3 replaced it.
        if version >= JavaMinecraftVersion::V_26_3 {
            -1
        } else {
            SPECTATE_ENTITY.to_id(version)
        }
    }
}

impl<'a> ServerPacket<'a> for SSpectateEntity {
    fn read(bytebuf: &mut &'a [u8], _version: &JavaMinecraftVersion) -> Result<Self, ReadingError> {
        Ok(Self {
            target: bytebuf.get_uuid()?,
        })
    }
}

impl crate::ClientPacket for SSpectateEntity {
    fn write_packet_data(
        &self,
        mut write: impl std::io::Write,
        _version: &JavaMinecraftVersion,
    ) -> Result<(), crate::ser::WritingError> {
        use crate::ser::NetworkWriteExt;
        write.write_uuid(&self.target)?;
        Ok(())
    }
}
