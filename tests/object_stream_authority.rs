use lopdf::{Document, LoadOptions, Object, ObjectId, dictionary, xref::XrefEntry};

fn fixture(entry: Option<XrefEntry>, duplicate: bool) -> Vec<u8> {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let mut records = vec![(0, 0, 0, 65535)];
    let catalog = bytes.len() as u32;
    bytes.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Member 5 0 R >>\nendobj\n");
    records.push((1, 1, catalog, 0));
    for container in 8..=if duplicate { 9 } else { 8 } {
        records.push((container, 1, bytes.len() as u32, 0));
        let content = format!("5 0 << /Title (copy {container}) >>");
        bytes.extend_from_slice(format!("{container} 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n{content}\nendstream\nendobj\n", content.len()).as_bytes());
    }
    match entry {
        None => {}
        Some(XrefEntry::Null) => records.push((5, 255, u32::MAX, u16::MAX)),
        Some(XrefEntry::Compressed { container, index }) => records.push((5, 2, container, index)),
        Some(XrefEntry::Free { next_free, generation }) => records.push((5, 0, next_free, generation)),
        Some(XrefEntry::Normal { generation, .. }) => {
            records.push((5, 1, bytes.len() as u32, generation));
            bytes.extend_from_slice(format!("5 {generation} obj\n<< /Title (normal) >>\nendobj\n").as_bytes());
        }
        Some(_) => unreachable!(),
    }
    let start = bytes.len();
    records.push((10, 1, start as u32, 0));
    records.sort_by_key(|record| record.0);
    let index = records
        .iter()
        .map(|r| format!("{} 1", r.0))
        .collect::<Vec<_>>()
        .join(" ");
    bytes.extend_from_slice(format!("10 0 obj\n<< /Type /XRef /Size 11 /Root 1 0 R /Info 5 0 R /W [1 4 2] /Index [{index}] /Length {} >>\nstream\n", records.len() * 7).as_bytes());
    for (_, kind, field, generation) in records {
        bytes.push(kind);
        bytes.extend_from_slice(&field.to_be_bytes());
        bytes.extend_from_slice(&generation.to_be_bytes());
    }
    bytes.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{start}\n%%EOF\n").as_bytes());
    bytes
}

fn cases() -> Vec<(Option<XrefEntry>, Option<&'static str>)> {
    vec![
        (None, None),
        (Some(XrefEntry::Null), None),
        (Some(XrefEntry::Compressed { container: 8, index: 0 }), Some("copy 8")),
        (
            Some(XrefEntry::Free {
                next_free: 0,
                generation: 1,
            }),
            None,
        ),
        (
            Some(XrefEntry::Normal {
                offset: 0,
                generation: 0,
            }),
            Some("normal"),
        ),
        (Some(XrefEntry::Compressed { container: 7, index: 0 }), None),
        (Some(XrefEntry::Compressed { container: 8, index: 1 }), None),
    ]
}

#[test]
fn effective_xref_controls_object_stream_materialization() {
    for (entry, expected) in cases() {
        let bytes = fixture(entry, false);
        let doc = Document::load_mem(&bytes).unwrap();
        assert_eq!(doc.objects.contains_key(&(5, 0)), expected.is_some());
        let title = doc
            .get_dictionary((5, 0))
            .ok()
            .map(|d| d.get(b"Title").unwrap().as_str().unwrap());
        assert_eq!(title, expected.map(str::as_bytes));
        assert!(doc.get_object((5, 1)).is_err());
        let reference = doc
            .get_dictionary((1, 0))
            .unwrap()
            .get(b"Member")
            .unwrap()
            .as_reference()
            .unwrap();
        assert_eq!(doc.get_object(reference).is_ok(), expected.is_some());
        assert_eq!(Document::load_metadata_mem(&bytes).unwrap().title.as_deref(), expected);
        // Physical inspection remains independent of document authority.
        let stream = doc.get_object((8, 0)).unwrap().as_stream().unwrap();
        assert!(lopdf::ObjectStream::new(stream).unwrap().objects.contains_key(&(5, 0)));
    }
}

fn reject_unlisted_callback(id: ObjectId, object: &mut Object) -> Option<(ObjectId, Object)> {
    assert_ne!(id, (5, 0), "unlisted member reached the filter callback");
    Some((id, object.clone()))
}

#[test]
fn unlisted_members_are_absent_before_filtering_in_single_and_duplicate_streams() {
    for duplicate in [false, true] {
        let bytes = fixture(None, duplicate);
        for options in [
            LoadOptions::default(),
            LoadOptions::with_filter(reject_unlisted_callback),
        ] {
            let doc = Document::load_mem_with_options(&bytes, options).unwrap();
            assert!(doc.reference_table.get(5).is_none());
            assert!(!doc.objects.contains_key(&(5, 0)));
            assert!(doc.get_object((5, 0)).is_err());
            assert!(doc.get_object((8, 0)).is_ok());
            if duplicate {
                assert!(doc.get_object((9, 0)).is_ok());
            }
        }
    }
}

