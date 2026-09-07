//! Terminal control shared by the interactive surfaces (`tui`, `code`).
//!
//! Raw mode, the reader and the terminal width live here so each surface
//! owns its rendering but none re-implements the terminal seam. Unix
//! uses termios; Windows uses console-mode flags (line input, echo and
//! processed input off, quick-edit off; VT processing on) with
//! `KEY_EVENT` records for input — the UTF-16 record model keeps
//! non-ASCII input independent of the console codepage.

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
pub(crate) fn term_width() -> Option<usize> {
    term_size().map(|(cols, _)| cols)
}

#[cfg(unix)]
use std::io::Read;

/// One byte, or `None` on EOF. With VMIN=1 this read blocks until a
/// byte arrives — it never times out; [`poll_byte`] is the timed
/// variant.
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

/// One byte if stdin delivers within the window, `None` on timeout —
/// the code surface's tick source: its reader loop must re-render the
/// approval countdown between keypresses, so it polls instead of
/// blocking inside [`read_byte`] (with VMIN=1 that read never times
/// out). The TUI keeps the blocking reader; only the coding surface
/// polls.
#[cfg(unix)]
pub(crate) fn poll_byte(stdin: &mut std::io::Stdin, millis: u64) -> Option<u8> {
    let mut fds = [libc::pollfd {
        fd: libc::STDIN_FILENO,
        events: libc::POLLIN,
        revents: 0,
    }];
    loop {
        match unsafe { libc::poll(fds.as_mut_ptr(), 1, millis as libc::c_int) } {
            n if n > 0 => {
                // POLLHUP can carry a final byte — read it out.
                if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                    return read_byte(stdin);
                }
                return None;
            }
            0 => return None,
            _ => {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return None;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Windows console layer
// ---------------------------------------------------------------------------

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
    System::Console::{
        GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, ReadConsoleInputW,
        SetConsoleMode, CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
        ENABLE_PROCESSED_INPUT, ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, LEFT_ALT_PRESSED,
        LEFT_CTRL_PRESSED, RIGHT_ALT_PRESSED, RIGHT_CTRL_PRESSED,
    },
    System::Threading::WaitForSingleObject,
    UI::Input::KeyboardAndMouse::{
        VK_BACK, VK_C, VK_D, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_INSERT, VK_LEFT,
        VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_TAB, VK_UP,
    },
};

/// Restores the original console modes (input and output) on drop.
#[cfg(windows)]
pub(crate) struct RawGuard {
    in_handle: HANDLE,
    out_handle: HANDLE,
    in_mode: u32,
    out_mode: u32,
}

/// The input mode for a raw reader: line input, echo, processed input
/// and quick-edit off (a click on legacy conhost would otherwise freeze
/// the session mid-read). Mouse input stays off — the renderer emits
/// the mouse enable sequence anyway, and with mouse off no mouse events
/// arrive, so that emission stays a harmless no-op.
#[cfg(windows)]
pub(crate) fn raw_in_mode(saved: u32) -> u32 {
    saved
        & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT | ENABLE_QUICK_EDIT_MODE)
}

/// The output mode for ANSI rendering: VT processing on (Windows
/// Terminal and Win10+ conhost render the escape sequences; older hosts
/// simply leave them unprocessed).
#[cfg(windows)]
pub(crate) fn out_mode(saved: u32) -> u32 {
    saved | ENABLE_VIRTUAL_TERMINAL_PROCESSING
}

/// Enters raw mode: raw input plus VT processing on stdout. `None` when
/// either handle is not a console (redirected output, CI) or a mode
/// change fails — on partial failure the already-changed modes are
/// restored, matching the unix best-effort contract.
#[cfg(windows)]
pub(crate) fn enter_raw_mode() -> Option<RawGuard> {
    unsafe {
        let in_handle = GetStdHandle(STD_INPUT_HANDLE);
        let out_handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if in_handle == INVALID_HANDLE_VALUE
            || in_handle.is_null()
            || out_handle == INVALID_HANDLE_VALUE
            || out_handle.is_null()
        {
            return None;
        }
        let mut in_saved = 0u32;
        if GetConsoleMode(in_handle, &mut in_saved) == 0 {
            return None;
        }
        let mut out_saved = 0u32;
        if GetConsoleMode(out_handle, &mut out_saved) == 0 {
            return None;
        }
        if SetConsoleMode(in_handle, raw_in_mode(in_saved)) == 0 {
            return None;
        }
        if SetConsoleMode(out_handle, out_mode(out_saved)) == 0 {
            SetConsoleMode(in_handle, in_saved);
            return None;
        }
        Some(RawGuard {
            in_handle,
            out_handle,
            in_mode: in_saved,
            out_mode: out_saved,
        })
    }
}

#[cfg(windows)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        unsafe {
            SetConsoleMode(self.in_handle, self.in_mode);
            SetConsoleMode(self.out_handle, self.out_mode);
        }
    }
}

