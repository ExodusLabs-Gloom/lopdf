use lopdf::{
    Document, Object, Stream, dictionary,
    xref::{Xref, XrefEntry, XrefStreamBuilder, XrefType, decode_xref_stream},
};

fn object(bytes: &mut Vec<u8>, id: u32, generation: u16, value: &str) -> usize {
    let offset = bytes.len();
    bytes.extend_from_slice(format!("{id} {generation} obj\n{value}\nendobj\n").as_bytes());
    offset
}

fn section(bytes: &mut Vec<u8>, entry: &str, extra: &str) -> usize {
    let offset = bytes.len();
    bytes.extend_from_slice(
        format!(
            "xref\n0 1\n0000000000 65535 f \n5 1\n{entry}\ntrailer\n<< /Size 6 {extra} >>\nstartxref\n{offset}\n%%EOF\n"
        )
        .as_bytes(),
    );
    offset
}

fn normal(offset: usize, generation: u16) -> String {
    format!("{offset:010} {generation:05} n ")
}

fn indexed_authority_pdf(index: &str, size: i64, placement: u8) -> Vec<u8> {
    let mut bytes = b"%PDF-2.0\n".to_vec();
    let root = object(&mut bytes, 1, 0, "<< /Type /Catalog >>");
    let stale = object(&mut bytes, 5, 0, "<< /Title (stale physical content) >>");
    let stream_start = bytes.len();
    let records = if index == "-1 3 5 1 8 1" {
        vec![(0, 0), (0, 0), (1, root), (1, stale), (1, stream_start)]
    } else if index == "5 1" {
        vec![(1, stale)]
    } else {
        vec![(0, 0), (1, root), (1, stale), (1, stream_start)]
    };
    bytes.extend_from_slice(format!("8 0 obj\n<< /Type /XRef /Size {size} /W [1 4 2] /Index [{index}] /Root 1 0 R /Info 5 0 R /Length {} >>\nstream\n", records.len() * 7).as_bytes());
    for (kind, offset) in records {
        bytes.push(kind);
        bytes.extend_from_slice(&u32::try_from(offset).unwrap().to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
    }
    bytes.extend_from_slice(b"\nendstream\nendobj\n");
    if placement == 0 {
        bytes.extend_from_slice(format!("startxref\n{stream_start}\n%%EOF\n").as_bytes());
    } else {
        if placement == 2 {
            bytes.extend_from_slice(format!("startxref\n{stream_start}\n%%EOF\n").as_bytes());
        }
        let main = bytes.len();
        let link = if placement == 1 { "XRefStm" } else { "Prev" };
        bytes.extend_from_slice(format!("xref\n0 2\n0000000000 65535 f \n{}\ntrailer\n<< /Size 9 /Root 1 0 R /Info 5 0 R /{link} {stream_start} >>\nstartxref\n{main}\n%%EOF\n", normal(root, 0)).as_bytes());
    }
    bytes
}

fn assert_index_authority_failure(bytes: &[u8], placement: u8) {
    for strict in [false, true] {
        let error = Document::load_mem_with_options(
            bytes,
            lopdf::LoadOptions {
                strict,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_index_error(error, placement, strict);
    }
    assert_index_error(Document::load_metadata_mem(bytes).unwrap_err(), placement, false);
    assert_index_error(
        Document::load_metadata_mem_with_password(bytes, "").unwrap_err(),
        placement,
        false,
    );
}

fn assert_index_error(error: lopdf::Error, placement: u8, strict: bool) {
    use lopdf::{Error, ParseError};
    match (placement, strict, error) {
        (0, true, Error::Parse(ParseError::InvalidXref)) => {}
        (0, false, Error::ReconstructionAuthority { source })
            if matches!(*source, Error::Parse(ParseError::InvalidXref)) => {}
        (1, _, Error::Xref(error)) => assert_eq!(format!("{error:?}"), "StreamStart"),
        (2, _, Error::Xref(error)) => assert_eq!(format!("{error:?}"), "PrevStart"),
        (_, _, error) => panic!("unexpected indexing error: {error:?}"),
    }
}

#[test]
fn malformed_index_cannot_expose_stale_content_in_any_revision_position() {
    for placement in 0..=2 {
        let valid = indexed_authority_pdf("0 2 5 1 8 1", 9, placement);
        for strict in [false, true] {
            let doc = Document::load_mem_with_options(
                &valid,
                lopdf::LoadOptions {
                    strict,
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(doc.get_object((5, 0)).is_ok());
        }
        assert_eq!(
            Document::load_metadata_mem(&valid).unwrap().title.as_deref(),
            Some("stale physical content")
        );
        assert_index_authority_failure(&indexed_authority_pdf("-1 3 5 1 8 1", 9, placement), placement);
    }
}

#[test]
fn supplement_local_size_is_checked_before_merge() {
    for size in [-1, i64::from(u32::MAX) + 1, 4] {
        assert_index_authority_failure(&indexed_authority_pdf("5 1", size, 1), 1);
    }
    // A different but sufficient local bound does not impose Size equality.
    let bytes = indexed_authority_pdf("5 1", 8, 1);
    assert!(Document::load_mem(&bytes).unwrap().get_object((5, 0)).is_ok());
}

#[test]
fn invalid_stream_index_rejects_all_entries() {
    for start in [1_i64 << 32, (1_i64 << 32) + 5, -1, i64::MAX - 1] {
        for entry_type in 0..=2 {
            let stream = Stream::new(
                dictionary! {
                    "Type" => "XRef", "Size" => 6,
                    "W" => vec![1.into(), 1.into(), 1.into()],
                    "Index" => vec![start.into(), 3.into(), 5.into(), 1.into()],
                },
                vec![entry_type, 7, 0, entry_type, 8, 0, entry_type, 9, 0, 1, 42, 0],
            );
            assert!(matches!(
                decode_xref_stream(stream),
                Err(lopdf::Error::Parse(lopdf::ParseError::InvalidXref))
            ));
        }
    }
}

#[test]
fn invalid_newest_stream_index_fails_for_both_loaders() {
    for start in [1_i64 << 32, (1_i64 << 32) + 5, -1, i64::MAX - 1] {
        for entry_type in 0..=2 {
            let mut bytes = b"%PDF-1.5\n".to_vec();
            let old = object(&mut bytes, 5, 0, "<< /Title (authoritative) >>");
            let previous = section(&mut bytes, &normal(old, 0), "/Info 5 0 R");
            let current = bytes.len();
            let count = if start == i64::MAX - 1 { 3 } else { 1 };
            bytes.extend_from_slice(format!(
                "8 0 obj\n<< /Type /XRef /Size 9 /Info 5 0 R /Prev {previous} /W [1 1 1] /Index [{start} {count}] /Length {} >>\nstream\n",
                count * 3
            ).as_bytes());
            for _ in 0..count {
                bytes.extend_from_slice(&[entry_type, 0, 0]);
            }
            bytes.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{current}\n%%EOF\n").as_bytes());
            assert_reconstruction_authority_lost(&bytes, "");
        }
    }
}

fn assert_reconstruction_authority_lost(bytes: &[u8], password: &str) {
    for error in [
        Document::load_mem(bytes).unwrap_err(),
        Document::load_metadata_mem(bytes).unwrap_err(),
        Document::load_mem_with_options(bytes, lopdf::LoadOptions::with_password(password)).unwrap_err(),
        Document::load_metadata_mem_with_password(bytes, password).unwrap_err(),
    ] {
        assert!(
            matches!(error, lopdf::Error::ReconstructionAuthority { .. }),
            "{error:?}"
        );
    }
}

#[test]
fn local_single_revision_recovery_preserves_free_and_generation_authority() {
    for entry_generation in [None, Some(1)] {
        let mut bytes = b"%PDF-1.5\n".to_vec();
        let old = object(&mut bytes, 5, 0, "(physical generation zero)");
        let entry = entry_generation.map_or_else(
            || "0000000000 00001 f ".to_string(),
            |generation| normal(old, generation),
        );
        let current = section(&mut bytes, &entry, "");
        replace_final_startxref(&mut bytes, current + 4);
        let doc = Document::load_mem(&bytes).unwrap();
        assert_eq!(doc.xref_start, current);
        assert!(doc.get_object((5, 0)).is_err());
        assert!(doc.get_object((5, 1)).is_err());
        assert!(Document::load_metadata_mem(&bytes).is_ok());
        match entry_generation {
            None => assert!(matches!(doc.reference_table.get(5), Some(XrefEntry::Free { .. }))),
            Some(generation) => assert!(matches!(doc.reference_table.get(5),
                Some(XrefEntry::Normal { generation: actual, .. }) if *actual == generation)),
        }
    }
}

#[test]
fn unavailable_latest_xref_cannot_resurrect_normal_or_old_generation() {
    for generation_reused in [false, true] {
        let mut bytes = b"%PDF-1.5\n".to_vec();
        object(&mut bytes, 1, 0, "<< /Type /Catalog >>");
        let old = object(&mut bytes, 5, 0, "(stale)");
        let previous = section(&mut bytes, &normal(old, 0), "/Root 1 0 R");
        let entry = if generation_reused {
            let new = object(&mut bytes, 5, 1, "(current)");
            normal(new, 1)
        } else {
            "0000000000 00001 f ".to_string()
        };
        let current = section(&mut bytes, &entry, &format!("/Root 1 0 R /Prev {previous}"));
        let baseline = Document::load_mem(&bytes).unwrap();
        assert!(baseline.get_object((5, 0)).is_err());
        if generation_reused {
            assert_eq!(baseline.get_object((5, 1)).unwrap().as_str().unwrap(), b"current");
        }
        // Exercise both a missing final target and a malformed latest table.
        let mut unavailable = bytes.clone();
        let past_eof = unavailable.len() + 4096;
        replace_final_startxref(&mut unavailable, past_eof);
        assert_reconstruction_authority_lost(&unavailable, "");
        bytes[current..current + 4].copy_from_slice(b"xxxx");
        assert_reconstruction_authority_lost(&bytes, "");
    }
}

#[test]
fn unavailable_latest_xref_cannot_resurrect_compressed_member() {
    let mut bytes = indexed_object_stream_fixture(4, 1, None);
    let baseline = Document::load_mem(&bytes).unwrap();
    assert!(baseline.get_object((5, 0)).is_ok());
    let root = object(&mut bytes, 1, 0, "<< /Type /Catalog >>");
    let current = bytes.len();
    bytes.extend_from_slice(format!(
        "xref\n1 1\n{}\n5 1\n0000000000 00001 f \ntrailer\n<< /Size 11 /Root 1 0 R /Prev {} >>\nstartxref\n{current}\n%%EOF\n",
        normal(root, 0), baseline.xref_start
    ).as_bytes());
    assert!(Document::load_mem(&bytes).unwrap().get_object((5, 0)).is_err());
    let past_eof = bytes.len() + 4096;
    replace_final_startxref(&mut bytes, past_eof);
    assert_reconstruction_authority_lost(&bytes, "");
}

#[test]
fn unavailable_latest_xref_cannot_authorize_encrypted_stale_identity() {
    use lopdf::{EncryptionState, EncryptionVersion, Permissions};
    for password in ["", "user"] {
        let mut doc = Document::with_version("1.5");
        doc.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
        doc.set_object((1, 0), dictionary! { "Type" => "Catalog" });
        doc.set_object((5, 0), dictionary! { "Title" => Object::string_literal("stale title") });
        doc.max_id = 5;
        doc.trailer.set("Root", (1, 0));
        doc.trailer.set("Info", (5, 0));
        doc.trailer.set(
            "ID",
            vec![
                Object::string_literal("identifier"),
                Object::string_literal("identifier"),
            ],
        );
        let state = EncryptionState::try_from(EncryptionVersion::V2 {
            document: &doc,
            owner_password: "owner",
            user_password: password,
            key_length: 128,
            permissions: Permissions::PRINTABLE,
        })
        .unwrap();
        doc.encrypt(&state).unwrap();
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let baseline = Document::load_mem_with_options(&bytes, lopdf::LoadOptions::with_password(password)).unwrap();
        assert_eq!(
            baseline
                .get_dictionary((5, 0))
                .unwrap()
                .get(b"Title")
                .unwrap()
                .as_str()
                .unwrap(),
            b"stale title"
        );
        let trailer_start = bytes.windows(8).rposition(|w| w == b"trailer\n").unwrap();
        let trailer_end = bytes.windows(2).rposition(|w| w == b">>").unwrap();
        let trailer = bytes[trailer_start..trailer_end].to_vec();
        let current = bytes.len();
        bytes.extend_from_slice(b"xref\n5 1\n0000000000 00001 f \n");
        bytes.extend_from_slice(&trailer);
        bytes.extend_from_slice(format!(" /Prev {} >>\nstartxref\n{current}\n%%EOF\n", baseline.xref_start).as_bytes());
        assert!(
            Document::load_mem_with_options(&bytes, lopdf::LoadOptions::with_password(password))
                .unwrap()
                .get_object((5, 0))
                .is_err()
        );
        let past_eof = bytes.len() + 4096;
        replace_final_startxref(&mut bytes, past_eof);
        assert_reconstruction_authority_lost(&bytes, password);
    }
}

#[test]
fn classic_free_fields_and_writer_roundtrip() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    section(&mut bytes, "0000000007 00042 f ", "");
    let doc = Document::load_mem(&bytes).unwrap();
    let entry = doc.reference_table.get(5).unwrap();
    assert!(matches!(
        entry,
        XrefEntry::Free {
            next_free: 7,
            generation: 42
        }
    ));
    let mut encoded = Vec::new();
    entry.write_xref_entry(&mut encoded).unwrap();
    assert_eq!(encoded, b"0000000007 00042 f \n");
}

#[test]
fn stream_free_fields_and_encoder_roundtrip() {
    let mut xref = Xref::new(6, XrefType::CrossReferenceStream);
    xref.insert(
        5,
        XrefEntry::Free {
            next_free: 70000,
            generation: 42000,
        },
    );
    let mut builder = XrefStreamBuilder::new(&xref);
    let widths = builder.calculate_optimal_widths();
    let content = builder.build_stream_content().unwrap();
    let stream = Stream::new(
        dictionary! { "Type" => "XRef", "Size" => 6, "W" => widths.into_iter().map(|v| Object::Integer(v as i64)).collect::<Vec<_>>(), "Index" => builder.build_index_array() },
        content,
    );
    let (decoded, _) = decode_xref_stream(stream).unwrap();
    assert!(matches!(
        decoded.get(5),
        Some(XrefEntry::Free {
            next_free: 70000,
            generation: 42000
        })
    ));
}

#[test]
fn latest_revision_wins_for_live_and_free_entries() {
    for (old_free, new_free) in [(false, true), (false, false), (true, false)] {
        let mut bytes = b"%PDF-1.5\n".to_vec();
        let old = object(&mut bytes, 5, 0, "(old)");
        let previous = section(
            &mut bytes,
            &if old_free {
                "0000000000 00001 f ".into()
            } else {
                normal(old, 0)
            },
            "",
        );
        let new = object(&mut bytes, 5, 1, "(new)");
        section(
            &mut bytes,
            &if new_free {
                "0000000000 00002 f ".into()
            } else {
                normal(new, 1)
            },
            &format!("/Prev {previous}"),
        );
        let doc = Document::load_mem(&bytes).unwrap();
        assert!(doc.get_object((5, 0)).is_err());
        if new_free {
            assert!(doc.get_object((5, 1)).is_err());
            assert!(matches!(
                doc.reference_table.get(5),
                Some(XrefEntry::Free { generation: 2, .. })
            ));
        } else {
            assert_eq!(doc.get_object((5, 1)).unwrap().as_str().unwrap(), b"new");
        }
    }
}

#[test]
fn cycles_and_recovered_self_reference_fail_closed() {
    for correction in [0, 4] {
        let mut bytes = b"%PDF-1.5\n".to_vec();
        let root = object(&mut bytes, 1, 0, "<< /Type /Catalog >>");
        let value = object(&mut bytes, 5, 0, "(old)");
        section(&mut bytes, &normal(value, 0), &format!("/Root 1 0 R /Unused {root}"));
        let current = bytes.len();
        section(
            &mut bytes,
            "0000000000 00001 f ",
            &format!("/Prev {} /Root 1 0 R", current + correction),
        );
        assert!(Document::load_mem(&bytes).is_err());
        assert!(Document::load_metadata_mem(&bytes).is_err());
        assert!(Document::load_metadata_mem_with_password(&bytes, "password").is_err());
    }
}

#[test]
fn unambiguous_miswritten_previous_offset_recovers() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let value = object(&mut bytes, 5, 0, "(old)");
    let previous = section(&mut bytes, &normal(value, 0), "");
    let current = bytes.len();
    bytes.extend_from_slice(
        format!(
            "xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 6 /Prev {} >>\nstartxref\n{current}\n%%EOF\n",
            previous + 4
        )
        .as_bytes(),
    );
    let doc = Document::load_mem(&bytes).unwrap();
    assert_eq!(doc.get_object((5, 0)).unwrap().as_str().unwrap(), b"old");
}

#[test]
fn hybrid_supplement_precedes_previous_revision() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let old = object(&mut bytes, 5, 0, "(old)");
    let previous = section(&mut bytes, &normal(old, 0), "");
    let new = object(&mut bytes, 5, 0, "(new)");
    let supplement = bytes.len();
    let mut content = vec![1];
    content.extend_from_slice(&(new as u32).to_be_bytes());
    content.extend_from_slice(&0u16.to_be_bytes());
    bytes.extend_from_slice(b"8 0 obj\n<< /Type /XRef /Size 9 /W [1 4 2] /Index [5 1] /Length 7 >>\nstream\n");
    bytes.extend(content);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");
    let current = bytes.len();
    bytes.extend_from_slice(format!("xref\n8 1\n{}\ntrailer\n<< /Size 9 /XRefStm {supplement} /Prev {previous} >>\nstartxref\n{current}\n%%EOF\n", normal(supplement, 0)).as_bytes());
    let doc = Document::load_mem(&bytes).unwrap();
    assert_eq!(doc.get_object((5, 0)).unwrap().as_str().unwrap(), b"new");

    // The supplemental stream belongs to this trailer even when a newer
    // revision is the traversal entry point.
    let newest = bytes.len();
    bytes.extend_from_slice(
        format!("xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 9 /Prev {current} >>\nstartxref\n{newest}\n%%EOF\n")
            .as_bytes(),
    );
    let doc = Document::load_mem(&bytes).unwrap();
    assert_eq!(doc.get_object((5, 0)).unwrap().as_str().unwrap(), b"new");
}

#[test]
fn generation_mismatch_does_not_materialize_wrong_identity() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let value = object(&mut bytes, 5, 0, "(wrong generation)");
    section(&mut bytes, &normal(value, 1), "");
    let doc = Document::load_mem(&bytes).unwrap();
    assert!(doc.get_object((5, 0)).is_err());
    assert!(doc.get_object((5, 1)).is_err());
}

#[test]
fn compressed_entry_retains_generation_zero() {
    let stream = Stream::new(
        dictionary! { "Type" => "XRef", "Size" => 6, "W" => vec![1.into(), 1.into(), 1.into()], "Index" => vec![5.into(), 1.into()] },
        vec![2, 8, 0],
    );
    let (xref, _) = decode_xref_stream(stream).unwrap();
    assert!(matches!(
        xref.get(5),
        Some(XrefEntry::Compressed { container: 8, index: 0 })
    ));
    let mut doc = Document::new();
    doc.objects.insert((5, 0), Object::Integer(42));
    assert!(doc.get_object((5, 1)).is_err());
}

#[test]
fn invalid_hybrid_contexts_fail_closed() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let prior = section(&mut bytes, "0000000000 00001 f ", "");
    section(&mut bytes, "0000000000 00002 f ", &format!("/XRefStm {prior}"));
    assert!(Document::load_mem(&bytes).is_err());
    assert!(Document::load_metadata_mem(&bytes).is_err());
}

