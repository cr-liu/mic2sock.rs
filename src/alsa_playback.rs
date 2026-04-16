use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleRate, StreamConfig, BufferSize};
use ringbuf::traits::{Consumer, Split};
use ringbuf::HeapRb;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub type PlaybackRingProducer = ringbuf::HeapProd<i16>;
pub type PlaybackRingConsumer = ringbuf::HeapCons<i16>;

/// Create a ring buffer for playback.
/// Capacity: 4 × sample_per_packet × n_channel.
pub fn create_playback_ring(sample_per_packet: usize, n_channel: usize) -> (PlaybackRingProducer, PlaybackRingConsumer) {
    let capacity = sample_per_packet * n_channel * 4;
    let rb = HeapRb::<i16>::new(capacity);
    rb.split()
}

/// Start cpal playback.
/// Returns the stream handle (must be kept alive).
pub fn start_playback(
    device_name: &str,
    sample_rate: usize,
    n_channel: usize,
    period: usize,
    mut consumer: PlaybackRingConsumer,
    _shutdown: Arc<AtomicBool>,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    let host = cpal::default_host();

    let device = if device_name.is_empty() || device_name == "default" {
        host.default_output_device()
            .ok_or("no default output device")?
    } else {
        host.output_devices()?
            .find(|d| d.name().map(|n| n.contains(device_name)).unwrap_or(false))
            .ok_or_else(|| format!("output device '{}' not found", device_name))?
    };

    println!("Playback device: {}", device.name().unwrap_or_default());

    let config = StreamConfig {
        channels: n_channel as u16,
        sample_rate: SampleRate(sample_rate as u32),
        buffer_size: BufferSize::Fixed(period as u32),
    };

    let stream = device.build_output_stream(
        &config,
        move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
            let read = consumer.pop_slice(data);
            // Fill remaining with silence if ring buffer underrun
            for sample in &mut data[read..] {
                *sample = 0;
            }
        },
        move |err| {
            eprintln!("Playback stream error: {}", err);
        },
        None,
    )?;

    stream.play()?;
    println!("Playback started ({} ch, {} Hz)", n_channel, sample_rate);

    Ok(stream)
}
