use lopdf::{
    Document, IncrementalDocument, LoadOptions, Object, SaveOptions,
    xref::{Xref, XrefEntry, XrefSection, XrefStreamBuilder, XrefType},
};

fn object(bytes: &mut Vec<u8>, id: u32, value: &str) -> usize {
    let offset = bytes.len();
    bytes.extend_from_slice(format!("{id} 0 obj\n{value}\nendobj\n").as_bytes());
    offset
}

fn stream(bytes: &mut Vec<u8>, id: u32, records: &[(u32, u8, u32, u16)], extra: &str) -> usize {
    let offset = bytes.len();
    let index = records
        .iter()
        .map(|r| format!("{} 1", r.0))
        .collect::<Vec<_>>()
        .join(" ");
    bytes.extend_from_slice(format!("{id} 0 obj\n<< /Type /XRef /Size {} /Root 1 0 R /Info 5 0 R /W [1 4 2] /Index [{index}] /Length {} {extra} >>\nstream\n", id + 1, records.len() * 7).as_bytes());
    for &(_, kind, field2, field3) in records {
        bytes.push(kind);
        bytes.extend_from_slice(&field2.to_be_bytes());
        bytes.extend_from_slice(&field3.to_be_bytes());
    }
    bytes.extend_from_slice(b"\nendstream\nendobj\n");
    offset
}

fn footer(bytes: &mut Vec<u8>, offset: usize) {
    bytes.extend_from_slice(format!("startxref\n{offset}\n%%EOF\n").as_bytes());
}

