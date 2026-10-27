//! The per-platform bits: raw mode, window size, reading keys, killing a process tree.

use std::{
    io::{self, IsTerminal},
    sync::mpsc::Sender,
    thread,
};

pub use imp::{ignore_ctrl_c, kill_tree, size, Raw};
#[cfg(unix)]
pub use imp::suspend;

pub fn is_tty() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

#[cfg(windows)]
static STOP_READER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Sends each chunk of keyboard input, then `None` when the terminal goes away.
pub fn spawn_reader(tx: Sender<Option<Vec<u8>>>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut input = imp::Input::default();
        loop {
            let got = input.read();
            #[cfg(windows)]
            if STOP_READER.load(std::sync::atomic::Ordering::Relaxed) {
                // Keys typed just before the stop still go to whoever stopped it.
                if let Some(b) = got {
                    let _ = tx.send(Some(String::from_utf8_lossy(&b).replace('\u{FFFF}', "").into_bytes()));
                }
                break;
            }
            let eof = got.is_none();
            if tx.send(got).is_err() || eof {
                break;
            }
        }
    })
}

/// Stops the reader from taking any more keys, so another program can read the console.
/// Cancelling its read can hang inside ConPTY, so it gets a key nobody types to wake it, and
/// stops there.
#[cfg(windows)]
pub fn stop_reader(reader: &thread::JoinHandle<()>) {
    use std::time::{Duration, Instant};
    STOP_READER.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Err(e) = imp::press('\u{FFFF}') {
        crate::log(format_args!("couldn't stop reading keys: {e}"));
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while !reader.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
mod imp {
    use std::io::{self, Read};

    pub struct Raw(libc::termios);

    impl Raw {
        pub fn enter() -> io::Result<Raw> {
            unsafe {
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(0, &mut t) != 0 {
                    return Err(io::Error::last_os_error());
                }
                let orig = t;
                libc::cfmakeraw(&mut t);
                if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(Raw(orig))
            }
        }
    }

    impl Drop for Raw {
        fn drop(&mut self) {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
        }
    }

    /// (rows, cols)
    pub fn size() -> (u16, u16) {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        for fd in [1, 0] {
            if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
                return (ws.ws_row, ws.ws_col);
            }
        }
        (24, 80)
    }

    pub struct Input(io::Stdin);

    impl Default for Input {
        fn default() -> Self {
            Input(io::stdin())
        }
    }

    impl Input {
        pub fn read(&mut self) -> Option<Vec<u8>> {
            let mut buf = [0u8; 8192];
            loop {
                match self.0.read(&mut buf) {
                    Ok(0) => return None,
                    Ok(n) => return Some(buf[..n].to_vec()),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => return None,
                }
            }
        }
    }

    /// Ctrl+Z: give the terminal back to the shell and stop until `fg` continues us.
    pub fn suspend(raw: &Raw) {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &raw.0);
            libc::kill(0, libc::SIGTSTP);
            let mut t = raw.0;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
        }
    }

    /// The pty child leads its own process group, which holds its MCP servers and shells too.
    pub fn kill_tree(pid: u32, force: bool) {
        let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
        unsafe {
            libc::kill(-(pid as libc::pid_t), sig);
            libc::kill(pid as libc::pid_t, sig);
        }
    }

    pub fn ignore_ctrl_c() {}
}

#[cfg(windows)]
mod imp {
    use std::{io, os::windows::process::CommandExt, process, ptr};
    use windows_sys::Win32::{Foundation::*, System::Console::*};

    fn h(which: STD_HANDLE) -> HANDLE {
        unsafe { GetStdHandle(which) }
    }

    pub struct Raw {
        input: CONSOLE_MODE,
        output: CONSOLE_MODE,
    }

