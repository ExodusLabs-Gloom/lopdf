//! `SaveOptions::use_xref_streams` on its own, without object streams.
//!
//! The two features are independent in the PDF specification, so asking for a
//! cross-reference stream should not require asking for object streams as well.

#![cfg(not(feature = "async"))]

use lopdf::xref::XrefType;
use lopdf::{Document, Object, ObjectStreamConfig, SaveOptions, Stream, dictionary};

/// A one page document written with a classic cross-reference table, as a document
/// loaded from a pre-1.5 file would be.
fn sample_document() -> Document {
    let mut doc = Document::with_version("1.4");
    doc.reference_table.cross_reference_type = XrefType::CrossReferenceTable;

    let pages_id = doc.new_object_id();
    let content_id = doc.add_object(Stream::new(dictionary! {}, b"BT ET".to_vec()));
    let page_id = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => Object::Reference(pages_id),
        "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
        "Contents" => Object::Reference(content_id),
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));

    doc
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

#[test]
fn xref_streams_are_written_without_object_streams() {
    let mut doc = sample_document();

    let options = SaveOptions::builder()
        .use_object_streams(false)
        .use_xref_streams(true)
        .build();
    let mut buffer = Vec::new();
    doc.save_with_options(&mut buffer, options).unwrap();

    assert!(
        contains(&buffer, b"/Type/XRef") || contains(&buffer, b"/Type /XRef"),
        "a requested cross-reference stream should be written even with object streams off"
    );
    assert!(
        !contains(&buffer, b"\nxref\n"),
        "the classic cross-reference table should not be written as well"
    );
    assert!(
        !contains(&buffer, b"/ObjStm"),
        "object streams were not requested and must not appear"
    );

    // A cross-reference stream is a PDF 1.5 construct, so the header has to say so.
    assert!(
        buffer.starts_with(b"%PDF-1.5"),
        "expected the version to be raised to 1.5, got {:?}",
        String::from_utf8_lossy(&buffer[..8.min(buffer.len())])
    );

    let mut reloaded = Document::load_mem(&buffer).unwrap();
    assert_eq!(reloaded.get_pages().len(), 1, "the saved document should still load");
    assert!(
        reloaded
            .get_dictionary(reloaded.trailer.get(b"Root").unwrap().as_reference().unwrap())
            .unwrap()
            .has(b"Type")
    );

    // Save/reload stability: the same options applied to the reloaded document
    // must produce another readable file.
    let mut second_buffer = Vec::new();
    reloaded
        .save_with_options(
            &mut second_buffer,
            SaveOptions::builder().use_xref_streams(true).build(),
        )
        .unwrap();
    let reloaded_again = Document::load_mem(&second_buffer).unwrap();
    assert_eq!(reloaded_again.get_pages().len(), 1, "the second save should still load");
}

#[test]
fn cross_reference_table_is_kept_when_xref_streams_are_not_requested() {
    let mut doc = sample_document();

    let options = SaveOptions::builder()
        .use_object_streams(false)
        .use_xref_streams(false)
        .build();
    let mut buffer = Vec::new();
    doc.save_with_options(&mut buffer, options).unwrap();

    assert!(
        contains(&buffer, b"\nxref\n"),
        "without the option the document keeps its cross-reference table"
    );
    assert!(
        !contains(&buffer, b"/Type/XRef") && !contains(&buffer, b"/Type /XRef"),
        "no cross-reference stream should appear when it was not requested"
    );
    assert!(
        !contains(&buffer, b"/ObjStm"),
        "object streams were not requested and must not appear"
    );
    assert!(
        buffer.starts_with(b"%PDF-1.4"),
        "the version should be left alone when no 1.5 feature is used"
    );

    let reloaded = Document::load_mem(&buffer).unwrap();
    assert_eq!(
        reloaded.get_pages().len(),
        1,
        "catalog and page must survive the round trip"
    );
}

#[test]
fn both_options_together_still_write_object_and_xref_streams() {
    let mut doc = sample_document();

    let options = SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(true)
        .build();
    let mut buffer = Vec::new();
    doc.save_with_options(&mut buffer, options.clone()).unwrap();

    assert!(
        contains(&buffer, b"/ObjStm"),
        "object streams were requested and should still be written"
    );
    assert!(
        contains(&buffer, b"/Type/XRef") || contains(&buffer, b"/Type /XRef"),
        "cross-reference streams were requested and should still be written"
    );
    assert!(
        !contains(&buffer, b"\nxref\n"),
        "no classic cross-reference table should appear next to the stream"
    );

    // Compressed objects remain live: the members packed into the object stream
    // must come back as ordinary objects after a reload.
    let mut reloaded = Document::load_mem(&buffer).unwrap();
    assert_eq!(
        reloaded.get_pages().len(),
        1,
        "catalog and page must survive the round trip"
    );
    let catalog = reloaded
        .get_dictionary(reloaded.trailer.get(b"Root").unwrap().as_reference().unwrap())
        .unwrap();
    assert_eq!(
        catalog.get(b"Type").unwrap(),
        &Object::Name(b"Catalog".to_vec()),
        "the catalog packed into an object stream must still be reachable"
    );

    let mut second_buffer = Vec::new();
    reloaded.save_with_options(&mut second_buffer, options).unwrap();
    let reloaded_again = Document::load_mem(&second_buffer).unwrap();
    assert_eq!(reloaded_again.get_pages().len(), 1, "the second save should still load");
}

