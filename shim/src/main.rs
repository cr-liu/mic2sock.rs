use shim_lib::{config, pipeline};
use std::path::PathBuf;

fn main() {
    let explicit = parse_config_arg();
    let cfg = match config::load(explicit.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            // Deliberately fatal. mic2sock silently falls back to defaults, which
            // makes a misconfiguration look like a runtime mystery instead of a
            // startup error.
            eprintln!("shim: {}", e);
            std::process::exit(2);
        }
    };
    eprintln!(
        "shim: source {}:{} -> localhost:{}, {} ch x {} samples = {} bytes/packet",
        cfg.source_host,
        cfg.source_port,
        cfg.sink_port,
        cfg.n_ch,
        cfg.spp_out,
        cfg.layout().packet_len()
    );

    tokio::runtime::Runtime::new()
        .expect("failed to start the tokio runtime")
        .block_on(pipeline::run(cfg));
}

/// `--config <path>`, the only argument. Anything else is a usage error rather than
/// something to guess at.
fn parse_config_arg() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    match args.next() {
        None => None,
        Some(flag) if flag == "--config" => match args.next() {
            Some(p) => Some(PathBuf::from(p)),
            None => {
                eprintln!("shim: --config needs a path");
                std::process::exit(2);
            }
        },
        Some(other) => {
            eprintln!(
                "shim: unexpected argument {:?}; usage: shim [--config <path>]",
                other
            );
            std::process::exit(2);
        }
    }
}
