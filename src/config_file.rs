use std::{io::{self, Write}, fs};
use serde::{Serialize, Deserialize};
use toml;

#[derive(Serialize, Deserialize)]
pub struct Config {
    pub mic: MicConfig,
    pub tcp: TcpConfig,
}

#[derive(Serialize, Deserialize)]
pub struct MicConfig {
    pub driver: String,
    pub device_name: String,
    pub sample_rate: u16,
    pub period: u16,
}

#[derive(Serialize, Deserialize)]
pub struct TcpConfig {
    pub listen_port: u16,
    pub max_clients: u16,
}

impl Config {
    pub fn new() -> Config {
        match Config::read_conf_file() {
            Ok(conf) => {
                conf
            }
            Err(err) => {
                println!("Failed reading config.toml! {}", err);
                let conf = Config {
                    mic: MicConfig {
                        driver: "alsa".to_string(),
                        device_name: "hw:seeed8micvoicec".to_string(),
                        sample_rate: 16000,
                        period: 16,
                    },
                    tcp: TcpConfig {
                        listen_port: 2345,
                        max_clients: 10,
                    },
                };
                let toml = toml::to_string(&conf).unwrap();
                let mut f = fs::OpenOptions::new().write(true).create(true)
                        .open("conf.toml").unwrap();
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