/// The terminal size, in (columns, rows) — `None` when stdout is not a
/// console or the size cannot be read.
#[cfg(windows)]
pub(crate) fn term_size() -> Option<(usize, usize)> {
    unsafe {
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return None;
        }
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(handle, &mut info) == 0 {
            return None;
        }
        let w = info.srWindow.Right - info.srWindow.Left + 1;
        let h = info.srWindow.Bottom - info.srWindow.Top + 1;
        if w > 0 && h > 0 {
            Some((w as usize, h as usize))
        } else {
            None
        }
    }
}

/// A translated keypress — the shared vocabulary the surfaces consume,
/// shaped by what both readers can provide.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsoleKey {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    CtrlC,
    CtrlD,
    Ignore,
}

/// Translates one `KEY_EVENT` record. Pure — this is the unit-test
/// seam: CI has no console, so every decision about routing a keypress
/// lives here and is exercised by table tests.
#[cfg(windows)]
pub(crate) fn key_from_event(vk: u16, control: u32, unicode: u16) -> ConsoleKey {
    use ConsoleKey::*;
    let ctrl = control & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0;
    let alt = control & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0;
    // Ctrl-C / Ctrl-D: with processed input off the record still carries
    // the control-character translation (or none at all), so match every
    // spelling — the control char, the letter, or the bare virtual key.
    if ctrl && (vk == VK_C || unicode == 0x03 || unicode == 'c' as u16 || unicode == 'C' as u16) {
        return CtrlC;
    }
    if ctrl && (vk == VK_D || unicode == 0x04 || unicode == 'd' as u16 || unicode == 'D' as u16) {
        return CtrlD;
    }
    if alt {
        // Alt combos reach the unix surfaces as unusable ESC-prefixed
        // bytes — ignore them here too, for parity.
        return Ignore;
    }
    // Semantic keys win over their own unicode (Enter carries '\r',
    // escape 0x1B, backspace 0x08). Delete/Insert/F-keys fall to the
    // unicode check — their unicode is always 0 on real consoles, so
    // only synthetic records can route through it.
    if let Some(k) = match vk {
        VK_UP => Some(Up),
        VK_DOWN => Some(Down),
        VK_LEFT => Some(Left),
        VK_RIGHT => Some(Right),
        VK_HOME => Some(Home),
        VK_END => Some(End),
        VK_PRIOR => Some(PageUp),
        VK_NEXT => Some(PageDown),
        VK_RETURN => Some(Enter),
        VK_ESCAPE => Some(Esc),
        VK_BACK => Some(Backspace),
        VK_TAB => Some(Char('\t')),
        _ => None,
    } {
        return k;
    }
    // Remaining Ctrl chords (Ctrl-Q and friends) carry their control
    // characters in `unicode` — the unix surfaces decode those bytes to
    // Ignore, so drop them here rather than type them into a prompt.
    if ctrl {
        return Ignore;
    }
    if unicode != 0 {
        if let Some(c) = char::from_u32(unicode as u32) {
            return Char(c);
        }
    }
    Ignore
}

