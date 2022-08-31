use alsa::pcm::*;
use alsa::{Direction, ValueOr, Error, device_name::HintIter};
use std::ffi::CString;

pub struct MicInput {
    device_name: String,
    sample_rate: u16,

}

impl MicInput {
    pub fn new(sr: u16) -> MicInput {
        MicInput {
            device_name: String::from("6 ch mic"),
            sample_rate: sr,
        }
    }

    pub fn list_devices() {
        println!("pcm devices: ");
        let devices = HintIter::new(None, &*CString::new("pcm").unwrap()).unwrap();
        for (i, d) in devices.enumerate() {
            if let (Some(name), Some(desc)) = (d.name, d.desc) {
                println!("{}\tname: {}, desc: {}", i+1, name, desc);
            }
            
        }
    }

    pub fn start_capture(&self) -> Result<PCM, Error> {
        let pcm = PCM::new(&self.device_name, Direction::Capture, false)?;
        {
            // For this example, we assume 44100Hz, one channel, 16 bit audio.
            let hwp = HwParams::any(&pcm)?;
            hwp.set_channels(8)?;
            hwp.set_rate(44100, ValueOr::Nearest)?;
            hwp.set_format(Format::s16())?;
            hwp.set_access(Access::RWInterleaved)?;
            pcm.hw_params(&hwp)?;
        }
        pcm.start()?;
        Ok(pcm)
    }
}



// Calculates RMS (root mean square) as a way to determine volume
fn rms(buf: &[i16]) -> f64 {
    if buf.len() == 0 { return 0f64; }
    let mut sum = 0f64;
    for &x in buf {
        sum += (x as f64) * (x as f64);
    }
    let r = (sum / (buf.len() as f64)).sqrt();
    // Convert value to decibels
    20.0 * (r / (i16::MAX as f64)).log10()
}


fn read_loop(pcm: &PCM) -> Result<(), Error> {
    let io = pcm.io_i16()?;
    let mut buf = [0i16; 8192];
    loop {
        // Block while waiting for 8192 samples to be read from the device.
        assert_eq!(io.readi(&mut buf)?, buf.len());
        let r = rms(&buf);
        println!("RMS: {:.1} dB", r);
    }
}