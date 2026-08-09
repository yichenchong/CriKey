//! The sendable ceiling and the decodable ceiling (spec 16.3, 12.4).
//!
//! `MAX_FRAME_BYTES` bounds the bytes a producer may put on the wire.
//! `wire::DECODE_ALLOCATION_BUDGET` bounds the heap the receiver will charge
//! turning those bytes back into messages. For a batch of repeated `Item`s
//! the second is far tighter than the first, and when the two were
//! independent constants a plugin could fill a frame to just under the frame
//! cap, watch it cross the wire, and be disconnected for a protocol violation
//! it did not commit.
//!
//! These tests pin the resolution: the budget is unchanged (it is the only
//! bound on what a hostile frame can allocate), the frame cap is derived from
//! it rather than restated, the producer discovers the real ceiling through
//! `message::max_decodable_items`, and a frame that exceeds the budget is
//! reported as `DecodeBudgetExceeded` rather than as a framing failure.

use std::collections::BTreeMap;

use crikey_native_protocol::message::{
    max_decodable_items, CatalogBatch, Envelope, Item, Payload, BATCH_OVERHEAD_RESERVE,
};
use crikey_native_protocol::wire::{
    RepeatedFieldCharge, UnknownFields, DECODE_ALLOCATION_BUDGET, DECODE_REPETITION_OVERHEAD,
};
use crikey_native_protocol::{Message, ProtocolError, MAX_FRAME_BYTES};

const ADJECTIVES: [&str; 4] = ["amber", "brisk", "candid", "dapper"];
const NOUNS: [&str; 4] = ["atlas", "beacon", "cipher", "delta"];

/// Same shape and roughly the same size as the catalog items ADR-0017's probe
/// measured, so the ceilings these tests compute are the shipping ones.
fn item(index: usize) -> Item {
    let target = format!("/synthetic/app-{index:06}");
    let label = format!(
        "{} {} {index:06}",
        ADJECTIVES[index % ADJECTIVES.len()],
        NOUNS[(index / ADJECTIVES.len()) % NOUNS.len()]
    );
    Item {
        stable_id: format!("22:crikey-derived-item-v1:11:application:0::{target}"),
        label: label.clone(),
        description: format!("utility {index:06} from the synthetic collection"),
        target,
        category: "application".to_owned(),
        search_terms: vec![label],
        icon_reference: String::new(),
        score_hint: 0,
        metadata: BTreeMap::new(),
        actions: Vec::new(),
        argument_policy: "forbidden".to_owned(),
        hit_policy: "recorded".to_owned(),
        unknown: UnknownFields::default(),
    }
}

fn envelope(items: Vec<Item>) -> Envelope {
    Envelope {
        connection_id: 1,
        request_id: 1,
        generation: 0,
        deadline_ms: 0,
        payload: Some(Payload::CatalogBatch(CatalogBatch {
            items,
            done: false,
            sequence: 1,
            error: None,
            unknown: UnknownFields::default(),
        })),
        unknown: UnknownFields::default(),
    }
}

fn batch_items(decoded: &Envelope) -> &[Item] {
    match &decoded.payload {
        Some(Payload::CatalogBatch(batch)) => &batch.items,
        other => panic!("a catalog batch must decode as one, got {other:?}"),
    }
}

/// Items enough to overrun any batch, so the ceiling is discovered rather
/// than assumed.
fn oversupply() -> Vec<Item> {
    (0..40_000).map(item).collect()
}