#[test]
fn two_revision_cycle_fails_closed() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let first = section(&mut bytes, "0000000000 00001 f ", "/Prev 9999999999");
    let second = bytes.len();
    let placeholder = bytes.windows(10).position(|v| v == b"9999999999").unwrap();
    bytes[placeholder..placeholder + 10].copy_from_slice(format!("{second:010}").as_bytes());
    section(&mut bytes, "0000000000 00002 f ", &format!("/Prev {first}"));
    assert!(Document::load_mem(&bytes).is_err());
    assert!(Document::load_metadata_mem(&bytes).is_err());
}

#[test]
fn single_revision_save_formats_and_readable_object_streams() {
    for compressed in [false, true] {
        let mut doc = Document::with_version("1.5");
        doc.reference_table.cross_reference_type = if compressed {
            XrefType::CrossReferenceStream
        } else {
            XrefType::CrossReferenceTable
        };
        let value = doc.add_object(Object::string_literal("value"));
        let mut bytes = Vec::new();
        if compressed {
            doc.save_modern(&mut bytes).unwrap();
        } else {
            doc.save_to(&mut bytes).unwrap();
        }
        let loaded = Document::load_mem(&bytes).unwrap();
        assert_eq!(loaded.get_object(value).unwrap().as_str().unwrap(), b"value");
        assert!(loaded.get_object((value.0, 1)).is_err());
        if compressed {
            assert!(matches!(
                loaded.reference_table.get(value.0),
                Some(XrefEntry::Compressed { .. })
            ));
        }
    }
}

