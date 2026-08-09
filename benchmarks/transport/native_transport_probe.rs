//! Standalone measurement probe for the native plugin transport (ADR-0004
//! trigger: "shared memory ... added only if profiling proves local IPC
//! insufficient", spec 16.8).
//!
//! It is deliberately NOT a workspace member and NOT a cargo target. The
//! reference host for this project has two cores and 7.4 GB of RAM, and a
//! workspace-wide cargo build is the single most expensive thing that can
//! happen on it. This file therefore compiles on its own, in about a second,
//! with no dependencies, while still measuring the *real* code:
//! `crates/crikey-native-protocol/src/{wire,message,frame}.rs` are included
//! verbatim through `#[path]` module declarations. Only the crate-root items
//! those files import (`MAX_FRAME_BYTES`, `ProtocolError`, `Message`) are
//! restated below, behaviourally identical to
//! `crates/crikey-native-protocol/src/lib.rs`.
//!
//! # How to rerun
//!
//! ```text
//! rustc -O --edition 2021 -o /tmp/native_transport_probe \
//!     benchmarks/transport/native_transport_probe.rs
//! /tmp/native_transport_probe            # default: 500,000 items
//! /tmp/native_transport_probe 50000      # smaller run
//! ```
//!
//! # What it measures
//!
//! The synthetic catalog is the one from `benchmarks/src/lib.rs`
//! (`synthetic_catalog`): same labels, same descriptions, same targets, same
//! `ItemId::derived` spelling, so the encoded bytes per item are the bytes the
//! real 500k harness would put on the wire.
//!
//! 1. `encode`   - protobuf encoding of `Envelope{CatalogBatch{items}}`.
//! 2. `frames`   - how many `MAX_FRAME_BYTES`-bounded frames a 500k catalog needs.
//! 3. `socket`   - the shipping transport: `frame::write_frame` into a
//!    `UnixStream` pair, `frame::FrameReader::read_frame` out of it, on two
//!    threads, transfer only (no decode).
//! 4. `shm`      - the proposed transport's data plane: one `MAP_SHARED`
//!    region in `/dev/shm`, `memcpy` in, a blocking doorbell over a
//!    socketpair (no spinning), `memcpy` out, blocking credit return.
//! 5. `decode`   - protobuf decoding of the received frames.
//! 6. `per-frame` - the same socket path driven with 4 KiB frames to separate
//!    fixed per-frame cost (two syscalls, prefix handling) from per-byte cost.
//!
//! The transport is the only variable between 3 and 4: both carry identical
//! bytes and both are followed by the identical decode in 5. That is exactly
//! the comparison ADR-0004's deferral trigger asks for.

#![allow(dead_code, unused_imports, unused_macros)]

use std::io::{Read, Write};
use std::os::raw::c_void;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

// --- crate-root items restated from crates/crikey-native-protocol/src/lib.rs ---

pub const MAX_FRAME_BYTES: usize = wire::DECODE_ALLOCATION_BUDGET;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    FrameTooLarge(usize),
    DecodeBudgetExceeded { requested: usize, remaining: usize },
    UnsupportedVersion(u32),
    Malformed(String),
    Closed,
    Io(String),
    Timeout,
    Rejected(String),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for ProtocolError {}

pub trait Message: Sized + std::fmt::Debug {
    fn encode(&self) -> Vec<u8>;
    fn decode(bytes: &[u8]) -> Result<Self, ProtocolError>;
}

// --- the real protocol source, included verbatim ---

#[path = "../../crates/crikey-native-protocol/src/wire.rs"]
pub mod wire;
#[path = "../../crates/crikey-native-protocol/src/message.rs"]
pub mod message;
#[path = "../../crates/crikey-native-protocol/src/frame.rs"]
pub mod frame;

use message::{CatalogBatch, Envelope, Item, Payload};

// --- synthetic catalog, mirroring benchmarks/src/lib.rs ---

const ADJECTIVES: [&str; 16] = [
    "Amber", "Basalt", "Cobalt", "Dusty", "Ember", "Frost", "Granite", "Harbor", "Indigo", "Jasper",
    "Kelp", "Lumen", "Marble", "Nimbus", "Onyx", "Pewter",
];
const NOUNS: [&str; 16] = [
    "Archive", "Browser", "Composer", "Dashboard", "Editor", "Finder", "Gallery", "Hub", "Inspector",
    "Journal", "Kanban", "Ledger", "Monitor", "Notebook", "Organizer", "Planner",
];
const BENCHMARK_PLUGIN: &str = "crikey.benchmarks";

