//! yrs and Yjs disagree when text is split inside a surrogate pair (an emoji). Yjs replaces each
//! half with U+FFFD and keeps both sides' UTF-16 length; yrs 0.28 keeps the whole character on the
//! left, so the halves' lengths no longer match their clocks and the document is corrupted.
//!
//!     cargo run --example surrogate_split

use yrs::updates::decoder::Decode;
use yrs::{Doc, GetString, OffsetKind, Options, ReadTxn, StateVector, Text, Transact, Update};

fn doc() -> Doc {
    Doc::with_options(Options {
        offset_kind: OffsetKind::Utf16,
        ..Options::default()
    })
}

fn main() {
    let a = doc();
    let text = a.get_or_insert_text("body");
    text.insert(&mut a.transact_mut(), 0, "a🎉b");
    // UTF-16 offset 2 falls between the emoji's two units: Yjs allows it.
    text.insert(&mut a.transact_mut(), 2, "x");
    let state = a.transact().encode_state_as_update_v1(&StateVector::default());
    let b = doc();
    b.transact_mut().apply_update(Update::decode_v1(&state).unwrap()).unwrap();
    let (ta, tb) = (text.get_string(&a.transact()), b.get_or_insert_text("body").get_string(&b.transact()));
    let len = |s: &str| s.encode_utf16().count();
    println!("author: {ta:?} ({} UTF-16 units, {} counted by yrs)", len(&ta), text.len(&a.transact()));
    println!("replica: {tb:?} ({} UTF-16 units)", len(&tb));
    println!("Yjs would read: \"a\u{FFFD}x\u{FFFD}b\" (5 units)");
    assert_eq!(tb, "a\u{FFFD}x\u{FFFD}b", "yrs should split a surrogate pair like Yjs does");
}
