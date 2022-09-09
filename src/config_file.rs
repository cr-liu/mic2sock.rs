use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
};
use toml;

#[derive(Serialize, Deserialize)]
pub struct Config {
    pub mic: MicConfig,
    pub audio_connection: AudioConnection,
    pub tcp: TcpConfig,
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
pub struct AudioConnection {
    pub connect_mic_speaker: bool,
    pub mic_idx: u16,
    pub speaker_idx: u16,
}

#[derive(Serialize, Deserialize)]
pub struct TcpConfig {
    pub listen_port: u16,
    pub max_clients: u16,
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
                        device_name: "hw:seeed8micvoicec".to_string(),
                        device_id: 0,
                        sample_rate: 16000,
                        period: 16,
                        n_channel: 8,
                    },
                    audio_connection: AudioConnection {
                        connect_mic_speaker: false,
                        mic_idx: 0,
                        speaker_idx: 0,
                    },
                    tcp: TcpConfig {
                        listen_port: 2345,
                        max_clients: 10,
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

    fn read_conf_file() -> Result<Config, io::Error> {
        let contents = fs::read_to_string("config.toml")?;
        let conf: Config = toml::from_str(&contents)?;
        Ok(conf)
    }
}
