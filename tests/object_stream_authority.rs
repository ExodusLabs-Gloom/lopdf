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
