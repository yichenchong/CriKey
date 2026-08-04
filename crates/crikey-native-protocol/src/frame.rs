//! Length-delimited transport framing (spec 12.4).

use std::io::{self, Read, Write};

use crate::{ProtocolError, MAX_FRAME_BYTES};

fn map_io(error: io::Error) -> ProtocolError {
    if error.kind() == io::ErrorKind::BrokenPipe {
        ProtocolError::Closed
    } else if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) {
        ProtocolError::Timeout
    } else {
        ProtocolError::Io(error.to_string())
    }
}

/// Writes a 4-byte big-endian length prefix followed by one bounded payload.
pub fn write_frame<W: Write>(writer: &mut W, payload: &[u8]) -> Result<(), ProtocolError> {
    if payload.is_empty() {
        return Err(ProtocolError::Malformed("zero-length frame".to_owned()));
    }
    if payload.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(payload.len()));
    }
    let length = u32::try_from(payload.len()).map_err(|_| ProtocolError::FrameTooLarge(payload.len()))?;
    writer.write_all(&length.to_be_bytes()).map_err(map_io)?;
    writer.write_all(payload).map_err(map_io)?;
    writer.flush().map_err(map_io)
}

/// Stateful reader for one length-delimited frame stream.
///
/// A timeout can happen after a peer has sent only part of a prefix or body.
/// Keep this value with the stream and call [`FrameReader::read_frame`] again
/// to resume without treating the remaining bytes as a new frame prefix.
#[derive(Debug, Default)]
pub struct FrameReader {
    prefix: [u8; 4],
    prefix_read: usize,
    payload: Vec<u8>,
    payload_read: usize,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    fn reset(&mut self) {
        self.prefix_read = 0;
        self.payload.clear();
        self.payload_read = 0;
    }

    /// Reads one frame into `buffer`, preserving partial state across
    /// [`ProtocolError::Timeout`].
    pub fn read_frame<R: Read>(&mut self, reader: &mut R, buffer: &mut Vec<u8>) -> Result<(), ProtocolError> {
        buffer.clear();
        while self.prefix_read < self.prefix.len() {
            match reader.read(&mut self.prefix[self.prefix_read..]) {
                Ok(0) if self.prefix_read == 0 => return Err(ProtocolError::Closed),
                Ok(0) => {
                    self.reset();
                    return Err(ProtocolError::Malformed(
                        "truncated frame length prefix".to_owned(),
                    ));
                }
                Ok(count) => {
                    let remaining = self.prefix.len().checked_sub(self.prefix_read).ok_or_else(|| {
                        self.reset();
                        ProtocolError::Malformed("frame prefix offset overflow".to_owned())
                    })?;
                    if count > remaining {
                        self.reset();
                        return Err(ProtocolError::Malformed(
                            "reader returned too many prefix bytes".to_owned(),
                        ));
                    }
                    self.prefix_read = self.prefix_read.checked_add(count).ok_or_else(|| {
                        self.reset();
                        ProtocolError::Malformed("frame prefix offset overflow".to_owned())
                    })?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let mapped = map_io(error);
                    if !matches!(mapped, ProtocolError::Timeout) {
                        self.reset();
                    }
                    return Err(mapped);
                }
            }
        }

        if self.payload.is_empty() {
            let length = match usize::try_from(u32::from_be_bytes(self.prefix)) {
                Ok(length) => length,
                Err(_) => {
                    self.reset();
                    return Err(ProtocolError::Malformed(
                        "frame length does not fit this platform".to_owned(),
                    ));
                }
            };
            if length == 0 {
                self.reset();
                return Err(ProtocolError::Malformed("zero-length frame".to_owned()));
            }
            if length > MAX_FRAME_BYTES {
                self.reset();
                return Err(ProtocolError::FrameTooLarge(length));
            }
            self.payload.try_reserve_exact(length).map_err(|_| {
                self.reset();
                ProtocolError::Malformed("frame allocation failed".to_owned())
            })?;
            self.payload.resize(length, 0);
        }

        while self.payload_read < self.payload.len() {
            match reader.read(&mut self.payload[self.payload_read..]) {
                Ok(0) => {
                    self.reset();
                    return Err(ProtocolError::Malformed("truncated frame body".to_owned()));
                }
                Ok(count) => {
                    let remaining = self.payload.len().checked_sub(self.payload_read).ok_or_else(|| {
                        self.reset();
                        ProtocolError::Malformed("frame body offset overflow".to_owned())
                    })?;
                    if count > remaining {
                        self.reset();
                        return Err(ProtocolError::Malformed(
                            "reader returned too many body bytes".to_owned(),
                        ));
                    }
                    self.payload_read = self.payload_read.checked_add(count).ok_or_else(|| {
                        self.reset();
                        ProtocolError::Malformed("frame body offset overflow".to_owned())
                    })?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let mapped = map_io(error);
                    if !matches!(mapped, ProtocolError::Timeout) {
                        self.reset();
                    }
                    return Err(mapped);
                }
            }
        }

        std::mem::swap(buffer, &mut self.payload);
        self.reset();
        Ok(())
    }
}

/// Reads one frame with a fresh state.
///
/// Callers that may retry after [`ProtocolError::Timeout`] must retain a
/// [`FrameReader`] instead; this convenience function is for one-shot reads.
pub fn read_frame<R: Read>(reader: &mut R, buffer: &mut Vec<u8>) -> Result<(), ProtocolError> {
    FrameReader::new().read_frame(reader, buffer)
}
