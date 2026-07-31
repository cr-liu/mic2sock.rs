use bytes::Bytes;
use std::collections::BTreeMap;

/// What the pipeline should emit next.
#[derive(Debug, Clone, PartialEq)]
pub enum Released {
    /// A real packet from the source.
    Real(Bytes),
    /// Concealment for a short gap: a repeat of the previous packet.
    ///
    /// Repeating rather than inserting silence keeps the inter-channel phase
    /// relationships intact, so the downstream separator sees a frozen source
    /// rather than "everything vanished at once".
    Repeat(Bytes),
    /// Silence, for an outage or while priming.
    Silence,
    /// Nothing may be emitted yet.
    Nothing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Normal,
    /// No arrivals for longer than the threshold. Emits silence; the release
    /// position is frozen.
    Outage,
    /// Re-anchored after an outage, accumulating depth before resuming.
    Priming,
}

/// Size of the sender's id space. `mic2sock`'s `process_send_buf` walks
/// `0, 1, ..., i32::MAX - 1` and then returns to 0, so `i32::MAX` itself never
/// appears and the modulus is exactly that.
const ID_MODULUS: i64 = i32::MAX as i64;

/// Upper bound on the accept / reorder / retain horizons.
///
/// Direction in a cyclic id space is only unambiguous while the horizons stay
/// far below half the modulus: with `accept_ahead_packets = i32::MAX` a mere
/// duplicate reads as nearly a whole modulus *ahead* and is stored a second time.
/// 65,536 packets is eleven minutes at 100 pps — far beyond any useful buffer for
/// a 3 s design, and small enough that the cold-start store's total downward
/// reach (bounded by one reorder window below an unmoving newest) stays a factor
/// of 32,000 short of the `1 << 32` headroom the sequence space starts with.
///
/// `1 << 20` was too generous to carry that second claim: 2,046 crafted arrivals,
/// each stepping a full million below the last, walked the store's span to 2.1
/// billion sequences and made direction ambiguous again.
pub const MAX_HORIZON_PACKETS: usize = 65_536;

/// How far forward from `from` to `to` in the sender's cyclic id space.
///
/// Exact modular arithmetic rather than stepping: this needs a genuine distance
/// (a 40-packet burst would otherwise cost 40 steps), and being exact is what
/// makes the `i32::MAX -> 0` discontinuity a non-event.
fn forward_distance(from: i32, to: i32) -> u64 {
    let d = (to as i64 - from as i64).rem_euclid(ID_MODULUS);
    d as u64
}

/// Reorders, deduplicates, conceals gaps in place, and decides what to release.
///
/// Release timing is **not** driven by a timer — the caller asks for the next
/// item only when the consumer's socket accepted a write, so the consumer's read
/// rate paces the whole pipeline.
pub struct JitterBuffer {
    /// Keyed by a monotonic internal sequence number, deliberately **not** by
    /// `pkt_id`: ordering by the wire id reports oldest and newest backwards for
    /// a store straddling the `i32::MAX -> 0` wrap.
    store: BTreeMap<u64, (i32, Bytes)>,
    /// Sequence number of the next packet to release, and its wire id. `None`
    /// until the first release, so an out-of-order opening pair can still be
    /// reordered instead of having the first arrival fix the anchor.
    next: Option<(u64, i32)>,
    /// Sequence below which nothing may be accepted, because the consumer either
    /// already has that slot or will never get it. See `abandon_upto`.
    floor_seq: Option<u64>,
    /// Monotonic counter for assigning sequence numbers.
    seq_hint: u64,
    state: State,
    last_real: Option<Bytes>,
    last_arrival_ms: u64,
    outage_threshold_ms: u64,
    /// Largest gap, in packets, still worth concealing at release time. Beyond
    /// this the timeline is broken deliberately and counted.
    max_conceal_packets: usize,
    /// How far ahead of the release position an arriving packet may be and still
    /// be buffered. Unrelated to `max_conceal_packets`: during a burst, packets
    /// far ahead are exactly what should be kept.
    accept_ahead_packets: usize,
    reorder_window: usize,
    /// Hard latency bound: on resync, retain at most this many packets.
    retain_cap_packets: usize,
    pub late_discards: u64,
    pub duplicate_discards: u64,
    pub conceal_events: u64,
    pub outage_events: u64,
    pub resync_events: u64,
}

