//! Blocking twin of `vectorhawkd_mcp::backend`'s length-prefixed socket
//! framing, for the rare callers that speak to the daemon socket without a
//! tokio runtime (`peer_handshake`, used from both daemon startup — before
//! the tokio accept loop exists — and the fully synchronous CLI
//! installer).
//!
//! Must stay byte-for-byte compatible with
//! `vectorhawkd_mcp::backend::{read_framed, write_framed}`: both sides of
//! the same wire protocol, just over `std::io` instead of `tokio::io`. See
//! that module's doc comment for why the format is a 4-byte big-endian
//! length prefix followed by a UTF-8 JSON body.

use std::io::{Read, Write};

/// Write one length-prefixed frame.
pub fn write_framed_blocking<W: Write>(writer: &mut W, body: &[u8]) -> std::io::Result<()> {
    let len = body.len() as u32;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(body)?;
    writer.flush()
}

/// Read one length-prefixed frame. `Ok(None)` on clean EOF.
pub fn read_framed_blocking<R: Read>(reader: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_frame() {
        let mut buf = Vec::new();
        write_framed_blocking(&mut buf, b"hello").unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let read_back = read_framed_blocking(&mut cursor).unwrap();
        assert_eq!(read_back, Some(b"hello".to_vec()));
    }

    #[test]
    fn empty_reader_is_clean_eof() {
        let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
        let read_back = read_framed_blocking(&mut cursor).unwrap();
        assert_eq!(read_back, None);
    }

    #[test]
    fn truncated_length_prefix_reads_as_eof_not_a_panic() {
        // Matches `vectorhawkd_mcp::backend::read_framed`'s convention: any
        // `UnexpectedEof` while reading the length prefix (zero bytes or a
        // partial one) is treated as a clean EOF, never a hard error — the
        // one thing this test pins down is that it never panics.
        let mut cursor = std::io::Cursor::new(vec![0u8, 1]); // only 2 of 4 bytes
        let result = read_framed_blocking(&mut cursor);
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn truncated_body_is_a_hard_error_not_a_panic() {
        // A fully-read 4-byte length prefix claiming more body bytes than
        // are actually present must surface as an `Err`, never panic and
        // never silently return a short/garbage body.
        let mut buf = 10u32.to_be_bytes().to_vec();
        buf.extend_from_slice(b"short"); // only 5 of the claimed 10 bytes
        let mut cursor = std::io::Cursor::new(buf);
        let result = read_framed_blocking(&mut cursor);
        assert!(result.is_err());
    }
}
