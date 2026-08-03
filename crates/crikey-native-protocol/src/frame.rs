//! Length-delimited transport framing (spec 12.4).

use std::io::{self, Read, Write};

use crate::{ProtocolError, MAX_FRAME_BYTES};

fn map_io(error: io::Error) -> ProtocolError {
    if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) {
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

/// Reads one frame, clearing `buffer` before touching the input.
///
/// The declared size is checked before allocation or body reads, so rejecting
/// an oversized peer consumes exactly its four-byte prefix (spec 12.4).
pub fn read_frame<R: Read>(reader: &mut R, buffer: &mut Vec<u8>) -> Result<(), ProtocolError> {
    buffer.clear();
    let mut prefix = [0_u8; 4];
    let mut read = 0_usize;
    while read < prefix.len() {
        match reader.read(&mut prefix[read..]) {
            Ok(0) if read == 0 => return Err(ProtocolError::Closed),
            Ok(0) => {
                return Err(ProtocolError::Malformed(
                    "truncated frame length prefix".to_owned(),
                ))
            }
            Ok(count) => {
                let remaining = prefix
                    .len()
                    .checked_sub(read)
                    .ok_or_else(|| ProtocolError::Malformed("frame prefix offset overflow".to_owned()))?;
                if count > remaining {
                    return Err(ProtocolError::Malformed(
                        "reader returned too many prefix bytes".to_owned(),
                    ));
                }
                read = read
                    .checked_add(count)
                    .ok_or_else(|| ProtocolError::Malformed("frame prefix offset overflow".to_owned()))?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(map_io(error)),
        }
    }

    let length = usize::try_from(u32::from_be_bytes(prefix))
        .map_err(|_| ProtocolError::Malformed("frame length does not fit this platform".to_owned()))?;
    if length == 0 {
        return Err(ProtocolError::Malformed("zero-length frame".to_owned()));
    }
    if length > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(length));
    }
    buffer
        .try_reserve_exact(length)
        .map_err(|_| ProtocolError::Malformed("frame allocation failed".to_owned()))?;
    buffer.resize(length, 0);
    let mut offset = 0_usize;
    while offset < length {
        match reader.read(&mut buffer[offset..]) {
            Ok(0) => {
                buffer.clear();
                return Err(ProtocolError::Malformed("truncated frame body".to_owned()));
            }
            Ok(count) => {
                let remaining = length
                    .checked_sub(offset)
                    .ok_or_else(|| ProtocolError::Malformed("frame body offset overflow".to_owned()))?;
                if count > remaining {
                    buffer.clear();
                    return Err(ProtocolError::Malformed(
                        "reader returned too many body bytes".to_owned(),
                    ));
                }
                offset = offset
                    .checked_add(count)
                    .ok_or_else(|| ProtocolError::Malformed("frame body offset overflow".to_owned()))?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                buffer.clear();
                return Err(map_io(error));
            }
        }
    }
    Ok(())
}
