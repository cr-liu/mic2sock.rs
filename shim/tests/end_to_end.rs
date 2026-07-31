//! End-to-end: a fake Pi feeds the shim's source, a fake consumer reads its
//! sink, and the output must be correctly framed, gapless in id, and
//! channel-aligned.
//!
//! The pipeline is driven through `pipeline::run_with_sink` rather than by
//! spawning the binary, so a failure points at a module instead of a process.
//! Both endpoints (`Sink::bind` here, `TcpListener::bind` for the fake Pi) bind
//! port 0 exactly once and read back the assigned port -- never bind-then-drop
//! a "free" port, which would race the next test picking it up before this one
//! rebinds it.
//!
//! The geometry is the real, pinned production shape (17 channels, 160
//! samples/packet, 5452-byte packets): `Config::validate` refuses anything
//! smaller, so there is no scaled-down geometry available to test against.

use protocol::block::{deblock_channel, reblock_channel};
use protocol::{Header, PacketLayout};
use shim_lib::{parse_config, pipeline, sink::Sink};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

/// The production geometry. `Config::validate` pins `n_ch`/`spp_out`/
/// `header_len`/`sample_rate` to exactly these values, so this is not a choice
/// -- it is the only shape `parse_config` will accept.
fn layout() -> PacketLayout {
    PacketLayout::new(17, 160, 12)
}

/// Builds one well-formed input packet. Every channel carries the same 0..spp
/// ramp shifted by `channel * 1000` (max offset 16000, comfortably inside
/// `i16` with the ramp itself, so nothing saturates). That constant per-channel
/// shift is what a downstream echo-canceller depends on, and it is also what
/// makes the check exact after resampling: `Resampler` evaluates every channel
/// at the same fractional phase (see `clocksync::Resampler`), and the Hermite
/// basis it uses is an affine combination of the four taps whose coefficients
/// sum to zero except for the constant term -- so shifting every tap of one
/// channel by a fixed integer shifts its interpolated output by exactly that
/// integer, with no rounding-tie divergence between channels (`to_i16` rounds
/// half-up, which `clocksync::hermite::to_i16`'s doc comment pins as exactly
/// translation-invariant away from saturation). Concretely: this is *not* the
/// same thing as building each channel's value with a modulo and then adding
/// the per-channel offset afterwards -- that would wrap unpredictably once the
/// sum passed the modulus and break the constant-offset property the check
/// relies on.
fn make_packet(l: &PacketLayout, pkt_id: i32) -> Vec<u8> {
    let mut buf = vec![0u8; l.packet_len()];
    Header {
        device_id: 7,
        secs: 1_700_000_000,
        ms: 0,
        pkt_id,
    }
    .write_to(&mut buf);
    for c in 0..l.n_ch {
        let samples: Vec<i16> = (0..l.spp).map(|i| i as i16 + (c as i16) * 1000).collect();
        reblock_channel(&mut buf, l, c, &samples);
    }
    buf
}