#[test]
fn deferred_decryption_requires_matching_compressed_authority() {
    use lopdf::{EncryptionState, EncryptionVersion, Permissions};
    for (entry, expected) in cases() {
        let mut doc = Document::load_mem(&fixture(entry, true)).unwrap();
        doc.objects.remove(&(5, 0));
        if expected == Some("normal") {
            doc.set_object((5, 0), dictionary! { "Title" => Object::string_literal("normal") });
        }
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
        doc.decrypt_raw(b"user").unwrap();
        assert_eq!(doc.objects.contains_key(&(5, 0)), expected.is_some());
        let title = doc
            .get_dictionary((5, 0))
            .ok()
            .map(|d| d.get(b"Title").unwrap().as_str().unwrap());
        assert_eq!(title, expected.map(str::as_bytes));
    }
}

fn set_container_type(doc: &mut Document, kind: Option<&str>) {
    let stream = doc.objects.get_mut(&(8, 0)).unwrap().as_stream_mut().unwrap();
    stream.dict.remove(b"Type");
    if let Some(kind) = kind {
        stream.dict.set("Type", Object::Name(kind.as_bytes().to_vec()));
    }
}

fn encrypt_container(doc: &mut Document) {
    use lopdf::{EncryptionState, EncryptionVersion, Permissions};
    doc.objects.remove(&(5, 0));
    doc.trailer.set(
        "ID",
        vec![
            Object::string_literal("identifier"),
            Object::string_literal("identifier"),
        ],
    );
    let state = EncryptionState::try_from(EncryptionVersion::V2 {
        document: doc,
        owner_password: "owner",
        user_password: "user",
        key_length: 128,
        permissions: Permissions::PRINTABLE,
    })
    .unwrap();
    doc.encrypt(&state).unwrap();
}

#[test]
fn container_type_controls_full_and_metadata_materialization() {
    for kind in ["/Type /ObjStm", "", "/Type /Other"] {
        let original = fixture(Some(XrefEntry::Compressed { container: 8, index: 0 }), false);
        let mut bytes = original;
        let marker = b"/Type /ObjStm";
        let start = bytes.windows(marker.len()).position(|w| w == marker).unwrap();
        let replacement = format!("{kind:width$}", width = marker.len());
        bytes[start..start + marker.len()].copy_from_slice(replacement.as_bytes());
        let expected = kind == "/Type /ObjStm";
        let doc = Document::load_mem(&bytes).unwrap();
        assert_eq!(doc.get_object((5, 0)).is_ok(), expected, "{kind}");
        assert!(doc.get_object((8, 0)).unwrap().as_stream().is_ok());
        assert_eq!(
            Document::load_metadata_mem(&bytes).unwrap().title.as_deref(),
            expected.then_some("copy 8")
        );
    }
}

#[test]
fn container_type_controls_encrypted_and_deferred_materialization() {
    for kind in [Some("ObjStm"), None, Some("XObject")] {
        for (container, index, member, expected) in
            [(8, 0, 5, true), (7, 0, 5, false), (8, 1, 5, false), (8, 0, 6, false)]
        {
            let mut doc =
                Document::load_mem(&fixture(Some(XrefEntry::Compressed { container, index }), false)).unwrap();
            doc.objects.get_mut(&(8, 0)).unwrap().as_stream_mut().unwrap().content[0] = b'0' + member;
            set_container_type(&mut doc, kind);
            encrypt_container(&mut doc);
            let expected = expected && kind == Some("ObjStm");
            let mut deferred = doc.clone();
            deferred.decrypt_raw(b"user").unwrap();
            assert_eq!(deferred.get_object((5, 0)).is_ok(), expected);

            // Save the physical container as a generic stream so the writer retains it.
            set_container_type(&mut doc, Some("Unused"));
            let encrypt_id = doc.trailer.get(b"Encrypt").unwrap().as_reference().unwrap();
            let mut bytes = Vec::new();
            doc.save_to(&mut bytes).unwrap();
            let marker = b"/Type/Unused";
            let start = bytes.windows(marker.len()).position(|w| w == marker).unwrap();
            let replacement = match kind {
                Some("ObjStm") => b"/Type/ObjStm".as_slice(),
                None => b"            ".as_slice(),
                _ => b"/Type/Other ".as_slice(),
            };
            bytes[start..start + marker.len()].copy_from_slice(replacement);
            let tail = String::from_utf8_lossy(&bytes);
            let prev: usize = tail
                .rsplit("startxref\n")
                .next()
                .unwrap()
                .lines()
                .next()
                .unwrap()
                .parse()
                .unwrap();
            let start = bytes.len() + 1;
            bytes.extend_from_slice(format!("\n20 0 obj\n<< /Type /XRef /Size 21 /Root 1 0 R /Info 5 0 R /Encrypt {} {} R /ID [(identifier)(identifier)] /Prev {prev} /W [1 4 2] /Index [5 1 20 1] /Length 14 >>\nstream\n", encrypt_id.0, encrypt_id.1).as_bytes());
            bytes.push(2);
            bytes.extend_from_slice(&container.to_be_bytes());
            bytes.extend_from_slice(&index.to_be_bytes());
            bytes.push(1);
            bytes.extend_from_slice(&(start as u32).to_be_bytes());
            bytes.extend_from_slice(&0u16.to_be_bytes());
            bytes.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{start}\n%%EOF\n").as_bytes());
            let loaded = Document::load_mem_with_options(&bytes, LoadOptions::with_password("user")).unwrap();
            assert!(!loaded.is_encrypted());
            assert_eq!(
                loaded.get_object((5, 0)).is_ok(),
                expected,
                "{kind:?}, {container}, {index}"
            );
            if expected {
                assert_eq!(
                    loaded
                        .get_dictionary((5, 0))
                        .unwrap()
                        .get(b"Title")
                        .unwrap()
                        .as_str()
                        .unwrap(),
                    b"copy 8"
                );
            }
        }
    }
}

