use crate::config_file::Config;
use crate::PACKET_N_SAMPLE;
use jack::RingBufferWriter;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::Notify;

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

pub fn inspect_device() -> (jack::Client, usize) {
    let (client, _status) =
        jack::Client::new("rust_client", jack::ClientOptions::NO_START_SERVER).unwrap();

    let in_ports_name = client.ports(Some("capture"), None, jack::PortFlags::IS_PHYSICAL);
    let out_ports_name = client.ports(Some("playback"), None, jack::PortFlags::IS_PHYSICAL);
    println!("physical input: {:?}", in_ports_name);
    println!("physical output: {:?}", out_ports_name);
    (client, in_ports_name.len())
}

pub async fn start_jack_client(
    cfg: Arc<Config>,
    client: jack::Client,
    notifier: Arc<Notify>,
    mut buf_writer: RingBufferWriter,
    shutdown: impl Future,
) {
    let mut i16_buf = [0_i16; PACKET_N_SAMPLE];
    let mut i_period = 0_usize;
    let period = cfg.mic.period;
    let n_period = PACKET_N_SAMPLE / cfg.mic.period;
    let in_ports_name = client.ports(Some("capture"), None, jack::PortFlags::IS_PHYSICAL);
    let out_ports_name = client.ports(Some("playback"), None, jack::PortFlags::IS_PHYSICAL);
    let n_ch = std::cmp::min(in_ports_name.len(), cfg.mic.n_channel);
    let mut n_ch_buf = vec![[0.0_f32; PACKET_N_SAMPLE]; n_ch];

    let mut in_ports = Vec::<jack::Port<jack::AudioIn>>::new();
    for i in 0..in_ports_name.len() {
        in_ports.push(
            client
                .register_port(format!("in_{i}").as_str(), jack::AudioIn::default())
                .unwrap(),
        );
    }

    // {
    let process_callback = move |_: &jack::Client, ps: &jack::ProcessScope| -> jack::Control {
        for (i, port) in in_ports.iter().enumerate() {
            let in_data = port.as_slice(ps);
            let write_pos = i_period * period as usize;
            n_ch_buf[i][write_pos..write_pos + period as usize].copy_from_slice(in_data);
        }
        i_period += 1;
        if i_period == n_period {
            i_period = 0;
            for i in 0..n_ch_buf.len() {
                for j in 0..n_ch_buf[i].len() {
                    i16_buf[j] = pcm_f32_to_i16(n_ch_buf[i][j]);
                }
                buf_writer.write_buffer(slice_i16_to_u8(i16_buf.as_ref()));
                // emit reading signal here
                notifier.notify_one();
            }
        }

        jack::Control::Continue
    };
    let process = jack::ClosureProcessHandler::new(process_callback);
    let active_client = client.activate_async(Notifications, process).unwrap();

    for (i, port_name) in in_ports_name.iter().enumerate() {
        active_client
            .as_client()
            .connect_ports_by_name(port_name, format!("rust_client:in_{i}").as_str())
            .unwrap();
    }
    let (mic_idx, speaker_idx) = (
        cfg.audio_connection.mic_idx as usize,
        cfg.audio_connection.speaker_idx as usize,
    );
    if cfg.audio_connection.connect_mic_speaker
        && in_ports_name.len() > mic_idx
        && out_ports_name.len() > speaker_idx
    {
        active_client
            .as_client()
            .connect_ports_by_name(
                in_ports_name[mic_idx].as_str(),
                out_ports_name[speaker_idx].as_str(),
            )
            .unwrap();
    }

    shutdown.await;
    println!("shutting down jack client");
    active_client.deactivate().unwrap();
    // }
}

#[inline(always)]
fn slice_i16_to_u8(slice: &[i16]) -> &[u8] {
    let byte_len = slice.len() * 2;
    unsafe { std::slice::from_raw_parts(slice.as_ptr().cast::<u8>(), byte_len) }
}

#[inline(always)]
fn pcm_f32_to_i16(s: f32) -> i16 {
    let mut i = (s * 32768.0).round() as i32;
    if i > 32767 {
        i = 32767;
    }
    if i < -32768 {
        i = -32768;
    }
    i as i16
}