#[test]
fn stream_free_shadows_previous_live() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let old = object(&mut bytes, 5, 0, "(old)");
    let previous = section(&mut bytes, &normal(old, 0), "");
    let current = bytes.len();
    bytes.extend_from_slice(
        format!("8 0 obj\n<< /Type /XRef /Size 9 /Prev {previous} /W [1 1 2] /Index [5 1] /Length 4 >>\nstream\n")
            .as_bytes(),
    );
    bytes.extend_from_slice(&[0, 7, 0, 42]);
    bytes.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{current}\n%%EOF\n").as_bytes());
    let doc = Document::load_mem(&bytes).unwrap();
    assert!(matches!(
        doc.reference_table.get(5),
        Some(XrefEntry::Free {
            next_free: 7,
            generation: 42
        })
    ));
    assert!(doc.get_object((5, 0)).is_err());
}

#[test]
fn updated_hybrid_preserves_main_section_precedence() {
    for main_entry in [false, true] {
        let mut bytes = b"%PDF-1.5\n".to_vec();
        let previous = section(&mut bytes, "0000000000 00001 f ", "");
        let value = object(&mut bytes, 5, 0, "(supplement)");
        let supplement = bytes.len();
        bytes.extend_from_slice(b"8 0 obj\n<< /Type /XRef /Size 9 /W [1 4 2] /Index [5 1] /Length 7 >>\nstream\n");
        bytes.push(1);
        bytes.extend_from_slice(&(value as u32).to_be_bytes());
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(b"\nendstream\nendobj\n");
        let main_value = object(&mut bytes, 5, 0, "(main)");
        let current = bytes.len();
        let entries = if main_entry {
            format!("5 1\n{}\n", normal(main_value, 0))
        } else {
            String::new()
        };
        bytes.extend_from_slice(format!("xref\n0 1\n0000000000 65535 f \n{entries}trailer\n<< /Size 9 /XRefStm {supplement} /Prev {previous} >>\nstartxref\n{current}\n%%EOF\n").as_bytes());
        let doc = Document::load_mem_with_options(
            &bytes,
            lopdf::LoadOptions {
                strict: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            doc.get_object((5, 0)).unwrap().as_str().unwrap(),
            if main_entry { &b"main"[..] } else { &b"supplement"[..] }
        );
    }
}

fn first_revision_hybrid(version: &str, target: &str) -> (Vec<u8>, usize) {
    let mut bytes = format!("%PDF-{version}\n").into_bytes();
    let catalog = object(&mut bytes, 1, 0, "<< /Type /Catalog /Pages 2 0 R >>");
    let pages = object(&mut bytes, 2, 0, "<< /Type /Pages /Kids [] /Count 0 >>");
    let info = object(&mut bytes, 5, 0, "<< /Title (supplement value) >>");
    let supplement = bytes.len();
    if target == "object" {
        object(&mut bytes, 8, 0, "(not a stream)");
    } else {
        let stream_type = if target == "wrong type" { "ObjStm" } else { "XRef" };
        let widths = if target == "bad widths" { "1 4" } else { "1 4 2" };
        bytes.extend_from_slice(
            format!("8 0 obj\n<< /Type /{stream_type} /Size 9 /W [{widths}] /Index [5 1] /Length 7 >>\nstream\n")
                .as_bytes(),
        );
        bytes.push(1);
        bytes.extend_from_slice(&(info as u32).to_be_bytes());
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(b"\nendstream\nendobj\n");
    }
    let current = bytes.len();
    let offset = match target {
        "outside" => "9999999999".to_owned(),
        "negative" => "-1".to_owned(),
        "self" => current.to_string(),
        _ => supplement.to_string(),
    };
    // Object 5 is absent unless the main table explicitly frees it.
    let main_entry = if target == "main free" {
        "5 1\n0000000000 00001 f \n"
    } else {
        ""
    };
    bytes.extend_from_slice(format!("xref\n0 3\n0000000000 65535 f \n{}\n{}\n{main_entry}8 1\n{}\ntrailer\n<< /Size 9 /Root 1 0 R /Info 5 0 R /XRefStm {offset} >>\nstartxref\n{current}\n%%EOF\n", normal(catalog, 0), normal(pages, 0), normal(supplement, 0)).as_bytes());
    (bytes, info)
}

fn assert_stream_start(error: lopdf::Error) {
    assert!(
        matches!(error, lopdf::Error::Xref(ref err) if format!("{err:?}") == "StreamStart"),
        "{error:?}"
    );
}

#[test]
fn first_revision_hybrid_processes_supplement_in_lenient_mode() {
    for version in ["1.7", "2.0"] {
        let (bytes, info) = first_revision_hybrid(version, "valid");
        let doc = Document::load_mem(&bytes).unwrap();
        assert_eq!(doc.version, version);
        assert_eq!(
            doc.get_dictionary((5, 0))
                .unwrap()
                .get(b"Title")
                .unwrap()
                .as_str()
                .unwrap(),
            b"supplement value"
        );
        assert!(
            matches!(doc.reference_table.get(5), Some(XrefEntry::Normal { offset, generation: 0 }) if *offset as usize == info)
        );
        assert_eq!(
            Document::load_metadata_mem(&bytes).unwrap().title.as_deref(),
            Some("supplement value")
        );
    }
}

#[test]
fn first_revision_hybrid_is_rejected_in_strict_mode() {
    let (bytes, _) = first_revision_hybrid("1.7", "valid");
    assert_stream_start(
        Document::load_mem_with_options(
            &bytes,
            lopdf::LoadOptions {
                strict: true,
                ..Default::default()
            },
        )
        .unwrap_err(),
    );
}

#[test]
fn pdf_2_first_revision_hybrid_is_accepted_in_strict_mode() {
    let (bytes, info) = first_revision_hybrid("2.0", "valid");
    let doc = Document::load_mem_with_options(
        &bytes,
        lopdf::LoadOptions {
            strict: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(doc.version, "2.0");
    assert!(
        matches!(doc.reference_table.get(5), Some(XrefEntry::Normal { offset, generation: 0 }) if *offset as usize == info)
    );
    assert_eq!(
        doc.get_dictionary((5, 0))
            .unwrap()
            .get(b"Title")
            .unwrap()
            .as_str()
            .unwrap(),
        b"supplement value"
    );
}

#[test]
fn first_revision_main_free_blocks_supplement() {
    for (version, strict) in [("1.7", false), ("2.0", false), ("2.0", true)] {
        let (bytes, _) = first_revision_hybrid(version, "main free");
        let doc = Document::load_mem_with_options(
            &bytes,
            lopdf::LoadOptions {
                strict,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(doc.reference_table.get(5), Some(XrefEntry::Free { .. })));
        assert!(doc.get_object((5, 0)).is_err());
    }
}

#[test]
fn malformed_first_revision_supplements_fail_closed() {
    for version in ["1.7", "2.0"] {
        for target in ["outside", "negative", "object", "wrong type", "bad widths", "self"] {
            let (bytes, _) = first_revision_hybrid(version, target);
            assert_stream_start(Document::load_mem(&bytes).unwrap_err());
            assert_stream_start(Document::load_metadata_mem(&bytes).unwrap_err());
            assert_stream_start(
                Document::load_mem_with_options(
                    &bytes,
                    lopdf::LoadOptions {
                        strict: true,
                        ..Default::default()
                    },
                )
                .unwrap_err(),
            );
        }
    }
}

#[test]
fn free_entry_blocks_stale_object_stream_contents() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let container = bytes.len();
    let content = b"5 0 (stale)";
    bytes.extend_from_slice(
        format!(
            "8 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n",
            content.len()
        )
        .as_bytes(),
    );
    bytes.extend_from_slice(content);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");
    let current = bytes.len();
    bytes.extend_from_slice(
        format!(
            "xref\n5 1\n0000000000 00001 f \n8 1\n{}\ntrailer\n<< /Size 9 >>\nstartxref\n{current}\n%%EOF\n",
            normal(container, 0)
        )
        .as_bytes(),
    );
    let doc = Document::load_mem(&bytes).unwrap();
    assert!(doc.get_object((8, 0)).is_ok());
    assert!(doc.get_object((5, 0)).is_err());
    assert!(matches!(
        doc.reference_table.get(5),
        Some(XrefEntry::Free { generation: 1, .. })
    ));
}

#[test]
fn decryption_does_not_materialize_freed_object_stream_member() {
    use lopdf::{EncryptionState, EncryptionVersion, Permissions};
    let mut doc = Document::with_version("1.5");
    doc.set_object(
        (8, 0),
        Stream::new(
            dictionary! { "Type" => "ObjStm", "N" => 1, "First" => 4 },
            b"5 0 (stale)".to_vec(),
        ),
    );
    doc.max_id = 8;
    doc.trailer.set(
        "ID",
        vec![
            Object::string_literal("identifier"),
            Object::string_literal("identifier"),
        ],
    );
    doc.reference_table.insert(
        5,
        XrefEntry::Free {
            next_free: 0,
            generation: 1,
        },
    );
    let state = EncryptionState::try_from(EncryptionVersion::V2 {
        document: &doc,
        owner_password: "owner",
        user_password: "user",
        key_length: 128,
        permissions: Permissions::PRINTABLE,
    })
    .unwrap();
    doc.encrypt(&state).unwrap();
    doc.decrypt("owner").unwrap();
    assert!(doc.get_object((8, 0)).is_ok());
    assert!(doc.get_object((5, 0)).is_err());
}

#[test]
fn oversized_free_stream_fields_are_rejected_without_truncation() {
    for content in [
        vec![0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1],
        vec![0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1],
    ] {
        let stream = Stream::new(
            dictionary! { "Type" => "XRef", "Size" => 6, "W" => vec![1.into(), 5.into(), 5.into()], "Index" => vec![5.into(), 1.into()] },
            content,
        );
        assert!(decode_xref_stream(stream).is_err());
    }
}

fn encrypted_identity_mismatch(expected: (u32, u16), password: &str) {
    use lopdf::{EncryptionState, EncryptionVersion, LoadOptions, Permissions};

    let mut original = Document::with_version("1.5");
    original.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
    original.set_object(
        (5, 0),
        dictionary! { "Title" => Object::string_literal("original title") },
    );
    original.set_object((7, 0), Object::string_literal("readable sibling"));
    original.max_id = 7;
    original.trailer.set(
        "ID",
        vec![
            Object::string_literal("identifier"),
            Object::string_literal("identifier"),
        ],
    );
    let state = EncryptionState::try_from(EncryptionVersion::V2 {
        document: &original,
        owner_password: "owner",
        user_password: password,
        key_length: 128,
        permissions: Permissions::PRINTABLE,
    })
    .unwrap();
    original.encrypt(&state).unwrap();
    let mut bytes = Vec::new();
    original.save_to(&mut bytes).unwrap();
    let options = LoadOptions::with_password(password);
    let baseline = Document::load_mem_with_options(&bytes, options.clone()).unwrap();
    assert_eq!(
        baseline
            .get_dictionary((5, 0))
            .unwrap()
            .get(b"Title")
            .unwrap()
            .as_str()
            .unwrap(),
        b"original title"
    );
    let offset = match baseline.reference_table.get(5).unwrap() {
        XrefEntry::Normal { offset, .. } => *offset,
        entry => panic!("expected normal entry, got {entry:?}"),
    };

    // Preserve the actual encryption dictionary and ID in the appended trailer.
    let trailer_start = bytes.windows(8).rposition(|w| w == b"trailer\n").unwrap();
    let trailer_end = bytes.windows(2).rposition(|w| w == b">>").unwrap();
    let trailer_prefix = bytes[trailer_start..trailer_end].to_vec();
    bytes.push(b'\n');
    let current = bytes.len();
    bytes.extend_from_slice(b"xref\n");
    if expected.0 != 5 {
        // A mislocated live entry must not resurrect the explicitly freed identity.
        bytes.extend_from_slice(b"5 1\n0000000000 00001 f \n");
    }
    bytes.extend_from_slice(format!("{} 1\n{offset:010} {:05} n \n", expected.0, expected.1).as_bytes());
    bytes.extend_from_slice(&trailer_prefix);
    bytes.extend_from_slice(
        format!(
            "/Prev {} /Info 5 0 R >>\nstartxref\n{current}\n%%EOF\n",
            baseline.xref_start
        )
        .as_bytes(),
    );

    let loaded = Document::load_mem_with_options(&bytes, options.clone()).unwrap();
    assert!(loaded.get_object((5, 0)).is_err());
    assert!(loaded.get_object(expected).is_err());
    assert_eq!(
        loaded.get_object((7, 0)).unwrap().as_str().unwrap(),
        b"readable sibling"
    );
    assert!(
        Document::load_metadata_mem_with_password(&bytes, password)
            .unwrap()
            .title
            .is_none()
    );
    assert!(matches!(
        Document::load_mem_with_options(
            &bytes,
            LoadOptions {
                strict: true,
                ..options
            }
        ),
        Err(lopdf::Error::ObjectIdMismatch)
    ));
}

#[test]
fn encrypted_normal_generation_mismatch_does_not_materialize() {
    for password in ["", "user"] {
        encrypted_identity_mismatch((5, 1), password);
    }
}

#[test]
fn encrypted_normal_object_number_mismatch_does_not_materialize() {
    for password in ["", "user"] {
        encrypted_identity_mismatch((6, 0), password);
    }
}

fn wide_xref_records(records: &[[u64; 3]]) -> Stream {
    Stream::new(
        dictionary! {
            "Type" => "XRef", "Size" => 5 + records.len() as i64,
            "W" => vec![8.into(), 8.into(), 8.into()],
            "Index" => vec![5.into(), (records.len() as i64).into()],
        },
        records.iter().flatten().flat_map(|field| field.to_be_bytes()).collect(),
    )
}

fn assert_invalid_xref_field(record: [u64; 3]) {
    // A preceding valid entry must not turn a failed decode into partial success.
    let stream = wide_xref_records(&[[1, 9, 0], record]);
    assert!(matches!(
        decode_xref_stream(stream),
        Err(lopdf::Error::Parse(lopdf::ParseError::InvalidXref))
    ));
}

#[test]
fn normal_xref_stream_generation_overflow_is_rejected() {
    for generation in [65536, u64::MAX] {
        assert_invalid_xref_field([1, 9, generation]);
    }
}

#[test]
fn normal_xref_stream_offset_overflow_is_rejected() {
    for offset in [u64::from(u32::MAX) + 1, u64::MAX] {
        assert_invalid_xref_field([1, offset, 0]);
    }
}

#[test]
fn compressed_xref_stream_container_overflow_is_rejected() {
    for container in [u64::from(u32::MAX) + 1, u64::MAX] {
        assert_invalid_xref_field([2, container, 0]);
    }
}

#[test]
fn compressed_xref_stream_index_overflow_is_rejected() {
    for index in [65536, u64::MAX] {
        assert_invalid_xref_field([2, 8, index]);
    }
}

#[test]
fn wide_xref_fields_preserve_representable_limits() {
    let (xref, _) = decode_xref_stream(wide_xref_records(&[
        [0, u64::from(u32::MAX), 65535],
        [1, u64::from(u32::MAX), 65535],
        [2, u64::from(u32::MAX), 65535],
    ]))
    .unwrap();
    assert!(matches!(
        xref.get(5),
        Some(XrefEntry::Free {
            next_free: u32::MAX,
            generation: u16::MAX
        })
    ));
    assert!(matches!(
        xref.get(6),
        Some(XrefEntry::Normal {
            offset: u32::MAX,
            generation: u16::MAX
        })
    ));
    assert!(matches!(
        xref.get(7),
        Some(XrefEntry::Compressed {
            container: u32::MAX,
            index: u16::MAX
        })
    ));
}

#[test]
fn wide_unknown_xref_type_does_not_alias_normal_or_shift_next_record() {
    let (xref, _) = decode_xref_stream(wide_xref_records(&[[0x1_0000_0001, 1, 0], [1, 42, 7]])).unwrap();
    assert!(xref.get(5).is_none());
    assert!(matches!(
        xref.get(6),
        Some(XrefEntry::Normal {
            offset: 42,
            generation: 7
        })
    ));
}

fn indexed_object_stream_fixture(first_id: u32, index: u16, password: Option<&str>) -> Vec<u8> {
    let mut doc = Document::with_version("1.5");
    doc.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
    let first = "<< /Title (first) >> ";
    let header = format!("{first_id} 0 5 {} ", first.len());
    doc.set_object(
        (8, 0),
        Stream::new(
            dictionary! { "Type" => "Member", "N" => 2, "First" => header.len() as i64 },
            format!("{header}{first}<< /Title (second) >>").into_bytes(),
        ),
    );
    doc.max_id = 8;
    if let Some(password) = password {
        use lopdf::{EncryptionState, EncryptionVersion, Permissions};
        doc.trailer.set(
            "ID",
            vec![
                Object::string_literal("identifier"),
                Object::string_literal("identifier"),
            ],
        );
        let state = EncryptionState::try_from(EncryptionVersion::V2 {
            document: &doc,
            owner_password: "owner",
            user_password: password,
            key_length: 128,
            permissions: Permissions::PRINTABLE,
        })
        .unwrap();
        doc.encrypt(&state).unwrap();
    }
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    // The full writer omits ObjStm objects, so restore the type after saving.
    let type_pos = bytes.windows(7).position(|w| w == b"/Member").unwrap();
    bytes[type_pos..type_pos + 7].copy_from_slice(b"/ObjStm");
    let baseline =
        Document::load_mem_with_options(&bytes, lopdf::LoadOptions::with_password(password.unwrap_or(""))).unwrap();
    let trailer_start = bytes.windows(8).rposition(|w| w == b"trailer\n").unwrap();
    let trailer_end = bytes.windows(2).rposition(|w| w == b">>").unwrap();
    let trailer_prefix = bytes[trailer_start..trailer_end].to_vec();
    bytes.push(b'\n');
    let supplement = bytes.len();
    bytes.extend_from_slice(b"10 0 obj\n<< /Type /XRef /Size 11 /W [1 4 2] /Index [5 1] /Length 7 >>\nstream\n");
    bytes.extend_from_slice(&[2, 0, 0, 0, 8]);
    bytes.extend_from_slice(&index.to_be_bytes());
    bytes.extend_from_slice(b"\nendstream\nendobj\n");
    let current = bytes.len();
    bytes.extend_from_slice(format!("xref\n10 1\n{}\n", normal(supplement, 0)).as_bytes());
    bytes.extend_from_slice(&trailer_prefix);
    bytes.extend_from_slice(
        format!(
            " /Prev {} /XRefStm {supplement} /Info 5 0 R >>\nstartxref\n{current}\n%%EOF\n",
            baseline.xref_start
        )
        .as_bytes(),
    );
    bytes
}

#[test]
fn object_stream_index_controls_full_and_metadata_identity() {
    for password in [None, Some(""), Some("user")] {
        for (first_id, index, title) in [
            (6, 0, None),
            (6, 1, Some("second")),
            (5, 0, Some("first")),
            (5, 1, Some("second")),
            (5, 2, None),
        ] {
            let bytes = indexed_object_stream_fixture(first_id, index, password);
            let doc =
                Document::load_mem_with_options(&bytes, lopdf::LoadOptions::with_password(password.unwrap_or("")))
                    .unwrap();
            let actual = doc
                .get_dictionary((5, 0))
                .ok()
                .and_then(|d| d.get(b"Title").ok())
                .and_then(|o| o.as_str().ok());
            assert_eq!(
                actual,
                title.map(str::as_bytes),
                "password={password:?}, first={first_id}, index={index}"
            );
            let metadata = Document::load_metadata_mem_with_password(&bytes, password.unwrap_or("")).unwrap();
            assert_eq!(metadata.title.as_deref(), title);
        }
    }
}

#[test]
fn deferred_decryption_uses_object_stream_index() {
    use lopdf::{EncryptionState, EncryptionVersion, Permissions};
    for index in [0, 1] {
        let mut doc = Document::with_version("1.5");
        doc.set_object(
            (8, 0),
            Stream::new(
                dictionary! { "Type" => "ObjStm", "N" => 2, "First" => 8 },
                b"5 0 5 8 (first) (second)".to_vec(),
            ),
        );
        doc.max_id = 8;
        doc.reference_table
            .insert(5, XrefEntry::Compressed { container: 8, index });
        doc.trailer.set(
            "ID",
            vec![
                Object::string_literal("identifier"),
                Object::string_literal("identifier"),
            ],
        );
        let state = EncryptionState::try_from(EncryptionVersion::V2 {
            document: &doc,
            owner_password: "owner",
            user_password: "user",
            key_length: 128,
            permissions: Permissions::PRINTABLE,
        })
        .unwrap();
        doc.encrypt(&state).unwrap();
        doc.decrypt("user").unwrap();
        assert_eq!(
            doc.get_object((5, 0)).unwrap().as_str().unwrap(),
            if index == 0 {
                b"first".as_slice()
            } else {
                b"second".as_slice()
            }
        );
    }
}

fn replace_final_startxref(bytes: &mut Vec<u8>, offset: usize) {
    let start = bytes.windows(9).rposition(|w| w == b"startxref").unwrap();
    bytes.truncate(start);
    bytes.extend_from_slice(format!("startxref\n{offset}\n%%EOF\n").as_bytes());
}

fn assert_ambiguous_final_start(bytes: &[u8]) {
    use lopdf::{Error, LoadOptions};
    assert!(matches!(
        Document::load_mem(bytes),
        Err(Error::Xref(err)) if err.to_string() == "ambiguous final startxref recovery"
    ));
    assert!(matches!(
        Document::load_mem_with_options(bytes, LoadOptions::with_password("password")),
        Err(Error::Xref(err)) if err.to_string() == "ambiguous final startxref recovery"
    ));
    assert!(matches!(
        Document::load_metadata_mem(bytes),
        Err(Error::Xref(err)) if err.to_string() == "ambiguous final startxref recovery"
    ));
    assert!(matches!(
        Document::load_metadata_mem_with_password(bytes, "password"),
        Err(Error::Xref(err)) if err.to_string() == "ambiguous final startxref recovery"
    ));
}

#[test]
fn final_startxref_exact_and_local_whitespace_keep_newest_revision() {
    for displacement in [-3isize, 0, 4] {
        let mut bytes = b"%PDF-1.5\n".to_vec();
        let old = object(&mut bytes, 5, 0, "(old)");
        let previous = section(&mut bytes, &normal(old, 0), "");
        let new = object(&mut bytes, 5, 0, "(new)");
        bytes.extend_from_slice(b"   \n");
        let current = section(&mut bytes, &normal(new, 0), &format!("/Prev {previous}"));
        replace_final_startxref(&mut bytes, current.checked_add_signed(displacement).unwrap());
        let doc = Document::load_mem(&bytes).unwrap();
        assert_eq!(doc.xref_start, current);
        assert_eq!(doc.get_object((5, 0)).unwrap().as_str().unwrap(), b"new");
    }
}

#[test]
fn final_startxref_recovery_to_older_completed_revision_fails_closed() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    object(&mut bytes, 1, 0, "<< /Type /Catalog >>");
    let old = object(&mut bytes, 5, 0, "(old)");
    let previous = section(&mut bytes, &normal(old, 0), "/Root 1 0 R");
    let new = object(&mut bytes, 5, 0, "(new)");
    section(&mut bytes, &normal(new, 0), &format!("/Root 1 0 R /Prev {previous}"));
    replace_final_startxref(&mut bytes, previous + 4);
    assert_ambiguous_final_start(&bytes);
}

#[test]
fn final_startxref_correction_across_eof_fails_closed() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    object(&mut bytes, 1, 0, "<< /Type /Catalog >>");
    let value = object(&mut bytes, 5, 0, "(value)");
    section(&mut bytes, &normal(value, 0), "/Root 1 0 R");
    let before_eof = bytes.len() - 7;
    section(&mut bytes, &normal(value, 0), "/Root 1 0 R");
    replace_final_startxref(&mut bytes, before_eof);
    assert_ambiguous_final_start(&bytes);
}

#[test]
fn final_startxref_ambiguous_nearby_sections_bypass_reconstruction() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    object(&mut bytes, 1, 0, "<< /Type /Catalog >>");
    bytes.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 2 >>\n   ");
    let current = bytes.len();
    bytes.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 2 /Root 1 0 R >>\n");
    bytes.extend_from_slice(format!("startxref\n{}\n%%EOF\n", current - 1).as_bytes());
    // An ordinary out-of-range pointer now also fails closed, but retains
    // a distinct diagnostic from the established topology error below.
    let mut reconstructable = bytes.clone();
    let past_eof = reconstructable.len() + 4096;
    replace_final_startxref(&mut reconstructable, past_eof);
    assert_reconstruction_authority_lost(&reconstructable, "");
    assert_ambiguous_final_start(&bytes);
}

