use std::io::{self, Cursor, Write};

use crate::{
    Error, Nbt, NbtCompound,
    deserializer::{NbtReadHelper, NbtReadHelperJava},
    nbt_compress::read_gzip_compound_tag,
    tag::NbtTag,
};

#[test]
fn incomplete_and_ambiguous_compounds_fail_reads_and_skips() {
    let invalid: &[&[u8]] = &[
        &[10, 0, 0],                                        // Missing root END.
        &[10, 0, 0, 10, 0, 1, b'x', 0],                     // Only inner compound ended.
        &[10, 0, 0, 1, 0, 1, b'x', 1, 1, 0, 1, b'x', 2, 0], // Duplicate key.
        &[10, 0, 0, 9, 0, 1, b'x', 255, 0, 0, 0, 0, 0],     // Invalid empty list subtype.
        &[10, 0, 0, 9, 0, 1, b'x', 0, 0, 0, 0, 1, 0],       // Nonempty END list.
        &[10, 0, 0, 7, 0, 1, b'x', 255, 255, 255, 255, 0],  // Negative array length.
    ];
    for bytes in invalid {
        let mut reader = NbtReadHelperJava::new(Cursor::new(*bytes));
        assert!(Nbt::read(&mut reader).is_err(), "accepted {bytes:?}");
        let mut reader = NbtReadHelperJava::new(Cursor::new(&bytes[3..]));
        assert!(
            NbtCompound::skip_content(&mut reader).is_err(),
            "skipped {bytes:?}"
        );
    }
}

#[test]
fn skipping_truncated_payloads_does_not_seek_past_eof() {
    // A declared four-byte byte array only contains one byte.
    let mut reader = NbtReadHelperJava::new(Cursor::new(&[0, 0, 0, 4, 1][..]));
    assert!(NbtTag::skip_data(&mut reader, crate::BYTE_ARRAY_ID).is_err());
    let mut reader = NbtReadHelperJava::new(Cursor::new(&[1][..]));
    assert!(NbtTag::skip_data(&mut reader, crate::INT_ID).is_err());
}

#[test]
fn complete_documents_reject_trailing_bytes_but_packet_reads_leave_them() {
    let bytes = [10, 0, 0, 0, 42];
    let mut reader = NbtReadHelperJava::new(Cursor::new(bytes.as_slice()));
    assert!(matches!(
        Nbt::read_complete(&mut reader),
        Err(Error::TrailingData)
    ));
    let mut reader = NbtReadHelperJava::new(Cursor::new(bytes.as_slice()));
    Nbt::read(&mut reader).unwrap();
    assert_eq!(reader.get_u8().unwrap(), 42);
    let mut reader = NbtReadHelperJava::new(Cursor::new(&[10, 0, 42][..]));
    assert!(matches!(
        Nbt::read_unnamed_complete(&mut reader),
        Err(Error::TrailingData)
    ));
}

#[test]
fn vanilla_mixed_list_wire_and_semantic_round_trips() {
    // Produced by 26.3 NbtIo.write: Items=[42, "minecraft:chest", {"":"opaque"}].
    // ListTag wraps mixed types and additionally wraps the empty-name compound.
    let bytes = [
        10, 0, 0, 9, 0, 5, 73, 116, 101, 109, 115, 10, 0, 0, 0, 3, 3, 0, 0, 0, 0, 0, 42, 0, 8, 0,
        0, 0, 15, 109, 105, 110, 101, 99, 114, 97, 102, 116, 58, 99, 104, 101, 115, 116, 0, 10, 0,
        0, 8, 0, 0, 0, 6, 111, 112, 97, 113, 117, 101, 0, 0, 0,
    ];
    let mut reader = NbtReadHelperJava::new_preserving(Cursor::new(bytes.as_slice()));
    let raw = Nbt::read_complete(&mut reader).unwrap();
    assert!(
        raw.get_list("Items")
            .unwrap()
            .iter()
            .all(|tag| matches!(tag, NbtTag::Compound(_)))
    );
    assert_eq!(raw.try_write_preserving().unwrap().as_ref(), bytes);

    let mut reader = NbtReadHelperJava::new(Cursor::new(bytes.as_slice()));
    let semantic = Nbt::read_complete(&mut reader).unwrap();
    let items = semantic.get_list("Items").unwrap();
    assert_eq!(items[0], NbtTag::Int(42));
    assert_eq!(items[1], NbtTag::String("minecraft:chest".into()));
    assert!(
        matches!(&items[2], NbtTag::Compound(compound) if compound.get_string("") == Some("opaque"))
    );
    assert_eq!(
        semantic.clone().try_write_preserving().unwrap().as_ref(),
        bytes
    );
    assert_eq!(semantic.try_write().unwrap().as_ref(), bytes);
}

#[test]
fn writers_reject_named_end_invalid_lists_and_oversized_strings() {
    let mut root = NbtCompound::new();
    root.put("invalid", NbtTag::End);
    assert!(matches!(
        Nbt::from(root.clone()).try_write(),
        Err(Error::NamedEndTag(_))
    ));
    assert!(Nbt::from(root).write_to_writer(Vec::new()).is_err());

    let mut root = NbtCompound::new();
    root.put_list("invalid", vec![NbtTag::End]);
    assert!(matches!(
        Nbt::from(root).try_write(),
        Err(Error::InvalidListTag(0))
    ));

    let mut root = NbtCompound::new();
    root.put_string("invalid", "x".repeat(65_536));
    assert!(matches!(
        Nbt::from(root.clone()).try_write(),
        Err(Error::LargeLength(65_536))
    ));
    assert_eq!(
        Nbt::from(root)
            .write_to_writer(Vec::new())
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn stream_writers_propagate_underlying_io_errors() {
    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::StorageFull, "disk full"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    assert_eq!(
        Nbt::default()
            .write_to_writer(FailingWriter)
            .unwrap_err()
            .kind(),
        io::ErrorKind::StorageFull
    );
}

#[test]
fn gzip_requires_complete_nbt_and_all_compressed_bytes() {
    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }
    for bytes in [&[10, 0, 0][..], &[10, 0, 0, 0, 42][..]] {
        assert!(read_gzip_compound_tag(Cursor::new(gzip(bytes))).is_err());
    }
    let good = gzip(&[10, 0, 0, 0]);
    assert!(read_gzip_compound_tag(Cursor::new(&good)).is_ok());
    let mut trailing = good.clone();
    trailing.push(42);
    assert!(read_gzip_compound_tag(Cursor::new(trailing)).is_err());
    let mut concatenated = good.clone();
    concatenated.extend_from_slice(&good);
    assert!(read_gzip_compound_tag(Cursor::new(concatenated)).is_err());
    assert!(read_gzip_compound_tag(Cursor::new(&good[..good.len() - 1])).is_err());
}
