use lopdf::{Document, SaveOptions, xref::XrefEntry};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug)]
enum Encoding {
    Table,
    Stream,
    ObjectStreams,
}

const ENCODINGS: [Encoding; 3] = [Encoding::Table, Encoding::Stream, Encoding::ObjectStreams];

fn input(live: &[(u32, u16)], free: &[(u32, u32, u16)], size: u32, stale: bool) -> Document {
    let mut bytes = b"%PDF-1.5\n".to_vec();
    let mut records = BTreeMap::new();
    for &(id, generation) in live {
        let offset = bytes.len();
        bytes.extend_from_slice(format!("{id} {generation} obj\n<< /Value {id} >>\nendobj\n").as_bytes());
        records.insert(id, format!("{offset:010} {generation:05} n \n"));
    }
    if stale {
        bytes.extend_from_slice(b"2 0 obj\n(stale-physical-payload)\nendobj\n");
    }
    for &(id, next, generation) in free {
        records.insert(id, format!("{next:010} {generation:05} f \n"));
    }
    let start = bytes.len();
    bytes.extend_from_slice(b"xref\n0 1\n0000000000 65535 f \n");
    for (id, record) in records {
        bytes.extend_from_slice(format!("{id} 1\n{record}").as_bytes());
    }
    bytes.extend_from_slice(format!("trailer\n<< /Size {size} >>\nstartxref\n{start}\n%%EOF\n").as_bytes());
    Document::load_mem(&bytes).unwrap()
}

fn save(doc: &mut Document, encoding: Encoding) -> (Vec<u8>, Document) {
    let mut bytes = Vec::new();
    if matches!(encoding, Encoding::Table) {
        doc.reference_table.cross_reference_type = lopdf::xref::XrefType::CrossReferenceTable;
    }
    let options = SaveOptions::builder()
        .use_xref_streams(!matches!(encoding, Encoding::Table))
        .use_object_streams(matches!(encoding, Encoding::ObjectStreams))
        .build();
    doc.save_with_options(&mut bytes, options).unwrap();
    let loaded = Document::load_mem(&bytes).unwrap();
    (bytes, loaded)
}

fn verify(bytes: &[u8], doc: &Document, encoding: Encoding, expected_free: &[(u32, u16)], live: &[(u32, u16)]) {
    let xref = &doc.reference_table;
    let max = xref.max_id();
    assert_eq!(xref.entries.len(), max as usize + 1);
    assert_eq!(doc.trailer.get(b"Size").unwrap().as_i64().unwrap(), i64::from(max + 1));
    let reusable: Vec<_> = expected_free
        .iter()
        .filter_map(|&(id, generation)| (generation < u16::MAX).then_some(id))
        .collect();
    let mut actual_free = Vec::new();
    for id in 0..=max {
        let entry = xref.get(id).expect("complete coverage");
        if let XrefEntry::Free { next_free, generation } = entry {
            if id == 0 {
                assert_eq!(*generation, u16::MAX);
                assert_eq!(*next_free, reusable.first().copied().unwrap_or(0));
            } else {
                actual_free.push((id, *generation));
                let next = reusable
                    .iter()
                    .position(|&free| free == id)
                    .and_then(|i| reusable.get(i + 1));
                assert_eq!(*next_free, next.copied().unwrap_or(0));
                assert!(!doc.objects.keys().any(|&(object_id, _)| object_id == id));
            }
        }
    }
    assert_eq!(actual_free, expected_free);
    // Follow the actual chain separately: all reusable IDs once, then object 0.
    let mut current = 0;
    for expected in reusable.iter().copied().chain(std::iter::once(0)) {
        let Some(XrefEntry::Free { next_free, .. }) = xref.get(current) else {
            panic!("free chain reached a live object");
        };
        assert_eq!(*next_free, expected);
        current = *next_free;
    }
    for &(id, generation) in live {
        assert!(matches!(
            xref.get(id),
            Some(XrefEntry::Normal { .. } | XrefEntry::Compressed { .. })
        ));
        assert_eq!(
            doc.get_object((id, generation))
                .unwrap()
                .as_dict()
                .unwrap()
                .get(b"Value")
                .unwrap()
                .as_i64()
                .unwrap(),
            i64::from(id)
        );
    }

    // Check the initial physical representation, not just the reader's effective map.
    let marker = b"startxref\n";
    let start = bytes.windows(marker.len()).rposition(|s| s == marker).unwrap() + marker.len();
    let offset: usize = std::str::from_utf8(&bytes[start..])
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let tail = &bytes[offset..];
    match encoding {
        Encoding::Table => {
            let text = std::str::from_utf8(tail).unwrap();
            let mut lines = text.lines();
            assert_eq!(lines.next(), Some("xref"));
            assert_eq!(lines.next().unwrap(), format!("0 {}", max + 1));
            for id in 0..=max {
                let mut encoded = Vec::new();
                xref.get(id).unwrap().write_xref_entry(&mut encoded).unwrap();
                assert_eq!(lines.next().unwrap().as_bytes(), encoded.strip_suffix(b"\n").unwrap());
            }
            assert_eq!(lines.next(), Some("trailer"));
        }
        Encoding::Stream | Encoding::ObjectStreams => {
            let stream = b"stream\n";
            let begin = tail.windows(stream.len()).position(|s| s == stream).unwrap() + stream.len();
            let header = std::str::from_utf8(&tail[..begin]).unwrap();
            assert!(header.contains(&format!("/Index[0 {}]", max + 1)));
            for id in 0..=max {
                let record = &tail[begin + id as usize * 7..begin + (id as usize + 1) * 7];
                assert_eq!(record, xref.get(id).unwrap().encode_for_xref_stream(&[1, 4, 2]));
            }
            assert_eq!(tail[begin], 0, "object 0 must have a type-0 record");
        }
    }
}