fn derived_id(target: &str) -> String {
    let mut encoded = String::new();
    for component in [
        "crikey-derived-item-v1",
        BENCHMARK_PLUGIN,
        "application",
        "",
        target,
    ] {
        encoded.push_str(&format!("{}:", component.len()));
        encoded.push_str(component);
    }
    encoded
}

fn synthetic_item(index: usize) -> Item {
    let target = format!("/synthetic/app-{index:06}");
    let adjective = ADJECTIVES[index % ADJECTIVES.len()];
    let noun = NOUNS[(index / ADJECTIVES.len()) % NOUNS.len()];
    let label = format!("{adjective} {noun} {index:06}");
    let description = format!(
        "{} utility from the {} collection",
        NOUNS[(index + 7) % NOUNS.len()],
        ADJECTIVES[(index + 11) % ADJECTIVES.len()]
    );
    Item {
        stable_id: derived_id(&target),
        label: label.clone(),
        description,
        target,
        category: "application".to_owned(),
        search_terms: vec![label],
        icon_reference: String::new(),
        score_hint: 0,
        metadata: std::collections::BTreeMap::new(),
        actions: Vec::new(),
        argument_policy: "forbidden".to_owned(),
        hit_policy: "recorded".to_owned(),
        unknown: wire::UnknownFields::default(),
    }
}

fn envelope_bytes(items: &[Item]) -> Vec<u8> {
    let batch = CatalogBatch {
        items: items.to_vec(),
        done: false,
        sequence: 1,
        error: None,
        unknown: wire::UnknownFields::default(),
    };
    let envelope = Envelope {
        connection_id: 1,
        request_id: 1,
        generation: 0,
        deadline_ms: 0,
        payload: Some(Payload::CatalogBatch(batch)),
        unknown: wire::UnknownFields::default(),
    };
    envelope.encode()
}