// Older type 0, 1, or 2 authority; optionally use a same-revision supplement.
fn fixture(older: u8, unknown: u8, hybrid: bool, main_live: bool) -> Vec<u8> {
    let mut bytes = b"%PDF-1.7\n".to_vec();
    let root = object(&mut bytes, 1, "<< /Type /Catalog >>");
    let stale = object(&mut bytes, 5, "<< /Title (older normal) >>");
    let content = "5 0 << /Title (older compressed) >>";
    let container = object(
        &mut bytes,
        8,
        &format!(
            "<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ),
    );
    let field2 = match older {
        0 => 4,
        1 => stale as u32,
        2 => 8,
        _ => unreachable!(),
    };
    let previous = stream(
        &mut bytes,
        10,
        &[
            (0, 0, 0, 65535),
            (1, 1, root as u32, 0),
            (5, older, field2, if older == 0 { 7 } else { 0 }),
            (8, 1, container as u32, 0),
        ],
        "",
    );
    footer(&mut bytes, previous);
    // The base is a positive control for each kind of older authority.
    let base = Document::load_mem(&bytes).unwrap();
    assert_eq!(base.get_object((5, 0)).is_ok(), older != 0);
    let current = stream(
        &mut bytes,
        11,
        &[(5, unknown, u32::MAX, u16::MAX)],
        &if hybrid {
            String::new()
        } else {
            format!("/Prev {previous}")
        },
    );
    if hybrid {
        let live = main_live.then(|| object(&mut bytes, 5, "<< /Title (current main) >>"));
        let main = bytes.len();
        bytes.extend_from_slice(b"xref\n");
        if let Some(live) = live {
            bytes.extend_from_slice(format!("5 1\n{live:010} 00000 n \n").as_bytes());
        }
        bytes.extend_from_slice(format!("11 1\n{current:010} 00000 n \ntrailer\n<< /Size 12 /Root 1 0 R /Info 5 0 R /XRefStm {current} /Prev {previous} >>\n").as_bytes());
        footer(&mut bytes, main);
    } else {
        footer(&mut bytes, current);
    }
    bytes
}

fn assert_null(bytes: &[u8]) {
    for strict in [false, true] {
        let doc = Document::load_mem_with_options(
            bytes,
            LoadOptions {
                strict,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(doc.reference_table.get(5), Some(XrefEntry::Null)));
        assert!(!doc.objects.keys().any(|&(id, _)| id == 5));
        for generation in [0, 7, u16::MAX] {
            assert!(doc.get_object((5, generation)).is_err());
        }
    }
    assert_eq!(Document::load_metadata_mem(bytes).unwrap().title, None);
    assert_eq!(
        Document::load_metadata_mem_with_password(bytes, "").unwrap().title,
        None
    );
}

#[test]
fn unknown_newest_shadows_normal_compressed_and_free_in_both_modes() {
    for older in 0..=2 {
        for unknown in [3, 255] {
            assert_null(&fixture(older, unknown, false, false));
        }
    }
}

#[test]
fn unknown_supplement_blocks_previous_live_authority() {
    for older in [1, 2] {
        for unknown in [3, 255] {
            assert_null(&fixture(older, unknown, true, false));
        }
    }
}

#[test]
fn main_normal_precedes_unknown_supplement() {
    for unknown in [3, 255] {
        let bytes = fixture(1, unknown, true, true);
        for strict in [false, true] {
            let doc = Document::load_mem_with_options(
                &bytes,
                LoadOptions {
                    strict,
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(matches!(doc.reference_table.get(5), Some(XrefEntry::Normal { .. })));
            assert_eq!(
                doc.get_dictionary((5, 0))
                    .unwrap()
                    .get(b"Title")
                    .unwrap()
                    .as_str()
                    .unwrap(),
                b"current main"
            );
        }
        assert_eq!(
            Document::load_metadata_mem(&bytes).unwrap().title.as_deref(),
            Some("current main")
        );
    }
}

#[test]
fn full_rewrite_normalizes_null_to_non_reusable_free() {
    for older in [1, 2] {
        for use_stream in [false, true] {
            let mut doc = Document::load_mem(&fixture(older, 255, false, false)).unwrap();
            assert!(matches!(doc.reference_table.get(5), Some(XrefEntry::Null)));
            doc.reference_table.cross_reference_type = if use_stream {
                XrefType::CrossReferenceStream
            } else {
                XrefType::CrossReferenceTable
            };
            let mut bytes = Vec::new();
            doc.save_with_options(&mut bytes, SaveOptions::builder().use_xref_streams(use_stream).build())
                .unwrap();
            let loaded = Document::load_mem(&bytes).unwrap();
            assert!(matches!(
                loaded.reference_table.get(5),
                Some(XrefEntry::Free {
                    next_free: 0,
                    generation: u16::MAX
                })
            ));
            assert!(loaded.get_object((5, 0)).is_err());
            assert_eq!(Document::load_metadata_mem(&bytes).unwrap().title, None);
            let mut current = 0;
            let mut visited = std::collections::BTreeSet::new();
            loop {
                assert!(visited.insert(current), "free-list cycle");
                let Some(XrefEntry::Free { next_free, .. }) = loaded.reference_table.get(current) else {
                    panic!("invalid free chain")
                };
                assert_ne!(*next_free, 5);
                current = *next_free;
                if current == 0 {
                    break;
                }
            }
        }
    }
}

#[test]
fn unrelated_incremental_update_preserves_null_and_explicit_update_supersedes_it() {
    for older in [1, 2] {
        let original = fixture(older, 3, false, false);
        let loaded = Document::load_mem(&original).unwrap();
        let mut incremental = IncrementalDocument::create_from(original.clone(), loaded);
        incremental.new_document.add_object(Object::Integer(42));
        let mut bytes = Vec::new();
        incremental.save_to(&mut bytes).unwrap();
        assert!(bytes.starts_with(&original));
        assert_null(&bytes);
        let loaded = Document::load_mem(&bytes).unwrap();
        let mut replacement = IncrementalDocument::create_from(bytes.clone(), loaded);
        replacement.new_document.set_object((5, 0), Object::Integer(99));
        let mut updated = Vec::new();
        replacement.save_to(&mut updated).unwrap();
        assert!(updated.starts_with(&bytes));
        for strict in [false, true] {
            let doc = Document::load_mem_with_options(
                &updated,
                LoadOptions {
                    strict,
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(matches!(doc.reference_table.get(5), Some(XrefEntry::Normal { .. })));
            assert_eq!(doc.get_object((5, 0)).unwrap().as_i64().unwrap(), 99);
        }
    }
}

#[test]
fn raw_null_serialization_fails_and_does_not_influence_widths() {
    let mut bytes = Vec::new();
    assert_eq!(
        XrefEntry::Null.write_xref_entry(&mut bytes).unwrap_err().kind(),
        std::io::ErrorKind::Unsupported
    );
    assert!(bytes.is_empty());
    let mut section = XrefSection::new(1);
    section.add_entry(XrefEntry::Normal {
        offset: 9,
        generation: 0,
    });
    section.add_entry(XrefEntry::Null);
    assert_eq!(
        section.write_xref_section(&mut bytes).unwrap_err().kind(),
        std::io::ErrorKind::Unsupported
    );
    assert!(bytes.is_empty());
    assert_eq!(
        XrefEntry::Null.encode_for_xref_stream(&[1, 4, 2]).unwrap_err().kind(),
        std::io::ErrorKind::Unsupported
    );
    let mut xref = Xref::new(3, XrefType::CrossReferenceStream);
    xref.insert(
        1,
        XrefEntry::Normal {
            offset: 9,
            generation: 0,
        },
    );
    let widths = XrefStreamBuilder::new(&xref).calculate_optimal_widths();
    xref.insert(2, XrefEntry::Null);
    let mut builder = XrefStreamBuilder::new(&xref);
    assert_eq!(builder.calculate_optimal_widths(), widths);
    for error in [
        builder.build_stream_content().unwrap_err(),
        builder.to_stream_object().unwrap_err(),
    ] {
        assert!(matches!(error, lopdf::Error::IO(e) if e.kind() == std::io::ErrorKind::Unsupported));
    }
}
