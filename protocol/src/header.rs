/// Header length. **Must be 12** — the byte offsets are hardcoded in
/// `mic2sock::process_send_buf`, and the `H12` struct in `asio_client.cpp`
/// depends on it.
pub const HEADER_LEN: usize = 12;

/// The 12-byte little-endian packet header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub device_id: u16,
    pub secs: u32,
    pub ms: i16,
    pub pkt_id: i32,
}

impl Header {
    /// Writes into the first `HEADER_LEN` bytes of `buf`.
    ///
    /// # Panics
    /// Panics if `buf.len() < HEADER_LEN`. That is a programming error rather
    /// than bad runtime input, so it fails loudly per this project's convention.
    pub fn write_to(&self, buf: &mut [u8]) {
        buf[0..2].copy_from_slice(&self.device_id.to_le_bytes());
        buf[2..6].copy_from_slice(&self.secs.to_le_bytes());
        buf[6..8].copy_from_slice(&self.ms.to_le_bytes());
        buf[8..12].copy_from_slice(&self.pkt_id.to_le_bytes());
    }

    /// Parses the first `HEADER_LEN` bytes of `buf`; returns `None` if too short.
    ///
    /// Note the receive side's `header_len` may exceed 12 (`tcp_receiver.header_len`
    /// defaults to 16), but these four fields always live at 0..12. Where the
    /// payload starts is `PacketLayout::header_len`'s business, not this function's.
    pub fn parse(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_LEN {
            return None;
        }
        Some(Header {
            device_id: u16::from_le_bytes(buf[0..2].try_into().unwrap()),
            secs: u32::from_le_bytes(buf[2..6].try_into().unwrap()),
            ms: i16::from_le_bytes(buf[6..8].try_into().unwrap()),
            pkt_id: i32::from_le_bytes(buf[8..12].try_into().unwrap()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let h = Header {
            device_id: 3,
            secs: 1_700_000_000,
            ms: 250,
            pkt_id: -5,
        };
        let mut buf = [0u8; HEADER_LEN];
        h.write_to(&mut buf);
        assert_eq!(Header::parse(&buf), Some(h));
    }

    /// Golden-bytes test: pins the wire format. If this fails, the closed-source
    /// consumer on the Windows box receives malformed packets. No refactor may
    /// change it.
    ///
    /// The field values must stay nonzero and asymmetric under byte reversal: a
    /// zero field is indistinguishable from a field that was never written at
    /// all, and a byte-palindrome value (like `-1i16`, `0xFFFF`) reads the same
    /// regardless of endianness, so either kind of value would let a real
    /// wire-format bug slip through this test undetected.
    #[test]
    fn golden_bytes() {
        let h = Header {
            device_id: 0xABCD,
            secs: 0x1122_3344,
            ms: 0x1234,
            pkt_id: 7,
        };
        let mut buf = [0u8; HEADER_LEN];
        h.write_to(&mut buf);
        assert_eq!(
            buf,
            [0xCD, 0xAB, 0x44, 0x33, 0x22, 0x11, 0x34, 0x12, 0x07, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn parse_rejects_short_buffer() {
        assert_eq!(Header::parse(&[0u8; HEADER_LEN - 1]), None);
    }

    #[test]
    fn parse_ignores_trailing_payload() {
        let h = Header {
            device_id: 1,
            secs: 2,
            ms: 3,
            pkt_id: 4,
        };
        let mut buf = vec![0u8; HEADER_LEN + 100];
        h.write_to(&mut buf);
        assert_eq!(Header::parse(&buf), Some(h));
    }
}
