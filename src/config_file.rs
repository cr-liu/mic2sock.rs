use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
};
use toml;

type Error = Box<dyn std::error::Error + Send + Sync>;

#[derive(Serialize, Deserialize)]
pub struct Config {
    pub mic: MicConfig,
    pub speaker: SpeakerConfig,
    pub audio_connection: AudioConnection,
    pub tcp_sender: TcpSenderConfig,
    pub tcp_receiver: TcpReceiverConfig,
}

#[derive(Serialize, Deserialize)]
pub struct MicConfig {
    pub driver: String,
    pub device_name: String,
    pub device_id: usize,
    pub sample_rate: usize,
    pub period: usize,
    pub n_channel: usize,
}

#[derive(Serialize, Deserialize)]
pub struct SpeakerConfig {
    pub use_alsa_out: bool,
    pub device_name: String,
    pub sample_rate: usize,
    pub period: usize,
    pub n_period: usize,
    pub n_channel: usize,
}

#[derive(Serialize, Deserialize)]
pub struct AudioConnection {
    pub connect_mic_speaker: bool,
    pub mic_idx: usize,
    pub speaker_idx: usize,
}

#[derive(Serialize, Deserialize)]
pub struct TcpSenderConfig {
    pub listen_port: usize,
    pub max_clients: usize,
    pub header_len: usize,
    pub sample_per_packet: usize,
}

#[derive(Serialize, Deserialize)]
pub struct TcpReceiverConfig {
    pub host: String,
    pub port: usize,
    pub header_len: usize,
    pub n_channel: usize,
    pub sample_per_packet: usize,
}

impl Config {
    pub fn new() -> Config {
        match Config::read_conf_file() {
            Ok(conf) => conf,
            Err(err) => {
                println!("failed reading config.toml! {}", err);
                println!("create new config file conf.toml; please rename it to config.toml");
                let conf = Config {
                    mic: MicConfig {
                        driver: "alsa".to_string(),
                        device_name: "hw:RASPZX16ch".to_string(),
                        device_id: 0,
                        sample_rate: 16000,
                        period: 32,
                        n_channel: 16,
                    },
                    speaker: SpeakerConfig { 
                        use_alsa_out: false,
                        device_name: "plughw:Device".to_string(),
                        sample_rate: 16000,
                        period: 32,
                        n_period: 2,
                        n_channel: 1,
                    },
                    audio_connection: AudioConnection {
                        connect_mic_speaker: false,
                        mic_idx: 0,
                        speaker_idx: 0,
                    },
                    tcp_sender: TcpSenderConfig {
                        listen_port: 7998,
                        max_clients: 100,
                        header_len: 12,
                        sample_per_packet: 160,
                    },
                    tcp_receiver: TcpReceiverConfig {
                        host: "none".to_string(),
                        port: 4000,
                        header_len: 16,
                        n_channel: 1,
                        sample_per_packet: 160
                    },
                };
                let toml = toml::to_string(&conf).unwrap();
                let mut f = fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .open("conf.toml")
                    .unwrap();
                f.write_all(toml.as_bytes()).unwrap();
                conf
            }
        }
    }

    fn read_conf_file() -> Result<Config, Error> {
        let contents = fs::read_to_string("config.toml")?;
        let conf: Config = toml::from_str(&contents)?;
        Ok(conf)
    }
}
