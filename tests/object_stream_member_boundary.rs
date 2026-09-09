use std::collections::{BTreeMap, BTreeSet};

use lopdf::{Document, Object, ObjectStreamConfig, SaveOptions, dictionary, xref::XrefEntry};

fn assert_member_round_trip(count: u32, configured_max: usize, expected_counts: &[usize]) {
    let mut document = Document::with_version("1.7");
    for ordinal in 0..count {
        document.add_object(Object::Integer(i64::from(ordinal)));
    }
    // Nonzero generations keep these structural objects out of the exact member count.
    let pages = (count + 1, 1);
    let catalog = (count + 2, 1);
    document.objects.insert(
        pages,
        dictionary! { "Type" => "Pages", "Kids" => Vec::<Object>::new(), "Count" => 0 }.into(),
    );
    document.objects.insert(
        catalog,
        dictionary! {
            "Type" => "Catalog", "Pages" => Object::Reference(pages),
            "Members" => (1..=count).map(|id| Object::Reference((id, 0))).collect::<Vec<_>>()
        }
        .into(),
    );
    document.max_id = count + 2;
    document.trailer.set("Root", Object::Reference(catalog));
    let original_max_id = document.max_id;
    let mut bytes = Vec::new();
    document
        .save_with_options(
            &mut bytes,
            SaveOptions {
                use_object_streams: true,
                use_xref_streams: true,
                object_stream_config: ObjectStreamConfig {
                    max_objects_per_stream: configured_max,
                    compression_level: 0,
                },
                ..Default::default()
            },
        )
        .unwrap();

    let loaded = Document::load_mem(&bytes).unwrap();
    let streams: BTreeMap<_, _> = loaded
        .objects
        .iter()
        .filter_map(|(&(id, generation), object)| {
            let stream = object.as_stream().ok()?;
            if !stream.dict.has_type(b"ObjStm") {
                return None;
            }
            assert_eq!(generation, 0);
            assert!(matches!(
                loaded.reference_table.get(id),
                Some(XrefEntry::Normal { generation: 0, .. })
            ));
            Some((
                id,
                usize::try_from(stream.dict.get(b"N").unwrap().as_i64().unwrap()).unwrap(),
            ))
        })
        .collect();
    assert_eq!(streams.values().copied().collect::<Vec<_>>(), expected_counts);
    assert_eq!(document.max_id, original_max_id + expected_counts.len() as u32 + 1);
    assert_eq!(loaded.max_id, document.max_id);
    assert_eq!(
        loaded.trailer.get(b"Size").unwrap().as_i64().unwrap(),
        i64::from(document.max_id) + 1
    );

    let expected_authority: Vec<_> = streams
        .iter()
        .flat_map(|(&container, &members)| {
            assert!(members <= configured_max);
            assert!(members <= 65_536);
            (0..members).map(move |index| (container, index))
        })
        .collect();
    assert_eq!(expected_authority.len(), count as usize);
    let mut coordinates = BTreeSet::new();
    let mut highest_index = 0;
    for (ordinal, &(container, index)) in expected_authority.iter().enumerate() {
        let id = ordinal as u32 + 1;
        match loaded.reference_table.get(id).unwrap() {
            XrefEntry::Compressed {
                container: actual_container,
                index: actual_index,
            } => {
                assert_eq!((*actual_container, usize::from(*actual_index)), (container, index));
                assert!(coordinates.insert((*actual_container, *actual_index)));
                highest_index = highest_index.max(usize::from(*actual_index));
            }
            other => panic!("member {id} has unexpected authority: {other:?}"),
        }
        // Check every payload as well as the first, last, and split-boundary identities.
        assert_eq!(loaded.get_object((id, 0)).unwrap(), &Object::Integer(ordinal as i64));
    }
    assert_eq!(highest_index, expected_counts.iter().max().unwrap() - 1);
    assert_eq!(
        loaded
            .reference_table
            .entries
            .values()
            .filter(|entry| matches!(entry, XrefEntry::Compressed { .. }))
            .count(),
        count as usize
    );
    assert_eq!(
        loaded
            .catalog()
            .unwrap()
            .get(b"Members")
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        count as usize
    );
}

#[test]
fn generated_member_ordinal_boundaries() {
    // Run sequentially to avoid multiplying peak memory across large fixtures.
    for (count, maximum, expected) in [
        (65_535, 65_535, vec![65_535]),
        (65_536, 65_536, vec![65_536]),
        (65_537, 65_537, vec![65_536, 1]),
        (70_000, 70_000, vec![65_536, 4_464]),
    ] {
        assert_member_round_trip(count, maximum, &expected);
    }
}

#[test]
fn smaller_requested_maximum_is_preserved() {
    assert_member_round_trip(250, 100, &[100, 100, 50]);
}