/// Largest catalog batch that both fits inside `MAX_FRAME_BYTES` *and*
/// survives `wire::DECODE_ALLOCATION_BUDGET` on the receiving side, found by
/// binary search over the real codec.
///
/// These two bounds are not the same, and the smaller one wins. The frame cap
/// bounds input bytes; the decode budget separately bounds the heap charged
/// while materialising repeated fields, and an item costs far more budget
/// than it costs wire bytes. A batch sized only against `MAX_FRAME_BYTES`
/// encodes fine and then fails to decode.
///
/// Producers no longer have to discover this by measurement:
/// `message::max_decodable_items` reports it directly, and this search is
/// what `main` checks that report against.
fn largest_decodable_batch() -> usize {
    let decodes = |count: usize| -> bool {
        let batch: Vec<Item> = (0..count).map(synthetic_item).collect();
        let bytes = envelope_bytes(&batch);
        bytes.len() <= MAX_FRAME_BYTES && Envelope::decode(&bytes).is_ok()
    };
    let mut low = 1usize;
    let mut high = 1usize;
    while decodes(high) && high < 1_000_000 {
        low = high;
        high *= 2;
    }
    while low + 1 < high {
        let mid = low + (high - low) / 2;
        if decodes(mid) {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

/// What the shipped protocol tells a producer it may put in one batch.
fn permitted_batch(measured: usize) -> usize {
    let items: Vec<Item> = (0..measured * 2).map(synthetic_item).collect();
    message::max_decodable_items(&items)
}

/// Splits the catalog into the largest batches the shipping stack can
/// actually carry end to end.
fn encode_frames(items: usize, per_batch: usize) -> (Vec<Vec<u8>>, Duration) {
    let started = Instant::now();
    let mut frames = Vec::new();
    let mut index = 0usize;
    while index < items {
        let end = (index + per_batch).min(items);
        let batch: Vec<Item> = (index..end).map(synthetic_item).collect();
        let bytes = envelope_bytes(&batch);
        assert!(
            bytes.len() <= MAX_FRAME_BYTES,
            "batch sizing must respect the frame cap"
        );
        frames.push(bytes);
        index = end;
    }
    let elapsed = started.elapsed();
    (frames, elapsed)
}

// --- transport A: the shipping length-delimited socket path ---

fn measure_socket(frames: &[Vec<u8>]) -> Duration {
    let (mut host, mut plugin) = UnixStream::pair().expect("socketpair");
    let outgoing: Vec<Vec<u8>> = frames.to_vec();
    let started = Instant::now();
    let writer = std::thread::spawn(move || {
        for frame in &outgoing {
            frame::write_frame(&mut plugin, frame).expect("write_frame");
        }
    });
    let mut reader = frame::FrameReader::new();
    let mut buffer = Vec::new();
    let mut received = 0usize;
    for _ in 0..frames.len() {
        reader.read_frame(&mut host, &mut buffer).expect("read_frame");
        received += buffer.len();
    }
    writer.join().expect("writer thread");
    let elapsed = started.elapsed();
    let expected: usize = frames.iter().map(Vec::len).sum();
    assert_eq!(received, expected, "socket transfer must be byte-complete");
    elapsed
}

// --- transport B: shared region + blocking doorbell ---

#[link(name = "c")]
extern "C" {
    fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> i32;
}

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 1;

struct Region {
    base: *mut u8,
    len: usize,
    _file: std::fs::File,
    path: std::path::PathBuf,
}

impl Region {
    fn create(len: usize) -> Region {
        let path = std::path::PathBuf::from(format!(
            "/dev/shm/crikey-transport-probe-{}",
            std::process::id()
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .expect("shm file");
        file.set_len(len as u64).expect("shm size");
        // SAFETY: a null hint with a valid fd and a non-zero length is the
        // documented mmap contract; the result is checked below.
        let base = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        assert!(base as isize != -1, "mmap failed");
        Region {
            base: base.cast::<u8>(),
            len,
            _file: file,
            path,
        }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: `base`/`len` are the exact values returned by the mmap above.
        unsafe {
            munmap(self.base.cast::<c_void>(), self.len);
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

struct RegionRef(*mut u8);
// SAFETY: exactly one writer thread holds this, and its access to the region
// is serialized against the reader by the blocking doorbell and credit
// sockets: the writer never touches the region between signalling and
// receiving the credit back, and the reader never touches it outside that
// window.
unsafe impl Send for RegionRef {}

fn measure_shared_memory(frames: &[Vec<u8>]) -> Duration {
    let region = Region::create(MAX_FRAME_BYTES);
    let (mut host_bell, mut plugin_bell) = UnixStream::pair().expect("doorbell socketpair");
    let outgoing: Vec<Vec<u8>> = frames.to_vec();
    let write_base = RegionRef(region.base);
    let read_base = region.base;
    let count = frames.len();

    let started = Instant::now();
    let writer = std::thread::spawn(move || {
        let base = write_base;
        let mut credit = [0u8; 1];
        for frame in &outgoing {
            // SAFETY: the region is MAX_FRAME_BYTES long and every frame is
            // bounded by that cap; the reader is parked on the doorbell.
            unsafe {
                std::ptr::copy_nonoverlapping(frame.as_ptr(), base.0, frame.len());
            }
            let header = (frame.len() as u64).to_be_bytes();
            plugin_bell.write_all(&header).expect("doorbell");
            plugin_bell.read_exact(&mut credit).expect("credit");
        }
    });

    let mut buffer = vec![0u8; MAX_FRAME_BYTES];
    let mut header = [0u8; 8];
    let mut received = 0usize;
    for _ in 0..count {
        // Blocking read on the doorbell: the reader parks in the kernel until
        // the writer publishes, exactly as the socket transport parks today.
        host_bell.read_exact(&mut header).expect("doorbell read");
        let len = u64::from_be_bytes(header) as usize;
        assert!(len <= MAX_FRAME_BYTES, "record length must be bounded");
        // SAFETY: `len` is bounded by the region length by the check above.
        unsafe {
            std::ptr::copy_nonoverlapping(read_base, buffer.as_mut_ptr(), len);
        }
        received += len;
        host_bell.write_all(&[1u8]).expect("credit write");
    }
    writer.join().expect("writer thread");
    let elapsed = started.elapsed();
    let expected: usize = frames.iter().map(Vec::len).sum();
    assert_eq!(received, expected, "shared-memory transfer must be byte-complete");
    drop(region);
    elapsed
}

// --- decode ---

fn measure_decode(frames: &[Vec<u8>]) -> (Duration, usize) {
    let started = Instant::now();
    let mut items = 0usize;
    for frame in frames {
        let envelope = Envelope::decode(frame).expect("decode");
        match envelope.payload {
            Some(Payload::CatalogBatch(batch)) => items += batch.items.len(),
            other => panic!("unexpected payload {other:?}"),
        }
    }
    (started.elapsed(), items)
}

// --- per-frame fixed cost ---

fn measure_per_frame_cost(frame_bytes: usize, frame_count: usize) -> Duration {
    let payload = vec![0x5au8; frame_bytes];
    let frames: Vec<Vec<u8>> = (0..frame_count).map(|_| payload.clone()).collect();
    measure_socket(&frames)
}

fn mb_per_second(bytes: usize, elapsed: Duration) -> f64 {
    let seconds = elapsed.as_secs_f64();
    if seconds <= 0.0 {
        return f64::INFINITY;
    }
    (bytes as f64 / (1024.0 * 1024.0)) / seconds
}

fn main() {
    let items: usize = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(500_000);

    println!("native transport probe: {items} synthetic catalog items");
    println!("MAX_FRAME_BYTES = {MAX_FRAME_BYTES}");

    let measured = largest_decodable_batch();
    let per_batch = permitted_batch(measured);
    assert!(
        per_batch <= measured,
        "the protocol may not promise producers more than it can decode"
    );
    let (frames, encode_time) = encode_frames(items, per_batch);
    let total_bytes: usize = frames.iter().map(Vec::len).sum();
    println!();
    println!(
        "batch cap   : {per_batch} items/frame from message::max_decodable_items \
         ({measured} measured decodable; the frame cap allows far more bytes than the \
         decode allocation budget allows heap)"
    );
    println!(
        "encode      : {:>10.3} ms  ({:.1} MB/s)",
        encode_time.as_secs_f64() * 1e3,
        mb_per_second(total_bytes, encode_time)
    );
    println!(
        "wire bytes  : {total_bytes} ({:.1} B/item)",
        total_bytes as f64 / items as f64
    );
    println!(
        "frames      : {} ({:.3} MiB each)",
        frames.len(),
        frames[0].len() as f64 / (1024.0 * 1024.0)
    );

    // Three runs of each transport; report the best, which is the least
    // scheduler-polluted sample on a two-core host.
    let mut socket_best = Duration::from_secs(u64::MAX);
    let mut shm_best = Duration::from_secs(u64::MAX);
    for _ in 0..3 {
        socket_best = socket_best.min(measure_socket(&frames));
        shm_best = shm_best.min(measure_shared_memory(&frames));
    }

    println!();
    println!(
        "socket xfer : {:>10.3} ms  ({:.1} MB/s)",
        socket_best.as_secs_f64() * 1e3,
        mb_per_second(total_bytes, socket_best)
    );
    println!(
        "shm xfer    : {:>10.3} ms  ({:.1} MB/s)",
        shm_best.as_secs_f64() * 1e3,
        mb_per_second(total_bytes, shm_best)
    );
    let saved = socket_best.as_secs_f64() - shm_best.as_secs_f64();
    println!("shm saves   : {:>10.3} ms over the whole catalog", saved * 1e3);

    let (decode_time, decoded) = measure_decode(&frames);
    assert_eq!(decoded, items, "decode must recover every item");
    println!(
        "decode      : {:>10.3} ms  ({:.1} MB/s)",
        decode_time.as_secs_f64() * 1e3,
        mb_per_second(total_bytes, decode_time)
    );

    let end_to_end = encode_time + socket_best + decode_time;
    println!();
    println!(
        "end-to-end (encode + socket + decode) : {:.3} ms",
        end_to_end.as_secs_f64() * 1e3
    );
    println!(
        "transport share of end-to-end         : {:.1} %",
        100.0 * socket_best.as_secs_f64() / end_to_end.as_secs_f64()
    );
    println!(
        "best case with a perfect transport    : {:.3} ms ({:.1} % faster)",
        (encode_time + shm_best + decode_time).as_secs_f64() * 1e3,
        100.0 * saved / end_to_end.as_secs_f64()
    );

    println!();
    let small = measure_per_frame_cost(4096, 20_000);
    println!(
        "per-frame   : 20,000 x 4 KiB frames in {:.3} ms => {:.2} us/frame fixed cost",
        small.as_secs_f64() * 1e3,
        small.as_secs_f64() * 1e6 / 20_000.0
    );
    let large_count = 15usize;
    let large = measure_per_frame_cost(MAX_FRAME_BYTES - 4096, large_count);
    println!(
        "per-byte    : {} x ~8 MiB frames in {:.3} ms => {:.1} MB/s",
        large_count,
        large.as_secs_f64() * 1e3,
        mb_per_second((MAX_FRAME_BYTES - 4096) * large_count, large)
    );
}
