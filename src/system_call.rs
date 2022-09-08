use crate::config_file::Config;
use std::sync::Arc;
use tokio::process::{Child, Command};

pub fn start_jack(conf: Arc<Config>) -> Child {
    let mut jack_server = Command::new("jackd");
    jack_server.kill_on_drop(true);
    if conf.mic.device_name.to_lowercase().contains("default") {
        jack_server
            .arg("-R")
            .arg(format!("-d{}", conf.mic.driver))
            .arg(format!("-p{}", conf.mic.period));
    } else {
        jack_server
            .arg("-R")
            .arg(format!("-d{}", conf.mic.driver))
            .arg(format!("-d{}", conf.mic.device_name))
            .arg(format!("-p{}", conf.mic.period))
            .arg(format!("-r{}", conf.mic.sample_rate));
    }
    jack_server.spawn().unwrap()
}