#[test]
fn no_reusable_entries() {
    for encoding in ENCODINGS {
        let mut doc = input(&[(1, 0)], &[], 2, false);
        let (bytes, loaded) = save(&mut doc, encoding);
        verify(&bytes, &loaded, encoding, &[], &[(1, 0)]);
    }
}

#[test]
fn single_free_preserves_generation_and_high_identity() {
    for encoding in ENCODINGS {
        let mut doc = input(&[(1, 0)], &[(2, 99, 7)], 3, false);
        let (bytes, loaded) = save(&mut doc, encoding);
        verify(&bytes, &loaded, encoding, &[(2, 7)], &[(1, 0)]);
    }
}

#[test]
fn rebuilds_noncontiguous_free_chain_and_ignores_malformed_links() {
    for encoding in ENCODINGS {
        // Source links include a cycle, a live target, and an out-of-range target.
        let mut doc = input(
            &[(1, 0), (4, 0)],
            &[(2, 2, 7), (3, 4, 9), (5, 999, 0), (6, 2, 65535)],
            7,
            false,
        );
        let (bytes, loaded) = save(&mut doc, encoding);
        verify(
            &bytes,
            &loaded,
            encoding,
            &[(2, 7), (3, 9), (5, 0), (6, 65535)],
            &[(1, 0), (4, 0)],
        );
    }
}

#[test]
fn unrepresented_gaps_have_generation_zero() {
    for encoding in ENCODINGS {
        let mut doc = input(&[(1, 0), (4, 0)], &[], 5, false);
        let max_id = doc.max_id;
        let (bytes, loaded) = save(&mut doc, encoding);
        verify(&bytes, &loaded, encoding, &[(2, 0), (3, 0)], &[(1, 0), (4, 0)]);
        if matches!(encoding, Encoding::Table) {
            assert_eq!(doc.max_id, max_id);
            assert_eq!(doc.new_object_id(), (max_id + 1, 0));
        }
    }
}