/// The defect this file exists for. A producer that fills a batch to the
/// ceiling the protocol reports must be able to send it and have it arrive.
#[test]
fn a_batch_filled_to_the_producer_ceiling_survives_encode_and_decode() {
    let items = oversupply();
    let permitted = max_decodable_items(&items);
    assert!(
        permitted > 1_000,
        "a useful ceiling carries thousands of items per frame, got {permitted}"
    );
    assert!(
        permitted < items.len(),
        "the fixture must be large enough to be truncated, or this proves nothing"
    );

    let encoded = envelope(items[..permitted].to_vec()).encode();
    assert!(
        encoded.len() <= MAX_FRAME_BYTES,
        "a batch at the decodable ceiling must also fit the wire cap: {} bytes",
        encoded.len()
    );

    let decoded = Envelope::decode(&encoded).expect(
        "a batch sized by max_decodable_items must decode; a producer has no other way to know \
         the limit",
    );
    assert_eq!(
        batch_items(&decoded).len(),
        permitted,
        "every item the producer was permitted to send must arrive"
    );
}

/// Largest count of `items` whose encoded envelope fits the wire cap. This is
/// exactly what a producer that only knows `MAX_FRAME_BYTES` would send.
fn largest_batch_within_the_frame_cap(items: &[Item]) -> usize {
    let (mut low, mut high) = (1_usize, items.len());
    assert!(
        envelope(items.to_vec()).encode().len() > MAX_FRAME_BYTES,
        "the fixture must be able to overflow the frame cap"
    );
    while low + 1 < high {
        let mid = low + (high - low) / 2;
        if envelope(items[..mid].to_vec()).encode().len() <= MAX_FRAME_BYTES {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

/// Largest count of `items` that genuinely survives encode *and* decode,
/// found by binary search over the real codec — the same oracle ADR-0017's
/// probe uses, and independent of the producer-side model.
fn largest_decodable_batch(items: &[Item]) -> usize {
    let (mut low, mut high) = (1_usize, items.len());
    while low + 1 < high {
        let mid = low + (high - low) / 2;
        let bytes = envelope(items[..mid].to_vec()).encode();
        if bytes.len() <= MAX_FRAME_BYTES && Envelope::decode(&bytes).is_ok() {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

/// The trap the resolution closes: sizing against the frame cap alone, which
/// is what a producer naturally reaches for, still produces an undecodable
/// frame — and the refusal now says so instead of blaming the framing.
#[test]
fn a_batch_sized_only_against_the_frame_cap_is_refused_by_name() {
    let items = oversupply();
    let count = largest_batch_within_the_frame_cap(&items);
    let encoded = envelope(items[..count].to_vec()).encode();
    assert!(
        encoded.len() > 4 * 1024 * 1024 && encoded.len() <= MAX_FRAME_BYTES,
        "the fixture must be a frame-cap-scale batch that the wire cap accepts, got {} bytes",
        encoded.len()
    );

    let error =
        Envelope::decode(&encoded).expect_err("a batch sized only against MAX_FRAME_BYTES is not decodable");
    match &error {
        ProtocolError::DecodeBudgetExceeded { requested, .. } => {
            assert!(
                *requested <= DECODE_ALLOCATION_BUDGET,
                "the refused allocation must itself be bounded, got {requested}"
            );
        }
        ProtocolError::FrameTooLarge(bytes) => {
            panic!("the frame cap is not what refused this: {bytes} bytes were within the wire limit")
        }
        other => panic!("the refusal must name the decode budget, got {other:?}"),
    }
    let text = error.to_string();
    assert!(
        text.contains("max_decodable_items"),
        "the diagnostic must point a producer at the limit it can query: {text}"
    );
}

/// The producer-side ceiling and the decoder's budget can no longer drift.
///
/// The model is checked in both directions against an oracle that knows
/// nothing about it: it may never promise more than the decoder accepts (or
/// the round trip above breaks), and it may not fall far short (or the
/// producer is paying for frames it did not need). Change the budget, the
/// vector growth rule, the per-repetition overhead, or the charge model, and
/// one of these two bounds gives way.
#[test]
fn the_producer_ceiling_tracks_the_real_decodable_maximum() {
    let items = oversupply();
    let permitted = max_decodable_items(&items);
    let truth = largest_decodable_batch(&items);

    assert!(
        permitted <= truth,
        "the ceiling handed to producers promises {permitted} items, but only {truth} decode"
    );
    assert!(
        truth - permitted <= truth / 20,
        "the ceiling wastes the budget: {permitted} permitted of {truth} decodable"
    );

    let mut vector = RepeatedFieldCharge::new::<Item>();
    let mut charged = 0_usize;
    for entry in &items[..permitted] {
        vector.push();
        charged += entry.decode_charge();
    }
    let modelled = charged + vector.charged();
    let ceiling = DECODE_ALLOCATION_BUDGET - BATCH_OVERHEAD_RESERVE;
    assert!(
        modelled <= ceiling,
        "the permitted batch must fit the model's own ceiling: {modelled} > {ceiling}"
    );

    // And nothing beyond the ceiling is quietly accepted.
    let excess = (permitted * 2).min(items.len());
    let error = Envelope::decode(&envelope(items[..excess].to_vec()).encode())
        .expect_err("twice the permitted batch must not decode");
    assert!(
        matches!(error, ProtocolError::DecodeBudgetExceeded { .. }),
        "the refusal must name the decode budget, got {error:?}"
    );
}

/// The wire cap is derived from the budget rather than restated, so there is
/// no second number to keep in step.
#[test]
fn the_wire_cap_never_exceeds_the_decode_budget() {
    const _: () = assert!(MAX_FRAME_BYTES <= DECODE_ALLOCATION_BUDGET);
    assert_eq!(
        MAX_FRAME_BYTES, DECODE_ALLOCATION_BUDGET,
        "MAX_FRAME_BYTES is defined as the decode budget; if this ever differs, one of them was \
         written out again by hand"
    );
}

/// A frame claiming an enormous repeated-field count is still refused, and
/// still refused before it can allocate proportionally to the claim.
#[test]
fn a_hostile_repeated_count_is_refused_with_a_bounded_allocation() {
    // Field 1 of `CatalogBatch`, length zero: two bytes on the wire for an
    // `Item` that costs hundreds of bytes of heap. A frame filled with these
    // is entirely legal bytes and entirely within the wire cap.
    let claimed = MAX_FRAME_BYTES / 2;
    let mut hostile = Vec::with_capacity(MAX_FRAME_BYTES);
    for _ in 0..claimed {
        hostile.extend_from_slice(&[0x0a, 0x00]);
    }
    assert!(hostile.len() <= MAX_FRAME_BYTES);

    let error = CatalogBatch::decode(&hostile).expect_err("a hostile repetition count is refused");
    let ProtocolError::DecodeBudgetExceeded { requested, remaining } = error else {
        panic!("the refusal must name the decode budget, got {error:?}");
    };
    assert!(
        requested <= DECODE_ALLOCATION_BUDGET,
        "the decoder must refuse a bounded step, never attempt one sized to the claim: {requested}"
    );
    assert!(
        remaining < requested,
        "the refusal must be the budget running out, not an unrelated failure"
    );

    // The claim is for millions of items; the budget cannot have paid for
    // more than this many, whatever the frame says.
    let affordable = DECODE_ALLOCATION_BUDGET / (std::mem::size_of::<Item>() + DECODE_REPETITION_OVERHEAD);
    assert!(
        affordable < claimed / 4,
        "the fixture must claim far more items than the budget can afford: {affordable} vs {claimed}"
    );
}

/// An empty batch, and a batch of one, must not fall off either end of the
/// sizing rule.
#[test]
fn sizing_degenerate_batches_makes_progress() {
    assert_eq!(max_decodable_items(&[]), 0);
    assert_eq!(max_decodable_items(&[item(0)]), 1);

    // One item too large for the whole budget cannot be carried by any batch.
    // It is handed on alone so the failure is a named refusal rather than a
    // producer that silently stops emitting.
    let mut giant = item(0);
    giant.description = "x".repeat(DECODE_ALLOCATION_BUDGET);
    assert_eq!(max_decodable_items(std::slice::from_ref(&giant)), 1);
}
