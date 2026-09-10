//! End-to-end checks that `Object::Real` values survive a document save and
//! load with their object variant and exact `f32` value.
//!
//! A PDF real number is written in plain decimal notation with a decimal point.
//! Before this, an integral value such as `1.0` was written as `1`, which the
//! reader parses as an `Integer`; a save/load cycle silently changed the object
//! type. These tests round-trip values through actual `Document` serialization
//! so both the writer token and the parser grammar are exercised.

use lopdf::{Document, Object, dictionary};

/// A deterministic grid across sign, exponent and mantissa patterns, plus
/// exact boundary bit patterns. This is a sampled sweep, not all 2^32 patterns.
fn sampled_finite_real_bits() -> Vec<u32> {
    let mut bits = Vec::new();
    for sign in [0u32, 0x8000_0000] {
        for exponent in [0u32, 1, 2, 3, 4, 7, 15, 30, 60, 100, 126, 127, 128, 150, 200, 254] {
            for mantissa in [0u32, 1, 2, 0x40_0000, 0x7f_ffff, 0x12_3456] {
                bits.push(sign | (exponent << 23) | mantissa);
            }
        }
    }
    bits.sort_unstable();
    bits.dedup();
    bits
}

fn required_values() -> Vec<f32> {
    vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        2.0,
        1.5,
        -1.5,
        0.5,
        0.1,
        f32::MIN_POSITIVE,
        f32::EPSILON,
        f32::MAX,
        -f32::MAX,
        f32::MIN,
        1.0e-20,
        1.0e-30,
        1.0e20,
        1.0e30,
        1.0e38,
        123456789.0,
        f32::from_bits(0x0000_0001),
        f32::from_bits(0x8000_0001),
        f32::from_bits(0x003f_ffff),
        f32::from_bits(0x807f_ffff),
    ]
}

fn assert_real(object: &Object, expected: f32, context: &str) {
    match object {
        Object::Real(actual) => assert_eq!(
            actual.to_bits(),
            expected.to_bits(),
            "{context}: expected {expected:?} ({:#010x}), got {actual:?} ({:#010x})",
            expected.to_bits(),
            actual.to_bits()
        ),
        other => panic!("{context}: expected Real({expected:?}), got {other:?}"),
    }
}

fn save_and_load(doc: &mut Document) -> Document {
    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    Document::load_mem(&bytes).unwrap()
}

#[test]
fn reals_round_trip_in_dictionaries_arrays_and_nested_values() {
    let values = required_values();
    let mut doc = Document::with_version("1.5");
    let direct_id = doc.add_object(Object::Real(1.0));
    let array_id = doc.add_object(Object::Array(values.iter().map(|&value| Object::Real(value)).collect()));
    let nested_id = doc.add_object(dictionary! {
        "Nested" => Object::Dictionary(dictionary! {
            "Inner" => Object::Array(vec![Object::Real(-1.0), Object::Real(0.1)]),
            "Leaf" => Object::Real(2.0),
        }),
    });
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Direct" => direct_id,
        "Values" => array_id,
        "Nested" => nested_id,
    });
    doc.trailer.set("Root", catalog_id);

    let loaded = save_and_load(&mut doc);
    assert_real(loaded.get_object(direct_id).unwrap(), 1.0, "dictionary value");

    let array = loaded.get_object(array_id).unwrap().as_array().unwrap();
    assert_eq!(array.len(), values.len());
    for (index, &expected) in values.iter().enumerate() {
        assert_real(&array[index], expected, &format!("array[{index}]"));
    }

    let nested = loaded.get_object(nested_id).unwrap().as_dict().unwrap();
    let inner = nested.get(b"Nested").unwrap().as_dict().unwrap();
    let inner_array = inner.get(b"Inner").unwrap().as_array().unwrap();
    assert_real(&inner_array[0], -1.0, "nested array[0]");
    assert_real(&inner_array[1], 0.1, "nested array[1]");
    assert_real(inner.get(b"Leaf").unwrap(), 2.0, "nested dictionary leaf");
}

