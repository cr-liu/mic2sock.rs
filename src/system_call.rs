use tokio::process::Command;
use std::sync::Arc;
use std::future::Future;
use crate::config_file::Config;

pub async fn start_jack(conf: Arc<Config>, shutdown: impl Future) {
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
    let mut child = jack_server.spawn().unwrap();

    tokio::select! {
        _ = child.wait() => {
            println!("failed to start jack server");
        }
        _ = shutdown => {
            println!("\nshutting down jack");
        }
    }
}