#[test]
fn exact_final_startxref_rejects_older_completed_revision() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let old = object(&mut bytes, 5, 0, "(old)");
    let previous = section(&mut bytes, &normal(old, 0), "");
    let new = object(&mut bytes, 5, 0, "(new)");
    section(&mut bytes, &normal(new, 0), &format!("/Prev {previous}"));
    replace_final_startxref(&mut bytes, previous);
    assert_ambiguous_final_start(&bytes);
}

#[test]
fn exact_newest_startxref_preserves_one_two_and_three_revisions() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let mut previous = None;
    for value in ["one", "two", "three"] {
        let offset = object(&mut bytes, 5, 0, &format!("({value})"));
        let extra = previous.map_or_else(String::new, |offset| format!("/Prev {offset}"));
        let current = section(&mut bytes, &normal(offset, 0), &extra);
        let doc = Document::load_mem(&bytes).unwrap();
        assert_eq!(doc.xref_start, current);
        assert_eq!(doc.get_object((5, 0)).unwrap().as_str().unwrap(), value.as_bytes());
        assert!(Document::load_metadata_mem(&bytes).is_ok());
        previous = Some(current);
    }
}

#[test]
fn forward_prev_cannot_merge_later_live_objects() {
    // The final entry point itself is current; its predecessor has an invalid
    // forward edge. This exercises /Prev independently of final-footer checks.
    for displacement in [0, 4] {
        let mut bytes = b"%PDF-1.5\n".to_vec();
        let old = object(&mut bytes, 5, 0, "(old)");
        let first = section(&mut bytes, &normal(old, 0), "/Prev 9999999999");
        let live = object(&mut bytes, 5, 0, "(unrelated live object)");
        let later = section(&mut bytes, &normal(live, 0), "");
        let placeholder = bytes.windows(10).position(|w| w == b"9999999999").unwrap();
        bytes[placeholder..placeholder + 10].copy_from_slice(format!("{:010}", later + displacement).as_bytes());
        section(&mut bytes, "0000000000 00000 f ", &format!("/Prev {first}"));
        for error in [
            Document::load_mem(&bytes).unwrap_err(),
            Document::load_metadata_mem(&bytes).unwrap_err(),
            Document::load_mem_with_options(&bytes, lopdf::LoadOptions::with_password("password")).unwrap_err(),
            Document::load_metadata_mem_with_password(&bytes, "password").unwrap_err(),
        ] {
            assert!(
                matches!(&error, lopdf::Error::Xref(err) if format!("{err:?}") == "PrevStart"),
                "{error:?}"
            );
        }
    }
}