/// Combines a UTF-16 surrogate pair into a char — `None` when the
/// halves don't form a valid pair.
#[cfg(windows)]
pub(crate) fn pair_surrogates(hi: u16, lo: u16) -> Option<char> {
    if (0xD800..=0xDBFF).contains(&hi) && (0xDC00..=0xDFFF).contains(&lo) {
        char::from_u32(0x10000 + (((hi - 0xD800) as u32) << 10) + (lo - 0xDC00) as u32)
    } else {
        None
    }
}

/// One keypress, or `None` when stdin is not a console or the read
/// fails. Blocks until a down-event arrives — [`poll_key`] is the timed
/// variant.
#[cfg(windows)]
pub(crate) fn read_key() -> Option<ConsoleKey> {
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return None;
        }
        loop {
            let mut record: INPUT_RECORD = std::mem::zeroed();
            let mut read = 0u32;
            if ReadConsoleInputW(handle, &mut record, 1, &mut read) == 0 || read == 0 {
                return None;
            }
            if record.EventType != KEY_EVENT as u16 {
                continue;
            }
            let key = record.Event.KeyEvent;
            if key.bKeyDown == 0 {
                continue;
            }
            let ch = key.uChar.UnicodeChar;
            if (0xD800..=0xDBFF).contains(&ch) {
                // A high surrogate always arrives paired with the next
                // record's low surrogate (`ReadConsoleInputW` delivers
                // UTF-16). Combine them; drop a malformed pair.
                let mut next_record: INPUT_RECORD = std::mem::zeroed();
                let mut next_read = 0u32;
                if ReadConsoleInputW(handle, &mut next_record, 1, &mut next_read) == 0
                    || next_read == 0
                    || next_record.EventType != KEY_EVENT as u16
                {
                    return None;
                }
                let next = next_record.Event.KeyEvent;
                return match pair_surrogates(ch, next.uChar.UnicodeChar) {
                    Some(c) => Some(ConsoleKey::Char(c)),
                    None => None,
                };
            }
            return Some(key_from_event(
                key.wVirtualKeyCode,
                key.dwControlKeyState,
                ch,
            ));
        }
    }
}

