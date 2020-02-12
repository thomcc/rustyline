//! Unix specific definitions
use std::cmp::Ordering;
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync;
use std::sync::atomic;

use log::{debug, warn};
use nix::poll::{self, PollFlags};
use nix::sys::signal;
use nix::sys::termios;
use nix::sys::termios::SetArg;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use utf8parse::{Parser, Receiver};

use super::{RawMode, RawReader, Renderer, Term};
use crate::config::{BellStyle, ColorMode, Config, OutputStreamType};
use crate::error;
use crate::highlight::Highlighter;
use crate::keys::{self, Key, KeyPress};
use crate::layout::{Layout, Position};
use crate::line_buffer::LineBuffer;
use crate::tty::add_prompt_and_highlight;
use crate::Result;

const STDIN_FILENO: RawFd = libc::STDIN_FILENO;

/// Unsupported Terminals that don't support RAW mode
const UNSUPPORTED_TERM: [&str; 3] = ["dumb", "cons25", "emacs"];

const BRACKETED_PASTE_ON: &[u8] = b"\x1b[?2004h";
const BRACKETED_PASTE_OFF: &[u8] = b"\x1b[?2004l";

impl AsRawFd for OutputStreamType {
    fn as_raw_fd(&self) -> RawFd {
        match self {
            OutputStreamType::Stdout => libc::STDOUT_FILENO,
            OutputStreamType::Stderr => libc::STDERR_FILENO,
        }
    }
}

nix::ioctl_read_bad!(win_size, libc::TIOCGWINSZ, libc::winsize);

#[allow(clippy::identity_conversion)]
fn get_win_size<T: AsRawFd + ?Sized>(fileno: &T) -> (usize, usize) {
    use std::mem::zeroed;

    unsafe {
        let mut size: libc::winsize = zeroed();
        match win_size(fileno.as_raw_fd(), &mut size) {
            Ok(0) => (size.ws_col as usize, size.ws_row as usize), // TODO getCursorPosition
            _ => (80, 24),
        }
    }
}

/// Check TERM environment variable to see if current term is in our
/// unsupported list
fn is_unsupported_term() -> bool {
    match std::env::var("TERM") {
        Ok(term) => {
            for iter in &UNSUPPORTED_TERM {
                if (*iter).eq_ignore_ascii_case(&term) {
                    return true;
                }
            }
            false
        }
        Err(_) => false,
    }
}

/// Return whether or not STDIN, STDOUT or STDERR is a TTY
fn is_a_tty(fd: RawFd) -> bool {
    unsafe { libc::isatty(fd) != 0 }
}

pub struct PosixMode {
    termios: termios::Termios,
    out: Option<OutputStreamType>,
}

#[cfg(not(test))]
pub type Mode = PosixMode;

impl RawMode for PosixMode {
    /// Disable RAW mode for the terminal.
    fn disable_raw_mode(&self) -> Result<()> {
        termios::tcsetattr(STDIN_FILENO, SetArg::TCSADRAIN, &self.termios)?;
        // disable bracketed paste
        if let Some(out) = self.out {
            write_and_flush(out, BRACKETED_PASTE_OFF)?;
        }
        Ok(())
    }
}

// Rust std::io::Stdin is buffered with no way to know if bytes are available.
// So we use low-level stuff instead...
struct StdinRaw {}

impl Read for StdinRaw {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let res = unsafe {
                libc::read(
                    STDIN_FILENO,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len() as libc::size_t,
                )
            };
            if res == -1 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted
                    || SIGWINCH.load(atomic::Ordering::Relaxed)
                {
                    return Err(error);
                }
            } else {
                #[allow(clippy::cast_sign_loss)]
                return Ok(res as usize);
            }
        }
    }
}

/// Console input reader
pub struct PosixRawReader {
    stdin: StdinRaw,
    timeout_ms: i32,
    buf: [u8; 1],
    parser: Parser,
    receiver: Utf8,
}

struct Utf8 {
    c: Option<char>,
    valid: bool,
}

