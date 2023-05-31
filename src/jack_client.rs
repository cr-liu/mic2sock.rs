use crate::config_file::Config;
use jack::{RingBufferWriter, RingBufferReader};
use std::sync::Arc;
use tokio::sync::Notify;
use crossbeam::channel::Receiver;

struct Notifications;

impl jack::NotificationHandler for Notifications {
    fn thread_init(&self, _: &jack::Client) {
        println!("JACK: thread init");
    }

    fn shutdown(&mut self, status: jack::ClientStatus, reason: &str) {
        println!(
            "JACK: shutdown with status {:?} because \"{}\"",
            status, reason
        );
    }

    fn freewheel(&mut self, _: &jack::Client, is_enabled: bool) {
        println!(
            "JACK: freewheel mode is {}",
            if is_enabled { "on" } else { "off" }
        );
    }

    fn sample_rate(&mut self, _: &jack::Client, srate: jack::Frames) -> jack::Control {
        println!("JACK: sample rate changed to {}", srate);
        jack::Control::Continue
    }

    fn client_registration(&mut self, _: &jack::Client, name: &str, is_reg: bool) {
        println!(
            "JACK: {} client with name \"{}\"",
            if is_reg { "registered" } else { "unregistered" },
            name
        );
    }

    fn port_registration(&mut self, _: &jack::Client, port_id: jack::PortId, is_reg: bool) {
        println!(
            "JACK: {} port with id {}",
            if is_reg { "registered" } else { "unregistered" },
            port_id
        );
    }

    fn port_rename(
        &mut self,
        _: &jack::Client,
        port_id: jack::PortId,
        old_name: &str,
        new_name: &str,
    ) -> jack::Control {
        println!(
            "JACK: port with id {} renamed from {} to {}",
            port_id, old_name, new_name
        );
        jack::Control::Continue
    }

    fn ports_connected(
        &mut self,
        _: &jack::Client,
        port_id_a: jack::PortId,
        port_id_b: jack::PortId,
        are_connected: bool,
    ) {
        println!(
            "JACK: ports with id {} and {} are {}",
            port_id_a,
            port_id_b,
            if are_connected {
                "connected"
            } else {
                "disconnected"
            }
        );
    }

    fn graph_reorder(&mut self, _: &jack::Client) -> jack::Control {
        println!("JACK: graph reordered");
        jack::Control::Continue
    }

    fn xrun(&mut self, _: &jack::Client) -> jack::Control {
        println!("JACK: xrun occurred! consider increasing period");
        jack::Control::Continue
    }
}

pub fn inspect_device() -> (jack::Client, usize, usize) {
    let (client, _status) =
        jack::Client::new("rust_client", jack::ClientOptions::NO_START_SERVER).unwrap();

    let in_ports_name = client.ports(Some("capture"), None, jack::PortFlags::IS_PHYSICAL);
    let out_ports_name = client.ports(Some("playback"), None, jack::PortFlags::IS_INPUT);
    println!("physical input: {:?}", in_ports_name);
    println!("physical output: {:?}", out_ports_name);
    (client, in_ports_name.len(), out_ports_name.len())
}

