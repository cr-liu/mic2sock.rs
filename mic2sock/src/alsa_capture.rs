// Direct ALSA capture, replacing the jackd + JACK-client pair (spec §7).
//
// jackd's ALSA backend assumes one clock for the whole graph; feeding it two
// physical devices (capture array + playback DAC) meant two independent
// crystals under a single-clock model, which is why multi-device setups kept
// failing. Opening the capture device directly removes that constraint and
// the jackd dependency with it.
//
// The capture thread blocks in snd_pcm_readi for exactly one packet's worth
// of frames, so the hardware clock itself paces packet production -- the
// ring buffers, the Notify handshake and the "sample_per_packet must divide
// by period" invariant of the JACK design all disappear.
//
// Device loss is handled in-process: the USB array drops off the bus every
// few days (kernel: "timeout: still N active urbs"), readi then fails with
// EIO and no amount of snd_pcm_recover brings the handle back. The thread
// closes the PCM and reopens it with backoff until the device has
// re-enumerated, then resumes on the same packet-id sequence. Only a device
// that comes back incompatible with the configured geometry is fatal.

use crate::config_file::Config;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};

/// Sample layout the device agreed to. S16 is used as-is; wider formats are
/// truncated to the top 16 bits when packing, which is what jackd's float
/// path effectively delivered downstream anyway.
#[derive(Clone, Copy, PartialEq)]
pub enum SampleFmt {
    S16,
    S24In3Bytes,
    S32,
}

impl SampleFmt {
    fn bytes_per_sample(self) -> usize {
        match self {
            SampleFmt::S16 => 2,
            SampleFmt::S24In3Bytes => 3,
            SampleFmt::S32 => 4,
        }
    }
}

pub struct CaptureDevice {
    pcm: PCM,
    pub fmt: SampleFmt,
    pub n_channel: usize,
}

/// Why a device could not be opened.
///
/// `Transient` is the device being absent, busy or mid-re-enumeration -- the
/// kind of failure that resolves by itself and is worth waiting out.
/// `Fatal` is a device that is present but cannot do the configured geometry:
/// waiting changes nothing, and streaming at a different rate or format would
/// corrupt every downstream timestamp, so the process must fail loudly.
pub enum OpenError {
    Transient(String),
    Fatal(String),
}

/// Sorts an ALSA error by its errno, not by where it happened: a device that
/// vanished mid-open fails `HwParams::any` with ENODEV (transient), and a
/// device we are not allowed to open fails `PCM::new` with EACCES (fatal) --
/// so classifying by call site retried the hopeless case forever and killed
/// the process on the recoverable one (review round 15).
fn classify(dev: &str, what: &str, e: alsa::Error) -> OpenError {
    let msg = format!("{}: {}: {}", dev, what, e);
    match e.errno() as i32 {
        libc::ENODEV | libc::ENXIO | libc::ENOENT | libc::EIO | libc::EBUSY | libc::EAGAIN
        | libc::EINTR | libc::EPIPE | libc::ESTRPIPE => OpenError::Transient(msg),
        _ => OpenError::Fatal(msg),
    }
}

