mod alsa_capture;
mod config_file;
use config_file::Config;

fn main() {
    let cfg = Config::new();
    println!("Loaded {} capture devices, {} total channels",
        cfg.capture_device.len(), cfg.total_capture_channels());
}