#[test]
fn final_xref_trailer_marker_bytes_are_not_revision_boundaries() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let value = object(&mut bytes, 5, 0, "(value)");
    section(&mut bytes, &normal(value, 0), "/Marker (startxref\n0\n%%EOF\nxref)");
    assert!(Document::load_mem(&bytes).is_ok());
}

#[test]
fn exact_final_startxref_cannot_skip_newer_xref_stream() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let old = object(&mut bytes, 5, 0, "(old)");
    let previous = section(&mut bytes, &normal(old, 0), "");
    let current = bytes.len();
    bytes.extend_from_slice(
        format!("8 0 obj\n<< /Type /XRef /Size 9 /Prev {previous} /W [1 1 2] /Index [5 1] /Length 4 >>\nstream\n")
            .as_bytes(),
    );
    bytes.extend_from_slice(&[0, 0, 0, 1]);
    bytes.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{current}\n%%EOF\n").as_bytes());
    assert!(Document::load_mem(&bytes).is_ok());
    replace_final_startxref(&mut bytes, previous);
    assert_ambiguous_final_start(&bytes);
}

fn fresh_linearized(stream: bool, target_extra: &str, middle: bool) -> Vec<u8> {
    let mut bytes = b"%PDF-1.7\n%binary comment\n".to_vec();
    object(
        &mut bytes,
        1,
        0,
        "<< /Linearized 1 /L 8888888888 /H [0 0] /O 5 /E 1 /N 1 /T 0 >>",
    );
    let entry = bytes.len();
    if stream {
        bytes.extend_from_slice(
            b"2 0 obj\n<< /Type /XRef /Size 6 /Prev 9999999999 /W [1 4 2] /Index [0 1] /Length 7 >>\nstream\n",
        );
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 255, 255]);
        bytes.extend_from_slice(b"\nendstream\nendobj\n");
    } else {
        bytes.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \ntrailer\n<< /Size 6 /Prev 9999999999 >>\n");
    }
    bytes.extend_from_slice(b"startxref\n0\n%%EOF\n");
    let value = object(&mut bytes, 5, 0, "(main section object)");
    let main = section(&mut bytes, &normal(value, 0), target_extra);
    if middle {
        section(&mut bytes, &normal(value, 0), "");
    }
    replace_final_startxref(&mut bytes, entry);
    let prev = bytes.windows(10).position(|w| w == b"9999999999").unwrap();
    bytes[prev..prev + 10].copy_from_slice(format!("{main:010}").as_bytes());
    refresh_linearized_length(&mut bytes);
    bytes
}

