use lopdf::{Document, Error, LoadOptions};

fn assert_authority_lost(pdf: &[u8]) {
    for error in [
        Document::load_mem(pdf).unwrap_err(),
        Document::load_metadata_mem(pdf).unwrap_err(),
    ] {
        assert!(matches!(error, Error::ReconstructionAuthority { .. }), "{error:?}");
        assert!(std::error::Error::source(&error).is_some());
    }
}

/// A locally displaced pointer is recoverable because the actual table still
/// supplies Free/live state and generations; no physical index is synthesized.
#[test]
fn single_revision_local_pointer_damage_preserves_authoritative_recovery() {
    let mut pdf = new_pdf();
    let offsets = append_objects(&mut pdf, &sample_bodies());
    let xref = pdf.len();
    append_xref_trailer(&mut pdf, &offsets, "<< /Size 5 /Root 1 0 R >>", xref + 4);
    let document = Document::load_mem(&pdf).unwrap();
    assert_eq!(document.xref_start, xref);
    assert_eq!(document.get_pages().len(), 1);
    assert_eq!(Document::load_metadata_mem(&pdf).unwrap().page_count, 1);
    assert_eq!(
        Document::new_from_prev(&document)
            .trailer
            .get(b"Prev")
            .unwrap()
            .as_i64()
            .unwrap(),
        xref as i64
    );
}

/// Even without incremental updates, missing xref state cannot be inferred
/// from physical headers. Malformed entry reconstruction is unsupported.
#[test]
fn malformed_single_revision_xref_fails_closed() {
    let mut pdf = new_pdf();
    append_objects(&mut pdf, &sample_bodies());
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 5\nunreadable entries\ntrailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
            .as_bytes(),
    );
    assert_authority_lost(&pdf);
}

/// Append objects, returning each one's `(number, physical offset)`.
fn append_objects(pdf: &mut Vec<u8>, bodies: &[(u32, u16, String)]) -> Vec<(u32, usize)> {
    let mut offsets = Vec::new();
    for (number, generation, body) in bodies {
        offsets.push((*number, pdf.len()));
        pdf.extend_from_slice(format!("{number} {generation} obj\n{body}\nendobj\n").as_bytes());
    }
    offsets
}