/// Opens the capture device and negotiates format/rate/geometry.
///
/// The requested channel count is clamped to what the hardware exposes, the
/// same policy `inspect_device` applied under JACK. The sample rate is NOT
/// negotiable: resampling silently would corrupt the wire timestamps, so a
/// device that cannot do the configured rate is `Fatal` (fail loudly rather
/// than stream subtly wrong audio -- repo convention).
pub fn open_capture(cfg: &Config) -> Result<CaptureDevice, OpenError> {
    let dev = &cfg.mic.device_name;
    let pcm = PCM::new(dev, Direction::Capture, false).map_err(|e| classify(dev, "open", e))?;

    let fatal = |what: &str, e: alsa::Error| classify(dev, what, e);

    let fmt;
    let n_channel;
    {
        let hwp = HwParams::any(&pcm).map_err(|e| fatal("hw_params any", e))?;
        hwp.set_access(Access::RWInterleaved)
            .map_err(|e| fatal("interleaved access", e))?;

        // Prefer S16 (wire format, no conversion); fall back to what 16-ch
        // USB arrays actually speak (the RASP-ZX is S24 in 3 bytes).
        fmt = if hwp.set_format(Format::s16()).is_ok() {
            SampleFmt::S16
        } else if hwp.set_format(Format::S243LE).is_ok() {
            SampleFmt::S24In3Bytes
        } else if hwp.set_format(Format::s32()).is_ok() {
            SampleFmt::S32
        } else {
            return Err(OpenError::Fatal(format!(
                "{}: no supported sample format (S16/S24_3LE/S32)",
                dev
            )));
        };

        let max_ch = hwp
            .get_channels_max()
            .map_err(|e| fatal("channels_max", e))? as usize;
        n_channel = cfg.mic.n_channel.min(max_ch);
        if n_channel < cfg.mic.n_channel {
            println!("n_mic set to {}", n_channel);
        }
        hwp.set_channels(n_channel as u32)
            .map_err(|e| fatal("set channels", e))?;

        hwp.set_rate(cfg.mic.sample_rate as u32, ValueOr::Nearest)
            .map_err(|e| fatal("set rate", e))?;
        let got_rate = hwp.get_rate().map_err(|e| fatal("get rate", e))? as usize;
        if got_rate != cfg.mic.sample_rate {
            return Err(OpenError::Fatal(format!(
                "{} cannot run at {} Hz (offers {})",
                dev, cfg.mic.sample_rate, got_rate
            )));
        }

        // Buffer geometry: period/n_period keep their config meaning as the
        // ALSA period size and period count. Nearest is fine here -- unlike
        // the JACK design nothing downstream depends on the exact period.
        hwp.set_period_size_near(cfg.mic.period as i64, ValueOr::Nearest)
            .map_err(|e| fatal("period size", e))?;
        hwp.set_periods(cfg.mic.n_period as u32, ValueOr::Nearest)
            .map_err(|e| fatal("periods", e))?;

        // Installing the parameters is the one step that touches the device
        // for real; a device that just vanished fails here with EIO/ENODEV
        // (transient), an incompatible parameter set with EINVAL (fatal).
        pcm.hw_params(&hwp).map_err(|e| fatal("hw_params", e))?;
    }

    Ok(CaptureDevice { pcm, fmt, n_channel })
}

/// Longest wait between reopen attempts. USB re-enumeration takes a few
/// seconds; a device that is gone for longer is probably unplugged, and one
/// log line per eight seconds is a tolerable heartbeat while it is.
const REOPEN_BACKOFF_MAX: Duration = Duration::from_secs(8);

/// How long a device must have streamed for its next loss to count as a fresh
/// incident (backoff reset) rather than the same one still going on.
const HEALTHY_RUN: Duration = Duration::from_secs(5);

/// Opens the device, retrying transient failures with doubling backoff
/// (1, 2, 4, 8, 8, ... s) until it succeeds, the process is shutting down
/// (`None`), or the failure is fatal (panics -- see `OpenError`).
///
/// Used both at startup, where the array may not have enumerated yet when the
/// fleet brings modules up at boot, and after an unrecoverable read error.
pub fn open_with_retry(cfg: &Config, shutdown: &AtomicBool) -> Option<CaptureDevice> {
    let mut backoff = Duration::from_secs(1);
    let mut attempt = 0u32;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return None;
        }
        match open_capture(cfg) {
            Ok(dev) => {
                if attempt > 0 {
                    println!("capture device reopened after {} attempt(s)", attempt);
                }
                return Some(dev);
            }
            Err(OpenError::Fatal(msg)) => panic!("capture device unusable: {}", msg),
            Err(OpenError::Transient(msg)) => {
                attempt += 1;
                println!(
                    "capture device unavailable ({}); retrying in {}s (attempt {})",
                    msg,
                    backoff.as_secs(),
                    attempt
                );
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(REOPEN_BACKOFF_MAX);
            }
        }
    }
}

/// Tries to give the capture thread the priority jackd's -R used to arrange.
/// Failure is a warning, not an error: at a 10 ms cadence a normal-priority
/// thread usually keeps up, and the daemon must still run where rtprio
/// limits are absent.
fn try_realtime_priority() {
    unsafe {
        if libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) != 0 {
            println!("mlockall failed (non-fatal)");
        }
        let param = libc::sched_param { sched_priority: 80 };
        if libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) != 0 {
            println!("SCHED_FIFO unavailable (check rtprio limits); running at normal priority");
        }
    }
}

