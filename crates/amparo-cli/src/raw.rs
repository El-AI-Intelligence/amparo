//! Terminal control shared by the interactive surfaces (`tui`, `code`).
//!
//! Raw mode, the single-byte reader and the terminal width live here so
//! each surface owns its rendering but none re-implements the terminal
//! seam. Everything except [`term_width`] is Unix-only by design — on
//! Windows the surfaces run piped, so the machinery compiles but is never
//! constructed.

/// Restores the original terminal settings on drop.
#[cfg(unix)]
pub(crate) struct RawGuard {
    orig: libc::termios,
}

/// Enters raw mode for a reader: no canonical line, no echo, no
/// signals, no flow control — but output processing stays, and reads
/// return within 0.1s so a lone ESC resolves as the Esc key.
#[cfg(unix)]
pub(crate) fn enter_raw_mode() -> Option<RawGuard> {
    unsafe {
        let mut orig: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut orig) != 0 {
            return None;
        }
        let mut raw = orig;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        raw.c_iflag &= !(libc::IXON | libc::ICRNL);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 1;
        if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) != 0 {
            return None;
        }
        Some(RawGuard { orig })
    }
}

#[cfg(unix)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.orig);
        }
    }
}

/// The terminal size, in (columns, rows) — `None` when stdout is not a
/// terminal or the size cannot be read.
#[cfg(unix)]
pub(crate) fn term_size() -> Option<(usize, usize)> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0
            && ws.ws_col > 0
            && ws.ws_row > 0
        {
            Some((ws.ws_col as usize, ws.ws_row as usize))
        } else {
            None
        }
    }
}

/// The terminal width, in columns — `None` when stdout is not a
/// terminal or the size cannot be read.
#[cfg(unix)]
pub(crate) fn term_width() -> Option<usize> {
    term_size().map(|(cols, _)| cols)
}

/// The terminal size, in (columns, rows) — always `None` on non-unix
/// platforms (no raw mode there at all).
#[cfg(not(unix))]
pub(crate) fn term_size() -> Option<(usize, usize)> {
    None
}

/// The terminal width, in columns — always `None` on non-unix platforms
/// (no raw mode there at all).
#[cfg(not(unix))]
pub(crate) fn term_width() -> Option<usize> {
    None
}

#[cfg(unix)]
use std::io::Read;

/// One byte, or `None` on a read timeout (VMIN=1, VTIME=1).
#[cfg(unix)]
pub(crate) fn read_byte(stdin: &mut std::io::Stdin) -> Option<u8> {
    let mut b = [0u8; 1];
    loop {
        match stdin.read(&mut b) {
            Ok(0) => return None,
            Ok(_) => return Some(b[0]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        }
    }
}
