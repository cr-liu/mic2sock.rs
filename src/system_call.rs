use crate::config_file::Config;
use std::sync::Arc;
use tokio::process::{Child, Command};

pub fn start_jackd(conf: Arc<Config>) -> Child {
    let mut jack_server = Command::new("jackd");
    jack_server.kill_on_drop(true);
    if !conf.mic.start_jackd {
        return jack_server.spawn().unwrap();
    }
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

pub fn _start_zita_j2a(conf: Arc<Config>) -> Child {
    let mut zita_j2a = Command::new("zita-j2a");
    zita_j2a.kill_on_drop(true);
    zita_j2a
        .arg(format!("-d{}", conf.speaker.device_name))
        .arg(format!("-r{}", conf.mic.sample_rate))
        .arg(format!("-p{}", conf.mic.period))
        .arg(format!("-n{}", conf.mic.n_period));
    zita_j2a.spawn().unwrap()
}

pub fn start_alsa_in(conf: Arc<Config>) -> Child {
    let mut alsa_in = Command::new("alsa_in");
    alsa_in.kill_on_drop(true);
    alsa_in
        .arg(format!("-j{}", conf.mic_2nd.device_name))
        .arg(format!("-d{}", conf.mic_2nd.device_name))
        .arg(format!("-r{}", conf.mic.sample_rate))
        .arg(format!("-c{}", conf.mic_2nd.n_channel));
    alsa_in.spawn().unwrap()
}

pub fn _start_zita_a2j(conf: Arc<Config>) -> Child {
    let mut zita_a2j = Command::new("zita-a2j");
    zita_a2j.kill_on_drop(true);
    zita_a2j
        .arg("-jzita-a2j")
        .arg(format!("-d{}", conf.mic.device_name))
        .arg(format!("-r{}", conf.mic.sample_rate))
        .arg(format!("-p{}", conf.mic.period))
        .arg(format!("-n{}", conf.mic.n_period));
    zita_a2j.spawn().unwrap()
}