/// Wire-packet packing state that outlives any one device handle: the packet
/// id must keep counting across a reopen, because a reopen is not a restart --
/// the consumer's shim distinguishes the two exactly by whether the id resets.
struct Packer {
    spp: usize,
    header_len: usize,
    n_ch: usize,
    pkt_len: usize,
    packet_time_len: i16,
    period_ms: u128,
    device_id: u16,
    pkt_id: i32,
}

impl Packer {
    fn pack(&mut self, raw: &[u8], fmt: SampleFmt) -> Vec<u8> {
        let bps = fmt.bytes_per_sample();
        // Header: back-dated by one hardware period plus one packet duration,
        // so the timestamp names the start of the packet's audio. The JACK-era
        // code used a flat 10 ms here, sized for 160-sample packets and a jackd
        // buffer that no longer exists; at 2 ms packets it overstated the
        // capture latency fourfold and every downstream latency figure with it.
        let mut pkt = vec![0_u8; self.pkt_len];
        let unix_time_in_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            - self.period_ms;
        let mut secs = (unix_time_in_millis / 1000) as u32;
        let mut ms = (unix_time_in_millis % 1000) as i16 - self.packet_time_len;
        if ms < 0 {
            secs -= 1;
            ms += 1000;
        }
        pkt[0..2].copy_from_slice(&self.device_id.to_le_bytes());
        pkt[2..6].copy_from_slice(&secs.to_le_bytes());
        pkt[6..8].copy_from_slice(&ms.to_le_bytes());
        pkt[8..12].copy_from_slice(&self.pkt_id.to_le_bytes());

        // Interleaved frames -> channel-blocked wire layout, truncating wide
        // samples to their top 16 bits.
        for ch in 0..self.n_ch {
            let ch_base = self.header_len + ch * self.spp * 2;
            for s in 0..self.spp {
                let src = (s * self.n_ch + ch) * bps;
                let (lo, hi) = match fmt {
                    SampleFmt::S16 => (raw[src], raw[src + 1]),
                    SampleFmt::S24In3Bytes => (raw[src + 1], raw[src + 2]),
                    SampleFmt::S32 => (raw[src + 2], raw[src + 3]),
                };
                pkt[ch_base + s * 2] = lo;
                pkt[ch_base + s * 2 + 1] = hi;
            }
        }

        self.pkt_id += 1;
        if self.pkt_id == i32::MAX {
            self.pkt_id = 0;
        }
        pkt
    }
}

/// Reads and publishes packets from one device handle until shutdown (`Ok`)
/// or an error that `snd_pcm_recover` cannot undo (`Err`, with the reason).
fn capture_from(
    dev: &CaptureDevice,
    packer: &mut Packer,
    packet_sender: &broadcast::Sender<Vec<u8>>,
    shutdown: &AtomicBool,
    xruns: &mut u64,
) -> Result<(), String> {
    let bps = dev.fmt.bytes_per_sample();
    let (spp, n_ch) = (packer.spp, packer.n_ch);
    let io = dev.pcm.io_bytes();
    let mut raw = vec![0_u8; spp * n_ch * bps];

    dev.pcm
        .start()
        .map_err(|e| format!("capture start failed: {}", e))?;

    while !shutdown.load(Ordering::SeqCst) {
        // Fill one packet's worth of frames, tolerating short reads.
        let mut filled = 0usize; // in frames
        while filled < spp {
            let want = &mut raw[filled * n_ch * bps..spp * n_ch * bps];
            match io.readi(want) {
                Ok(frames) => filled += frames,
                Err(err) => {
                    // Over/under-run or suspend: recover and continue. Same
                    // weakness as the JACK xrun callback -- the lost frames
                    // shift the timeline silently -- but no worse (§7.3
                    // documents the timestamp-based fix as future work).
                    *xruns += 1;
                    println!("capture xrun #{}: {}", xruns, err);
                    if dev.pcm.try_recover(err, true).is_err() {
                        // EIO/ENODEV: the handle is dead because the device
                        // is. Hand back to the reopen loop.
                        return Err(format!("{}", err));
                    }
                    let _ = dev.pcm.start();
                }
            }
        }

        // No subscribers is not an error -- it just means no client and no
        // shim are attached right now.
        let _ = packet_sender.send(packer.pack(&raw, dev.fmt));
    }
    Ok(())
}

