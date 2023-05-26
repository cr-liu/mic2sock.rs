use crate::config_file::Config;
use std::sync::Arc;
use tokio::process::{Child, Command};

#[inline(always)]
pub fn start_jackd(conf: Arc<Config>) -> Child {
    let mut jack_server = Command::new("jackd");
    jack_server.kill_on_drop(true);
    if conf.mic.driver.to_lowercase().contains("coreaudio") {
        jack_server
            .arg("-R")
            .arg(format!("-d{}", conf.mic.driver))
            .arg(format!("-p{}", conf.mic.period));
    } else {
        jack_server
            .arg("-R")
            .arg(format!("-d{}", conf.mic.driver))
            .arg(format!("-C{}", conf.mic.device_name))
            .arg(format!("-P{}", conf.speaker.device_name))
            .arg(format!("-p{}", conf.mic.period))
            .arg(format!("-n{}", conf.mic.n_period))
            .arg(format!("-r{}", conf.mic.sample_rate));
    }
    jack_server.spawn().unwrap()
}

#[inline(always)]
pub fn _start_alsa_out(conf: Arc<Config>) -> Child {
    let mut alsa_out = Command::new("alsa_out");
    alsa_out.kill_on_drop(true);
    if conf.speaker.use_alsa_out {
        alsa_out
            .arg(format!("-d{}", conf.speaker.device_name))
            .arg(format!("-r{}", conf.mic.sample_rate))
            .arg(format!("-p{}", conf.mic.period))
            .arg(format!("-n{}", conf.mic.n_period));
    }
    alsa_out.spawn().unwrap()
}

#[inline(always)]
pub fn _start_zita_j2a(conf: Arc<Config>) -> Child {
    let mut alsa_out = Command::new("zita-j2a");
    alsa_out.kill_on_drop(true);
    alsa_out
        .arg(format!("-d{}", conf.speaker.device_name))
        .arg(format!("-r{}", conf.mic.sample_rate))
        .arg(format!("-p{}", conf.mic.period))
        .arg(format!("-n{}", conf.mic.n_period));
    alsa_out.spawn().unwrap()
}