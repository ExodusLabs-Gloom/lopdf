use lopdf::{Document, ObjectStream, xref::XrefEntry};

type Record = (u32, u8, u32, u16);

fn object(bytes: &mut Vec<u8>, id: u32, body: &str) -> Record {
    let offset = bytes.len() as u32;
    bytes.extend_from_slice(format!("{id} 0 obj\n{body}\nendobj\n").as_bytes());
    (id, 1, offset, 0)
}

fn table(bytes: &mut Vec<u8>, records: &[Record], size: u32, extra: &str) -> usize {
    let start = bytes.len();
    bytes.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
    for &(id, kind, field, generation) in records {
        assert!(kind <= 1);
        let flag = if kind == 0 { 'f' } else { 'n' };
        bytes.extend_from_slice(format!("{id} 1\n{field:010} {generation:05} {flag} \n").as_bytes());
    }
    bytes.extend_from_slice(format!("trailer\n<< /Size {size} {extra} >>\nstartxref\n{start}\n%%EOF\n").as_bytes());
    start
}

fn stream(bytes: &mut Vec<u8>, records: &[Record], extra: &str) -> usize {
    let start = bytes.len();
    let mut records = records.to_vec();
    records.push((7, 1, start as u32, 0));
    let index = records
        .iter()
        .map(|r| format!("{} 1", r.0))
        .collect::<Vec<_>>()
        .join(" ");
    bytes.extend_from_slice(
        format!(
            "7 0 obj\n<< /Type /XRef /Size 10 /W [1 4 2] /Index [{index}] /Length {} {extra} >>\nstream\n",
            records.len() * 7
        )
        .as_bytes(),
    );
    for (_, kind, field, generation) in records {
        bytes.push(kind);
        bytes.extend_from_slice(&field.to_be_bytes());
        bytes.extend_from_slice(&generation.to_be_bytes());
    }
    bytes.extend_from_slice(b"\nendstream\nendobj\n");
    start
}

fn check(bytes: &[u8], id: u32) -> Document {
    let doc = Document::load_mem(bytes).unwrap();
    let allowed = id < 10;
    assert_eq!(doc.trailer.get(b"Size").unwrap().as_i64().unwrap(), 10);
    assert_eq!(doc.reference_table.size, 10);
    assert_eq!(doc.max_id, doc.reference_table.max_id());
    assert!(doc.reference_table.entries.keys().all(|&id| id < 10));
    assert_eq!(doc.reference_table.get(id).is_some(), allowed);
    assert_eq!(doc.objects.contains_key(&(id, 0)), allowed);
    assert_eq!(doc.get_object((id, 0)).is_ok(), allowed);
    assert_eq!(
        Document::load_metadata_mem(bytes).unwrap().title.as_deref(),
        allowed.then_some("member")
    );
    doc
}

#[test]
fn classic_table_respects_exclusive_size_bound() {
    for id in [9, 10, 11] {
        let mut bytes = b"%PDF-1.5\n".to_vec();
        let record = object(&mut bytes, id, "<< /Title (member) >>");
        table(&mut bytes, &[record], 10, &format!("/Info {id} 0 R"));
        check(&bytes, id);
    }
}

#[test]
fn stream_and_hybrid_respect_size_for_normal_and_compressed_objects() {
    for hybrid in [false, true] {
        for compressed in [false, true] {
            for id in [9, 10, 11] {
                let mut bytes = b"%PDF-1.5\n".to_vec();
                let previous = hybrid.then(|| table(&mut bytes, &[], 10, ""));
                let records = if compressed {
                    let header = format!("{id} 0 ");
                    let content = format!("{header}<< /Title (member) >>");
                    let container = object(
                        &mut bytes,
                        8,
                        &format!(
                            "<< /Type /ObjStm /N 1 /First {} /Length {} >>\nstream\n{content}\nendstream",
                            header.len(),
                            content.len()
                        ),
                    );
                    vec![container, (id, 2, 8, 0)]
                } else {
                    vec![object(&mut bytes, id, "<< /Title (member) >>")]
                };
                let supplement = stream(&mut bytes, &records, &format!("/Info {id} 0 R"));
                if let Some(previous) = previous {
                    table(
                        &mut bytes,
                        &[],
                        10,
                        &format!("/Prev {previous} /XRefStm {supplement} /Info {id} 0 R"),
                    );
                } else {
                    bytes.extend_from_slice(format!("startxref\n{supplement}\n%%EOF\n").as_bytes());
                }
                let doc = check(&bytes, id);
                if compressed {
                    let physical = ObjectStream::new(doc.get_object((8, 0)).unwrap().as_stream().unwrap()).unwrap();
                    assert!(physical.objects.contains_key(&(id, 0)));
                }
            }
        }
    }
}