impl JitterBuffer {
    pub fn new(
        outage_threshold_ms: u64,
        max_conceal_packets: usize,
        accept_ahead_packets: usize,
        reorder_window: usize,
        retain_cap_packets: usize,
    ) -> Self {
        assert!(retain_cap_packets > 0, "retain_cap_packets must be > 0");
        for (name, horizon) in [
            ("max_conceal_packets", max_conceal_packets),
            ("accept_ahead_packets", accept_ahead_packets),
            ("reorder_window", reorder_window),
            ("retain_cap_packets", retain_cap_packets),
        ] {
            assert!(
                horizon <= MAX_HORIZON_PACKETS,
                "{} ({}) exceeds MAX_HORIZON_PACKETS ({})",
                name,
                horizon,
                MAX_HORIZON_PACKETS
            );
        }
        JitterBuffer {
            store: BTreeMap::new(),
            next: None,
            floor_seq: None,
            seq_hint: 1 << 32,
            state: State::Normal,
            last_real: None,
            last_arrival_ms: 0,
            outage_threshold_ms,
            max_conceal_packets,
            accept_ahead_packets,
            reorder_window,
            retain_cap_packets,
            late_discards: 0,
            duplicate_discards: 0,
            conceal_events: 0,
            outage_events: 0,
            resync_events: 0,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// Wire id of the next packet to release, if anchored.
    pub fn next_id(&self) -> Option<i32> {
        self.next.map(|(_, id)| id)
    }

    pub fn buffered(&self) -> usize {
        self.store.len()
    }

    /// Sequence number and wire id of the oldest stored packet.
    fn oldest(&self) -> Option<(u64, i32)> {
        self.store.iter().next().map(|(&s, &(id, _))| (s, id))
    }

    /// Sequence number and wire id of the newest stored packet.
    fn newest(&self) -> Option<(u64, i32)> {
        self.store.iter().next_back().map(|(&s, &(id, _))| (s, id))
    }

    /// Accepts an arrival.
    pub fn insert(&mut self, pkt_id: i32, payload: Bytes, now_ms: u64) {
        self.last_arrival_ms = now_ms;

        // Anchor relative to whatever reference we have: the release position if
        // anchored, else the newest stored packet, else this packet itself.
        // Newest rather than oldest, so that a long cold-start burst stays within
        // `accept_ahead_packets` of its reference no matter how long it runs.
        let reference = self
            .next
            .or_else(|| self.newest())
            .unwrap_or((self.seq_hint, pkt_id));
        let (ref_seq, ref_id) = reference;

        let ahead = forward_distance(ref_id, pkt_id);
        if ahead as usize <= self.accept_ahead_packets {
            let seq = ref_seq + ahead;
            if self.store.contains_key(&seq) {
                self.duplicate_discards += 1;
                return;
            }
            self.store.insert(seq, (pkt_id, payload));
            self.seq_hint = self.seq_hint.max(seq + 1);
            self.on_accepted();
            return;
        }

        // Behind the reference. Two separate questions, and the old code asked
        // only one of them.
        let behind = forward_distance(pkt_id, ref_id);
        // Cannot fire: the sequence space starts at 1 << 32 and the downward reach
        // is one reorder window below a reference that does not move down. A
        // restart is nevertheless the right answer if it ever does.
        let Some(seq) = ref_seq.checked_sub(behind) else {
            self.resync_to(pkt_id, payload);
            return;
        };

        // First question: do we already hold this slot? A re-sent packet from the
        // start of a cold-start burst is behind the *newest* by the whole length of
        // the burst, far outside the reorder window, and reading that as a source
        // restart cleared the entire store. The lookup is exact, so it may reach as
        // far back as the store itself does.
        if self.store.contains_key(&seq) {
            self.duplicate_discards += 1;
            return;
        }

        // Second question: straggler, or a new generation? A *vacant* slot that far
        // back is genuinely ambiguous — id 0 arriving while we hold 5..=20 could be
        // a late packet or a restarted sender — so the reorder window still decides
        // that one, which is what it is for.
        if behind as usize <= self.reorder_window {
            if self.floor_seq.map_or(false, |floor| seq < floor) {
                self.late_discards += 1;
                return;
            }
            self.store.insert(seq, (pkt_id, payload));
            self.on_accepted();
            return;
        }

        self.resync_to(pkt_id, payload);
    }

    /// Bookkeeping common to every accepted arrival.
    fn on_accepted(&mut self) {
        // The *first arrival* ends an outage, per the state machine — any arrival,
        // not only one ahead of the reference. Gating this on the ahead branch left
        // a genuine recovery packet accepted but stranded in Outage, with the
        // buffer holding it and no path back out.
        if self.state == State::Outage {
            self.state = State::Priming;
            self.resync_events += 1;
        }
        if self.state == State::Priming {
            // The anchor must be re-evaluated as the burst lands: the spec's
            // "newest arrived id" keeps moving, so a one-shot anchor taken on the
            // first arrival would be computed from a single packet. This is also
            // what grows the anchor downward onto a straggler below it.
            self.reanchor_for_priming();
        }
    }

    /// A source restart: neither plausibly ahead nor a recent straggler. Break the
    /// timeline deliberately, count it, and prime again.
    fn resync_to(&mut self, pkt_id: i32, payload: Bytes) {
        self.store.clear();
        self.seq_hint += 1;
        let seq = self.seq_hint;
        self.store.insert(seq, (pkt_id, payload));
        self.seq_hint = seq + 1;
        self.next = Some((seq, pkt_id));
        self.abandon_upto(seq);
        self.state = State::Priming;
        self.resync_events += 1;
    }

    /// Raises the floor below which no arrival may be accepted.
    ///
    /// The floor is what answers "has the consumer had this slot?". `next` cannot:
    /// re-anchoring reassigns it without anything being emitted. Using the *state*
    /// as a proxy was wrong in both directions — it re-admitted audio that had
    /// already been played (and played it twice), and it discarded audio that never
    /// had been.
    ///
    /// It rises only where a slot is genuinely consumed or discarded, never where
    /// the anchor is merely reassigned: priming deliberately moves the anchor down
    /// onto a straggler, and that has to stay possible.
    fn abandon_upto(&mut self, seq: u64) {
        self.floor_seq = Some(match self.floor_seq {
            Some(f) => f.max(seq),
            None => seq,
        });
    }

    /// Keeps at most `retain_cap_packets`, anchoring at the oldest retained.
    ///
    /// The lower bound is clamped to the oldest packet actually present, per
    /// spec: without that clamp the anchor can land behind everything in the
    /// store, after which every subsequent arrival looks implausible.
    fn reanchor_for_priming(&mut self) {
        while self.store.len() > self.retain_cap_packets {
            let oldest_seq = *self.store.keys().next().expect("non-empty");
            self.store.remove(&oldest_seq);
            // Trimming to the cap discards audio, so those slots must not be
            // reopened by a straggler that arrives for one of them later.
            self.abandon_upto(oldest_seq + 1);
        }
        if let Some((seq, id)) = self.oldest() {
            self.next = Some((seq, id));
        }
    }

    /// Releases the next item, with no priming gate.
    pub fn release(&mut self, now_ms: u64) -> Released {
        self.release_with_target(now_ms, 0)
    }

    /// Releases the next item.
    ///
    /// `target_packets` gates the exit from `Priming`: real audio does not resume
    /// until that much is buffered. Occupancy only grows by concealing (output
    /// without consuming input), so a shallow buffer cannot deepen on its own
    /// while input and output rates are equal.
    pub fn release_with_target(&mut self, now_ms: u64, target_packets: usize) -> Released {
        // The target cannot exceed what we are allowed to keep: priming trims the
        // store back to `retain_cap_packets` on every arrival, so a deeper target
        // is unreachable by construction and honouring it literally meant emitting
        // silence forever. The cap is the hard latency bound, so the cap wins.
        //
        // `Config::validate` makes the mismatch unreachable in production (the
        // target is capped by `d_max_adaptive_ms`, which is one term of
        // `retain_cap_packets`), hence an assertion in tests but a clamp in
        // release: a live audio relay should not panic over a bound it can honour.
        debug_assert!(
            target_packets <= self.retain_cap_packets,
            "target {} exceeds retain_cap {}",
            target_packets,
            self.retain_cap_packets
        );
        let target_packets = target_packets.min(self.retain_cap_packets);

        // A priming buffer still below its target is emitting silence and cannot
        // make progress on its own, so it is exactly as stalled as an empty one.
        // Testing `store.is_empty()` instead left it in Priming forever while
        // holding one packet, and the second outage went uncounted.
        let stalled = match self.state {
            State::Priming => self.store.len() < target_packets,
            _ => self.store.is_empty(),
        };
        if self.state != State::Outage
            && stalled
            && now_ms.saturating_sub(self.last_arrival_ms) >= self.outage_threshold_ms
            && self.next.is_some()
        {
            self.state = State::Outage;
            self.outage_events += 1;
        }

        match self.state {
            // The release position is deliberately not advanced here.
            State::Outage => Released::Silence,
            State::Priming => {
                if self.store.len() >= target_packets {
                    self.state = State::Normal;
                    self.release_normal(now_ms)
                } else {
                    Released::Silence
                }
            }
            State::Normal => self.release_normal(now_ms),
        }
    }

    fn release_normal(&mut self, _now_ms: u64) -> Released {
        // Anchor lazily, at the oldest stored packet, so an out-of-order opening
        // pair is reordered rather than half discarded.
        if self.next.is_none() {
            match self.oldest() {
                Some(pos) => self.next = Some(pos),
                None => return Released::Nothing,
            }
        }
        let (next_seq, _) = self.next.expect("anchored above");

        if let Some((id, payload)) = self.store.remove(&next_seq) {
            self.next = Some((next_seq + 1, wrapping_next_id(id)));
            self.abandon_upto(next_seq + 1);
            self.last_real = Some(payload.clone());
            return Released::Real(payload);
        }

        let Some((oldest_seq, oldest_id)) = self.oldest() else {
            return Released::Nothing;
        };
        // Guaranteed by the floor: nothing below the release position can be
        // accepted, and every discard raises the floor with it.
        debug_assert!(
            oldest_seq >= next_seq,
            "the store holds a slot below the release position"
        );
        let gap = oldest_seq - next_seq;

        if gap as usize <= self.max_conceal_packets {
            // Conceal exactly one packet, in place, and advance by one.
            let (_, next_id) = self.next.expect("anchored");
            self.next = Some((next_seq + 1, wrapping_next_id(next_id)));
            // The consumer has been given this slot, even though the audio was
            // synthetic, so a straggler for it must not reopen it later.
            self.abandon_upto(next_seq + 1);
            self.conceal_events += 1;
            return match &self.last_real {
                Some(p) => Released::Repeat(p.clone()),
                None => Released::Silence,
            };
        }

        // Beyond the conceal horizon: break the timeline deliberately and count it.
        self.resync_events += 1;
        self.next = Some((oldest_seq, oldest_id));
        self.abandon_upto(oldest_seq);
        self.release_normal(_now_ms)
    }

    /// Safety valve: discards oldest until at most `max_packets` remain. Returns
    /// how many were discarded. Should never fire in normal operation.
    ///
    /// Re-anchors only when it actually dropped something. Re-anchoring
    /// unconditionally would jump the release position over a gap that was still
    /// awaiting in-place concealment, shifting the far-end AEC reference against
    /// the mic channels by the width of that gap with nothing counted -- the very
    /// failure this module exists to prevent.
    pub fn enforce_max_depth(&mut self, max_packets: usize) -> usize {
        let mut dropped = 0;
        let mut last_dropped = None;
        while self.store.len() > max_packets {
            let oldest_seq = *self.store.keys().next().expect("non-empty");
            let (id, _) = self.store.remove(&oldest_seq).expect("just located");
            last_dropped = Some((oldest_seq, id));
            dropped += 1;
        }
        if dropped > 0 {
            // Re-anchor past the discarded audio. When the store is left empty
            // there is no retained oldest to anchor at, and leaving the position
            // where it was made the next release conceal the very packets that
            // were deliberately thrown away -- stretching the timeline by the
            // width of the discard, with the concealment counted as if it had
            // covered real loss.
            self.next = match self.oldest() {
                Some(pos) => Some(pos),
                None => last_dropped.map(|(seq, id)| (seq + 1, wrapping_next_id(id))),
            };
            // Deliberately discarded audio: the consumer will never get those
            // slots, so a straggler for one of them must not reopen it — that
            // would leave a packet below the release position, which nothing
            // downstream can express.
            if let Some((seq, _)) = self.next {
                self.abandon_upto(seq);
            }
            // Discarding buffered audio breaks the timeline deliberately, so it
            // is counted like any other resync rather than being silent.
            self.resync_events += 1;
        }
        dropped
    }
}

/// The sender's successor for a wire id: `i32::MAX` never appears, so `MAX - 1`
/// is followed by `0`.
fn wrapping_next_id(id: i32) -> i32 {
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

    fn pkt(n: u8) -> Bytes {
        Bytes::from(vec![n; 4])
    }

    /// outage_threshold 200 ms, conceal up to 8 packets, accept up to 200 packets
    /// ahead, reorder window 16, retain at most 12 packets on resync.
    fn jb() -> JitterBuffer {
        JitterBuffer::new(200, 8, 200, 16, 12)
    }

    #[test]
    fn forward_distance_is_exact_across_the_wrap() {
        assert_eq!(forward_distance(100, 100), 0);
        assert_eq!(forward_distance(100, 103), 3);
        // MAX-1 is the last id before 0, so 0 is one step further on.
        assert_eq!(forward_distance(i32::MAX - 1, 0), 1);
        assert_eq!(forward_distance(i32::MAX - 2, 1), 3);
        // Going backwards wraps all the way round.
        assert_eq!(forward_distance(0, i32::MAX - 1), (i32::MAX - 1) as u64);
    }

    #[test]
    fn releases_in_order() {
        let mut j = jb();
        j.insert(100, pkt(1), 1000);
        j.insert(101, pkt(2), 1010);
        assert_eq!(j.release(1020), Released::Real(pkt(1)));
        assert_eq!(j.release(1030), Released::Real(pkt(2)));
        assert_eq!(j.release(1040), Released::Nothing);
    }

    /// The anchor is taken at the first *release*, not the first arrival, so an
    /// out-of-order opening pair is still reordered rather than half discarded.
    #[test]
    fn out_of_order_arrival_is_reordered() {
        let mut j = jb();
        j.insert(101, pkt(2), 1000);
        j.insert(100, pkt(1), 1005);
        assert_eq!(j.late_discards, 0, "nothing should have been discarded yet");
        assert_eq!(j.release(1010), Released::Real(pkt(1)));
        assert_eq!(j.release(1020), Released::Real(pkt(2)));
    }

    #[test]
    fn duplicate_is_discarded() {
        let mut j = jb();
        j.insert(100, pkt(1), 1000);
        j.insert(100, pkt(9), 1001);
        assert_eq!(j.duplicate_discards, 1);
        assert_eq!(j.release(1010), Released::Real(pkt(1)));
    }

    #[test]
    fn a_packet_already_released_is_discarded_as_late() {
        let mut j = jb();
        j.insert(100, pkt(1), 1000);
        j.release(1010);
        j.insert(100, pkt(1), 1020);
        assert_eq!(j.late_discards, 1);
    }

    /// The core rule: a gap is concealed **in place**, before the packet that
    /// follows it. Concealing late would shift the far-end reference channel
    /// against the mic channels, which at 16 kHz is 160 samples -- far outside an
    /// AEC filter's converged region.
    #[test]
    fn a_gap_is_concealed_in_place_before_the_next_packet() {
        let mut j = jb();
        j.insert(100, pkt(1), 1000);
        j.insert(102, pkt(3), 1010); // 101 is missing
        assert_eq!(j.release(1020), Released::Real(pkt(1)));
        assert_eq!(
            j.release(1030),
            Released::Repeat(pkt(1)),
            "gap not concealed in place"
        );
        assert_eq!(j.release(1040), Released::Real(pkt(3)));
        assert_eq!(j.conceal_events, 1);
    }

    #[test]
    fn concealment_repeats_the_previous_packet_not_silence() {
        let mut j = jb();
        j.insert(100, pkt(7), 1000);
        j.insert(102, pkt(9), 1010);
        j.release(1020);
        assert_eq!(j.release(1030), Released::Repeat(pkt(7)));
    }

    #[test]
    fn no_arrivals_for_the_threshold_enters_outage_and_emits_silence() {
        let mut j = jb();
        j.insert(100, pkt(1), 1000);
        assert_eq!(j.release(1010), Released::Real(pkt(1)));
        assert_eq!(j.release(1210), Released::Silence);
        assert_eq!(j.state(), State::Outage);
        assert_eq!(j.outage_events, 1);
    }

    /// The release position must be frozen during an outage. If concealment
    /// advanced it, a one-second outage would advance it by 100 packets and the
    /// resync anchor would have to move *backwards*, which is self-contradictory.
    #[test]
    fn the_release_position_is_frozen_during_an_outage() {
        let mut j = jb();
        j.insert(100, pkt(1), 1000);
        j.release(1010);
        let frozen = j.next_id();
        for t in 0..20 {
            j.release(1210 + t * 10);
        }
        assert_eq!(j.next_id(), frozen, "the release position advanced");
    }

    /// After an outage, retain at most `retain_cap_packets`, discarding older
    /// data. This is the hard latency bound: however long the outage was and
    /// however much arrived at once, the buffer never resumes deeper than the cap.
    ///
    /// The cap is a **count of packets**, so 12 means 12 retained ids.
    #[test]
    fn resync_retains_exactly_the_cap_and_discards_older() {
        let mut j = jb();
        j.insert(100, pkt(1), 1000);
        j.release(1010);
        j.release(1300); // -> Outage

        // 40 packets arrive at once; the cap is 12.
        for k in 0..40i32 {
            j.insert(200 + k, pkt(2), 2000);
        }
        assert_eq!(
            j.buffered(),
            12,
            "retained {} packets, cap is 12",
            j.buffered()
        );
        // Newest is 239, so the 12 retained are 228..=239 and the release
        // position is the oldest of those.
        assert_eq!(j.next_id(), Some(228));
        assert_eq!(j.state(), State::Priming);
    }

    /// The opposite case: too little data arrives to reach the target depth. The
    /// buffer must keep emitting silence and let arrivals accumulate, because
    /// occupancy only grows by concealing -- it cannot "catch up on its own" when
    /// input and output rates are equal.
    ///
    /// This also pins the spec's lower clamp: with only one packet present, the
    /// anchor must clamp to that packet rather than land 12 behind it.
    #[test]
    fn priming_holds_silence_until_the_target_depth_is_reached() {
        let mut j = jb();
        j.insert(100, pkt(1), 1000);
        j.release(1010);
        j.release(1300); // -> Outage

        j.insert(200, pkt(2), 2000); // one packet only
        assert_eq!(j.state(), State::Priming);
        assert_eq!(
            j.next_id(),
            Some(200),
            "anchor was not clamped to the oldest"
        );
        assert_eq!(j.release_with_target(2010, 4), Released::Silence);
        for k in 1..4i32 {
            j.insert(200 + k, pkt(2), 2000 + k as u64);
        }
        assert_eq!(j.release_with_target(2100, 4), Released::Real(pkt(2)));
        assert_eq!(j.state(), State::Normal);
    }

    /// During priming the release position was *assigned*, not reached: nothing
    /// has been emitted at it. A packet landing behind it is therefore a
    /// straggler, not a late arrival, and discarding it loses that audio for
    /// good -- 100 used to be dropped here and 101 released in its place.
    #[test]
    fn priming_accepts_a_packet_behind_its_anchor() {
        let mut j = jb();
        j.insert(99, pkt(1), 1000);
        assert_eq!(j.release(1010), Released::Real(pkt(1)));
        j.release(1300); // -> Outage, the position frozen at 100
        j.insert(101, pkt(3), 2000); // -> Priming, anchored at 101
        assert_eq!(j.state(), State::Priming);

        j.insert(100, pkt(2), 2001); // one behind the priming anchor
        assert_eq!(j.late_discards, 0, "a straggler was discarded as late");
        assert_eq!(j.next_id(), Some(100), "the anchor did not grow downward");
        assert_eq!(j.buffered(), 2);
        assert_eq!(j.release_with_target(2010, 2), Released::Real(pkt(2)));
        assert_eq!(j.release_with_target(2020, 2), Released::Real(pkt(3)));
        assert_eq!(j.conceal_events, 0);
    }

    /// The other direction of the same proxy error: a packet that *was* emitted
    /// must not be re-admitted just because the state is Priming. The anchor is
    /// provisional there, but the release floor is not — 99 was played, so it is
    /// late however the anchor has since been reassigned.
    #[test]
    fn priming_still_refuses_a_packet_it_has_already_played() {
        let mut j = jb();
        j.insert(99, pkt(1), 1000);
        assert_eq!(j.release(1010), Released::Real(pkt(1)));
        j.release(1300); // -> Outage, floor at 100
        j.insert(105, pkt(6), 2000); // -> Priming, anchored at 105
        assert_eq!(j.next_id(), Some(105));

        j.insert(99, pkt(9), 2001); // already played
        assert_eq!(j.late_discards, 1, "replayed audio was re-admitted");
        assert_eq!(j.next_id(), Some(105), "the anchor was dragged backwards");
        assert_eq!(j.buffered(), 1);
    }

    /// A never-emitted packet must be accepted even if priming has already given
    /// way to a *second* outage. Gating acceptance on the state left this packet
    /// discarded as late and the buffer with no way out of Outage.
    #[test]
    fn a_recovery_packet_is_accepted_after_priming_falls_back_to_outage() {
        let mut j = jb();
        j.insert(99, pkt(1), 1000);
        assert_eq!(j.release(1010), Released::Real(pkt(1)));
        j.release(1300); // -> Outage
        j.insert(101, pkt(3), 2000); // -> Priming, anchored at 101
        assert_eq!(j.release_with_target(2010, 4), Released::Silence);
        assert_eq!(j.release_with_target(2300, 4), Released::Silence);
        assert_eq!(j.state(), State::Outage, "priming did not fall back");

        j.insert(100, pkt(2), 2400); // never emitted
        assert_eq!(j.late_discards, 0, "a recovery packet was discarded");
        assert_eq!(
            j.state(),
            State::Priming,
            "an arrival did not end the outage"
        );
        assert_eq!(j.next_id(), Some(100));
        assert_eq!(j.release_with_target(2410, 2), Released::Real(pkt(2)));
        assert_eq!(j.release_with_target(2420, 2), Released::Real(pkt(3)));
    }

    /// A genuine source restart must still be detected when the id it restarts to
    /// happens to land inside the span the cold-start store covers. Widening the
    /// reach for *vacant* slots mixed two generations of audio without counting a
    /// resync: new id 0, four concealments, then old id 5.
    #[test]
    fn a_source_restart_into_the_cold_start_span_is_still_a_restart() {
        let mut j = jb();
        for k in 5..=20i32 {
            j.insert(k, pkt(1), 1000);
        }
        assert_eq!(j.buffered(), 16);

        j.insert(0, pkt(9), 1001); // 20 behind the newest, window is 16
        assert_eq!(j.resync_events, 1, "a restart was read as a straggler");
        assert_eq!(j.buffered(), 1, "the old generation was kept");
        assert_eq!(j.release(1010), Released::Real(pkt(9)));
        assert_eq!(j.conceal_events, 0, "concealed across two generations");
    }

    /// A re-sent packet from the start of a cold-start burst is behind the newest
    /// by the whole length of the burst, which is more than the reorder window.
    /// Reading that as a source restart cleared all 21 buffered packets and did
    /// not even count the duplicate.
    #[test]
    fn a_resent_packet_from_the_start_of_a_burst_is_not_a_source_restart() {
        let mut j = jb();
        for k in 0..21i32 {
            j.insert(100 + k, pkt(1), 1000);
        }
        assert_eq!(j.buffered(), 21);

        j.insert(100, pkt(9), 1001); // 20 behind the newest, window is 16
        assert_eq!(j.buffered(), 21, "the whole burst was discarded");
        assert_eq!(j.duplicate_discards, 1, "the duplicate was not counted");
        assert_eq!(j.resync_events, 0, "a duplicate was read as a restart");
        assert_eq!(j.release(1010), Released::Real(pkt(1)));
    }

    /// A target deeper than the retention cap is unreachable by construction:
    /// priming trims the store back to the cap on every arrival, so the buffer
    /// used to sit in silence forever waiting for a depth it was not allowed to
    /// hold. That pair is a programming error — `Config::validate` cannot produce
    /// it, because the target is capped by one of the two terms of `retain_cap` —
    /// so it is refused loudly here rather than merely clamped in silence.
    #[test]
    #[should_panic(expected = "exceeds retain_cap")]
    fn a_target_deeper_than_the_retention_cap_is_a_programming_error() {
        let mut j = JitterBuffer::new(200, 8, 200, 16, 2);
        j.insert(100, pkt(1), 1000);
        j.release(1010);
        j.release(1300); // -> Outage
        for k in 0..5i32 {
            j.insert(200 + k, pkt(2), 2000);
        }
        assert_eq!(j.buffered(), 2, "the retention cap was not applied");
        j.release_with_target(2010, 3);
    }

    /// The reachable edge of the same case: a target exactly equal to the cap must
    /// resume, not stall one packet short of a depth it can never exceed.
    #[test]
    fn a_target_equal_to_the_retention_cap_still_resumes() {
        let mut j = JitterBuffer::new(200, 8, 200, 16, 2);
        j.insert(100, pkt(1), 1000);
        j.release(1010);
        j.release(1300); // -> Outage
        for k in 0..5i32 {
            j.insert(200 + k, pkt(2), 2000);
        }
        assert_eq!(j.release_with_target(2010, 2), Released::Real(pkt(2)));
        assert_eq!(j.state(), State::Normal);
    }

    /// Discarding every stored packet must move the release position past them.
    /// Leaving it behind made the next release conceal the very audio that was
    /// deliberately thrown away, counting it as if it had covered real loss.
    #[test]
    fn max_depth_zero_re_anchors_past_everything_it_discarded() {
        let mut j = jb();
        j.insert(100, pkt(1), 1000);
        assert_eq!(j.release(1010), Released::Real(pkt(1)));
        for k in 1..=5i32 {
            j.insert(100 + k, pkt(2), 1000 + k as u64);
        }

        assert_eq!(j.enforce_max_depth(0), 5);
        assert_eq!(
            j.next_id(),
            Some(106),
            "the anchor stayed behind the discards"
        );
        j.insert(106, pkt(6), 1100);
        assert_eq!(j.release(1110), Released::Real(pkt(6)));
        assert_eq!(j.conceal_events, 0, "reconcealed audio it had discarded");
    }

    /// A priming buffer that cannot reach its target is emitting silence and
    /// cannot progress on its own, so a further silence past the threshold is
    /// another outage and must be counted as one.
    #[test]
    fn priming_re_enters_outage_when_arrivals_stop_again() {
        let mut j = jb();
        j.insert(100, pkt(1), 1000);
        j.release(1010);
        j.release(1300); // -> Outage
        assert_eq!(j.outage_events, 1);

        j.insert(200, pkt(2), 2000); // -> Priming, holding one packet
        assert_eq!(j.release_with_target(2010, 4), Released::Silence);
        assert_eq!(j.release_with_target(2300, 4), Released::Silence);
        assert_eq!(j.state(), State::Outage, "priming never re-entered outage");
        assert_eq!(j.outage_events, 2);
        assert_eq!(j.buffered(), 1, "the held packet was thrown away");
    }

    /// Horizons near half the modulus make direction ambiguous: a duplicate then
    /// reads as almost a whole modulus *ahead* and is stored a second time.
    #[test]
    #[should_panic(expected = "exceeds MAX_HORIZON_PACKETS")]
    fn an_accept_horizon_near_half_the_modulus_is_refused() {
        JitterBuffer::new(200, 8, i32::MAX as usize, 16, 12);
    }

    /// A gap larger than is worth concealing breaks the timeline deliberately; it
    /// must be counted rather than silently papered over.
    #[test]
    fn a_gap_beyond_max_conceal_resyncs_and_is_counted() {
        let mut j = jb();
        j.insert(100, pkt(1), 1000);
        assert_eq!(j.release(1010), Released::Real(pkt(1)));
        j.insert(150, pkt(2), 1020); // 49 packets missing, conceal cap is 8
        assert_eq!(j.release(1030), Released::Real(pkt(2)));
        assert_eq!(j.resync_events, 1);
        assert_eq!(j.conceal_events, 0, "should not have tried to conceal");
    }

    /// The sender resets pkt_id to 0 at i32::MAX, so this must not look like a
    /// source restart. Happens once every ~248 days at 100 pps.
    #[test]
    fn the_id_wrap_is_continuous() {
        let mut j = jb();
        j.insert(i32::MAX - 2, pkt(1), 1000);
        assert_eq!(j.release(1010), Released::Real(pkt(1)));
        j.insert(i32::MAX - 1, pkt(2), 1020);
        assert_eq!(j.release(1030), Released::Real(pkt(2)));
        j.insert(0, pkt(3), 1040);
        assert_eq!(j.release(1050), Released::Real(pkt(3)), "wrap misread");
        assert_eq!(j.resync_events, 0);
        assert_eq!(j.conceal_events, 0);
    }

    /// A store holding ids on both sides of the wrap must still report its oldest
    /// and newest correctly -- the defect that keying by `pkt_id` would reintroduce.
    #[test]
    fn a_burst_straddling_the_wrap_orders_correctly() {
        let mut j = jb();
        j.insert(i32::MAX - 2, pkt(1), 1000);
        j.insert(i32::MAX - 1, pkt(2), 1001);
        j.insert(0, pkt(3), 1002);
        j.insert(1, pkt(4), 1003);
        assert_eq!(j.buffered(), 4);
        assert_eq!(j.release(1010), Released::Real(pkt(1)));
        assert_eq!(j.release(1020), Released::Real(pkt(2)));
        assert_eq!(j.release(1030), Released::Real(pkt(3)));
        assert_eq!(j.release(1040), Released::Real(pkt(4)));
        assert_eq!(j.conceal_events, 0);
        assert_eq!(j.resync_events, 0);
    }

    #[test]
    fn a_source_restart_resyncs() {
        let mut j = jb();
        for k in 0..20i32 {
            j.insert(1000 + k, pkt(1), 1000 + k as u64);
            j.release(1000 + k as u64);
        }
        j.insert(0, pkt(2), 1100); // pkt_id went back to 0
        assert_eq!(j.resync_events, 1);
        assert_eq!(j.release(1110), Released::Real(pkt(2)));
    }

    /// The safety valve. If the consumer stops reading, discard oldest rather
    /// than grow without bound. Reachable only because `accept_ahead_packets` is
    /// independent of the conceal horizon.
    #[test]
    fn exceeding_the_max_depth_discards_oldest() {
        let mut j = jb();
        for k in 0..100i32 {
            j.insert(100 + k, pkt(1), 1000);
        }
        assert_eq!(
            j.buffered(),
            100,
            "accept horizon too small to fill the store"
        );
        let dropped = j.enforce_max_depth(20);
        assert_eq!(dropped, 80);
        assert_eq!(j.buffered(), 20);
    }

    /// The safety valve must not re-anchor when it dropped nothing. Jumping the
    /// release position over a gap still awaiting concealment would shift the
    /// far-end AEC reference against the mic channels with nothing counted.
    #[test]
    fn max_depth_does_not_skip_a_pending_gap_when_it_drops_nothing() {
        let mut j = jb();
        j.insert(100, pkt(1), 1000);
        j.insert(105, pkt(6), 1001); // 101..104 missing
        assert_eq!(j.release(1010), Released::Real(pkt(1)));

        assert_eq!(
            j.enforce_max_depth(100),
            0,
            "nothing should have been dropped"
        );
        // The gap must still be concealed in place rather than skipped.
        assert_eq!(j.release(1020), Released::Repeat(pkt(1)));
        assert_eq!(j.conceal_events, 1);
    }
}