#[test]
fn removed_normal_increments_once_and_saturates() {
    for encoding in ENCODINGS {
        let mut doc = input(&[(1, 0), (2, 7), (3, 65534), (4, 65535)], &[], 5, false);
        for id in [(2, 7), (3, 65534), (4, 65535)] {
            doc.objects.remove(&id).unwrap();
        }
        let (bytes, mut loaded) = save(&mut doc, encoding);
        let expected = [(2, 8), (3, 65535), (4, 65535)];
        verify(&bytes, &loaded, encoding, &expected, &[(1, 0)]);
        // A second table rewrite must preserve the now-effective free generations.
        loaded.reference_table.cross_reference_type = lopdf::xref::XrefType::CrossReferenceTable;
        let (bytes, reloaded) = save(&mut loaded, Encoding::Table);
        for &(id, generation) in &expected {
            assert!(
                matches!(reloaded.reference_table.get(id), Some(XrefEntry::Free { generation: actual, .. }) if *actual == generation)
            );
        }
        assert!(!bytes.is_empty());
    }
}

#[test]
fn removed_compressed_member_becomes_generation_one() {
    let mut source = input(&[(1, 0), (2, 0)], &[], 3, false);
    let (_, source) = save(&mut source, Encoding::ObjectStreams);
    assert!(matches!(
        source.reference_table.get(2),
        Some(XrefEntry::Compressed { .. })
    ));
    for encoding in ENCODINGS {
        let mut doc = source.clone();
        doc.objects.remove(&(2, 0)).unwrap();
        let (bytes, loaded) = save(&mut doc, encoding);
        assert!(matches!(
            loaded.reference_table.get(2),
            Some(XrefEntry::Free { generation: 1, .. })
        ));
        // Existing serialization may omit old container/xref objects too.
        let expected: Vec<_> = (1..=loaded.reference_table.max_id())
            .filter(|&id| {
                !matches!(
                    loaded.reference_table.get(id),
                    Some(XrefEntry::Normal { .. } | XrefEntry::Compressed { .. })
                )
            })
            .map(|id| {
                let generation = match source.reference_table.get(id).unwrap() {
                    XrefEntry::Compressed { .. } => 1,
                    XrefEntry::Normal { generation, .. } => generation.saturating_add(1),
                    other => panic!("unexpected prior entry: {other:?}"),
                };
                (id, generation)
            })
            .collect();
        verify(&bytes, &loaded, encoding, &expected, &[(1, 0)]);
    }
}

#[test]
fn effective_free_over_stale_physical_object_stays_free() {
    for encoding in ENCODINGS {
        let mut doc = input(&[(1, 0)], &[(2, 1, 7)], 3, true);
        assert!(doc.get_object((2, 0)).is_err());
        let (bytes, loaded) = save(&mut doc, encoding);
        assert!(
            !bytes
                .windows(b"stale-physical-payload".len())
                .any(|s| s == b"stale-physical-payload")
        );
        verify(&bytes, &loaded, encoding, &[(2, 7)], &[(1, 0)]);
    }
}

#[test]
fn explicit_high_free_survives_but_trailer_capacity_does_not_expand_output() {
    for encoding in ENCODINGS {
        let mut doc = input(&[(1, 0)], &[(4, 88, 7)], 1000, false);
        assert_eq!(doc.reference_table.size, 1000);
        assert_eq!(doc.max_id, 4);
        let (bytes, loaded) = save(&mut doc, encoding);
        verify(&bytes, &loaded, encoding, &[(2, 0), (3, 0), (4, 7)], &[(1, 0)]);
        assert_eq!(
            loaded.reference_table.max_id(),
            match encoding {
                Encoding::Table => 4,
                Encoding::Stream => 5,
                Encoding::ObjectStreams => 6,
            }
        );
        let mut no_free = input(&[(1, 0)], &[], 1000, false);
        let (bytes, loaded) = save(&mut no_free, encoding);
        verify(&bytes, &loaded, encoding, &[], &[(1, 0)]);
        assert!(loaded.reference_table.max_id() <= 3);
    }
}

#[test]
fn unusable_free_shorthand_is_excluded_from_chain() {
    for encoding in ENCODINGS {
        let mut doc = input(&[(1, 0)], &[(2, 99, 7)], 3, false);
        doc.reference_table.entries.insert(2, XrefEntry::UnusableFree);
        let (bytes, loaded) = save(&mut doc, encoding);
        verify(&bytes, &loaded, encoding, &[(2, 65535)], &[(1, 0)]);
    }
}