    impl Raw {
        pub fn enter() -> io::Result<Raw> {
            let (mut input, mut output) = (0, 0);
            unsafe {
                if GetConsoleMode(h(STD_INPUT_HANDLE), &mut input) == 0
                    || GetConsoleMode(h(STD_OUTPUT_HANDLE), &mut output) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                let raw = ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT;
                SetConsoleMode(h(STD_INPUT_HANDLE), (input & !raw) | ENABLE_VIRTUAL_TERMINAL_INPUT);
                SetConsoleMode(
                    h(STD_OUTPUT_HANDLE),
                    output | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN,
                );
            }
            Ok(Raw { input, output })
        }
    }

    impl Drop for Raw {
        fn drop(&mut self) {
            unsafe {
                SetConsoleMode(h(STD_INPUT_HANDLE), self.input);
                SetConsoleMode(h(STD_OUTPUT_HANDLE), self.output);
            }
        }
    }

    /// (rows, cols)
    pub fn size() -> (u16, u16) {
        let mut i: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        if unsafe { GetConsoleScreenBufferInfo(h(STD_OUTPUT_HANDLE), &mut i) } != 0 {
            let w = i.srWindow;
            return ((w.Bottom - w.Top + 1) as u16, (w.Right - w.Left + 1) as u16);
        }
        (24, 80)
    }

    /// Reads with ReadConsoleW rather than std's stdin, which treats Ctrl+Z as end of input.
    #[derive(Default)]
    pub struct Input {
        high_surrogate: Option<u16>,
    }

    impl Input {
        pub fn read(&mut self) -> Option<Vec<u8>> {
            let mut buf = [0u16; 1024];
            loop {
                let mut n = 0u32;
                let ok = unsafe {
                    ReadConsoleW(
                        h(STD_INPUT_HANDLE),
                        buf.as_mut_ptr().cast(),
                        buf.len() as u32,
                        &mut n,
                        ptr::null(),
                    )
                };
                if ok == 0 {
                    return None;
                }
                let mut units: Vec<u16> =
                    self.high_surrogate.take().into_iter().chain(buf[..n as usize].iter().copied()).collect();
                if units.last().is_some_and(|u| (0xD800..0xDC00).contains(u)) {
                    self.high_surrogate = units.pop();
                }
                let s: String = char::decode_utf16(units).map(|c| c.unwrap_or('\u{FFFD}')).collect();
                if !s.is_empty() {
                    return Some(s.into_bytes());
                }
            }
        }
    }

    /// Puts a key press into this console's input, as if typed.
    pub fn press(c: char) -> io::Result<()> {
        use std::{fs::OpenOptions, os::windows::io::AsRawHandle};
        let input = OpenOptions::new().read(true).write(true).open("CONIN$")?;
        let mut unit = [0u16; 2];
        c.encode_utf16(&mut unit);
        let key = |down: bool| INPUT_RECORD {
            EventType: KEY_EVENT as u16,
            Event: INPUT_RECORD_0 {
                KeyEvent: KEY_EVENT_RECORD {
                    bKeyDown: down as i32,
                    wRepeatCount: 1,
                    wVirtualKeyCode: 0,
                    wVirtualScanCode: 0,
                    uChar: KEY_EVENT_RECORD_0 { UnicodeChar: unit[0] },
                    dwControlKeyState: 0,
                },
            },
        };
        let records = [key(true), key(false)];
        let mut written = 0;
        if unsafe { WriteConsoleInputW(input.as_raw_handle() as _, records.as_ptr(), 2, &mut written) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn kill_tree(pid: u32, _force: bool) {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(process::Stdio::null())
            .stdout(process::Stdio::null())
            .stderr(process::Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }

    /// Claude handles Ctrl+C and Ctrl+Break itself; don't let them kill the wrapper first.
    pub fn ignore_ctrl_c() {
        unsafe extern "system" fn handler(ctrl: u32) -> BOOL {
            (ctrl == CTRL_C_EVENT || ctrl == CTRL_BREAK_EVENT) as BOOL
        }
        unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
    }
}
