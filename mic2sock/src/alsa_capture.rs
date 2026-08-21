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

use crate::config_file::Config;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
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

/// Opens the capture device and negotiates format/rate/geometry.
///
/// The requested channel count is clamped to what the hardware exposes, the
/// same policy `inspect_device` applied under JACK. The sample rate is NOT
/// negotiable: resampling silently would corrupt the wire timestamps, so a
/// device that cannot do the configured rate is a startup failure (fail loudly
/// rather than stream subtly wrong audio -- repo convention).
pub fn open_capture(cfg: &Config) -> (CaptureDevice, usize) {
    let dev = &cfg.mic.device_name;
    let pcm = PCM::new(dev, Direction::Capture, false)
        .unwrap_or_else(|e| panic!("cannot open capture device {}: {}", dev, e));

    let fmt;
    let n_channel;
    {
        let hwp = HwParams::any(&pcm).unwrap();
        hwp.set_access(Access::RWInterleaved).unwrap();

        // Prefer S16 (wire format, no conversion); fall back to what 16-ch
        // USB arrays actually speak (the RASP-ZX is S24 in 3 bytes).
        fmt = if hwp.set_format(Format::s16()).is_ok() {
            SampleFmt::S16
        } else if hwp.set_format(Format::S243LE).is_ok() {
            SampleFmt::S24In3Bytes
        } else if hwp.set_format(Format::s32()).is_ok() {
            SampleFmt::S32
        } else {
            panic!("{}: no supported sample format (S16/S24_3LE/S32)", dev);
        };

        let max_ch = hwp.get_channels_max().unwrap() as usize;
        n_channel = cfg.mic.n_channel.min(max_ch);
        if n_channel < cfg.mic.n_channel {
            println!("n_mic set to {}", n_channel);
        }
        hwp.set_channels(n_channel as u32).unwrap();

        hwp.set_rate(cfg.mic.sample_rate as u32, ValueOr::Nearest).unwrap();
        let got_rate = hwp.get_rate().unwrap() as usize;
        assert_eq!(
            got_rate, cfg.mic.sample_rate,
            "{} cannot run at {} Hz (offers {})",
            dev, cfg.mic.sample_rate, got_rate
        );

        // Buffer geometry: period/n_period keep their config meaning as the
        // ALSA period size and period count. Nearest is fine here -- unlike
        // the JACK design nothing downstream depends on the exact period.
        hwp.set_period_size_near(cfg.mic.period as i64, ValueOr::Nearest)
            .unwrap();
        hwp.set_periods(cfg.mic.n_period as u32, ValueOr::Nearest).unwrap();

        pcm.hw_params(&hwp).unwrap();
    }

    (
        CaptureDevice { pcm, fmt, n_channel },
        n_channel,
    )
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

/// The capture loop: one blocking read of `sample_per_packet` frames per
/// iteration, packed into a complete wire packet and broadcast. Runs on a
/// plain std::thread; the hardware clock is the pacing.
///
/// On a fatal ALSA error the panic flag is raised and the thread exits; main
/// turns that into a process panic so the supervisor restarts the daemon,
/// the same contract the JACK watchdog implemented.
pub fn run_capture(
    dev: CaptureDevice,
    cfg: Arc<Config>,
    packet_sender: broadcast::Sender<Vec<u8>>,
    shutdown: Arc<AtomicBool>,
    panic_flag: Arc<AtomicBool>,
) {
    try_realtime_priority();

    let spp = cfg.tcp_sender.sample_per_packet;
    let header_len = cfg.tcp_sender.header_len;
    let n_ch = dev.n_channel;
    let pkt_len = header_len + n_ch * spp * 2;
    let packet_time_len = (spp * 1000 / cfg.mic.sample_rate) as i16;
    let device_id = cfg.mic.device_id as u16;
    let bps = dev.fmt.bytes_per_sample();

    let io = dev.pcm.io_bytes();
    let mut raw = vec![0_u8; spp * n_ch * bps];
    let mut pkt_id = 0_i32;
    let mut xruns = 0_u64;

    if let Err(e) = dev.pcm.start() {
        println!("capture start failed: {}", e);
        panic_flag.store(true, Ordering::SeqCst);
        return;
    }

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
                    xruns += 1;
                    println!("capture xrun #{}: {}", xruns, err);
                    if dev.pcm.try_recover(err, true).is_err() {
                        println!("capture device unrecoverable: {}", err);
                        panic_flag.store(true, Ordering::SeqCst);
                        return;
                    }
                    let _ = dev.pcm.start();
                }
            }
        }

        // Header: identical arithmetic to the JACK-era process_send_buf --
        // back-dated by a 10 ms fudge minus one packet duration, so the
        // timestamp names the start of the packet's audio.
        let mut pkt = vec![0_u8; pkt_len];
        let unix_time_in_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            - 10;
        let mut secs = (unix_time_in_millis / 1000) as u32;
        let mut ms = (unix_time_in_millis % 1000) as i16 - packet_time_len;
        if ms < 0 {
            secs -= 1;
            ms += 1000;
        }
        pkt[0..2].copy_from_slice(&device_id.to_le_bytes());
        pkt[2..6].copy_from_slice(&secs.to_le_bytes());
        pkt[6..8].copy_from_slice(&ms.to_le_bytes());
        pkt[8..12].copy_from_slice(&pkt_id.to_le_bytes());

        // Interleaved frames -> channel-blocked wire layout, truncating wide
        // samples to their top 16 bits.
        for ch in 0..n_ch {
            let ch_base = header_len + ch * spp * 2;
            for s in 0..spp {
                let src = (s * n_ch + ch) * bps;
                let (lo, hi) = match dev.fmt {
                    SampleFmt::S16 => (raw[src], raw[src + 1]),
                    SampleFmt::S24In3Bytes => (raw[src + 1], raw[src + 2]),
                    SampleFmt::S32 => (raw[src + 2], raw[src + 3]),
                };
                pkt[ch_base + s * 2] = lo;
                pkt[ch_base + s * 2 + 1] = hi;
            }
        }

        // No subscribers is not an error -- it just means no client and no
        // shim are attached right now.
        let _ = packet_sender.send(pkt);

        pkt_id += 1;
        if pkt_id == i32::MAX {
            pkt_id = 0;
        }
    }
    println!("capture thread exiting ({} xruns)", xruns);
}
