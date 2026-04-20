use serde::{Deserialize, Serialize};
use std::{fs, io::Write};

type Error = Box<dyn std::error::Error + Send + Sync>;

pub const HEADER_LEN: usize = 12;

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub general: GeneralConfig,
    pub capture_device: Vec<CaptureDeviceConfig>,
    pub playback: PlaybackConfig,
    pub sender: SenderConfig,
    pub receiver: ReceiverConfig,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GeneralConfig {
    pub sample_rate: usize,
    pub period: usize,
    pub n_period: usize,
    pub sample_per_packet: usize,
    pub device_id: usize,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CaptureDeviceConfig {
    pub device_name: String,
    pub n_channel: usize,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PlaybackConfig {
    pub device_name: String,
    pub n_channel: usize,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SenderConfig {
    pub protocol: String,
    pub listen_port: usize,
    pub max_clients: usize,
    #[serde(default)]
    pub static_receivers: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ReceiverConfig {
    pub protocol: String,
    pub host: String,
    pub port: usize,
    pub n_channel: usize,
    pub pkt_len: Option<usize>,
}

impl Config {
    pub fn new() -> Config {
        match Config::read_conf_file() {
            Ok(conf) => conf,
            Err(err) => {
                println!("failed reading config.toml! {}", err);
                println!("creating default conf.toml; please rename it to config.toml");
                let conf = Config::default();
                let toml_str = toml::to_string(&conf).unwrap();
                let mut f = fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open("conf.toml")
                    .unwrap();
                f.write_all(toml_str.as_bytes()).unwrap();
                conf
            }
        }
    }

    fn read_conf_file() -> Result<Config, Error> {
        let contents = fs::read_to_string("config.toml")?;
        let conf: Config = toml::from_str(&contents)?;
        Ok(conf)
    }

    pub fn total_capture_channels(&self) -> usize {
        self.capture_device.iter().map(|d| d.n_channel).sum()
    }

    fn default() -> Config {
        Config {
            general: GeneralConfig {
                sample_rate: 16000,
                period: 32,
                n_period: 3,
                sample_per_packet: 32,
                device_id: 0,
            },
            capture_device: vec![CaptureDeviceConfig {
                device_name: "hw:RASPZX16ch".to_string(),
                n_channel: 16,
            }],
            playback: PlaybackConfig {
                device_name: "plughw:Device".to_string(),
                n_channel: 1,
            },
            sender: SenderConfig {
                protocol: "udp".to_string(),
                listen_port: 7998,
                max_clients: 100,
                static_receivers: Vec::new(),
            },
            receiver: ReceiverConfig {
                protocol: "udp".to_string(),
                host: "none".to_string(),
                port: 4000,
                n_channel: 1,
                pkt_len: None,
            },
        }
    }
}