/// Append a cross-reference table and trailer whose `startxref` records
/// `startxref_value` (tests pass broken values to require fail-closed loading).
fn append_xref_trailer(pdf: &mut Vec<u8>, offsets: &[(u32, usize)], trailer: &str, startxref_value: usize) {
    pdf.extend_from_slice(b"xref\n");
    pdf.extend_from_slice(format!("0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
    for (_, offset) in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(format!("trailer\n{trailer}\nstartxref\n{startxref_value}\n%%EOF\n").as_bytes());
}

/// One-page document whose content stream embeds object-like decoy tokens
/// (`99 88 objx`, `1 2 objects`) that must never become markers.
fn sample_bodies() -> Vec<(u32, u16, String)> {
    let contents = b"BT /F1 12 Tf 20 100 Td (Hello World) Tj (99 88 objx) Tj (1 2 objects) Tj ET";
    vec![
        (1, 0, "<< /Type /Catalog /Pages 2 0 R >>".to_string()),
        (2, 0, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string()),
        (
            3,
            0,
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R >>".to_string(),
        ),
        (
            4,
            0,
            format!(
                "<< /Length {} >>\nstream\n{}\nendstream",
                contents.len(),
                std::str::from_utf8(contents).unwrap()
            ),
        ),
    ]
}

fn new_pdf() -> Vec<u8> {
    b"%PDF-1.4\n".to_vec()
}

#[test]
fn startxref_past_eof_does_not_authorize_physical_objects() {
    let mut pdf = new_pdf();
    let offsets = append_objects(&mut pdf, &sample_bodies());
    let broken_startxref = pdf.len() + 4096;
    append_xref_trailer(&mut pdf, &offsets, "<< /Size 5 /Root 1 0 R >>", broken_startxref);

    assert_authority_lost(&pdf);
}

#[test]
fn strict_mode_does_not_reconstruct() {
    let mut pdf = new_pdf();
    let offsets = append_objects(&mut pdf, &sample_bodies());
    let broken_startxref = pdf.len() + 4096;
    append_xref_trailer(&mut pdf, &offsets, "<< /Size 5 /Root 1 0 R >>", broken_startxref);
    let strict = LoadOptions {
        strict: true,
        ..Default::default()
    };

    assert!(Document::load_mem_with_options(&pdf, strict).is_err());
}

#[test]
fn startxref_pointing_into_a_stream_fails_closed() {
    // Pointer lands mid-payload, beyond the correction window.
    let mut pdf = new_pdf();
    let offsets = append_objects(&mut pdf, &sample_bodies());
    let payload = pdf.windows(7).position(|window| window == b"stream\n").unwrap() + 7;
    append_xref_trailer(&mut pdf, &offsets, "<< /Size 5 /Root 1 0 R >>", payload + 30);

    assert_authority_lost(&pdf);
}

#[test]
fn reconstruction_rejects_duplicated_revision_objects() {
    // A second revision redefines object 2; its startxref is corrupted.
    let mut pdf = new_pdf();
    let mut offsets = append_objects(&mut pdf, &sample_bodies());
    let first_startxref = pdf.len();
    append_xref_trailer(&mut pdf, &offsets, "<< /Size 5 /Root 1 0 R >>", first_startxref);

    let revised_pages = "<< /Type /Pages /Kids [3 0 R] /Count 42 >>";
    let revision_offset = pdf.len();
    pdf.extend_from_slice(format!("2 0 obj\n{revised_pages}\nendobj\n").as_bytes());
    offsets.retain(|(number, _)| *number != 2);
    offsets.push((2, revision_offset));
    let broken_startxref = pdf.len() + 4096;
    append_xref_trailer(&mut pdf, &offsets, "<< /Size 5 /Root 1 0 R >>", broken_startxref);

    assert_authority_lost(&pdf);
}

#[test]
fn reconstruction_does_not_skip_an_unusable_latest_trailer() {
    // A trailing bogus trailer references absent catalog object 77.
    let mut pdf = new_pdf();
    let offsets = append_objects(&mut pdf, &sample_bodies());
    let broken_startxref = pdf.len() + 4096;
    append_xref_trailer(&mut pdf, &offsets, "<< /Size 5 /Root 1 0 R >>", broken_startxref);
    pdf.extend_from_slice(b"trailer\n<< /Size 12 /Root 77 0 R >>\n%%EOF\n");

    assert_authority_lost(&pdf);
}

#[test]
fn reconstruction_fails_when_no_trailer_has_a_usable_root() {
    let mut pdf = new_pdf();
    append_objects(&mut pdf, &sample_bodies());
    pdf.extend_from_slice(b"trailer\n<< /Size 5 >>\n");
    let broken_startxref = pdf.len() + 4096;
    append_xref_trailer(&mut pdf, &[], "", broken_startxref);

    assert!(Document::load_mem(&pdf).is_err());
}

/// A one-page document whose uncompressed content stream embeds whole
/// object-header lines that must never be scanned as markers.
fn sample_bodies_with_decoy_stream() -> Vec<(u32, u16, String)> {
    let contents = b"(1 0 obj)\nBT ET\n5 0 obj";
    let mut bodies = sample_bodies();
    bodies[3] = (
        4,
        0,
        format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            contents.len(),
            std::str::from_utf8(contents).unwrap()
        ),
    );
    bodies
}

#[test]
fn stream_decoys_do_not_permit_reconstruction() {
    // The decoys sit at line starts inside the payload; the `(1 0 obj)` one
    // must not overwrite the genuine catalog entry for object 1.
    let mut pdf = new_pdf();
    let offsets = append_objects(&mut pdf, &sample_bodies_with_decoy_stream());
    let broken_startxref = pdf.len() + 4096;
    append_xref_trailer(&mut pdf, &offsets, "<< /Size 6 /Root 1 0 R >>", broken_startxref);

    assert_authority_lost(&pdf);
}

#[test]
fn reconstruction_rejects_absurd_object_numbers() {
    // A comment forging a huge object header would inflate size/max_id via
    // Xref::insert and eventually overflow new_object_id().
    let mut pdf = new_pdf();
    let offsets = append_objects(&mut pdf, &sample_bodies());
    pdf.extend_from_slice(b"%4294967290 0 obj\n");
    let broken_startxref = pdf.len() + 4096;
    append_xref_trailer(&mut pdf, &offsets, "<< /Size 5 /Root 1 0 R >>", broken_startxref);

    assert_authority_lost(&pdf);
}

#[test]
fn duplicate_physical_copies_do_not_establish_authority() {
    // Two revisions of every object: 8 markers, but the highest number is 4.
    let mut pdf = new_pdf();
    let mut offsets = append_objects(&mut pdf, &sample_bodies());
    offsets.extend(append_objects(&mut pdf, &sample_bodies()));
    let broken_startxref = pdf.len() + 4096;
    append_xref_trailer(&mut pdf, &offsets, "<< /Size 9 /Root 1 0 R >>", broken_startxref);

    assert_authority_lost(&pdf);
}

/// Append an `/ObjStm` container holding `members` (object number + body).
fn append_objstm(pdf: &mut Vec<u8>, number: u32, members: &[(u32, &[u8])]) {
    use std::io::Write as _;

    use flate2::Compression;
    use flate2::write::ZlibEncoder;

    let mut index = String::new();
    let mut body: Vec<u8> = Vec::new();
    for (id, data) in members {
        index.push_str(&format!("{id} {}\n", body.len()));
        body.extend_from_slice(data);
    }
    let first = index.len();
    let content = [index.as_bytes(), body.as_slice()].concat();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&content).unwrap();
    let compressed = encoder.finish().unwrap();

    pdf.extend_from_slice(
        format!(
            "{number} 0 obj\n<< /Type /ObjStm /N {} /First {first} /Length {} /Filter /FlateDecode >>\nstream\n",
            members.len(),
            compressed.len()
        )
        .as_bytes(),
    );
    pdf.extend_from_slice(&compressed);
    pdf.extend_from_slice(b"\nendstream\nendobj\n");
}