fn compressed_container_fixture(containers: &[u32], root: u32, encrypted: bool) -> Vec<u8> {
    let mut bytes = fixture(Some(XrefEntry::Compressed { container: 8, index: 0 }), false);
    let mut encryption = String::new();
    if encrypted {
        let mut doc = Document::load_mem(&bytes).unwrap();
        encrypt_container(&mut doc);
        let id = doc.trailer.get(b"Encrypt").unwrap().as_reference().unwrap();
        encryption = format!("/Encrypt {} {} R /ID [(identifier)(identifier)]", id.0, id.1);
        bytes.clear();
        doc.save_to(&mut bytes).unwrap();
    }
    let prev: usize = String::from_utf8_lossy(&bytes)
        .rsplit("startxref\n")
        .next()
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let catalog = bytes.len();
    bytes.extend_from_slice(b"2 0 obj\n<< /Type /Catalog /Pages 5 0 R >>\nendobj\n");
    // Also exercise the parser's ordinary indirect stream-length lookup.
    let stream = bytes.len();
    bytes.extend_from_slice(b"3 0 obj\n<< /Length 5 0 R >>\nstream\nx\nendstream\nendobj\n");
    let start = bytes.len();
    let mut records = vec![(2, 1u8, catalog as u32, 0u16), (3, 1, stream as u32, 0)];
    for (ordinal, &container) in containers.iter().enumerate() {
        let member = if ordinal == 0 { 5 } else { 19 + ordinal as u32 };
        records.push((member, 2, container, 0));
    }
    let xref_id = 20 + containers.len() as u32;
    records.push((xref_id, 1, start as u32, 0));
    let index = records
        .iter()
        .map(|r| format!("{} 1", r.0))
        .collect::<Vec<_>>()
        .join(" ");
    bytes.extend_from_slice(format!("{xref_id} 0 obj\n<< /Type /XRef /Size {} /Root {root} 0 R /Info 5 0 R {encryption} /Prev {prev} /W [1 4 2] /Index [{index}] /Length {} >>\nstream\n", xref_id + 1, records.len() * 7).as_bytes());
    for (_, kind, field, generation) in records {
        bytes.push(kind);
        bytes.extend_from_slice(&field.to_be_bytes());
        bytes.extend_from_slice(&generation.to_be_bytes());
    }
    bytes.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{start}\n%%EOF\n").as_bytes());
    bytes
}

fn assert_compressed_containers_rejected(containers: &[u32]) {
    for root in [2, 5] {
        for encrypted in [false, true] {
            let bytes = compressed_container_fixture(containers, root, encrypted);
            let options = LoadOptions::with_password("user");
            let metadata = Document::load_metadata_mem_with_password(&bytes, "user").unwrap();
            assert_eq!(metadata.encrypted, encrypted);
            assert_eq!(metadata.title, None);
            assert_eq!(metadata.page_count, 0);
            let doc = Document::load_mem_with_options(&bytes, options).unwrap();
            for ordinal in 0..containers.len() {
                let id = if ordinal == 0 { 5 } else { 19 + ordinal as u32 };
                assert!(matches!(
                    doc.reference_table.get(id),
                    Some(XrefEntry::Compressed { .. })
                ));
                assert!(!doc.objects.contains_key(&(id, 0)), "materialized {id}");
            }
        }
    }
}