pub fn start_jack_client(
    cfg: Arc<Config>,
    client: jack::Client,
    notifier: Arc<Notify>,
    mut buf_writers: Vec<RingBufferWriter>,
    mut playback_buf_readers: Vec<RingBufferReader>,
    shutdown: Receiver<()>,
) {
    let mut i16_buf = vec![0_i16; cfg.mic.period];
    let period = cfg.mic.period;
    let sample_per_packet = cfg.tcp_sender.sample_per_packet;
    let mut i_sample : usize = 0;

    let in_ports_name = client.ports(Some("capture"), None, jack::PortFlags::IS_PHYSICAL);
    let out_ports_name = client.ports(Some("playback"), None, jack::PortFlags::IS_INPUT);

    let mut in_ports = Vec::<jack::Port<jack::AudioIn>>::new();
    for i in 0..cfg.mic.n_channel {
        in_ports.push(
            client
                .register_port(format!("in_{i}").as_str(), jack::AudioIn::default())
                .unwrap()
        );
    }
    let mut out_ports = Vec::<jack::Port<jack::AudioOut>>::new();
    for i in 0..cfg.speaker.n_channel {
        out_ports.push(
            client
                .register_port(format!("out_{i}").as_str(), jack::AudioOut::default())
                .unwrap()
        );
        
    }

    // jack client will call this function each period
    let mut _fade_in = 0.01;
    let process_callback = move |_: &jack::Client, ps: &jack::ProcessScope| -> jack::Control {
        for (i, port) in in_ports.iter().enumerate() {
            let in_data = port.as_slice(ps);
            assert_eq!(in_data.len(), period);
            for j in 0..period {
                i16_buf[j] = pcm_f32_to_i16(in_data[j]);
            }
            buf_writers[i].write_buffer(slice_i16_to_u8(i16_buf.as_slice()));
        }
        i_sample += period;
        if i_sample >= sample_per_packet {
            notifier.notify_one();
            i_sample -= sample_per_packet;
        }

        let mut playback_data_available = true;
        if out_ports.len() == 0 || playback_buf_readers[0].space() < period * 2 { 
            playback_data_available = false;
        }
        for i in 0..out_ports.len() {
            let out_data_mut= out_ports[i].as_mut_slice(ps);
            if playback_data_available {
                let _n_bytes = playback_buf_readers[i].read_buffer(slice_i16_to_u8_mut(i16_buf.as_mut_slice())); 
                assert_eq!(_n_bytes, period *2);
            }
            for j in 0..out_data_mut.len() {
                if playback_data_available {
                    out_data_mut[j] = pcm_i16_to_f32(i16_buf[j]);// * fade_in;
                } else {
                    out_data_mut[j] = 0.0;
                }
            }
        }
        if _fade_in < 1.0 {
            _fade_in += 0.01;
        }

        jack::Control::Continue
    };

    let process = jack::ClosureProcessHandler::new(process_callback);
    let active_client = 
        client.activate_async(Notifications, process).unwrap();

    for i in 0..cfg.mic.n_channel {
        active_client
            .as_client()
            .connect_ports_by_name(&in_ports_name[i], format!("rust_client:in_{i}").as_str())
            .unwrap();
    }

    for i in 0..cfg.speaker.n_channel {
        active_client
            .as_client()
            .connect_ports_by_name(format!("rust_client:out_{i}").as_str(), &out_ports_name[i])
            .unwrap();
    }

    if cfg.audio_connection.connect_mic_speaker
        && cfg.mic.n_channel > cfg.audio_connection.mic_idx
        && cfg.speaker.n_channel > cfg.audio_connection.speaker_idx
    {
        active_client
            .as_client()
            .connect_ports_by_name(
                &in_ports_name[cfg.audio_connection.mic_idx].as_str(),
                &out_ports_name[cfg.audio_connection.speaker_idx].as_str(),
            )
            .unwrap();
    }

    let _ = shutdown.recv();
    println!("shutting down jack client");
    active_client.deactivate().unwrap();
}

#[inline(always)]
fn slice_i16_to_u8(slice: &[i16]) -> &[u8] {
    let byte_len = slice.len() * 2;
    unsafe { std::slice::from_raw_parts(slice.as_ptr().cast::<u8>(), byte_len) }
}

fn slice_i16_to_u8_mut(slice: &mut [i16]) -> &mut [u8] {
    let byte_len = slice.len() * 2;
    unsafe { std::slice::from_raw_parts_mut(slice.as_ptr().cast::<u8>().cast_mut(), byte_len) }
}

#[inline(always)]
fn pcm_f32_to_i16(s: f32) -> i16 {
    let mut i = (s * 32768.0).round() as i32;
    if i > 32767 { i = 32767; }
    if i < -32768 { i = -32768; }
    i as i16
}

#[inline(always)]
fn pcm_i16_to_f32(s: i16) -> f32 {
    let mut f = s as f32 / 32768.0;
    if f > 1.0 { f = 1.0; }
    if f < -1.0 { f = -1.0; }
    f
}
