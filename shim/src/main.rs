use shim_lib::{config, pipeline};
use std::path::PathBuf;

/// Exit code for a missing, malformed or invalid config — and for a usage error,
/// which is the same class of operator mistake. Codes 3 and 4 live in `pipeline`.
const EXIT_CONFIG: i32 = 2;

fn main() {
    let explicit = parse_config_arg();
    let cfg = match config::load(explicit.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            // Deliberately fatal. mic2sock silently falls back to defaults, which
            // makes a misconfiguration look like a runtime mystery instead of a
            // startup error.
            eprintln!("shim: {}", e);
            std::process::exit(EXIT_CONFIG);
        }
    };
    // The console is for errors only from here on; everything informational
    // goes to log_path (and nowhere, if none is configured).
    if let Err(e) = shim_lib::log::init(cfg.log_path.as_deref()) {
        eprintln!("shim: {}", e);
        std::process::exit(EXIT_CONFIG);
    }
    shim_lib::log::info(&format!(
        "shim: source {}:{} -> localhost:{}, {} ch x {} samples = {} bytes/packet",
        cfg.source_host,
        cfg.source_port,
        cfg.sink_port,
        cfg.n_ch,
        cfg.spp_out,
        cfg.layout().packet_len()
    ));

    spawn_console_quit();

    tokio::runtime::Runtime::new()
        .expect("failed to start the tokio runtime")
        .block_on(pipeline::run(cfg));
}

/// `q` + Enter on the console exits cleanly (code 0). A line read rather than a
/// raw keypress: raw mode would need a terminal crate, and a bare `q` typed into
/// the wrong window is exactly the accident Enter protects against.
///
/// When stdin is not a terminal -- the Windows scheduled task runs with no
/// console at all -- `read_line` returns EOF at once. EOF ends this thread and
/// nothing else: treating it as a quit would stop every unattended instance the
/// moment it started.
fn spawn_console_quit() {
    std::thread::spawn(|| {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    if matches!(line.trim(), "q" | "Q" | "quit") {
                        shim_lib::log::info("shim: quit requested from the console");
                        std::process::exit(0);
                    }
                }
            }
        }
    });
}

/// `--config <path>`, the only argument. Anything else is a usage error rather than
/// something to guess at.
fn parse_config_arg() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    match args.next() {
        None => None,
        Some(flag) if flag == "--config" => match args.next() {
            Some(p) => {
                if let Some(extra) = args.next() {
                    // Refused rather than ignored: an argument the operator meant to
                    // matter, silently dropped, is the same class of confusion as a
                    // misspelled config key.
                    eprintln!(
                        "shim: unexpected argument {:?} after --config <path>",
                        extra
                    );
                    std::process::exit(EXIT_CONFIG);
                }
                Some(PathBuf::from(p))
            }
            None => {
                eprintln!("shim: --config needs a path");
                std::process::exit(EXIT_CONFIG);
            }
        },
        Some(other) => {
            eprintln!(
                "shim: unexpected argument {:?}; usage: shim [--config <path>]",
                other
            );
            std::process::exit(EXIT_CONFIG);
        }
    }
}