/// One keypress if the console delivers within the window, `None` on
/// timeout — the code surface's tick source: its reader loop must
/// re-render the approval countdown between keypresses, so it polls
/// instead of blocking inside [`read_key`]. The TUI keeps the blocking
/// reader; only the coding surface polls.
#[cfg(windows)]
pub(crate) fn poll_key(millis: u64) -> Option<ConsoleKey> {
    unsafe {
        let handle = GetStdHandle(STD_INPUT_HANDLE);
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return None;
        }
        if WaitForSingleObject(handle, millis as u32) == WAIT_OBJECT_0 {
            read_key()
        } else {
            None
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn key(vk: u16, unicode: u16, control: u32) -> ConsoleKey {
        key_from_event(vk, control, unicode)
    }

    #[test]
    fn vk_table_routes_navigation() {
        assert_eq!(key(VK_UP, 0, 0), ConsoleKey::Up);
        assert_eq!(key(VK_DOWN, 0, 0), ConsoleKey::Down);
        assert_eq!(key(VK_LEFT, 0, 0), ConsoleKey::Left);
        assert_eq!(key(VK_RIGHT, 0, 0), ConsoleKey::Right);
        assert_eq!(key(VK_HOME, 0, 0), ConsoleKey::Home);
        assert_eq!(key(VK_END, 0, 0), ConsoleKey::End);
        assert_eq!(key(VK_PRIOR, 0, 0), ConsoleKey::PageUp);
        assert_eq!(key(VK_NEXT, 0, 0), ConsoleKey::PageDown);
    }

    #[test]
    fn semantic_keys_win_over_their_unicode() {
        // Enter carries '\r', escape 0x1B, backspace 0x08 — the
        // semantic keys must win so the surfaces see them as keys.
        assert_eq!(key(VK_RETURN, '\r' as u16, 0), ConsoleKey::Enter);
        assert_eq!(key(VK_ESCAPE, 0x1B, 0), ConsoleKey::Esc);
        assert_eq!(key(VK_BACK, 0x08, 0), ConsoleKey::Backspace);
        assert_eq!(key(VK_TAB, '\t' as u16, 0), ConsoleKey::Char('\t'));
    }

    #[test]
    fn ctrl_c_and_d_match_every_spelling() {
        for u in [0x03u16, 'c' as u16, 'C' as u16, 0] {
            assert_eq!(key(VK_C, u, LEFT_CTRL_PRESSED), ConsoleKey::CtrlC, "u={u}");
        }
        for u in [0x04u16, 'd' as u16, 'D' as u16, 0] {
            assert_eq!(key(VK_D, u, RIGHT_CTRL_PRESSED), ConsoleKey::CtrlD, "u={u}");
        }
        // Without ctrl held, a plain 'c' is a character.
        assert_eq!(key(VK_C, 'c' as u16, 0), ConsoleKey::Char('c'));
    }

    #[test]
    fn other_ctrl_chords_are_ignored() {
        // Ctrl-Q arrives as VK_Q (0x51) with its control character 0x11 —
        // the unix decoder ignores those bytes, so they must not type
        // into a prompt here.
        assert_eq!(key(0x51, 0x11, LEFT_CTRL_PRESSED), ConsoleKey::Ignore);
        assert_eq!(key(0x41, 0x01, RIGHT_CTRL_PRESSED), ConsoleKey::Ignore);
        // Ctrl+Backspace keeps its semantic meaning.
        assert_eq!(key(VK_BACK, 0x7F, LEFT_CTRL_PRESSED), ConsoleKey::Backspace);
    }

    #[test]
    fn alt_combos_are_ignored() {
        assert_eq!(key(VK_UP, 0, LEFT_ALT_PRESSED), ConsoleKey::Ignore);
        assert_eq!(key(VK_C, 'c' as u16, RIGHT_ALT_PRESSED), ConsoleKey::Ignore);
    }

    #[test]
    fn unicode_wins_when_the_vk_has_no_meaning() {
        // Synthetic: an unlisted VK (Delete) carrying unicode — the
        // character must win; without unicode the key is ignored.
        assert_eq!(key(VK_DELETE, 'x' as u16, 0), ConsoleKey::Char('x'));
        assert_eq!(key(VK_DELETE, 0, 0), ConsoleKey::Ignore);
        assert_eq!(key(VK_INSERT, 0, 0), ConsoleKey::Ignore);
    }

    #[test]
    fn masks() {
        let cooked = ENABLE_LINE_INPUT
            | ENABLE_ECHO_INPUT
            | ENABLE_PROCESSED_INPUT
            | ENABLE_QUICK_EDIT_MODE;
        assert_eq!(raw_in_mode(cooked), 0);
        assert_eq!(raw_in_mode(0), 0);
        assert_eq!(out_mode(0), ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        assert_eq!(
            out_mode(ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT),
            ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING
        );
    }

    #[test]
    fn surrogate_pairs() {
        let c = '😀';
        let mut buf = [0u16; 2];
        let _ = c.encode_utf16(&mut buf);
        assert_eq!(pair_surrogates(buf[0], buf[1]), Some(c));
        assert_eq!(pair_surrogates(buf[1], buf[0]), None);
        assert_eq!(pair_surrogates(buf[0], buf[0]), None);
        assert_eq!(pair_surrogates('a' as u16, 'b' as u16), None);
    }
}