#[test]
fn physical_object_stream_members_do_not_establish_authority() {
    // Physical member headers cannot supply effective Compressed entries.
    let mut pdf = new_pdf();
    let pages = b"<< /Type /Pages /Kids [6 0 R] /Count 1 >>";
    let page = b"<< /Type /Page /Parent 5 0 R /MediaBox [0 0 200 200] >>";
    let info = b"<< /Type /Info /Producer (x) >>";
    let mut offsets = append_objects(&mut pdf, &[(1, 0, "<< /Type /Catalog /Pages 5 0 R >>".to_string())]);
    let objstm_offset = pdf.len();
    append_objstm(&mut pdf, 2, &[(5, pages), (6, page), (7, info)]);
    offsets.push((2, objstm_offset));
    let broken_startxref = pdf.len() + 4096;
    append_xref_trailer(&mut pdf, &offsets, "<< /Size 8 /Root 1 0 R >>", broken_startxref);

    assert_authority_lost(&pdf);
}

/// Catalog, pages, and page bodies without the content stream object.
fn page_tree_bodies() -> Vec<(u32, u16, String)> {
    vec![
        (1, 0, "<< /Type /Catalog /Pages 2 0 R >>".to_string()),
        (2, 0, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string()),
        (
            3,
            0,
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R >>".to_string(),
        ),
    ]
}

#[test]
fn unterminated_stream_does_not_permit_reconstruction() {
    // Objects after a damaged stream cannot establish revision authority.
    let mut pdf = new_pdf();
    pdf.extend_from_slice(b"4 0 obj\n<< /Length 5 >>\nstream\nBT ET\nendstrXXm\nendobj\n");
    let offsets = append_objects(&mut pdf, &page_tree_bodies());
    let broken_startxref = pdf.len() + 4096;
    append_xref_trailer(&mut pdf, &offsets, "<< /Size 5 /Root 1 0 R >>", broken_startxref);

    assert_authority_lost(&pdf);
}

#[test]
fn stream_length_hint_does_not_permit_reconstruction() {
    // A length hint and parseable objects cannot replace an unresolved xref.
    let mut pdf = new_pdf();
    let mut offsets = append_objects(&mut pdf, &page_tree_bodies());
    let payload = b"1 0 obj\n<< /Bogus true >>\nendobj\n";
    let fourth_offset = pdf.len();
    pdf.extend_from_slice(format!("4 0 obj\n<< /Length {} >>\nstream\n", payload.len()).as_bytes());
    pdf.extend_from_slice(payload);
    pdf.extend_from_slice(b"endstrXXm\nendobj\n");
    offsets.push((4, fourth_offset));
    let broken_startxref = pdf.len() + 4096;
    append_xref_trailer(&mut pdf, &offsets, "<< /Size 5 /Root 1 0 R >>", broken_startxref);

    assert_authority_lost(&pdf);
}

#[test]
fn previous_objects_length_hint_does_not_permit_reconstruction() {
    // Neither a neighboring length nor physical objects establish authority.
    let mut pdf = new_pdf();
    let fifth_offset = pdf.len();
    pdf.extend_from_slice(b"5 0 obj\n<< /Length 300 >>\nstream\nBT ET\nendstream\nendobj\n");
    let fourth_offset = pdf.len();
    pdf.extend_from_slice(b"4 0 obj\n<< /Type /XObject >>\nstream\nPAYLOAD\nendstrXXm\nendobj\n");
    let mut offsets = append_objects(&mut pdf, &page_tree_bodies());
    offsets.push((4, fourth_offset));
    offsets.push((5, fifth_offset));
    let broken_startxref = pdf.len() + 4096;
    append_xref_trailer(&mut pdf, &offsets, "<< /Size 6 /Root 1 0 R >>", broken_startxref);

    assert_authority_lost(&pdf);
}

#[test]
fn indented_object_headers_do_not_establish_authority() {
    // Real generators indent object headers; blanks between the line start
    // and the digits must not hide the marker.
    let mut pdf = new_pdf();
    let first_offset = pdf.len();
    pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let second_offset = pdf.len();
    pdf.extend_from_slice(b" 2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let third_offset = pdf.len();
    pdf.extend_from_slice(b"\t3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>\nendobj\n");
    let offsets = vec![(1, first_offset), (2, second_offset), (3, third_offset)];
    let broken_startxref = pdf.len() + 4096;
    append_xref_trailer(&mut pdf, &offsets, "<< /Size 4 /Root 1 0 R >>", broken_startxref);

    assert_authority_lost(&pdf);
}

#[test]
fn unresolved_xref_cannot_seed_an_incremental_update() {
    // An unresolved xref cannot produce a document usable as a revision base.
    let mut pdf = new_pdf();
    let offsets = append_objects(&mut pdf, &sample_bodies());
    let broken_startxref = pdf.len() + 4096;
    append_xref_trailer(&mut pdf, &offsets, "<< /Size 5 /Root 1 0 R >>", broken_startxref);
    assert_authority_lost(&pdf);
}
