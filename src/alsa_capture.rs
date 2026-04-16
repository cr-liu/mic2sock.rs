use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use crossbeam::channel::Sender;
use ringbuf::traits::{Producer, Split};
use ringbuf::HeapRb;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub type CaptureRingProducer = ringbuf::HeapProd<i16>;
pub type CaptureRingConsumer = ringbuf::HeapCons<i16>;

pub struct CaptureDevice {
    pub device_name: String,
    pub n_channel: usize,
}

/// Open and configure an ALSA PCM device for capture.
fn open_capture(
    device_name: &str,
    sample_rate: u32,
    n_channel: u32,
    period: u64,
    n_period: u64,
) -> Result<PCM, Box<dyn std::error::Error>> {
    let pcm = PCM::new(device_name, Direction::Capture, false)?;

    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_format(Format::s16())?;
        hwp.set_rate(sample_rate, ValueOr::Nearest)?;
        hwp.set_channels(n_channel)?;
        hwp.set_period_size(period as alsa::pcm::Frames, ValueOr::Nearest)?;
        hwp.set_buffer_size((period * n_period) as alsa::pcm::Frames)?;
        pcm.hw_params(&hwp)?;
    }

    pcm.prepare()?;
    Ok(pcm)
}

/// Start the primary capture device thread.
/// Sends each period of interleaved i16 samples via crossbeam channel.
pub fn start_primary_capture(
    device: CaptureDevice,
    sample_rate: usize,
    period: usize,
    n_period: usize,
    sender: Sender<Vec<i16>>,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let pcm = match open_capture(
            &device.device_name,
            sample_rate as u32,
            device.n_channel as u32,
            period as u64,
            n_period as u64,
        ) {
            Ok(pcm) => pcm,
            Err(e) => {
                eprintln!("Failed to open primary capture {}: {}", device.device_name, e);
                shutdown.store(true, Ordering::SeqCst);
                return;
            }
        };

        let io = match pcm.io_i16() {
            Ok(io) => io,
            Err(e) => {
                eprintln!("Failed to get IO handle for {}: {}", device.device_name, e);
                shutdown.store(true, Ordering::SeqCst);
                return;
            }
        };

        let frame_size = period * device.n_channel;
        let mut buf = vec![0i16; frame_size];

        println!("Primary capture started: {} ({} ch)", device.device_name, device.n_channel);

        while !shutdown.load(Ordering::Relaxed) {
            match io.readi(&mut buf) {
                Ok(n) => {
                    if n != period {
                        eprintln!("Primary capture: short read {} < {}", n, period);
                    }
                    if sender.send(buf.clone()).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("Primary capture error: {}", e);
                    // Try to recover from xrun
                    if pcm.prepare().is_err() {
                        shutdown.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        }
        println!("Primary capture stopped: {}", device.device_name);
    })
}

/// Start a secondary capture device thread.
/// Writes interleaved i16 samples into a lock-free ring buffer.
pub fn start_secondary_capture(
    device: CaptureDevice,
    sample_rate: usize,
    period: usize,
    n_period: usize,
    mut producer: CaptureRingProducer,
    shutdown: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let pcm = match open_capture(
            &device.device_name,
            sample_rate as u32,
            device.n_channel as u32,
            period as u64,
            n_period as u64,
        ) {
            Ok(pcm) => pcm,
            Err(e) => {
                eprintln!("Failed to open secondary capture {}: {}", device.device_name, e);
                return;
            }
        };

        let io = match pcm.io_i16() {
            Ok(io) => io,
            Err(e) => {
                eprintln!("Failed to get IO handle for {}: {}", device.device_name, e);
                return;
            }
        };

        let frame_size = period * device.n_channel;
        let mut buf = vec![0i16; frame_size];

        println!("Secondary capture started: {} ({} ch)", device.device_name, device.n_channel);

        while !shutdown.load(Ordering::Relaxed) {
            match io.readi(&mut buf) {
                Ok(_) => {
                    producer.push_slice(&buf);
                }
                Err(e) => {
                    eprintln!("Secondary capture {} error: {}", device.device_name, e);
                    if pcm.prepare().is_err() {
                        break;
                    }
                }
            }
        }
        println!("Secondary capture stopped: {}", device.device_name);
    })
}

/// Create a ring buffer for a secondary device.
/// Capacity: 4 × period × n_channel (enough for drift compensation).
pub fn create_ring_buffer(period: usize, n_channel: usize) -> (CaptureRingProducer, CaptureRingConsumer) {
    let capacity = period * n_channel * 4;
    let rb = HeapRb::<i16>::new(capacity);
    rb.split()
}
