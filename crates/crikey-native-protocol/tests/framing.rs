//! Red-first tests for length-delimited native protocol frames (spec 12.4).

use std::io::{Cursor, Read};

use crikey_native_protocol::{
    frame::{read_frame, write_frame},
    ProtocolError, MAX_FRAME_BYTES,
};

#[test]
fn write_and_read_frame_round_trip() {
    let payload = b"native protocol payload";
    let mut encoded = Vec::new();
    write_frame(&mut encoded, payload).expect("a bounded payload is writable");

    assert_eq!(&encoded[..4], &(payload.len() as u32).to_be_bytes());
    let mut reader = Cursor::new(encoded);
    let mut decoded = Vec::new();
    read_frame(&mut reader, &mut decoded).expect("a complete frame is readable");
    assert_eq!(decoded, payload);
}

#[test]
fn write_rejects_payload_above_max_frame_bytes() {
    let payload = vec![0_u8; MAX_FRAME_BYTES + 1];
    let mut output = Vec::new();
    let result = write_frame(&mut output, &payload);
    assert!(matches!(
        result,
        Err(ProtocolError::FrameTooLarge(size)) if size == MAX_FRAME_BYTES + 1
    ));
    assert!(output.is_empty(), "rejected payload must not emit a prefix");
}

#[test]
fn read_rejects_declared_oversize_without_consuming_body() {
    let declared = (MAX_FRAME_BYTES as u32).saturating_add(1);
    let mut stream = declared.to_be_bytes().to_vec();
    stream.extend_from_slice(b"body-must-remain-unread");
    let mut reader = Cursor::new(stream);
    let mut buffer = vec![1, 2, 3];

    let result = read_frame(&mut reader, &mut buffer);
    assert!(matches!(
        result,
        Err(ProtocolError::FrameTooLarge(size)) if size == MAX_FRAME_BYTES + 1
    ));
    assert_eq!(reader.position(), 4, "oversize rejection consumed the body");
    assert!(buffer.is_empty(), "failed frame must leave no stale payload");
}

#[test]
fn clean_eof_at_frame_boundary_is_closed() {
    let mut reader = Cursor::new(Vec::<u8>::new());
    let mut buffer = Vec::new();
    assert!(matches!(
        read_frame(&mut reader, &mut buffer),
        Err(ProtocolError::Closed)
    ));

    let mut encoded = Vec::new();
    write_frame(&mut encoded, b"one").expect("frame write");
    let mut reader = Cursor::new(encoded);
    read_frame(&mut reader, &mut buffer).expect("first frame");
    assert_eq!(buffer, b"one");
    assert!(matches!(
        read_frame(&mut reader, &mut buffer),
        Err(ProtocolError::Closed)
    ));
}

#[test]
fn truncated_prefix_or_body_is_malformed() {
    for stream in [vec![0_u8, 0, 0], vec![0, 0, 0, 5, 1, 2]] {
        let mut reader = Cursor::new(stream);
        let mut buffer = Vec::new();
        let result = read_frame(&mut reader, &mut buffer);
        assert!(
            matches!(result, Err(ProtocolError::Malformed(_))),
            "unexpected result for truncated frame: {result:?}"
        );
    }
}

#[test]
fn frame_reader_clears_previous_payload_before_reading() {
    let mut encoded = Vec::new();
    write_frame(&mut encoded, b"next").expect("frame write");
    let mut reader = Cursor::new(encoded);
    let mut buffer = b"stale".to_vec();
    read_frame(&mut reader, &mut buffer).expect("frame read");
    assert_eq!(buffer, b"next");

    let mut eof = Cursor::new(Vec::<u8>::new());
    let result = read_frame(&mut eof, &mut buffer);
    assert!(matches!(result, Err(ProtocolError::Closed)));
    assert!(buffer.is_empty(), "EOF left a stale frame payload");
}

#[test]
fn zero_length_frame_is_malformed() {
    let mut reader = Cursor::new(0_u32.to_be_bytes());
    let mut buffer = Vec::new();
    assert!(matches!(
        read_frame(&mut reader, &mut buffer),
        Err(ProtocolError::Malformed(message)) if message.contains("zero-length")
    ));
    assert!(buffer.is_empty());
}

#[test]
fn writer_rejects_zero_length_frame() {
    let mut output = Vec::new();
    assert!(matches!(
        write_frame(&mut output, &[]),
        Err(ProtocolError::Malformed(message)) if message.contains("zero-length")
    ));
    assert!(output.is_empty());
}

struct OneByteReader(Cursor<Vec<u8>>);

impl Read for OneByteReader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.0.read(&mut bytes[..1])
    }
}

#[test]
fn frame_reader_resumes_partial_prefix_and_body_reads() {
    let mut encoded = Vec::new();
    write_frame(&mut encoded, b"partial reads are safe").expect("frame write");
    let mut reader = OneByteReader(Cursor::new(encoded));
    let mut decoded = Vec::new();
    read_frame(&mut reader, &mut decoded).expect("partial reads must be resumed");
    assert_eq!(decoded, b"partial reads are safe");
}