/// Plays the role of the Pi: accepts one connection on an already-bound
/// listener and emits `count` real, sequential headers, skipping the ids in
/// `skip`.
///
/// `TcpSource`'s geometry cross-check inspects only the first
/// `GEOMETRY_CHECK_PACKETS` (8) *framed* packets of a connection
/// (`shim/src/source.rs`), and treats a gap inside that window as a fatal
/// configuration mismatch -- fatal enough that it calls `std::process::exit`,
/// which tears down the whole test binary rather than failing one test.
/// Callers must therefore keep every id in `skip` at 8 or above.
async fn fake_pi(listener: TcpListener, count: i32, skip: Vec<i32>) {
    let l = layout();
    let (mut sock, _) = listener.accept().await.expect("fake Pi: accept failed");
    for id in 0..count {
        if skip.contains(&id) {
            continue;
        }
        if sock.write_all(&make_packet(&l, id)).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    // Keep the socket open a little past the last write so a test that is
    // still reading is never met with an EOF it did not ask for. This is not
    // a synchronization wait -- nothing here is being awaited for readiness --
    // it just holds a resource open past its last use.
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// Builds a config against the pinned production geometry. Only the fields
/// that vary between tests are set explicitly; the rest -- including
/// `n_ch`/`spp_out`/`header_len`/`sample_rate`, which already default to the
/// production values -- take whatever `parse_config` fills in.
fn test_config(source_port: u16, sink_port: u16) -> shim_lib::Config {
    let text = format!(
        "source_host = \"127.0.0.1\"\nsource_port = {}\nsink_port = {}\n",
        source_port, sink_port
    );
    parse_config(&text).expect("test config must be valid")
}

/// Retries the connect until the sink's listener has a pending connection to
/// hand over. There is no notification for "the listener is ready to accept",
/// so a short poll is the only observable event available before the first
/// successful connect; the surrounding `timeout` is what keeps a wiring
/// mistake from hanging the test instead of failing it.
async fn connect_retrying(port: u16) -> TcpStream {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(s) = TcpStream::connect(("127.0.0.1", port)).await {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("shim sink never accepted a connection")
}

/// Reads one whole output packet, with a timeout so a stalled pipeline fails
/// the test instead of hanging the run.
async fn read_one_packet(consumer: &mut TcpStream, want: usize) -> Vec<u8> {
    let mut buf = vec![0u8; want];
    timeout(Duration::from_secs(10), consumer.read_exact(&mut buf))
        .await
        .expect("timed out waiting for an output packet")
        .expect("consumer read failed");
    buf
}

#[tokio::test]
async fn clean_stream_arrives_correctly_framed_and_channel_aligned() {
    let l = layout();
    let pi_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pi_port = pi_listener.local_addr().unwrap().port();
    tokio::spawn(fake_pi(pi_listener, 60, vec![]));

    let sink = Sink::bind("127.0.0.1:0", 3, l.packet_len()).await.unwrap();
    let sink_port = sink.local_addr().unwrap().port();
    tokio::spawn(pipeline::run_with_sink(
        test_config(pi_port, sink_port),
        sink,
    ));

    let mut consumer = connect_retrying(sink_port).await;
    let mut prev_id: Option<i32> = None;
    let mut c0 = vec![0i16; l.spp];
    let mut cx = vec![0i16; l.spp];

    for _ in 0..8 {
        let buf = read_one_packet(&mut consumer, l.packet_len()).await;

        let h = Header::parse(&buf).expect("output packet too short to hold a header");
        if let Some(p) = prev_id {
            assert_eq!(h.pkt_id, p + 1, "output ids must be gapless");
        }
        prev_id = Some(h.pkt_id);

        // Channel c is channel 0 plus c*1000, by construction. This is the
        // property that matters: reframing must not disturb inter-channel
        // relationships, because the downstream echo-canceller depends on them.
        deblock_channel(&buf, &l, 0, &mut c0);
        for c in 1..l.n_ch {
            deblock_channel(&buf, &l, c, &mut cx);
            for i in 0..l.spp {
                assert_eq!(
                    cx[i].wrapping_sub(c0[i]),
                    (c * 1000) as i16,
                    "inter-channel offset broken: output packet {} channel {} sample {}",
                    h.pkt_id,
                    c,
                    i
                );
            }
        }
    }
}

/// A gap in the source must still yield a gapless id sequence downstream: the
/// consumer's behaviour on an id jump is unknown, so it must never see one.
///
/// This holds unconditionally in `Reframer::drain` -- `out_pkt_id` advances by
/// exactly one for every emitted packet regardless of whether it carried real
/// audio, a repeat, or silence -- so this test is exercising that guarantee
/// end-to-end rather than a timing-sensitive path.
#[tokio::test]
async fn a_source_gap_still_yields_a_gapless_output_id_sequence() {
    let l = layout();
    let pi_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pi_port = pi_listener.local_addr().unwrap().port();
    // All three skipped ids are well past the first 8 packets that
    // `TcpSource`'s geometry cross-check inspects (see `fake_pi`'s doc
    // comment); a gap inside that window would exit the process instead of
    // failing this test.
    tokio::spawn(fake_pi(pi_listener, 80, vec![20, 21, 40]));

    let sink = Sink::bind("127.0.0.1:0", 3, l.packet_len()).await.unwrap();
    let sink_port = sink.local_addr().unwrap().port();
    tokio::spawn(pipeline::run_with_sink(
        test_config(pi_port, sink_port),
        sink,
    ));

    let mut consumer = connect_retrying(sink_port).await;
    let mut ids = Vec::new();
    for _ in 0..40 {
        let buf = read_one_packet(&mut consumer, l.packet_len()).await;
        ids.push(
            Header::parse(&buf)
                .expect("output packet too short to hold a header")
                .pkt_id,
        );
    }
    for w in ids.windows(2) {
        assert_eq!(w[1], w[0] + 1, "id gap in output: {:?}", ids);
    }
}

/// Every output packet must be exactly `packet_len()` bytes, and the stream as
/// a whole a clean multiple of it.
///
/// TCP carries no framing of its own, so a wrong packet length is not
/// something a consumer can observe directly -- there is no length field to
/// check it against. The only way to catch it from outside is indirectly: read
/// exactly `packet_len()` bytes per iteration (so a short or long packet would
/// misalign every following read) and confirm the header at each boundary
/// keeps advancing by 1. A drift of even one byte would very quickly land a
/// "header" on the wrong data and break that sequence.
#[tokio::test]
async fn every_output_packet_has_exactly_the_expected_length() {
    let l = layout();
    let want = l.packet_len();
    let pi_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pi_port = pi_listener.local_addr().unwrap().port();
    tokio::spawn(fake_pi(pi_listener, 60, vec![]));

    let sink = Sink::bind("127.0.0.1:0", 3, want).await.unwrap();
    let sink_port = sink.local_addr().unwrap().port();
    tokio::spawn(pipeline::run_with_sink(
        test_config(pi_port, sink_port),
        sink,
    ));

    let mut consumer = connect_retrying(sink_port).await;
    const N: usize = 30;
    let mut prev_id: Option<i32> = None;
    let mut total = 0usize;
    for _ in 0..N {
        let buf = read_one_packet(&mut consumer, want).await;
        assert_eq!(
            buf.len(),
            want,
            "a short or long packet shifts every later boundary"
        );
        total += buf.len();
        let id = Header::parse(&buf)
            .expect("header did not parse at the expected boundary")
            .pkt_id;
        if let Some(p) = prev_id {
            assert_eq!(id, p + 1, "boundary drifted: ids stopped advancing by 1");
        }
        prev_id = Some(id);
    }
    assert_eq!(
        total % want,
        0,
        "{} bytes is not a whole number of packets",
        total
    );
}