/// `use_object_streams` with a classic cross-reference table would serialize every
/// live object stream member as an unusable free entry, silently corrupting the
/// file. The save must fail before anything is written or modified.
#[test]
fn object_streams_with_classic_xref_are_rejected_before_any_output() {
    let mut doc = sample_document();

    let version_before = doc.version.clone();
    let xref_type_before = doc.reference_table.cross_reference_type;
    let max_id_before = doc.max_id;
    let trailer_before = doc.trailer.clone();
    let objects_before = doc.objects.clone();

    let options = SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(false)
        .build();
    let mut buffer = Vec::new();
    let error = doc
        .save_with_options(&mut buffer, options)
        .expect_err("classic cross-reference output cannot carry object stream members");

    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert!(
        buffer.is_empty(),
        "a rejected save must not write a single byte to the target"
    );

    assert_eq!(
        doc.version, version_before,
        "the version must not be raised before failing"
    );
    assert_eq!(
        matches!(doc.reference_table.cross_reference_type, XrefType::CrossReferenceTable),
        matches!(xref_type_before, XrefType::CrossReferenceTable),
        "the cross-reference type must not change before failing"
    );
    assert_eq!(doc.max_id, max_id_before, "max_id must not grow before failing");
    assert_eq!(doc.trailer, trailer_before, "the trailer must not be modified");
    assert_eq!(doc.objects, objects_before, "the objects must not be modified");
}

/// `use_xref_streams = false` preserves the document's current cross-reference
/// representation. A document already on cross-reference streams therefore keeps
/// them, with object streams enabled.
#[test]
fn existing_xref_stream_source_stays_on_xref_streams_when_not_forced() {
    let mut doc = sample_document();
    doc.version = "1.5".to_string();
    doc.reference_table.cross_reference_type = XrefType::CrossReferenceStream;

    let options = SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(false)
        .build();
    let mut buffer = Vec::new();
    doc.save_with_options(&mut buffer, options.clone()).unwrap();

    assert!(
        contains(&buffer, b"/ObjStm"),
        "object streams were requested and the cross-reference stream can carry them"
    );
    assert!(
        contains(&buffer, b"/Type/XRef") || contains(&buffer, b"/Type /XRef"),
        "the existing cross-reference stream representation must be preserved, not demoted to a table"
    );
    assert!(
        !contains(&buffer, b"\nxref\n"),
        "no classic cross-reference table should appear"
    );

    let mut reloaded = Document::load_mem(&buffer).unwrap();
    assert_eq!(
        reloaded.get_pages().len(),
        1,
        "catalog and page must survive the round trip"
    );

    let mut second_buffer = Vec::new();
    reloaded.save_with_options(&mut second_buffer, options).unwrap();
    let reloaded_again = Document::load_mem(&second_buffer).unwrap();
    assert_eq!(reloaded_again.get_pages().len(), 1, "the second save should still load");
}

/// The rejection keys off actual compressibility, not the raw flag: with no object
/// eligible for an object stream, a classic save stays allowed and unchanged.
#[test]
fn object_streams_without_eligible_objects_keep_classic_table() {
    let mut doc = Document::with_version("1.4");
    doc.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
    // Only stream objects: nothing an object stream can hold.
    doc.objects
        .insert((1, 0), Object::Stream(Stream::new(dictionary! {}, b"BT ET".to_vec())));
    doc.max_id = 1;

    let options = SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(false)
        .build();
    let mut buffer = Vec::new();
    doc.save_with_options(&mut buffer, options).unwrap();

    assert!(
        contains(&buffer, b"\nxref\n"),
        "with nothing to compress the classic cross-reference table is kept"
    );
    assert!(
        !contains(&buffer, b"/ObjStm"),
        "no object stream can be built from stream-only content"
    );

    let reloaded = Document::load_mem(&buffer).unwrap();
    assert!(
        reloaded
            .reference_table
            .entries
            .values()
            .all(|entry| !entry.is_compressed()),
        "a classic output cannot contain compressed entries"
    );
    assert_eq!(
        reloaded.get_object((1, 0)).unwrap(),
        &Object::Stream(Stream::new(dictionary! {}, b"BT ET".to_vec())),
        "the only object must survive the round trip"
    );
}

