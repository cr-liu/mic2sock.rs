//! Bare-key quit: press `q` in the console and the shim exits cleanly.
//!
//! A bare key needs the console in raw (non-line-buffered) mode, which is
//! platform-specific; a line read would have needed Enter, and the user's
//! verdict on that was that Ctrl+C is then just as good. No terminal crate:
//! the Unix side is termios through `libc`, the Windows side the Console API
//! through `windows-sys`, both already in the lockfile.
//!
//! Raw mode is applied only when stdin IS a console. A pipe, `/dev/null` or
//! the no-console Windows scheduled task is an unattended run: the reader
//! returns at once and the console mode is never touched. Ctrl+C keeps
//! working in both modes (ISIG / ENABLE_PROCESSED_INPUT stay on), and the
//! original mode is restored on quit -- and, on Unix, on Ctrl+C too, so the
//! shell is not left with echo off.

pub fn spawn() {
    std::thread::spawn(run);
}

fn quit() -> ! {
    crate::log::info("shim: quit requested from the console (q)");
    std::process::exit(0)
}

#[cfg(unix)]
mod imp {
    use std::io::Read;
    use std::os::unix::io::AsRawFd;

    use std::sync::Mutex;

    /// The mode to restore. A signal handler may not block, so it only
    /// `try_lock`s: the lock is held for microseconds by the reader thread's
    /// restore, and losing that race merely skips a best-effort restore that
    /// the shell's own line discipline will redo at its next prompt.
    static ORIGINAL: Mutex<Option<libc::termios>> = Mutex::new(None);

    fn restore(fd: libc::c_int) {
        if let Ok(guard) = ORIGINAL.try_lock() {
            if let Some(orig) = guard.as_ref() {
                unsafe {
                    libc::tcsetattr(fd, libc::TCSANOW, orig);
                }
            }
        }
    }

    extern "C" fn on_sigint(_: libc::c_int) {
        restore(libc::STDIN_FILENO);
        // 128 + SIGINT, as the default disposition would have reported.
        unsafe { libc::_exit(130) }
    }

    pub fn run() {
        let stdin = std::io::stdin();
        let fd = stdin.as_raw_fd();
        unsafe {
            if libc::isatty(fd) == 0 {
                return;
            }
            let mut orig: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut orig) != 0 {
                return;
            }
            let mut raw = orig;
            // Byte at a time, no echo. ISIG is left on so Ctrl+C still works.
            raw.c_lflag &= !(libc::ICANON | libc::ECHO);
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return;
            }
            *ORIGINAL.lock().unwrap_or_else(|e| e.into_inner()) = Some(orig);
            let handler: extern "C" fn(libc::c_int) = on_sigint;
            libc::signal(libc::SIGINT, handler as libc::sighandler_t);
        }

        let mut lock = stdin.lock();
        let mut buf = [0u8; 1];
        loop {
            match lock.read(&mut buf) {
                Ok(1) if buf[0] == b'q' || buf[0] == b'Q' => {
                    restore(fd);
                    super::quit();
                }
                Ok(0) | Err(_) => {
                    restore(fd);
                    return;
                }
                _ => {}
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, ReadConsoleInputW, SetConsoleMode, ENABLE_ECHO_INPUT,
        ENABLE_LINE_INPUT, INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE,
    };

    pub fn run() {
        unsafe {
            let h = GetStdHandle(STD_INPUT_HANDLE);
            if h == INVALID_HANDLE_VALUE || h == 0 {
                return;
            }
            let mut mode = 0u32;
            // Fails when stdin is not a console (a pipe, or the scheduled
            // task's absent console): unattended, leave everything alone.
            if GetConsoleMode(h, &mut mode) == 0 {
                return;
            }
            // Key at a time, no echo. ENABLE_PROCESSED_INPUT stays, so Ctrl+C
            // still raises the console control signal.
            if SetConsoleMode(h, mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT)) == 0 {
                return;
            }
            let mut rec: INPUT_RECORD = std::mem::zeroed();
            let mut n = 0u32;
            loop {
                if ReadConsoleInputW(h, &mut rec, 1, &mut n) == 0 || n == 0 {
                    SetConsoleMode(h, mode);
                    return;
                }
                // EventType is a u16 field; the constant is declared u32.
                if u32::from(rec.EventType) == KEY_EVENT {
                    let k = rec.Event.KeyEvent;
                    if k.bKeyDown != 0 {
                        let ch = k.uChar.UnicodeChar;
                        if ch == 'q' as u16 || ch == 'Q' as u16 {
                            SetConsoleMode(h, mode);
                            super::quit();
                        }
                    }
                }
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    pub fn run() {}
}

use imp::run;
