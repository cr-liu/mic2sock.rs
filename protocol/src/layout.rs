use core::ops::Range;

/// The geometry of one packet.
///
/// Audio is **channel-blocked, not sample-interleaved**: every sample of channel
/// 0, then every sample of channel 1, and so on. Mic channels come first, the
/// resend (far-end reference) channel last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketLayout {
    /// Total channel count (mic + resend).
    pub n_ch: usize,
    /// Samples per channel per packet.
    pub spp: usize,
    /// Offset at which the payload starts. Fixed at 12 on the send side; variable
    /// on the receive side, where it defaults to 16.
    pub header_len: usize,
}

impl PacketLayout {
    pub const fn new(n_ch: usize, spp: usize, header_len: usize) -> Self {
        PacketLayout {
            n_ch,
            spp,
            header_len,
        }
    }

    /// Payload bytes for a single channel (i16 samples).
    pub const fn channel_bytes(&self) -> usize {
        self.spp * 2
    }

    /// Total packet bytes: header + n_ch * spp * 2.
    pub const fn packet_len(&self) -> usize {
        self.header_len + self.n_ch * self.spp * 2
    }

    /// Byte range of channel `ch`'s payload within the packet.
    ///
    /// # Panics
    /// Panics if `ch >= n_ch`.
    pub fn channel_range(&self, ch: usize) -> Range<usize> {
        assert!(
            ch < self.n_ch,
            "channel index {} out of range 0..{}",
            ch,
            self.n_ch
        );
        let start = self.header_len + ch * self.channel_bytes();
        start..start + self.channel_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The current production configuration: 16 mic + 1 resend, 160 samples, a
    /// 12-byte header. 5452 is hardcoded in `asio_client.cpp` and is the packet
    /// length the Windows consumer expects.
    #[test]
    fn production_packet_len_is_5452() {
        let l = PacketLayout::new(17, 160, 12);
        assert_eq!(l.packet_len(), 5452);
    }

    /// A later phase drops samples-per-packet to 32; pin that length now.
    #[test]
    fn packet_len_at_spp_32_is_1100() {
        assert_eq!(PacketLayout::new(17, 32, 12).packet_len(), 1100);
    }

    #[test]
    fn channel_ranges_are_contiguous_and_non_overlapping() {
        let l = PacketLayout::new(17, 160, 12);
        assert_eq!(l.channel_bytes(), 320);
        assert_eq!(l.channel_range(0), 12..332);
        assert_eq!(l.channel_range(1), 332..652);
        // The last channel must land exactly on the end of the packet.
        assert_eq!(l.channel_range(16), 5132..5452);
    }

    /// The receive side's header may be longer than 12; the payload start has to
    /// follow it.
    #[test]
    fn recv_layout_with_16_byte_header() {
        let l = PacketLayout::new(1, 160, 16);
        assert_eq!(l.packet_len(), 336);
        assert_eq!(l.channel_range(0), 16..336);
    }

    #[test]
    #[should_panic(expected = "channel index")]
    fn channel_range_panics_out_of_bounds() {
        PacketLayout::new(2, 160, 12).channel_range(2);
    }
}
