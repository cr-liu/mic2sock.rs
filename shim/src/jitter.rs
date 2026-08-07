use bytes::Bytes;
use protocol::next_pkt_id;
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
/// Direction in a cyclic id space is only unambiguous while the horizons stay far
/// below half the modulus: with `accept_ahead_packets = i32::MAX` a mere duplicate
/// reads as nearly a whole modulus *ahead* and is stored a second time. 65,536
/// packets is eleven minutes at 100 pps — far beyond any useful buffer for a 3 s
/// design.
///
/// This is the coarse bound only. What actually keeps the store's *span* below the
/// modulus is the cap in `insert`: repeated downward re-anchoring while priming
/// moves the reference down a reorder window at a time, so no per-step horizon
/// bounds the total on its own (32,768 steps of `1 << 20` covered 2^31).
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
    /// Set by `on_source_reconnect`, cleared once a packet is accepted. Only while
    /// it is set may a backward id jump be read as a new generation.
    generation_may_reset: bool,
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
    /// Timeline breaks caused by a *proven* new generation — a pkt_id reset with
    /// transport evidence behind it. A subset of `resync_events`, which also counts
    /// gap-driven breaks and the safety valve. The pipeline watches this one to know
    /// when the sender's clock offset may have changed, because that is the only
    /// event justifying a reset of the depth statistic.
    pub generation_resets: u64,
    pub duplicate_discards: u64,
    /// The buffer's own view, observed by its tests. NOT the JSONL field of the
    /// same name: the pipeline defines that one itself (a `Repeat` release), and
    /// the two legitimately differ — a conceal with no previous packet to repeat
    /// releases `Silence`, which is counted here and not there.
    pub conceal_events: u64,
    /// Likewise the buffer's own view (entries into `State::Outage`); the JSONL
    /// field of the same name counts the depth estimator's per-arrival delay
    /// classification, a different event.
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
            generation_may_reset: false,
            seq_hint: 1 << 32,
            // Starts priming, not normal. Occupancy only grows by concealing, so a
            // buffer that begins releasing on its first arrival stays pinned at zero
            // depth for as long as input and output rates match — it would forward the
            // burstiness it exists to absorb. The target gate is exactly the wait that
            // cold start needs, and it is already implemented for outage recovery.
            state: State::Priming,
            last_real: None,
            last_arrival_ms: 0,
            outage_threshold_ms,
            max_conceal_packets,
            accept_ahead_packets,
            reorder_window,
            retain_cap_packets,
            late_discards: 0,
            generation_resets: 0,
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
        // `last_arrival_ms` is deliberately *not* set here: it drives the outage
        // timer, and an arrival that is refused has not restarted the stream. Set
        // here, a restarted stream whose every packet is being refused kept its own
        // timer perpetually fresh, so the outage that would have rescued it never
        // fired and the buffer emitted silence until the new ids caught up with the
        // old ones — hours at 100 pps, and up to 248 days from the top of the id
        // space. It is set at each acceptance point instead.

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
            // Keep the store's span inside the horizons it accepts within on this
            // path too. `newest()` advances with every forward arrival, so a rolling
            // reference bounds each *step* but not the total: a sparse stream walked
            // the span across the whole id space, after which one wire id maps to two
            // sequences and is stored twice. Trimming the oldest is the same
            // deliberate discard the safety valve makes, and is counted the same way.
            self.trim_span_below(seq);
            self.on_accepted(true);
            self.last_arrival_ms = now_ms;
            return;
        }

        // Behind the reference, which is ambiguous in a way the ids cannot settle:
        // the same backward distance means "audio from this stream, re-sent or
        // reordered" and "a sender that restarted onto a nearby id", and choosing
        // either interpretation from the distance alone is wrong in the other case.
        // Guessing restart replays audio the consumer already heard and throws away
        // the store; guessing replay swallows a restart and can stall forever.
        //
        // The transport settles it. A TCP stream cannot deliver a backward jump
        // inside one connection, and a sender restart always drops the connection,
        // so only a reconnect may be read as a new generation — plus an outage, where
        // the timeline is already broken and there is nothing left to protect, which
        // is the backstop if the reconnect is never reported. A UDP source would need
        // an epoch or source id in the header; there is nowhere else for the evidence
        // to come from.
        let behind = forward_distance(pkt_id, ref_id);
        let new_generation = self.generation_may_reset || self.state == State::Outage;
        // Cannot fire: the sequence space starts at 1 << 32, and the store's span is
        // capped below. A restart is nevertheless the right answer if it ever does.
        let Some(seq) = ref_seq.checked_sub(behind) else {
            self.resync_to(pkt_id, payload, now_ms);
            return;
        };

        if new_generation && behind as usize > self.reorder_window {
            // Evidence outranks the floor: the floor describes slots of the stream
            // that just ended, and this packet belongs to the next one.
            self.generation_may_reset = false;
            self.resync_to(pkt_id, payload, now_ms);
            return;
        }

        // Same generation, and this slot was already emitted or deliberately
        // discarded. Refused however far back it is — re-emitting it is the one
        // outcome with no upside, and checking the floor only inside the reorder
        // window let a played packet 17 behind fall through to a resync that wiped
        // the store and played that audio a second time.
        if self.floor_seq.map_or(false, |floor| seq < floor) {
            self.late_discards += 1;
            return;
        }

        // An exact lookup finds a duplicate however far back it is,
        // which is what a re-sent packet from the start of a cold-start burst needs:
        // it is behind the *newest* by the whole length of the burst.
        if self.store.contains_key(&seq) {
            self.duplicate_discards += 1;
            return;
        }
        if behind as usize > self.reorder_window {
            // Further back than we promise to reorder, and not a new generation.
            self.late_discards += 1;
            return;
        }
        // The store may never span more than the horizons it accepts within. An
        // unbounded span lets one wire id map to two sequences once it exceeds the id
        // modulus, which repeated downward re-anchoring in Priming can reach: each
        // step moves the reference down by a reorder window, and 32,768 of them cover
        // 2^31. Downward there is nothing to trim to make room, so this one is
        // refused rather than accommodated.
        if let Some((newest_seq, _)) = self.newest() {
            if newest_seq.saturating_sub(seq)
                > (self.accept_ahead_packets + self.reorder_window) as u64
            {
                self.late_discards += 1;
                return;
            }
        }
        self.store.insert(seq, (pkt_id, payload));
        self.on_accepted(false);
        self.last_arrival_ms = now_ms;
    }

    /// The source connection was re-established.
    ///
    /// This is the only evidence that a backward id jump may be a new generation
    /// rather than a replay of audio already emitted — see `insert`. The pipeline
    /// must call it on every successful connect, including the first.
    pub fn on_source_reconnect(&mut self) {
        self.generation_may_reset = true;
    }

    /// Discards from the oldest until the store spans no more than the horizons it
    /// accepts within, given a newly placed `seq` at the top of the span.
    fn trim_span_below(&mut self, seq: u64) {
        let span_cap = (self.accept_ahead_packets + self.reorder_window) as u64;
        let mut dropped = false;
        while let Some((oldest_seq, _)) = self.oldest() {
            if seq.saturating_sub(oldest_seq) <= span_cap {
                break;
            }
            self.store.remove(&oldest_seq);
            self.abandon_upto(oldest_seq + 1);
            dropped = true;
        }
        if dropped {
            self.resync_events += 1;
        }
    }

    /// Bookkeeping common to every accepted arrival. `forward` distinguishes one that
    /// continues the stream from a straggler placed behind the reference.
    fn on_accepted(&mut self, forward: bool) {
        // A reconnect explains the arrivals up to the point the stream resumes, and
        // only a *forward* arrival is the stream resuming: after an ordinary
        // reconnect the sender's ids carry on, so the evidence has done its job. A
        // straggler accepted behind the anchor is leftover from the generation that
        // just ended and must not consume it — doing so left the restarted stream
        // with no evidence and every one of its packets refused as late.
        if forward {
            self.generation_may_reset = false;
        }
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
    ///
    /// The new generation starts a reorder window clear of everything the old one
    /// used, and the floor is deliberately **not** raised to the new anchor. The
    /// arriving packet is the anchor but has not been released, so the ids just
    /// behind it are stragglers of the new generation that still belong to the
    /// timeline — the same argument as priming, and abandoning that slot for
    /// symmetry with the genuine discard sites would drop them. The gap is what
    /// keeps those stragglers from landing in the old generation's abandoned slots,
    /// where the floor would refuse them.
    fn resync_to(&mut self, pkt_id: i32, payload: Bytes, now_ms: u64) {
        self.store.clear();
        let seq = self.seq_hint + self.reorder_window as u64 + 1;
        self.seq_hint = seq + 1;
        self.store.insert(seq, (pkt_id, payload));
        self.next = Some((seq, pkt_id));
        self.state = State::Priming;
        self.resync_events += 1;
        self.generation_resets += 1;
        self.last_arrival_ms = now_ms;
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
        // Re-anchor only if there is an anchor to correct. After an outage the old
        // release position is stale and has to follow the retained audio. At cold start
        // there is none, and inventing one here would make the reference for the next
        // arrival the oldest packet rather than the newest — after which a chain of
        // stragglers walks the accepted window backwards a reorder window at a time,
        // and the boundary between "straggler" and "restarted sender" stops meaning
        // anything. Cold start anchors lazily at the first release instead.
        if self.next.is_some() {
            if let Some((seq, id)) = self.oldest() {
                self.next = Some((seq, id));
            }
        }
    }

    /// Test convenience: release with no priming gate.
    ///
    /// `#[cfg(test)]` on purpose. Target 0 turns off the cold-start prime, and a
    /// public method with the shorter, more inviting name must not be the one
    /// that silently drops that invariant — production always goes through
    /// `release_with_target`.
    #[cfg(test)]
    fn release(&mut self, now_ms: u64) -> Released {
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
                    self.release_normal()
                } else {
                    Released::Silence
                }
            }
            State::Normal => self.release_normal(),
        }
    }

    fn release_normal(&mut self) -> Released {
        // Anchor lazily, at the oldest stored packet, so an out-of-order opening
        // pair is reordered rather than half discarded.
        if self.next.is_none() {
            match self.oldest() {
                Some(pos) => self.next = Some(pos),
                None => return Released::Nothing,
            }
        }
        let (next_seq, next_id) = self.next.expect("anchored above");

        if let Some((id, payload)) = self.store.remove(&next_seq) {
            self.next = Some((next_seq + 1, next_pkt_id(id)));
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
            self.next = Some((next_seq + 1, next_pkt_id(next_id)));
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
        self.release_normal()
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
        self.trim_oldest(max_packets, true)
    }

    /// Spec §6.5's rolling window: while no consumer is attached, keep only the
    /// most recent `max_packets` so the buffer is primed the moment one appears.
    ///
    /// The same discipline as the safety valve — discard oldest, re-anchor, raise
    /// the floor — but **not counted as a resync**: with no consumer there is no
    /// timeline to break, and charging ~100 events/s of normal idling to an alarm
    /// counter the README says should stay near zero made that counter unreadable.
    pub fn retain_window(&mut self, max_packets: usize) -> usize {
        self.trim_oldest(max_packets, false)
    }

    fn trim_oldest(&mut self, max_packets: usize, break_timeline: bool) -> usize {
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
                None => last_dropped.map(|(seq, id)| (seq + 1, next_pkt_id(id))),
            };
            // Deliberately discarded audio: the consumer will never get those
            // slots, so a straggler for one of them must not reopen it — that
            // would leave a packet below the release position, which nothing
            // downstream can express.
            if let Some((seq, _)) = self.next {
                self.abandon_upto(seq);
            }
            // Discarding buffered audio breaks the timeline deliberately, so it
            // is counted like any other resync rather than being silent — except
            // for the idle rolling window, where nothing is being released and
            // there is no timeline to break.
            if break_timeline {
                self.resync_events += 1;
            }
        }
        dropped
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

    /// Same, with room to hold a long burst: the retention cap applies from the first
    /// arrival now that the buffer starts priming, so a test about restart or duplicate
    /// classification needs a cap that will not trim its fixture out from under it.
    fn jb_holding(cap: usize) -> JitterBuffer {
        JitterBuffer::new(200, 8, 200, 16, cap)
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

    /// The floor decides before the distance does. A packet whose slot was already
    /// emitted must be refused however far behind it is — checking the floor only
    /// inside the reorder window let a played packet 17 behind fall through to
    /// `resync_to`, which wiped the store and emitted that audio a second time.
    #[test]
    fn a_played_packet_beyond_the_reorder_window_is_late_not_a_restart() {
        let mut j = jb();
        j.insert(99, pkt(1), 1000);
        assert_eq!(j.release(1010), Released::Real(pkt(1)));
        j.release(1300); // -> Outage, floor at 100
        j.insert(116, pkt(6), 2000); // 16 ahead -> Priming, anchored at 116
        let resyncs = j.resync_events;

        j.insert(99, pkt(9), 2001); // played, and 17 behind the anchor
        assert_eq!(j.late_discards, 1, "played audio was accepted again");
        assert_eq!(j.resync_events, resyncs, "a replay was read as a restart");
        assert_eq!(j.buffered(), 1, "the store was wiped");
        assert_eq!(j.release_with_target(2010, 1), Released::Real(pkt(6)));
    }

    /// The opposite horn: a genuine restart landing on a slot the buffer still holds
    /// must not be swallowed as a duplicate. The exact-lookup rule alone said
    /// duplicate; the transport says the connection was re-established, which is the
    /// only evidence that can tell these two apart.
    #[test]
    fn a_restart_onto_an_occupied_slot_is_a_restart_not_a_duplicate() {
        let mut j = jb_holding(64);
        for k in 0..=20i32 {
            j.insert(k, pkt(1), 1000);
        }
        assert_eq!(j.buffered(), 21);

        j.on_source_reconnect();
        j.insert(0, pkt(9), 1001); // new generation, same id as a stored packet
        assert_eq!(j.resync_events, 1, "a restart was swallowed as a duplicate");
        assert_eq!(j.duplicate_discards, 0);
        assert_eq!(j.buffered(), 1, "the old generation was kept");
        assert_eq!(j.release(1010), Released::Real(pkt(9)));
    }

    /// The evidence has to outlive a straggler from the generation that just ended.
    /// Clearing it on *any* accepted arrival left the restarted stream unexplained,
    /// and then — because a refused arrival also refreshed the outage timer — every
    /// one of its packets was rejected while its own traffic kept the backstop from
    /// ever firing. The buffer emitted silence until the new ids caught up with the
    /// old: hours at 100 pps, and up to 248 days from the top of the id space.
    #[test]
    fn reconnect_evidence_outlives_a_straggler_from_the_old_generation() {
        let mut j = jb();
        j.insert(999_999, pkt(1), 1000);
        assert_eq!(j.release(1010), Released::Real(pkt(1)));
        j.release(1300); // -> Outage
        j.insert(1_000_002, pkt(2), 2000); // the old generation resumes -> Priming

        j.on_source_reconnect();
        j.insert(1_000_001, pkt(3), 2001); // a straggler, still the old generation
        assert_eq!(j.late_discards, 0);

        // The restarted stream begins at 0, and must be recognised at once.
        j.insert(0, pkt(9), 2010);
        assert_eq!(j.generation_resets, 1, "the evidence was consumed early");
        assert_eq!(j.late_discards, 0, "the restarted stream was refused");
        assert_eq!(j.release_with_target(2020, 1), Released::Real(pkt(9)));
    }

    /// And refused arrivals must not hold the outage timer open, or the backstop that
    /// bounds a missed reconnect never fires.
    #[test]
    fn refused_arrivals_do_not_hold_the_outage_timer_open() {
        let mut j = jb();
        j.insert(999_999, pkt(1), 1000);
        assert_eq!(j.release(1010), Released::Real(pkt(1)));
        j.release(1300); // -> Outage
        j.insert(1_000_002, pkt(2), 2000); // -> Priming, one packet, target 4

        // A restarted stream with no reconnect reported: every packet is refused,
        // one every 10 ms, for well over the threshold.
        for k in 0..100i32 {
            j.insert(k, pkt(9), 2010 + k as u64 * 10);
        }
        assert_eq!(j.late_discards, 100, "the restarted stream was not refused");
        assert_eq!(
            j.release_with_target(2300, 4),
            Released::Silence,
            "still priming, as expected"
        );
        assert_eq!(
            j.state(),
            State::Outage,
            "refused traffic kept the outage timer fresh"
        );
    }

    /// The span cap has to hold on the forward path too. `newest()` advances with
    /// every arrival, so a rolling reference bounds each step but not the total: a
    /// sparse stream walked the span across the whole id space, after which one wire
    /// id maps to two sequences and is stored twice.
    #[test]
    fn a_sparse_forward_stream_does_not_walk_the_store_span() {
        let mut j = JitterBuffer::new(200, 8, 32, 16, 1024); // span cap 32 + 16
        for k in 0..20i32 {
            j.insert(k * 32, pkt(1), 1000 + k as u64); // each a full horizon ahead
        }
        assert!(
            j.buffered() <= 3,
            "store spans the whole stream: {} packets",
            j.buffered()
        );
        assert!(j.resync_events > 0, "the span trim was not counted");
    }

    /// Without that evidence a backward jump is a replay, not a restart: a TCP
    /// connection cannot deliver one, so a sender restart always brings a reconnect.
    /// The outage backstop is what keeps a missed reconnect from stalling forever.
    #[test]
    fn a_backward_jump_without_a_reconnect_resyncs_only_after_an_outage() {
        let mut j = jb();
        for k in 0..20i32 {
            j.insert(1000 + k, pkt(1), 1000 + k as u64);
            j.release(1000 + k as u64);
        }

        j.insert(0, pkt(2), 1100); // no reconnect reported
        assert_eq!(j.resync_events, 0, "guessed a restart without evidence");
        assert_eq!(j.late_discards, 1);

        // Nothing acceptable arrives, so the buffer declares an outage — and then the
        // timeline is already broken, so the same jump is taken at face value.
        assert_eq!(j.release(1400), Released::Silence);
        assert_eq!(j.state(), State::Outage);
        j.insert(1, pkt(3), 1500);
        assert_eq!(j.resync_events, 1, "the backstop did not resync");
        assert_eq!(j.release(1510), Released::Real(pkt(3)));
    }

    /// Repeated downward re-anchoring while priming must not walk the store's span
    /// past the id modulus, after which one wire id maps to two sequences and can be
    /// stored twice. Reachable only where there is no release floor to stop the walk
    /// — after a resync, before anything has been emitted.
    #[test]
    fn the_store_span_is_capped_while_priming_walks_downward() {
        let mut j = JitterBuffer::new(200, 8, 32, 16, 64); // span cap 32 + 16
        j.insert(0, pkt(1), 1000);
        j.on_source_reconnect();
        j.insert(1000, pkt(2), 1001); // -> resync, Priming, nothing emitted yet
        assert_eq!(j.buffered(), 1);

        // Each of these is a full reorder window behind the last, and each
        // re-anchors, so the reference walks down with them.
        for k in 1..=5i32 {
            j.insert(1000 - k * 16, pkt(3), 1001 + k as u64);
        }
        assert_eq!(j.buffered(), 4, "the span cap did not engage");
        assert_eq!(j.late_discards, 2);
    }

    /// The reorder window is the promise about how far back a straggler is still
    /// placed. Just inside it the packet is kept; just outside — with no evidence of
    /// a new generation — it is late, rather than placed anyway or read as a restart.
    #[test]
    fn the_reorder_window_bounds_how_far_back_a_straggler_is_placed() {
        let mut j = jb(); // reorder window 16
        j.insert(1000, pkt(1), 1000);

        j.insert(1000 - 16, pkt(2), 1001); // exactly 16 behind: still reordered
        assert_eq!(j.buffered(), 2);
        assert_eq!(j.late_discards, 0);

        j.insert(1000 - 17, pkt(3), 1002); // one further: beyond the promise
        assert_eq!(j.late_discards, 1, "placed a straggler beyond the window");
        assert_eq!(j.buffered(), 2);
        assert_eq!(j.resync_events, 0, "read as a restart without evidence");
        assert_eq!(j.release(1010), Released::Real(pkt(2)));
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
        let mut j = jb_holding(64);
        for k in 5..=20i32 {
            j.insert(k, pkt(1), 1000);
        }
        assert_eq!(j.buffered(), 16);

        j.on_source_reconnect();
        j.insert(0, pkt(9), 1001); // 20 behind the newest, window is 16
        assert_eq!(j.resync_events, 1, "a restart was read as a straggler");
        assert_eq!(j.buffered(), 1, "the old generation was kept");
        assert_eq!(j.release(1010), Released::Real(pkt(9)));
        assert_eq!(j.conceal_events, 0, "concealed across two generations");
    }

    /// The restart anchor is the arriving packet, which has not been *released*, so
    /// an id just behind it is a straggler of the new generation and still belongs
    /// to the timeline. Raising the floor to the resync anchor for symmetry with the
    /// genuine discard sites dropped it, and without the generation gap it would
    /// land in a slot the old generation had already abandoned.
    #[test]
    fn a_reordered_head_of_a_new_generation_survives_the_restart() {
        let mut j = jb();
        for k in 0..20i32 {
            j.insert(5000 + k, pkt(1), 1000 + k as u64);
            j.release(1000 + k as u64);
        }

        // A head several ids behind the first to arrive, not one or two: nearer than
        // that, the new generation's slot coincides with the old floor and is admitted
        // by equality however small the gap, which left the gap itself untested.
        j.on_source_reconnect();
        j.insert(16, pkt(2), 1100); // restart; id 16 arrives first
        assert_eq!(j.resync_events, 1);
        j.insert(8, pkt(3), 1101); // eight behind it, inside the reorder window
        assert_eq!(j.late_discards, 0, "the new generation's head was dropped");
        assert_eq!(j.next_id(), Some(8), "the anchor did not grow downward");
        assert_eq!(j.release_with_target(1110, 2), Released::Real(pkt(3)));
        // 9..=15 never arrived, so they are concealed in place before id 16.
        assert_eq!(j.release_with_target(1120, 2), Released::Repeat(pkt(3)));
    }

    /// A re-sent packet from the start of a cold-start burst is behind the newest
    /// by the whole length of the burst, which is more than the reorder window.
    /// Reading that as a source restart cleared all 21 buffered packets and did
    /// not even count the duplicate.
    #[test]
    fn a_resent_packet_from_the_start_of_a_burst_is_not_a_source_restart() {
        let mut j = jb_holding(64);
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
    /// Debug-only, because the refusal is a `debug_assert`: a live audio relay
    /// should not panic over a bound it can honour, so release builds clamp instead.
    /// Without the gate this test fails under `cargo test --release`.
    #[cfg(debug_assertions)]
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

    /// The floor has to rise at every site that hands a slot to the consumer or
    /// throws it away, not only at a real release. Concealment hands over the slot
    /// too — synthetic audio, but the consumer got it — so a straggler for a
    /// concealed slot is late. Accepting it would leave a packet below the release
    /// position, which nothing downstream can express.
    #[test]
    fn a_straggler_for_a_concealed_slot_is_late() {
        let mut j = jb();
        j.insert(100, pkt(1), 1000);
        j.insert(102, pkt(3), 1001);
        assert_eq!(j.release(1010), Released::Real(pkt(1)));
        assert_eq!(j.release(1020), Released::Repeat(pkt(1))); // 101 concealed

        j.insert(101, pkt(2), 1030);
        assert_eq!(j.late_discards, 1, "a concealed slot was reopened");
        assert_eq!(j.buffered(), 1);
        assert_eq!(j.release(1040), Released::Real(pkt(3)));
    }

    /// Same for the priming trim: those packets were discarded to hold the latency
    /// bound, so their slots must not reopen.
    #[test]
    fn a_straggler_for_a_trimmed_slot_is_late() {
        let mut j = JitterBuffer::new(200, 8, 200, 16, 2);
        j.insert(100, pkt(1), 1000);
        j.release(1010);
        j.release(1300); // -> Outage
        for k in 0..5i32 {
            j.insert(200 + k, pkt(2), 2000);
        }
        assert_eq!(j.buffered(), 2, "the retention cap was not applied");
        assert_eq!(j.next_id(), Some(203));

        j.insert(202, pkt(9), 2001); // trimmed a moment ago
        assert_eq!(j.late_discards, 1, "a trimmed slot was reopened");
        assert_eq!(j.buffered(), 2);
    }

    /// And for the safety valve.
    #[test]
    fn a_straggler_for_a_slot_the_valve_discarded_is_late() {
        let mut j = jb();
        j.insert(100, pkt(1), 1000);
        assert_eq!(j.release(1010), Released::Real(pkt(1)));
        for k in 1..=5i32 {
            j.insert(100 + k, pkt(2), 1000 + k as u64);
        }

        assert_eq!(j.enforce_max_depth(2), 3);
        assert_eq!(j.next_id(), Some(104));
        j.insert(102, pkt(9), 1100);
        assert_eq!(j.late_discards, 1, "a discarded slot was reopened");
        assert_eq!(j.buffered(), 2);
    }

    /// Cold start primes rather than releasing on its first arrival. Occupancy only
    /// grows by concealing, so a buffer that starts in Normal stays pinned at zero depth
    /// for as long as input and output rates match -- it would forward the very
    /// burstiness it exists to absorb. And while priming, the retention cap applies from
    /// the first arrival, so an opening burst cannot exceed the latency bound either.
    #[test]
    fn a_cold_start_primes_and_is_bounded_by_the_retention_cap() {
        let mut j = jb(); // retain_cap 12
        assert_eq!(j.state(), State::Priming);

        j.insert(100, pkt(1), 1000);
        assert_eq!(
            j.release_with_target(1010, 4),
            Released::Silence,
            "released on the first arrival instead of priming"
        );
        for k in 1..40i32 {
            j.insert(100 + k, pkt(2), 1000 + k as u64);
        }
        assert_eq!(j.buffered(), 12, "the opening burst was not bounded");
        assert_eq!(j.release_with_target(1100, 4), Released::Real(pkt(2)));
        assert_eq!(j.state(), State::Normal);
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
        j.on_source_reconnect(); // a sender restart always drops the connection
        j.insert(0, pkt(2), 1100); // pkt_id went back to 0
        assert_eq!(j.resync_events, 1);
        assert_eq!(j.release(1110), Released::Real(pkt(2)));
    }

    /// The safety valve. If the consumer stops reading, discard oldest rather
    /// than grow without bound. Reachable only because `accept_ahead_packets` is
    /// independent of the conceal horizon.
    #[test]
    fn exceeding_the_max_depth_discards_oldest() {
        let mut j = jb_holding(200);
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

    /// The idle rolling window trims with the same discipline as the safety valve
    /// but must not be counted as a resync: nothing is being released, so there is
    /// no timeline to break — and charging normal idling to an alarm counter the
    /// README says should stay near zero would make that counter unreadable.
    #[test]
    fn idle_retention_trims_without_counting_a_resync() {
        let mut j = jb_holding(64);
        for k in 0..20i32 {
            j.insert(100 + k, pkt(1), 1000 + k as u64);
        }
        let resyncs = j.resync_events;

        assert_eq!(j.retain_window(4), 16);
        assert_eq!(j.buffered(), 4);
        assert_eq!(
            j.resync_events, resyncs,
            "idle retention counted as a resync"
        );

        // The floor discipline is unchanged: a straggler for a trimmed slot is late.
        j.insert(100, pkt(9), 2000);
        assert_eq!(j.late_discards, 1, "a trimmed slot was reopened");
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