impl PosixRawReader {
    fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            stdin: StdinRaw {},
            timeout_ms: config.keyseq_timeout(),
            buf: [0; 1],
            parser: Parser::new(),
            receiver: Utf8 {
                c: None,
                valid: true,
            },
        })
    }

    /// Handle ESC <seq1> sequences
    fn escape_sequence(&mut self) -> Result<KeyPress> {
        // Read the next byte representing the escape sequence.
        let seq1 = self.next_char()?;
        if seq1 == '[' {
            // ESC [ sequences. (CSI)
            self.escape_csi()
        } else if seq1 == 'O' {
            // xterm
            // ESC O sequences. (SS3)
            self.escape_o()
        } else if seq1 == '\x1b' {
            // ESC ESC
            Ok(KeyPress::ESC)
        } else {
            // TODO ESC-R (r): Undo all changes made to this line.
            Ok(KeyPress::meta(seq1))
        }
    }

    /// Handle ESC [ <seq2> escape sequences
    fn escape_csi(&mut self) -> Result<KeyPress> {
        let seq2 = self.next_char()?;
        if seq2.is_digit(10) {
            match seq2 {
                '0' | '9' => {
                    debug!(target: "rustyline", "unsupported esc sequence: ESC [ {:?}", seq2);
                    Ok(Key::UnknownEscSeq.into())
                }
                _ => {
                    // Extended escape, read additional byte.
                    self.extended_escape(seq2)
                }
            }
        } else if seq2 == '[' {
            let seq3 = self.next_char()?;
            // Linux console
            Ok(match seq3 {
                'A' => key_press!(F(1)),
                'B' => key_press!(F(2)),
                'C' => key_press!(F(3)),
                'D' => key_press!(F(4)),
                'E' => key_press!(F(5)),
                _ => {
                    debug!(target: "rustyline", "unsupported esc sequence: ESC [ [ {:?}", seq3);
                    Key::UnknownEscSeq.into()
                }
            })
        } else {
            // ANSI
            Ok(match seq2 {
                'A' => Key::Up.into(),    // kcuu1
                'B' => Key::Down.into(),  // kcud1
                'C' => Key::Right.into(), // kcuf1
                'D' => Key::Left.into(),  // kcub1
                'F' => Key::End.into(),
                'H' => Key::Home.into(), // khome
                'Z' => Key::BackTab.into(),
                _ => {
                    debug!(target: "rustyline", "unsupported esc sequence: ESC [ {:?}", seq2);
                    Key::UnknownEscSeq.into()
                }
            })
        }
    }

    /// Handle ESC [ <seq2:digit> escape sequences
    #[allow(clippy::cognitive_complexity)]
    fn extended_escape(&mut self, seq2: char) -> Result<KeyPress> {
        let seq3 = self.next_char()?;
        if seq3 == '~' {
            Ok(match seq2 {
                '1' | '7' => KeyPress::HOME, // tmux, xrvt
                '2' => KeyPress::INSERT,
                '3' => KeyPress::DELETE,    // kdch1
                '4' | '8' => KeyPress::END, // tmux, xrvt
                '5' => KeyPress::PAGE_UP,   // kpp
                '6' => KeyPress::PAGE_DOWN, // knp
                _ => {
                    debug!(target: "rustyline",
                           "unsupported esc sequence: ESC [ {} ~", seq2);
                    Key::UnknownEscSeq.into()
                }
            })
        } else if seq3.is_digit(10) {
            let seq4 = self.next_char()?;
            if seq4 == '~' {
                Ok(match (seq2, seq3) {
                    ('1', '1') => Key::F(1).into(),  // rxvt-unicode
                    ('1', '2') => Key::F(2).into(),  // rxvt-unicode
                    ('1', '3') => Key::F(3).into(),  // rxvt-unicode
                    ('1', '4') => Key::F(4).into(),  // rxvt-unicode
                    ('1', '5') => Key::F(5).into(),  // kf5
                    ('1', '7') => Key::F(6).into(),  // kf6
                    ('1', '8') => Key::F(7).into(),  // kf7
                    ('1', '9') => Key::F(8).into(),  // kf8
                    ('2', '0') => Key::F(9).into(),  // kf9
                    ('2', '1') => Key::F(10).into(), // kf10
                    ('2', '3') => Key::F(11).into(), // kf11
                    ('2', '4') => Key::F(12).into(), // kf12
                    _ => {
                        debug!(target: "rustyline",
                               "unsupported esc sequence: ESC [ {}{} ~", seq2, seq3);
                        Key::UnknownEscSeq.into()
                    }
                })
            } else if seq4 == ';' {
                let seq5 = self.next_char()?;
                if seq5.is_digit(10) {
                    let seq6 = self.next_char()?;
                    if seq6.is_digit(10) {
                        self.next_char()?; // 'R' expected
                    } else if seq6 == 'R' {
                    } else {
                        debug!(target: "rustyline",
                               "unsupported esc sequence: ESC [ {}{} ; {} {}", seq2, seq3, seq5, seq6);
                    }
                } else {
                    debug!(target: "rustyline",
                           "unsupported esc sequence: ESC [ {}{} ; {:?}", seq2, seq3, seq5);
                }
                Ok(Key::UnknownEscSeq.into())
            } else if seq4.is_digit(10) {
                let seq5 = self.next_char()?;
                if seq5 == '~' {
                    Ok(match (seq2, seq3, seq4) {
                        ('2', '0', '0') => Key::BracketedPasteStart.into(),
                        ('2', '0', '1') => Key::BracketedPasteEnd.into(),
                        _ => {
                            debug!(target: "rustyline",
                                   "unsupported esc sequence: ESC [ {}{}{}~", seq2, seq3, seq4);
                            Key::UnknownEscSeq.into()
                        }
                    })
                } else {
                    debug!(target: "rustyline",
                           "unsupported esc sequence: ESC [ {}{}{} {}", seq2, seq3, seq4, seq5);
                    Ok(Key::UnknownEscSeq.into())
                }
            } else {
                debug!(target: "rustyline",
                       "unsupported esc sequence: ESC [ {}{} {:?}", seq2, seq3, seq4);
                Ok(Key::UnknownEscSeq.into())
            }
        } else if seq3 == ';' {
            let seq4 = self.next_char()?;
            if seq4.is_digit(10) {
                let seq5 = self.next_char()?;
                if seq5.is_digit(10) {
                    self.next_char()?; // 'R' expected
                    Ok(Key::UnknownEscSeq.into())
                } else if seq2 == '1' {
                    Ok(match (seq4, seq5) {
                        ('5', 'A') => KeyPress::ctrl(Key::Up),
                        ('5', 'B') => KeyPress::ctrl(Key::Down),
                        ('5', 'C') => KeyPress::ctrl(Key::Right),
                        ('5', 'D') => KeyPress::ctrl(Key::Left),
                        ('2', 'A') => KeyPress::shift(Key::Up),
                        ('2', 'B') => KeyPress::shift(Key::Down),
                        ('2', 'C') => KeyPress::shift(Key::Right),
                        ('2', 'D') => KeyPress::shift(Key::Left),
                        _ => {
                            debug!(target: "rustyline",
                                   "unsupported esc sequence: ESC [ 1 ; {} {:?}", seq4, seq5);
                            Key::UnknownEscSeq.into()
                        }
                    })
                } else {
                    debug!(target: "rustyline",
                           "unsupported esc sequence: ESC [ {} ; {} {:?}", seq2, seq4, seq5);
                    Ok(Key::UnknownEscSeq.into())
                }
            } else {
                debug!(target: "rustyline",
                       "unsupported esc sequence: ESC [ {} ; {:?}", seq2, seq4);
                Ok(Key::UnknownEscSeq.into())
            }
        } else {
            Ok(match (seq2, seq3) {
                ('5', 'A') => KeyPress::ctrl(Key::Up),
                ('5', 'B') => KeyPress::ctrl(Key::Down),
                ('5', 'C') => KeyPress::ctrl(Key::Right),
                ('5', 'D') => KeyPress::ctrl(Key::Left),
                _ => {
                    debug!(target: "rustyline",
                           "unsupported esc sequence: ESC [ {} {:?}", seq2, seq3);
                    Key::UnknownEscSeq.into()
                }
            })
        }
    }

    /// Handle ESC O <seq2> escape sequences
    fn escape_o(&mut self) -> Result<KeyPress> {
        let seq2 = self.next_char()?;
        Ok(match seq2 {
            'A' => KeyPress::UP,     // kcuu1
            'B' => KeyPress::DOWN,   // kcud1
            'C' => KeyPress::RIGHT,  // kcuf1
            'D' => KeyPress::LEFT,   // kcub1
            'F' => KeyPress::END,    // kend
            'H' => KeyPress::HOME,   // khome
            'P' => Key::F(1).into(), // kf1
            'Q' => Key::F(2).into(), // kf2
            'R' => Key::F(3).into(), // kf3
            'S' => Key::F(4).into(), // kf4
            'a' => KeyPress::ctrl(Key::Up),
            'b' => KeyPress::ctrl(Key::Down),
            'c' => KeyPress::ctrl(Key::Right), // rxvt
            'd' => KeyPress::ctrl(Key::Left),  // rxvt
            _ => {
                debug!(target: "rustyline", "unsupported esc sequence: ESC O {:?}", seq2);
                Key::UnknownEscSeq.into()
            }
        })
    }

    fn poll(&mut self, timeout_ms: i32) -> ::nix::Result<i32> {
        let mut fds = [poll::PollFd::new(STDIN_FILENO, PollFlags::POLLIN)];
        poll::poll(&mut fds, timeout_ms)
    }
}