#[test]
fn sampled_real_bit_patterns_round_trip_through_a_document() {
    let bits = sampled_finite_real_bits();
    assert!(bits.len() > 100, "the sample must span the sign/exponent/mantissa grid");
    let values: Vec<f32> = bits.iter().copied().map(f32::from_bits).collect();

    let mut doc = Document::with_version("1.5");
    let array_id = doc.add_object(Object::Array(values.iter().map(|&value| Object::Real(value)).collect()));
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Values" => array_id });
    doc.trailer.set("Root", catalog_id);

    let loaded = save_and_load(&mut doc);
    let array = loaded.get_object(array_id).unwrap().as_array().unwrap();
    assert_eq!(array.len(), values.len());
    for (index, &expected) in values.iter().enumerate() {
        assert_real(&array[index], expected, &format!("sampled[{index}]"));
    }
}

#[test]
fn integral_real_is_written_with_a_decimal_point() {
    let mut doc = Document::with_version("1.5");
    let real_id = doc.add_object(Object::Real(2.0));
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Value" => real_id });
    doc.trailer.set("Root", catalog_id);

    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("2.0"), "an integral Real must keep real syntax");
    assert!(
        !text.contains("\n2\n"),
        "an integral Real must not be written as an integer"
    );

    let loaded = Document::load_mem(&bytes).unwrap();
    assert_real(loaded.get_object(real_id).unwrap(), 2.0, "integral real");
}

#[test]
fn negative_zero_round_trips_with_its_sign() {
    let mut doc = Document::with_version("1.5");
    let real_id = doc.add_object(Object::Real(-0.0));
    let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Value" => real_id });
    doc.trailer.set("Root", catalog_id);

    let mut bytes = Vec::new();
    doc.save_to(&mut bytes).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("-0.0"), "negative zero must keep its sign");

    let loaded = Document::load_mem(&bytes).unwrap();
    let Object::Real(value) = loaded.get_object(real_id).unwrap() else {
        panic!("negative zero must remain a Real");
    };
    assert_eq!(value.to_bits(), (-0.0f32).to_bits());
    assert_ne!(value.to_bits(), 0.0f32.to_bits());
}

#[test]
fn nonfinite_reals_refuse_serialization() {
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut doc = Document::with_version("1.5");
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Value" => Object::Real(value),
        });
        doc.trailer.set("Root", catalog_id);

        let mut bytes = Vec::new();
        let error = doc.save_to(&mut bytes).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("NaN"), "a non-finite Real must not be written as NaN");
        assert!(
            !text.contains("inf"),
            "a non-finite Real must not be written as infinity"
        );
    }
}

#[test]
fn nonfinite_reals_nested_in_containers_refuse_serialization() {
    let mut doc = Document::with_version("1.5");
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Value" => Object::Array(vec![Object::Dictionary(dictionary! {
            "Inner" => Object::Real(f32::NAN),
        })]),
    });
    doc.trailer.set("Root", catalog_id);

    let mut bytes = Vec::new();
    let error = doc.save_to(&mut bytes).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn reals_round_trip_inside_object_streams() {
    let mut doc = Document::with_version("1.5");
    let direct_id = doc.add_object(Object::Real(1.0));
    let array_id = doc.add_object(Object::Array(vec![Object::Real(-2.0), Object::Real(0.5)]));
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Direct" => direct_id,
        "Values" => array_id,
    });
    doc.trailer.set("Root", catalog_id);

    let mut bytes = Vec::new();
    doc.save_modern(&mut bytes).unwrap();
    assert!(
        bytes.windows(b"/ObjStm".len()).any(|window| window == b"/ObjStm"),
        "the modern save should use an object stream for an integral Real"
    );

    let loaded = Document::load_mem(&bytes).unwrap();
    assert_real(loaded.get_object(direct_id).unwrap(), 1.0, "object-stream direct real");
    let array = loaded.get_object(array_id).unwrap().as_array().unwrap();
    assert_real(&array[0], -2.0, "object-stream array[0]");
    assert_real(&array[1], 0.5, "object-stream array[1]");
}