fn refresh_linearized_length(bytes: &mut [u8]) {
    let pos = bytes.windows(3).position(|w| w == b"/L ").unwrap() + 3;
    let length = format!("{:010}", bytes.len());
    bytes[pos..pos + 10].copy_from_slice(length.as_bytes());
}

#[test]
fn fresh_linearized_classic_and_stream_load() {
    for stream in [false, true] {
        let bytes = fresh_linearized(stream, "", false);
        for strict in [false, true] {
            let doc = Document::load_mem_with_options(
                &bytes,
                lopdf::LoadOptions {
                    strict,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(
                doc.get_object((5, 0)).unwrap().as_str().unwrap(),
                b"main section object"
            );
        }
    }
}

#[test]
fn fresh_linearized_encrypted_fixture_loads_including_strict() {
    for strict in [false, true] {
        let doc = Document::load_mem_with_options(
            include_bytes!("../assets/encrypted.pdf"),
            lopdf::LoadOptions {
                strict,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!doc.objects.is_empty());
    }
}

#[test]
fn fresh_linearized_forward_target_must_be_terminal_and_without_links() {
    for stream in [false, true] {
        for (extra, middle) in [("", true), ("/Prev 0", false), ("/XRefStm 0", false)] {
            let bytes = fresh_linearized(stream, extra, middle);
            assert!(Document::load_mem(&bytes).is_err());
            assert!(Document::load_metadata_mem(&bytes).is_err());
        }
    }
}

#[test]
fn fresh_linearized_requires_exact_length_and_first_complete_dictionary() {
    for stream in [false, true] {
        let original = fresh_linearized(stream, "", false);
        for mutation in 0..7 {
            let mut bytes = original.clone();
            match mutation {
                0 => {
                    let p = bytes.windows(3).position(|w| w == b"/L ").unwrap() + 3;
                    bytes[p] = b'1';
                }
                1 => {
                    bytes.extend_from_slice(b"\n");
                }
                2 => {
                    let p = bytes.windows(11).position(|w| w == b"/Linearized").unwrap();
                    bytes[p + 1] = b'X';
                }
                3 => {
                    let p = bytes.windows(6).position(|w| w == b"endobj").unwrap();
                    bytes[p] = b'X';
                }
                4 => {
                    let p = bytes.windows(3).position(|w| w == b"/H ").unwrap();
                    bytes[p + 1] = b'X';
                }
                5 => {
                    bytes.splice(9..9, std::iter::repeat_n(b' ', 1024));
                    refresh_linearized_length(&mut bytes);
                }
                6 => {
                    let previous = Document::load_mem(&bytes).unwrap().xref_start;
                    section(&mut bytes, "0000000000 00001 f ", &format!("/Prev {previous}"));
                }
                _ => unreachable!(),
            }
            assert!(
                Document::load_mem(&bytes).is_err(),
                "stream={stream}, mutation={mutation}"
            );
        }
    }
}

#[test]
fn newest_generation_limits_preserve_authority_across_formats_and_loaders() {
    for stream in [false, true] {
        for free in [false, true] {
            for generation in [0_u32, 1, 65535, 65536, 70000, 99999] {
                let mut bytes = b"%PDF-1.5\n".to_vec();
                let old = object(&mut bytes, 5, 0, "<< /Title (stale) >>");
                let previous = section(&mut bytes, &normal(old, 0), "/Info 5 0 R");
                let before = object(&mut bytes, 4, 0, "(before)");
                let after = object(&mut bytes, 6, 0, "(after)");
                let current_value = u16::try_from(generation)
                    .map(|g| object(&mut bytes, 5, g, "<< /Title (current) >>"))
                    .unwrap_or(old);
                let info_generation = u16::try_from(generation).unwrap_or(0);
                let current = bytes.len();
                if stream {
                    bytes.extend_from_slice(format!(
                        "8 0 obj\n<< /Type /XRef /Size 9 /Prev {previous} /Info 5 {info_generation} R /W [1 4 4] /Index [4 3] /Length 27 >>\nstream\n"
                    ).as_bytes());
                    for (kind, offset, gen_value) in [
                        (1, before, 0),
                        (u8::from(!free), current_value, generation),
                        (1, after, 0),
                    ] {
                        bytes.push(kind);
                        bytes.extend_from_slice(&u32::try_from(offset).unwrap().to_be_bytes());
                        bytes.extend_from_slice(&gen_value.to_be_bytes());
                    }
                    bytes.extend_from_slice(b"\nendstream\nendobj\n");
                } else {
                    bytes.extend_from_slice(format!(
                        "xref\n4 3\n{}\n{current_value:010} {generation:05} {} \n{}\ntrailer\n<< /Size 7 /Prev {previous} /Info 5 {info_generation} R >>\n",
                        normal(before, 0), if free { 'f' } else { 'n' }, normal(after, 0)
                    ).as_bytes());
                }
                bytes.extend_from_slice(format!("startxref\n{current}\n%%EOF\n").as_bytes());
                for strict in [false, true] {
                    let result = Document::load_mem_with_options(
                        &bytes,
                        lopdf::LoadOptions {
                            strict,
                            ..Default::default()
                        },
                    );
                    if let Ok(generation) = u16::try_from(generation) {
                        let doc = result.unwrap();
                        assert_eq!(doc.get_object((4, 0)).unwrap().as_str().unwrap(), b"before");
                        assert_eq!(doc.get_object((6, 0)).unwrap().as_str().unwrap(), b"after");
                        if free {
                            assert!(
                                matches!(doc.reference_table.get(5), Some(XrefEntry::Free { generation: g, .. }) if *g == generation)
                            );
                            assert!(doc.get_object((5, 0)).is_err());
                        } else {
                            assert!(
                                matches!(doc.reference_table.get(5), Some(XrefEntry::Normal { generation: g, .. }) if *g == generation)
                            );
                        }
                    } else {
                        assert!(
                            result.is_err(),
                            "stream={stream}, free={free}, generation={generation}, strict={strict}"
                        );
                    }
                }
                let metadata = Document::load_metadata_mem(&bytes);
                if generation > u32::from(u16::MAX) {
                    assert!(metadata.is_err());
                } else {
                    assert_eq!(
                        metadata.unwrap().title.as_deref(),
                        if free { None } else { Some("current") }
                    );
                }
            }
        }
    }
}

#[test]
fn invalid_classic_generation_cannot_resurrect_compressed_member() {
    for generation in [65536, 70000, 99999] {
        let mut bytes = indexed_object_stream_fixture(4, 1, None);
        let previous = Document::load_mem(&bytes).unwrap().xref_start;
        section(
            &mut bytes,
            &format!("0000000000 {generation} f "),
            &format!("/Prev {previous}"),
        );
        assert_reconstruction_authority_lost(&bytes, "");
    }
}