#[test]
fn self_compressed_container_is_rejected() {
    assert_compressed_containers_rejected(&[5]);
}

#[test]
fn mutual_compressed_containers_are_rejected() {
    assert_compressed_containers_rejected(&[20, 5]);
}

#[test]
fn three_compressed_container_cycle_is_rejected() {
    assert_compressed_containers_rejected(&[20, 21, 5]);
}

#[test]
fn long_compressed_container_chain_is_rejected() {
    let mut containers: Vec<u32> = (20..83).collect();
    containers.push(8);
    assert_compressed_containers_rejected(&containers);
}

fn indirect_length_fixture(length_ref: Option<u32>, encrypted: bool) -> Vec<u8> {
    let mut doc = Document::load_mem(&fixture(Some(XrefEntry::Compressed { container: 8, index: 0 }), false)).unwrap();
    if encrypted {
        encrypt_container(&mut doc);
    }
    let content = doc.get_object((8, 0)).unwrap().as_stream().unwrap().content.clone();
    let encryption = if encrypted {
        let id = doc.trailer.get(b"Encrypt").unwrap().as_reference().unwrap();
        format!("/Encrypt {} {} R /ID [(identifier)(identifier)]", id.0, id.1)
    } else {
        String::new()
    };
    set_container_type(&mut doc, Some("Unused"));
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let prev: usize = String::from_utf8_lossy(&bytes)
        .rsplit("startxref\n")
        .next()
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let length = length_ref.map_or_else(|| content.len().to_string(), |id| format!("{id} 0 R"));
    let container_offset = bytes.len();
    bytes
        .extend_from_slice(format!("8 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length {length} >>\nstream\n").as_bytes());
    bytes.extend_from_slice(&content);
    bytes.extend_from_slice(b"\nendstream\nendobj\n");
    let length_offset = bytes.len();
    bytes.extend_from_slice(format!("9 0 obj\n{}\nendobj\n", content.len()).as_bytes());
    let start = bytes.len();
    // Member 6 shares the normal container, so /Length 6 re-enters active container 8.
    let records = [
        (2u8, 8u32, 0u16),
        (2, 8, 1),
        (1, container_offset as u32, 0),
        (1, length_offset as u32, 0),
        (1, start as u32, 0),
    ];
    bytes.extend_from_slice(format!("20 0 obj\n<< /Type /XRef /Size 21 /Root 1 0 R /Info 5 0 R {encryption} /Prev {prev} /W [1 4 2] /Index [5 2 8 2 20 1] /Length 35 >>\nstream\n").as_bytes());
    for (kind, field, generation) in records {
        bytes.push(kind);
        bytes.extend_from_slice(&field.to_be_bytes());
        bytes.extend_from_slice(&generation.to_be_bytes());
    }
    bytes.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{start}\n%%EOF\n").as_bytes());
    bytes
}

#[test]
fn object_stream_indirect_length_cycles_terminate() {
    for length_ref in [5, 6] {
        for encrypted in [false, true] {
            let bytes = indirect_length_fixture(Some(length_ref), encrypted);
            let metadata = Document::load_metadata_mem_with_password(&bytes, "user").unwrap();
            assert_eq!(metadata.encrypted, encrypted);
            assert_eq!(metadata.title, None);
            let doc = Document::load_mem_with_options(&bytes, LoadOptions::with_password("user")).unwrap();
            assert!(matches!(
                doc.reference_table.get(5),
                Some(XrefEntry::Compressed { container: 8, index: 0 })
            ));
            assert!(matches!(
                doc.reference_table.get(8),
                Some(XrefEntry::Normal { generation: 0, .. })
            ));
            assert!(!doc.objects.contains_key(&(5, 0)));
            assert!(!doc.objects.contains_key(&(6, 0)));
            if !encrypted {
                assert_eq!(Document::load_metadata_mem(&bytes).unwrap().title, None);
                assert!(!Document::load_mem(&bytes).unwrap().objects.contains_key(&(5, 0)));
            }
        }
    }
}

#[test]
fn object_stream_direct_and_indirect_lengths_resolve() {
    for length_ref in [None, Some(9)] {
        for encrypted in [false, true] {
            let bytes = indirect_length_fixture(length_ref, encrypted);
            let metadata = Document::load_metadata_mem_with_password(&bytes, "user").unwrap();
            assert_eq!(metadata.encrypted, encrypted);
            assert_eq!(metadata.title.as_deref(), Some("copy 8"));
            let doc = Document::load_mem_with_options(&bytes, LoadOptions::with_password("user")).unwrap();
            assert_eq!(
                doc.get_dictionary((5, 0))
                    .unwrap()
                    .get(b"Title")
                    .unwrap()
                    .as_str()
                    .unwrap(),
                b"copy 8"
            );
        }
    }
}