impl RawReader for PosixRawReader {
    fn next_key(&mut self, single_esc_abort: bool) -> Result<KeyPress> {
        let c = self.next_char()?;

        let mut key = keys::char_to_key_press(c);
        if key == KeyPress::ESC {
            let timeout_ms = if single_esc_abort && self.timeout_ms == -1 {
                0
            } else {
                self.timeout_ms
            };
            match self.poll(timeout_ms) {
                Ok(n) if n == 0 => {
                    // single escape
                }
                Ok(_) => {
                    // escape sequence
                    key = self.escape_sequence()?
                }
                // Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        debug!(target: "rustyline", "key: {:?}", key);
        Ok(key)
    }

    fn next_char(&mut self) -> Result<char> {
        loop {
            let n = self.stdin.read(&mut self.buf)?;
            if n == 0 {
                return Err(error::ReadlineError::Eof);
            }
            let b = self.buf[0];
            self.parser.advance(&mut self.receiver, b);
            if !self.receiver.valid {
                return Err(error::ReadlineError::Utf8Error);
            } else if let Some(c) = self.receiver.c.take() {
                return Ok(c);
            }
        }
    }

    fn read_pasted_text(&mut self) -> Result<String> {
        let mut buffer = String::new();
        loop {
            match self.next_char()? {
                '\x1b' => {
                    let key = self.escape_sequence()?;
                    if key.key == Key::BracketedPasteEnd {
                        break;
                    } else {
                        continue; // TODO validate
                    }
                }
                c => buffer.push(c),
            };
        }
        let buffer = buffer.replace("\r\n", "\n");
        let buffer = buffer.replace("\r", "\n");
        Ok(buffer)
    }
}

impl Receiver for Utf8 {
    /// Called whenever a code point is parsed successfully
    fn codepoint(&mut self, c: char) {
        self.c = Some(c);
        self.valid = true;
    }

    /// Called when an invalid_sequence is detected
    fn invalid_sequence(&mut self) {
        self.c = None;
        self.valid = false;
    }
}

/// Console output writer
pub struct PosixRenderer {
    out: OutputStreamType,
    cols: usize, // Number of columns in terminal
    buffer: String,
    tab_stop: usize,
    colors_enabled: bool,
    bell_style: BellStyle,
}

impl PosixRenderer {
    fn new(
        out: OutputStreamType,
        tab_stop: usize,
        colors_enabled: bool,
        bell_style: BellStyle,
    ) -> Self {
        let (cols, _) = get_win_size(&out);
        Self {
            out,
            cols,
            buffer: String::with_capacity(1024),
            tab_stop,
            colors_enabled,
            bell_style,
        }
    }
}

impl Renderer for PosixRenderer {
    type Reader = PosixRawReader;

    fn move_cursor(&mut self, old: Position, new: Position) -> Result<()> {
        use std::fmt::Write;
        self.buffer.clear();
        let row_ordering = new.row.cmp(&old.row);
        if row_ordering == Ordering::Greater {
            // move down
            let row_shift = new.row - old.row;
            if row_shift == 1 {
                self.buffer.push_str("\x1b[B");
            } else {
                write!(self.buffer, "\x1b[{}B", row_shift).unwrap();
            }
        } else if row_ordering == Ordering::Less {
            // move up
            let row_shift = old.row - new.row;
            if row_shift == 1 {
                self.buffer.push_str("\x1b[A");
            } else {
                write!(self.buffer, "\x1b[{}A", row_shift).unwrap();
            }
        }
        let col_ordering = new.col.cmp(&old.col);
        if col_ordering == Ordering::Greater {
            // move right
            let col_shift = new.col - old.col;
            if col_shift == 1 {
                self.buffer.push_str("\x1b[C");
            } else {
                write!(self.buffer, "\x1b[{}C", col_shift).unwrap();
            }
        } else if col_ordering == Ordering::Less {
            // move left
            let col_shift = old.col - new.col;
            if col_shift == 1 {
                self.buffer.push_str("\x1b[D");
            } else {
                write!(self.buffer, "\x1b[{}D", col_shift).unwrap();
            }
        }
        self.write_and_flush(self.buffer.as_bytes())
    }

    fn refresh_line(
        &mut self,
        prompt: &str,
        line: &LineBuffer,
        hint: Option<&str>,
        old_layout: &Layout,
        new_layout: &Layout,
        highlighter: Option<&dyn Highlighter>,
    ) -> Result<()> {
        use std::fmt::Write;
        self.buffer.clear();

        let default_prompt = new_layout.default_prompt;
        let mut cursor = new_layout.cursor;
        let end_pos = new_layout.end;
        let current_row = old_layout.cursor.row;
        let old_rows = old_layout.end.row;

        // old_rows < cursor.row if the prompt spans multiple lines and if
        // this is the default State.
        let cursor_row_movement = old_rows.saturating_sub(current_row);
        // move the cursor down as required
        if cursor_row_movement > 0 {
            write!(self.buffer, "\x1b[{}B", cursor_row_movement).unwrap();
        }
        // clear old rows
        for _ in 0..old_rows {
            self.buffer.push_str("\r\x1b[0K\x1b[A");
        }
        // clear the line
        self.buffer.push_str("\r\x1b[0K");

        add_prompt_and_highlight(
            &mut self.buffer,
            highlighter,
            line,
            prompt,
            default_prompt,
            &new_layout,
            &mut cursor,
        );
        // display hint
        if let Some(hint) = hint {
            if let Some(highlighter) = highlighter {
                self.buffer.push_str(&highlighter.highlight_hint(hint));
            } else {
                self.buffer.push_str(hint);
            }
        }
        // position the cursor
        let new_cursor_row_movement = end_pos.row - cursor.row;
        // move the cursor up as required
        if new_cursor_row_movement > 0 {
            write!(self.buffer, "\x1b[{}A", new_cursor_row_movement).unwrap();
        }
        // position the cursor within the line
        if cursor.col == 0 {
            self.buffer.push('\r');
        } else {
            write!(self.buffer, "\r\x1b[{}C", cursor.col).unwrap();
        }

        self.write_and_flush(self.buffer.as_bytes())?;

        Ok(())
    }

    fn write_and_flush(&self, buf: &[u8]) -> Result<()> {
        write_and_flush(self.out, buf)
    }

    /// Control characters are treated as having zero width.
    /// Characters with 2 column width are correctly handled (not split).
    fn calculate_position(&self, s: &str, orig: Position) -> Position {
        let mut pos = orig;
        let mut esc_seq = 0;
        for c in s.graphemes(true) {
            if c == "\n" {
                pos.row += 1;
                pos.col = 0;
                continue;
            }
            let cw = if c == "\t" {
                self.tab_stop - (pos.col % self.tab_stop)
            } else {
                width(c, &mut esc_seq)
            };
            pos.col += cw;
            if pos.col > self.cols {
                pos.row += 1;
                pos.col = cw;
            }
        }
        if pos.col == self.cols {
            pos.col = 0;
            pos.row += 1;
        }
        pos
    }

    fn beep(&mut self) -> Result<()> {
        match self.bell_style {
            BellStyle::Audible => {
                io::stderr().write_all(b"\x07")?;
                io::stderr().flush()?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Clear the screen. Used to handle ctrl+l
    fn clear_screen(&mut self) -> Result<()> {
        self.write_and_flush(b"\x1b[H\x1b[2J")
    }

    /// Check if a SIGWINCH signal has been received
    fn sigwinch(&self) -> bool {
        SIGWINCH.compare_and_swap(true, false, atomic::Ordering::SeqCst)
    }

    /// Try to update the number of columns in the current terminal,
    fn update_size(&mut self) {
        let (cols, _) = get_win_size(&self.out);
        self.cols = cols;
    }

    fn get_columns(&self) -> usize {
        self.cols
    }

    /// Try to get the number of rows in the current terminal,
    /// or assume 24 if it fails.
    fn get_rows(&self) -> usize {
        let (_, rows) = get_win_size(&self.out);
        rows
    }

    fn colors_enabled(&self) -> bool {
        self.colors_enabled
    }

    fn move_cursor_at_leftmost(&mut self, rdr: &mut PosixRawReader) -> Result<()> {
        if rdr.poll(0)? != 0 {
            debug!(target: "rustyline", "cannot request cursor location");
            return Ok(());
        }
        /* Report cursor location */
        self.write_and_flush(b"\x1b[6n")?;
        /* Read the response: ESC [ rows ; cols R */
        if rdr.poll(100)? == 0
            || rdr.next_char()? != '\x1b'
            || rdr.next_char()? != '['
            || read_digits_until(rdr, ';')?.is_none()
        {
            warn!(target: "rustyline", "cannot read initial cursor location");
            return Ok(());
        }
        let col = read_digits_until(rdr, 'R')?;
        debug!(target: "rustyline", "initial cursor location: {:?}", col);
        if col.is_some() && col != Some(1) {
            self.write_and_flush(b"\n")?;
        }
        Ok(())
    }
}

fn width(s: &str, esc_seq: &mut u8) -> usize {
    if *esc_seq == 1 {
        if s == "[" {
            // CSI
            *esc_seq = 2;
        } else {
            // two-character sequence
            *esc_seq = 0;
        }
        0
    } else if *esc_seq == 2 {
        if s == ";" || (s.as_bytes()[0] >= b'0' && s.as_bytes()[0] <= b'9') {
            /*} else if s == "m" {
            // last
             *esc_seq = 0;*/
        } else {
            // not supported
            *esc_seq = 0;
        }
        0
    } else if s == "\x1b" {
        *esc_seq = 1;
        0
    } else if s == "\n" {
        0
    } else {
        s.width()
    }
}

fn read_digits_until(rdr: &mut PosixRawReader, sep: char) -> Result<Option<u32>> {
    let mut num: u32 = 0;
    loop {
        match rdr.next_char()? {
            digit @ '0'..='9' => {
                num = num
                    .saturating_mul(10)
                    .saturating_add(digit.to_digit(10).unwrap());
                continue;
            }
            c if c == sep => break,
            _ => return Ok(None),
        }
    }
    Ok(Some(num))
}

static SIGWINCH_ONCE: sync::Once = sync::Once::new();
static SIGWINCH: atomic::AtomicBool = atomic::AtomicBool::new(false);

fn install_sigwinch_handler() {
    SIGWINCH_ONCE.call_once(|| unsafe {
        let sigwinch = signal::SigAction::new(
            signal::SigHandler::Handler(sigwinch_handler),
            signal::SaFlags::empty(),
            signal::SigSet::empty(),
        );
        let _ = signal::sigaction(signal::SIGWINCH, &sigwinch);
    });
}

extern "C" fn sigwinch_handler(_: libc::c_int) {
    SIGWINCH.store(true, atomic::Ordering::SeqCst);
    debug!(target: "rustyline", "SIGWINCH");
}

#[cfg(not(test))]
pub type Terminal = PosixTerminal;

#[derive(Clone, Debug)]
pub struct PosixTerminal {
    unsupported: bool,
    stdin_isatty: bool,
    stdstream_isatty: bool,
    pub(crate) color_mode: ColorMode,
    stream_type: OutputStreamType,
    tab_stop: usize,
    bell_style: BellStyle,
}

impl PosixTerminal {
    fn colors_enabled(&self) -> bool {
        match self.color_mode {
            ColorMode::Enabled => self.stdstream_isatty,
            ColorMode::Forced => true,
            ColorMode::Disabled => false,
        }
    }
}

impl Term for PosixTerminal {
    type Mode = PosixMode;
    type Reader = PosixRawReader;
    type Writer = PosixRenderer;

    fn new(
        color_mode: ColorMode,
        stream_type: OutputStreamType,
        tab_stop: usize,
        bell_style: BellStyle,
    ) -> Self {
        let term = Self {
            unsupported: is_unsupported_term(),
            stdin_isatty: is_a_tty(STDIN_FILENO),
            stdstream_isatty: is_a_tty(stream_type.as_raw_fd()),
            color_mode,
            stream_type,
            tab_stop,
            bell_style,
        };
        if !term.unsupported && term.stdin_isatty && term.stdstream_isatty {
            install_sigwinch_handler();
        }
        term
    }

    // Init checks:

    /// Check if current terminal can provide a rich line-editing user
    /// interface.
    fn is_unsupported(&self) -> bool {
        self.unsupported
    }

    /// check if stdin is connected to a terminal.
    fn is_stdin_tty(&self) -> bool {
        self.stdin_isatty
    }

    fn is_output_tty(&self) -> bool {
        self.stdstream_isatty
    }

    // Interactive loop:

    fn enable_raw_mode(&mut self) -> Result<Self::Mode> {
        use nix::errno::Errno::ENOTTY;
        use nix::sys::termios::{ControlFlags, InputFlags, LocalFlags, SpecialCharacterIndices};
        if !self.stdin_isatty {
            return Err(nix::Error::from_errno(ENOTTY).into());
        }
        let original_mode = termios::tcgetattr(STDIN_FILENO)?;
        let mut raw = original_mode.clone();
        // disable BREAK interrupt, CR to NL conversion on input,
        // input parity check, strip high bit (bit 8), output flow control
        raw.input_flags &= !(InputFlags::BRKINT
            | InputFlags::ICRNL
            | InputFlags::INPCK
            | InputFlags::ISTRIP
            | InputFlags::IXON);
        // we don't want raw output, it turns newlines into straight line feeds
        // disable all output processing
        // raw.c_oflag = raw.c_oflag & !(OutputFlags::OPOST);

        // character-size mark (8 bits)
        raw.control_flags |= ControlFlags::CS8;
        // disable echoing, canonical mode, extended input processing and signals
        raw.local_flags &=
            !(LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::IEXTEN | LocalFlags::ISIG);
        raw.control_chars[SpecialCharacterIndices::VMIN as usize] = 1; // One character-at-a-time input
        raw.control_chars[SpecialCharacterIndices::VTIME as usize] = 0; // with blocking read
        termios::tcsetattr(STDIN_FILENO, SetArg::TCSADRAIN, &raw)?;

        // enable bracketed paste
        let out = if let Err(e) = write_and_flush(self.stream_type, BRACKETED_PASTE_ON) {
            debug!(target: "rustyline", "Cannot enable bracketed paste: {}", e);
            None
        } else {
            Some(self.stream_type)
        };
        Ok(PosixMode {
            termios: original_mode,
            out,
        })
    }

    /// Create a RAW reader
    fn create_reader(&self, config: &Config) -> Result<PosixRawReader> {
        PosixRawReader::new(config)
    }

    fn create_writer(&self) -> PosixRenderer {
        PosixRenderer::new(
            self.stream_type,
            self.tab_stop,
            self.colors_enabled(),
            self.bell_style,
        )
    }
}

#[cfg(not(test))]
pub fn suspend() -> Result<()> {
    use nix::unistd::Pid;
    // suspend the whole process group
    signal::kill(Pid::from_raw(0), signal::SIGTSTP)?;
    Ok(())
}

fn write_and_flush(out: OutputStreamType, buf: &[u8]) -> Result<()> {
    match out {
        OutputStreamType::Stdout => {
            io::stdout().write_all(buf)?;
            io::stdout().flush()?;
        }
        OutputStreamType::Stderr => {
            io::stderr().write_all(buf)?;
            io::stderr().flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod test {
    use super::{Position, PosixRenderer, PosixTerminal, Renderer};
    use crate::config::{BellStyle, OutputStreamType};

    #[test]
    #[ignore]
    fn prompt_with_ansi_escape_codes() {
        let out = PosixRenderer::new(OutputStreamType::Stdout, 4, true, BellStyle::default());
        let pos = out.calculate_position("\x1b[1;32m>>\x1b[0m ", Position::default());
        assert_eq!(3, pos.col);
        assert_eq!(0, pos.row);
    }

    #[test]
    fn test_unsupported_term() {
        ::std::env::set_var("TERM", "xterm");
        assert_eq!(false, super::is_unsupported_term());

        ::std::env::set_var("TERM", "dumb");
        assert_eq!(true, super::is_unsupported_term());
    }

    #[test]
    fn test_send() {
        fn assert_send<T: Send>() {}
        assert_send::<PosixTerminal>();
    }

    #[test]
    fn test_sync() {
        fn assert_sync<T: Sync>() {}
        assert_sync::<PosixTerminal>();
    }
}
