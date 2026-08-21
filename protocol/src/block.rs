use crate::layout::PacketLayout;

/// Copies channel `ch`'s samples out into `out`.
///
/// # Panics
/// Panics if `out.len() != layout.spp`, if `ch` is out of range, or if `packet`
/// is too short.
pub fn deblock_channel(packet: &[u8], layout: &PacketLayout, ch: usize, out: &mut [i16]) {
    assert_eq!(out.len(), layout.spp, "out length must equal spp");
    let bytes = &packet[layout.channel_range(ch)];
    for (i, s) in out.iter_mut().enumerate() {
        *s = i16::from_le_bytes([bytes[i * 2], bytes[i * 2 + 1]]);
    }
}

/// Writes `samples` into channel `ch`'s payload range.
///
/// # Panics
/// Panics if `samples.len() != layout.spp`, if `ch` is out of range, or if
/// `packet` is too short.
pub fn reblock_channel(packet: &mut [u8], layout: &PacketLayout, ch: usize, samples: &[i16]) {
    assert_eq!(samples.len(), layout.spp, "samples length must equal spp");
    let range = layout.channel_range(ch);
    let bytes = &mut packet[range];
    for (i, s) in samples.iter().enumerate() {
        bytes[i * 2..i * 2 + 2].copy_from_slice(&s.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> PacketLayout {
        PacketLayout::new(3, 4, 12)
    }

    #[test]
    fn roundtrip_single_channel() {
        let l = layout();
        let mut pkt = vec![0u8; l.packet_len()];
        let samples: [i16; 4] = [1, -2, 3, -4];
        reblock_channel(&mut pkt, &l, 1, &samples);

        let mut out = [0i16; 4];
        deblock_channel(&pkt, &l, 1, &mut out);
        assert_eq!(out, samples);
    }

    /// Writing one channel must not corrupt its neighbours — the single easiest
    /// thing to get wrong about a channel-blocked layout.
    #[test]
    fn writing_one_channel_leaves_others_untouched() {
        let l = layout();
        let mut pkt = vec![0u8; l.packet_len()];
        reblock_channel(&mut pkt, &l, 1, &[0x7FFF; 4]);

        let mut out = [0i16; 4];
        deblock_channel(&pkt, &l, 0, &mut out);
        assert_eq!(out, [0; 4], "channel 0 was corrupted");
        deblock_channel(&pkt, &l, 2, &mut out);
        assert_eq!(out, [0; 4], "channel 2 was corrupted");
    }

    #[test]
    fn all_channels_roundtrip_independently() {
        let l = layout();
        let mut pkt = vec![0u8; l.packet_len()];
        for ch in 0..l.n_ch {
            let s: Vec<i16> = (0..4).map(|i| (ch * 100 + i) as i16).collect();
            reblock_channel(&mut pkt, &l, ch, &s);
        }
        for ch in 0..l.n_ch {
            let mut out = [0i16; 4];
            deblock_channel(&pkt, &l, ch, &mut out);
            let want: Vec<i16> = (0..4).map(|i| (ch * 100 + i) as i16).collect();
            assert_eq!(out.to_vec(), want, "channel {} mismatch", ch);
        }
    }

    /// Sample byte order must match the header's little-endian convention.
    #[test]
    fn samples_are_little_endian() {
        let l = PacketLayout::new(1, 1, 0);
        let mut pkt = vec![0u8; l.packet_len()];
        reblock_channel(&mut pkt, &l, 0, &[0x0102]);
        assert_eq!(pkt, vec![0x02, 0x01]);
    }
}
