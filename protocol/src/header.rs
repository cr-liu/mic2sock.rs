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

    /// Milliseconds since the Unix epoch named by this header.
    ///
    /// `ms` is signed on the wire, so the sum is computed in `i64` and clamped at
    /// zero — the sender never emits a negative `ms` (it borrows from `secs`
    /// instead), but a parser must not underflow on one. This is the one place
    /// that clamping rule lives; before it existed, two callers had invented two
    /// different rules.
    pub fn epoch_ms(&self) -> u64 {
        (self.secs as i64 * 1000 + self.ms as i64).max(0) as u64
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

/// The sender's successor for a wire packet id.
///
/// `mic2sock`'s `process_send_buf` walks `0, 1, …, i32::MAX - 1` and then returns
/// to 0 — `i32::MAX` itself never appears on the wire — so the successor of
/// `i32::MAX - 1` is `0`. This one wire fact was once written out independently in
/// four places; if the Pi ever changes its reset point, this is the only line to
/// touch. Wrapping add, so an (invalid) `i32::MAX` input cannot panic a debug
/// build in the middle of validating a malformed stream.
pub fn next_pkt_id(id: i32) -> i32 {
    let n = id.wrapping_add(1);
    if n == i32::MAX {
        0
    } else {
        n
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
    /// Every field must stay nonzero and asymmetric under byte reversal:
    /// - Nonzero, because a zero field is indistinguishable from a field that
    ///   was never written at all -- an implementation that writes only one
    ///   byte of a multi-byte field would still pass with a zero-initialized
    ///   buffer and a zero expected value. `pkt_id` in particular used to be
    ///   `7` (LE bytes `[07, 00, 00, 00]`), which is exactly what an
    ///   implementation writing only `buf[8]` would also produce, and that
    ///   mutant also survives `roundtrip` above (`-5i32` sign-extends the same
    ///   way as an `i8`). `pkt_id` is now `0x1234_5678`, which needs all four
    ///   bytes to be correct.
    /// - Asymmetric under byte reversal, because a byte-palindrome value (like
    ///   `-1i16`, `0xFFFF`) reads the same regardless of endianness, so a
    ///   swapped-endianness bug would slip through undetected.
    #[test]
    fn golden_bytes() {
        let h = Header {
            device_id: 0xABCD,
            secs: 0x1122_3344,
            ms: 0x1234,
            pkt_id: 0x1234_5678,
        };
        let mut buf = [0u8; HEADER_LEN];
        h.write_to(&mut buf);
        assert_eq!(
            buf,
            [0xCD, 0xAB, 0x44, 0x33, 0x22, 0x11, 0x34, 0x12, 0x78, 0x56, 0x34, 0x12]
        );
    }

    /// Parses a hand-written golden buffer, independently of `write_to`, so an
    /// error shared by both directions cannot hide.
    #[test]
    fn parses_golden_bytes() {
        let buf = [
            0xCD, 0xAB, 0x44, 0x33, 0x22, 0x11, 0x34, 0x12, 0x78, 0x56, 0x34, 0x12,
        ];
        assert_eq!(
            Header::parse(&buf),
            Some(Header {
                device_id: 0xABCD,
                secs: 0x1122_3344,
                ms: 0x1234,
                pkt_id: 0x1234_5678,
            })
        );
    }

    #[test]
    fn the_wire_id_wraps_at_i32_max() {
        assert_eq!(next_pkt_id(0), 1);
        assert_eq!(next_pkt_id(i32::MAX - 2), i32::MAX - 1);
        assert_eq!(next_pkt_id(i32::MAX - 1), 0, "i32::MAX never appears");
    }

    #[test]
    fn epoch_ms_combines_and_clamps() {
        let mut h = Header {
            device_id: 1,
            secs: 100,
            ms: 250,
            pkt_id: 0,
        };
        assert_eq!(h.epoch_ms(), 100_250);
        // Negative ms never comes off the wire, but must not underflow if it does.
        h.secs = 0;
        h.ms = -5;
        assert_eq!(h.epoch_ms(), 0);
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