#[test]
fn newest_size_bounds_older_tables_and_supplements() {
    for hybrid in [false, true] {
        for id in [9, 10, 11] {
            let mut bytes = b"%PDF-1.5\n".to_vec();
            let record = object(&mut bytes, id, "<< /Title (member) >>");
            let previous = if hybrid {
                let first = table(&mut bytes, &[], 12, "");
                let supplement = stream(&mut bytes, &[record], "");
                table(&mut bytes, &[], 10, &format!("/Prev {first} /XRefStm {supplement}"))
            } else {
                table(&mut bytes, &[record], 12, "")
            };
            table(&mut bytes, &[], 10, &format!("/Prev {previous} /Info {id} 0 R"));
            check(&bytes, id);
        }
    }
}

#[test]
fn sparse_size_is_preserved_and_main_free_remains_authoritative() {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let old = object(&mut bytes, 5, "<< /Title (old) >>");
    let previous = table(&mut bytes, &[old], 10, "");
    let current = object(&mut bytes, 5, "<< /Title (supplement) >>");
    let supplement = stream(&mut bytes, &[current], "");
    table(
        &mut bytes,
        &[(5, 0, 0, 1)],
        10,
        &format!("/Prev {previous} /XRefStm {supplement} /Info 5 0 R"),
    );
    let doc = Document::load_mem(&bytes).unwrap();
    assert_eq!(doc.reference_table.size, 10);
    assert_eq!(doc.reference_table.max_id(), 7);
    assert_eq!(doc.max_id, 7);
    assert!(matches!(
        doc.reference_table.get(5),
        Some(XrefEntry::Free { generation: 1, .. })
    ));
    assert!(doc.get_object((5, 0)).is_err());
    assert!(Document::load_metadata_mem(&bytes).unwrap().title.is_none());
}

fn sparse_document(size_entry: &str, xref_stream: bool) -> Vec<u8> {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let record = object(&mut bytes, 5, "<< /Title (member) >>");
    if xref_stream {
        let start = stream(&mut bytes, &[record], "/Info 5 0 R");
        bytes.extend_from_slice(format!("startxref\n{start}\n%%EOF\n").as_bytes());
    } else {
        table(&mut bytes, &[record], 10, "/Info 5 0 R");
    }
    let marker = b"/Size 10";
    let start = bytes.windows(marker.len()).position(|w| w == marker).unwrap();
    bytes.splice(start..start + marker.len(), size_entry.bytes());
    bytes
}

#[test]
fn malformed_authoritative_size_fails_closed() {
    for xref_stream in [false, true] {
        for size in ["/Size -1", "/Size 4294967297", "/Size 1.5", ""] {
            let bytes = sparse_document(size, xref_stream);
            assert!(Document::load_mem(&bytes).is_err(), "{size}, stream={xref_stream}");
            assert!(Document::load_metadata_mem(&bytes).is_err());
        }
    }
}

#[test]
fn sparse_size_does_not_reserve_object_ids_and_round_trips() {
    for size in [10, u32::MAX] {
        for xref_stream in [false, true] {
            let bytes = sparse_document(&format!("/Size {size}"), xref_stream);
            let doc = Document::load_mem(&bytes).unwrap();
            let highest = if xref_stream { 7 } else { 5 };
            assert_eq!(doc.reference_table.size, size);
            assert_eq!(doc.max_id, highest);
            let mut allocating = doc.clone();
            assert_eq!(allocating.add_object(42), (highest + 1, 0));
            assert_eq!(Document::new_from_prev(&doc).max_id, highest);
            for (objects, streams) in [(false, false), (false, true), (true, true)] {
                let mut saving = doc.clone();
                saving.reference_table.cross_reference_type = lopdf::xref::XrefType::CrossReferenceTable;
                let mut output = Vec::new();
                saving
                    .save_with_options(
                        &mut output,
                        lopdf::SaveOptions {
                            use_object_streams: objects,
                            use_xref_streams: streams,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                let reloaded = Document::load_mem(&output).unwrap();
                assert_eq!(reloaded.reference_table.size, saving.max_id + 1);
                assert!(reloaded.reference_table.size > highest);
                assert!(reloaded.reference_table.size <= highest + 3);
                assert_eq!(reloaded.get_object((5, 0)).unwrap(), doc.get_object((5, 0)).unwrap());
                assert_eq!(
                    Document::load_metadata_mem(&output).unwrap().title.as_deref(),
                    Some("member")
                );
                if objects {
                    assert!(matches!(
                        reloaded.reference_table.get(5),
                        Some(XrefEntry::Compressed { .. })
                    ));
                }
            }
        }
    }
}