/// The capture thread: one blocking read of `sample_per_packet` frames per
/// iteration, packed into a complete wire packet and broadcast. Runs on a
/// plain std::thread; the hardware clock is the pacing.
///
/// Owns the device for its whole life. When a read error proves the device
/// gone, the handle is dropped and reopened with backoff; the stream resumes
/// on the same packet-id sequence, so the shim sees an outage, not a restart.
/// A device that comes back with a different channel count would change the
/// wire geometry mid-stream, which nothing downstream can express -- that, and
/// a fatal open error, raise the panic flag so main fails the process loudly.
pub fn run_capture(
    dev: CaptureDevice,
    cfg: Arc<Config>,
    packet_sender: broadcast::Sender<Vec<u8>>,
    shutdown: Arc<AtomicBool>,
    panic_flag: Arc<AtomicBool>,
) {
    try_realtime_priority();

    let spp = cfg.tcp_sender.sample_per_packet;
    let n_ch = dev.n_channel;
    let mut packer = Packer {
        spp,
        header_len: cfg.tcp_sender.header_len,
        n_ch,
        pkt_len: cfg.tcp_sender.header_len + n_ch * spp * 2,
        packet_time_len: (spp * 1000 / cfg.mic.sample_rate) as i16,
        // readi returns once the last frame of the packet has reached the
        // buffer, and ALSA hands frames over one hardware period at a time, so
        // the packet's first sample was captured about one packet plus one
        // period ago.
        period_ms: (cfg.mic.period * 1000 / cfg.mic.sample_rate).max(1) as u128,
        device_id: cfg.mic.device_id as u16,
        pkt_id: 0,
    };
    let mut xruns = 0_u64;
    let mut reopens = 0_u64;
    let mut dev = dev;
    // Backoff for the open-succeeds-then-dies-at-once cycle. open_with_retry
    // only waits between FAILED opens; a device that opens fine and then
    // returns ENODATA/EIO on the first read would otherwise be reopened in a
    // tight loop with no sleep at all -- a log flood, and under SCHED_FIFO a
    // core pinned. The backoff grows while the device keeps dying young and
    // resets only after it has streamed for HEALTHY_RUN.
    let mut reopen_backoff = Duration::from_secs(1);
    let mut opened_at = Instant::now();

    loop {
        match capture_from(&dev, &mut packer, &packet_sender, &shutdown, &mut xruns) {
            Ok(()) => break,
            Err(reason) => {
                println!("capture device lost ({}); closing and reopening", reason);
            }
        }
        drop(dev);
        if opened_at.elapsed() >= HEALTHY_RUN {
            reopen_backoff = Duration::from_secs(1);
        } else {
            println!(
                "capture device died {:.1}s after opening; waiting {}s before reopening",
                opened_at.elapsed().as_secs_f64(),
                reopen_backoff.as_secs()
            );
            let deadline = Instant::now() + reopen_backoff;
            while Instant::now() < deadline {
                if shutdown.load(Ordering::SeqCst) {
                    println!("capture thread exiting during reopen backoff");
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            reopen_backoff = (reopen_backoff * 2).min(REOPEN_BACKOFF_MAX);
        }
        // The panic path for a fatal open lives inside open_with_retry; a
        // panic on this thread must still fail the process, hence the guard.
        let reopened = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            open_with_retry(&cfg, &shutdown)
        })) {
            Ok(Some(d)) => d,
            Ok(None) => break, // shutdown requested while waiting
            Err(_) => {
                panic_flag.store(true, Ordering::SeqCst);
                return;
            }
        };
        if reopened.n_channel != n_ch {
            println!(
                "capture device came back with {} channels, was {}: wire geometry cannot change mid-stream",
                reopened.n_channel, n_ch
            );
            panic_flag.store(true, Ordering::SeqCst);
            return;
        }
        reopens += 1;
        opened_at = Instant::now();
        println!("capture resumed (reopen #{}, pkt_id continues at {})", reopens, packer.pkt_id);
        dev = reopened;
    }
    println!(
        "capture thread exiting ({} xruns, {} reopens)",
        xruns, reopens
    );
}