/// Zero capacity is malformed input whenever an object stream would actually be
/// built: every insertion into the stream would fail. The builder never produces
/// this configuration (zero is normalized to the default), so reaching it means
/// the public struct fields were constructed directly.
#[test]
fn object_streams_with_direct_zero_capacity_are_rejected_for_classic_output() {
    let mut doc = sample_document();

    let version_before = doc.version.clone();
    let xref_type_before = doc.reference_table.cross_reference_type;
    let max_id_before = doc.max_id;
    let trailer_before = doc.trailer.clone();
    let objects_before = doc.objects.clone();

    let options = SaveOptions {
        use_object_streams: true,
        use_xref_streams: false,
        object_stream_config: ObjectStreamConfig {
            max_objects_per_stream: 0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buffer = Vec::new();
    let error = doc
        .save_with_options(&mut buffer, options)
        .expect_err("a zero-capacity object stream cannot hold any object");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(buffer.is_empty(), "a rejected save must not write a single byte");
    assert_eq!(
        doc.version, version_before,
        "the version must not change before failing"
    );
    assert_eq!(
        matches!(doc.reference_table.cross_reference_type, XrefType::CrossReferenceTable),
        matches!(xref_type_before, XrefType::CrossReferenceTable),
        "the cross-reference type must not change before failing"
    );
    assert_eq!(doc.max_id, max_id_before, "max_id must not grow before failing");
    assert_eq!(doc.trailer, trailer_before, "the trailer must not be modified");
    assert_eq!(doc.objects, objects_before, "the objects must not be modified");
}

/// The reviewer's reproduction: a directly constructed zero-capacity
/// configuration with a cross-reference stream used to be accepted, emitting
/// empty object streams while the objects selected for compression were silently
/// dropped from direct serialization. It must fail before anything is written.
#[test]
fn object_streams_with_direct_zero_capacity_are_rejected_for_xref_stream_output() {
    let mut doc = sample_document();

    let version_before = doc.version.clone();
    let xref_type_before = doc.reference_table.cross_reference_type;
    let max_id_before = doc.max_id;
    let trailer_before = doc.trailer.clone();
    let objects_before = doc.objects.clone();

    let options = SaveOptions {
        use_object_streams: true,
        use_xref_streams: true,
        object_stream_config: ObjectStreamConfig {
            max_objects_per_stream: 0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buffer = Vec::new();
    let error = doc
        .save_with_options(&mut buffer, options)
        .expect_err("a zero-capacity object stream cannot hold any object");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(buffer.is_empty(), "a rejected save must not write a single byte");
    // Reaching the rejection through the cross-reference-stream path must not
    // have promoted the document along the way.
    assert_eq!(
        doc.version, version_before,
        "the version must not be raised before failing"
    );
    assert_eq!(
        matches!(doc.reference_table.cross_reference_type, XrefType::CrossReferenceStream),
        matches!(xref_type_before, XrefType::CrossReferenceStream),
        "the cross-reference type must not change before failing"
    );
    assert_eq!(doc.max_id, max_id_before, "max_id must not grow before failing");
    assert_eq!(doc.trailer, trailer_before, "the trailer must not be modified");
    assert_eq!(doc.objects, objects_before, "the objects must not be modified");
}

/// The builder normalizes zero to its default capacity, so a builder-produced
/// configuration stays valid even though a directly constructed zero is not.
#[test]
fn builder_zero_capacity_is_normalized_and_saves_with_object_streams() {
    let mut doc = sample_document();

    let options = SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(true)
        .max_objects_per_stream(0)
        .build();

    let mut buffer = Vec::new();
    doc.save_with_options(&mut buffer, options).unwrap();

    assert!(
        contains(&buffer, b"/ObjStm"),
        "the builder-normalized capacity must still produce object streams"
    );
    let reloaded = Document::load_mem(&buffer).unwrap();
    assert_eq!(
        reloaded.get_pages().len(),
        1,
        "the builder-zero save must produce a readable document"
    );
}

/// Configuration is only validated when it would be used: with nothing eligible
/// for compression, a directly constructed zero capacity is never exercised.
#[test]
fn zero_capacity_without_eligible_objects_keeps_classic_table() {
    let mut doc = Document::with_version("1.4");
    doc.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
    // Only stream objects: nothing an object stream can hold.
    doc.objects
        .insert((1, 0), Object::Stream(Stream::new(dictionary! {}, b"BT ET".to_vec())));
    doc.max_id = 1;

    let options = SaveOptions {
        use_object_streams: true,
        use_xref_streams: false,
        object_stream_config: ObjectStreamConfig {
            max_objects_per_stream: 0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buffer = Vec::new();
    doc.save_with_options(&mut buffer, options).unwrap();

    assert!(
        contains(&buffer, b"\nxref\n"),
        "with nothing to compress the classic cross-reference table is kept"
    );
    assert!(
        !contains(&buffer, b"/ObjStm"),
        "no object stream can be built from stream-only content"
    );
    let reloaded = Document::load_mem(&buffer).unwrap();
    assert_eq!(
        reloaded.get_object((1, 0)).unwrap(),
        &Object::Stream(Stream::new(dictionary! {}, b"BT ET".to_vec())),
        "the only object must survive the round trip"
    );
}
