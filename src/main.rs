use std::io::{self, Read, Write, BufRead, BufReader};
use std::fs::File;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::env;
use std::process;
use std::cmp;

use libc::{termios, STDIN_FILENO, TCSAFLUSH, tcgetattr, tcsetattr, ECHO, ICANON, ISIG, IXON, IEXTEN, ICRNL, BRKINT, INPCK, ISTRIP, OPOST, CS8, VMIN, VTIME};
use signal_hook::consts::signal::SIGWINCH;
use signal_hook::flag;

const HL_NORMAL: u8 = 0;
const HL_NONPRINT: u8 = 1;
const HL_COMMENT: u8 = 2;
const HL_MLCOMMENT: u8 = 3;
const HL_KEYWORD1: u8 = 4;
const HL_KEYWORD2: u8 = 5;
const HL_STRING: u8 = 6;
const HL_NUMBER: u8 = 7;
const HL_MATCH: u8 = 8;

const HL_HIGHLIGHT_STRINGS: i32 = 1 << 0;
const HL_HIGHLIGHT_NUMBERS: i32 = 1 << 1;

const CTRL_F: i32 = 6;
const CTRL_H: i32 = 8;
const TAB: i32 = 9;
const CTRL_L: i32 = 12;
const ENTER: i32 = 13;
const CTRL_Q: i32 = 17;
const CTRL_S: i32 = 19;
const ESC: i32 = 27;
const BACKSPACE: i32 = 127;

const ARROW_LEFT: i32 = 1000;
const ARROW_RIGHT: i32 = 1001;
const ARROW_UP: i32 = 1002;
const ARROW_DOWN: i32 = 1003;
const DEL_KEY: i32 = 1004;
const HOME_KEY: i32 = 1005;
const END_KEY: i32 = 1006;
const PAGE_UP: i32 = 1007;
const PAGE_DOWN: i32 = 1008;

struct EditorSyntax {
    filematch: &'static [&'static str],
    keywords: &'static [&'static str],
    singleline_comment_start: &'static str,
    multiline_comment_start: &'static str,
    multiline_comment_end: &'static str,
    flags: i32,
}

static HLDB: [EditorSyntax; 1] = [
    EditorSyntax {
        filematch: &[".c", ".h", ".cpp", ".hpp", ".cc", ".go", ".rs"],
        keywords: &[
            "auto", "break", "case", "continue", "default", "do", "else", "enum",
            "extern", "for", "goto", "if", "register", "return", "sizeof", "static",
            "struct", "switch", "typedef", "union", "volatile", "while", "NULL",
            "func", "package", "import", "type", "var", "const", "range", "return",
            "fn", "let", "mut", "use", "mod", "pub", "crate", "struct", "enum",
            "impl", "trait", "where", "for", "loop", "while", "if", "else", "match",
            "int|", "long|", "double|", "float|", "char|", "unsigned|", "signed|",
            "void|", "short|", "auto|", "const|", "bool|", "string|", "byte|",
            "u8|", "u16|", "u32|", "u64|", "u128|", "i8|", "i16|", "i32|", "i64|", "i128|",
            "f32|", "f64|", "str|", "String|", "Option|", "Result|", "Vec|", "Box|",
        ],
        singleline_comment_start: "//",
        multiline_comment_start: "/*",
        multiline_comment_end: "*/",
        flags: HL_HIGHLIGHT_STRINGS | HL_HIGHLIGHT_NUMBERS,
    },
];

struct Erow {
    idx: usize,
    chars: Vec<u8>,
    render: Vec<u8>,
    hl: Vec<u8>,
    hl_oc: bool,
}

struct EditorConfig {
    cx: usize,
    cy: usize,
    rowoff: usize,
    coloff: usize,
    screenrows: usize,
    screencols: usize,
    rows: Vec<Erow>,
    dirty: usize,
    quit_times: usize,
    filename: Option<String>,
    statusmsg: String,
    statusmsg_time: u64,
    syntax: Option<&'static EditorSyntax>,
}

struct RawMode {
    orig_termios: termios,
}

impl RawMode {
    fn enable() -> Result<Self, io::Error> {
        let mut orig_termios = unsafe { std::mem::zeroed() };
        if unsafe { tcgetattr(STDIN_FILENO, &mut orig_termios) } == -1 {
            return Err(io::Error::last_os_error());
        }

        let mut raw = orig_termios;
        raw.c_iflag &= !(BRKINT | ICRNL | INPCK | ISTRIP | IXON);
        raw.c_oflag &= !(OPOST);
        raw.c_cflag |= CS8;
        raw.c_lflag &= !(ECHO | ICANON | IEXTEN | ISIG);
        raw.c_cc[VMIN] = 0;
        raw.c_cc[VTIME] = 1;

        if unsafe { tcsetattr(STDIN_FILENO, TCSAFLUSH, &raw) } == -1 {
            return Err(io::Error::last_os_error());
        }

        Ok(RawMode { orig_termios })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { tcsetattr(STDIN_FILENO, TCSAFLUSH, &self.orig_termios) };
    }
}

fn die<S: AsRef<str>>(msg: S) -> ! {
    let _ = io::stdout().write_all(b"\x1b[2J");
    let _ = io::stdout().write_all(b"\x1b[H");
    let _ = io::stdout().flush();
    panic!("{}", msg.as_ref());
}

fn get_window_size() -> Result<(usize, usize), io::Error> {
    let mut ws = unsafe { std::mem::zeroed::<libc::winsize>() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == -1 || ws.ws_col == 0 {
        // Fallback: move cursor to bottom right and ask for position
        if io::stdout().write_all(b"\x1b[999C\x1b[999B").is_err() {
            return Err(io::Error::last_os_error());
        }
        return Err(io::Error::new(io::ErrorKind::Other, "Unable to get window size"));
    }
    Ok((ws.ws_row as usize, ws.ws_col as usize))
}

fn is_separator(c: u8) -> bool {
    c.is_ascii_whitespace() || c == 0 || b",.()+-/*=~%[];".contains(&c)
}

impl Erow {
    fn new(idx: usize, chars: Vec<u8>) -> Self {
        let mut row = Erow {
            idx,
            chars,
            render: Vec::new(),
            hl: Vec::new(),
            hl_oc: false,
        };
        row.update_render();
        row
    }

    fn update_render(&mut self) {
        let mut tabs = 0;
        for &c in &self.chars {
            if c == TAB as u8 { tabs += 1; }
        }

        self.render = Vec::with_capacity(self.chars.len() + tabs * 7);
        for &c in &self.chars {
            if c == TAB as u8 {
                self.render.push(b' ');
                while self.render.len() % 8 != 0 {
                    self.render.push(b' ');
                }
            } else {
                self.render.push(c);
            }
        }
    }
}

static RESIZE_EVENT: OnceLock<Arc<AtomicBool>> = OnceLock::new();

fn main() -> Result<(), io::Error> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: kilo <filename>");
        process::exit(1);
    }

    let _raw_mode = RawMode::enable()?;

    let mut editor = EditorConfig::new();
    editor.open(&args[1]);
    editor.set_status_message("HELP: Ctrl-S = save | Ctrl-Q = quit | Ctrl-F = find");

    // Signal handler for SIGWINCH
    let resize_flag = Arc::new(AtomicBool::new(false));
    RESIZE_EVENT.set(resize_flag.clone()).unwrap();
    flag::register(SIGWINCH, resize_flag).unwrap();

    loop {
        if let Some(flag) = RESIZE_EVENT.get() {
            if flag.load(Ordering::Relaxed) {
                flag.store(false, Ordering::Relaxed);
                editor.handle_resize();
            }
        }
        editor.refresh_screen();
        if !editor.process_keypress() {
            break;
        }
    }

    // Terminal is restored automatically by RawMode drop
    let _ = io::stdout().write_all(b"\x1b[2J");
    let _ = io::stdout().write_all(b"\x1b[H");
    let _ = io::stdout().flush();
    Ok(())
}


impl EditorConfig {
    fn new() -> Self {
        let (rows, cols) = get_window_size().unwrap_or((24, 80));
        EditorConfig {
            cx: 0,
            cy: 0,
            rowoff: 0,
            coloff: 0,
            screenrows: rows.saturating_sub(2),
            screencols: cols,
            rows: Vec::new(),
            dirty: 0,
            quit_times: 1,
            filename: None,
            statusmsg: String::new(),
            statusmsg_time: 0,
            syntax: None,
        }
    }

    fn handle_resize(&mut self) {
        if let Ok((rows, cols)) = get_window_size() {
            self.screenrows = rows.saturating_sub(2);
            self.screencols = cols;
        }
    }

    fn set_status_message(&mut self, msg: &str) {
        self.statusmsg = msg.to_string();
        self.statusmsg_time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    }

    fn open(&mut self, filename: &str) {
        self.filename = Some(filename.to_string());
        self.select_syntax_highlight();

        let path = Path::new(filename);
        if !path.exists() {
            return;
        }

        let file = File::open(path).unwrap_or_else(|_| die("Failed to open file"));
        let reader = BufReader::new(file);

        for line in reader.lines() {
            let line = line.unwrap_or_else(|_| die("Failed to read line"));
            self.insert_row(self.rows.len(), line.into_bytes());
        }
        self.dirty = 0;
    }

    fn select_syntax_highlight(&mut self) {
        self.syntax = None;
        if let Some(ref filename) = self.filename {
            let ext = Path::new(filename).extension().and_then(|e| e.to_str()).unwrap_or("");
            for s in HLDB.iter() {
                for m in s.filematch.iter() {
                    if (m.starts_with('.') && *m == format!(".{}", ext)) || filename.contains(m) {
                        self.syntax = Some(s);
                        return;
                    }
                }
            }
        }
    }

    fn insert_row(&mut self, at: usize, chars: Vec<u8>) {
        if at > self.rows.len() { return; }
        let row = Erow::new(at, chars);
        self.rows.insert(at, row);
        for i in at + 1..self.rows.len() {
            self.rows[i].idx = i;
        }
        self.update_syntax_iterative(at);
        self.dirty += 1;
    }

    fn update_syntax_iterative(&mut self, mut start_idx: usize) {
        while start_idx < self.rows.len() {
            let prev_oc = if start_idx > 0 { self.rows[start_idx-1].hl_oc } else { false };
            let row = &mut self.rows[start_idx];
            let changed = row.update_syntax(self.syntax, prev_oc);
            if !changed { break; }
            start_idx += 1;
        }
    }

    fn refresh_screen(&self) {
        let mut ab = Vec::new();
        ab.extend_from_slice(b"\x1b[?25l");
        ab.extend_from_slice(b"\x1b[H");

        for y in 0..self.screenrows {
            let filerow = y + self.rowoff;
            if filerow >= self.rows.len() {
                if self.rows.is_empty() && y == self.screenrows / 3 {
                    let mut welcome = format!("Kilo editor -- version 0.0.1");
                    if welcome.len() > self.screencols {
                        welcome.truncate(self.screencols);
                    }
                    let mut padding = (self.screencols - welcome.len()) / 2;
                    if padding > 0 {
                        ab.push(b'~');
                        padding -= 1;
                    }
                    for _ in 0..padding { ab.push(b' '); }
                    ab.extend_from_slice(welcome.as_bytes());
                } else {
                    ab.push(b'~');
                }
            } else {
                let row = &self.rows[filerow];
                let mut render = &row.render[..];
                let mut hl = &row.hl[..];
                
                let start = cmp::min(self.coloff, render.len());
                render = &render[start..];
                hl = &hl[start..];

                let len = cmp::min(render.len(), self.screencols);
                render = &render[..len];
                hl = &hl[..len];

                let mut current_color: i32 = -1;
                for (i, &c) in render.iter().enumerate() {
                    if hl[i] == HL_NONPRINT {
                        ab.extend_from_slice(b"\x1b[7m");
                        if c <= 26 {
                            ab.push(b'@' + c);
                        } else {
                            ab.push(b'?');
                        }
                        ab.extend_from_slice(b"\x1b[0m");
                        current_color = -1;
                    } else if hl[i] == HL_NORMAL {
                        if current_color != -1 {
                            ab.extend_from_slice(b"\x1b[39m");
                            current_color = -1;
                        }
                        ab.push(c);
                    } else {
                        let color = self.syntax_to_color(hl[i]);
                        if color != current_color {
                            ab.extend_from_slice(format!("\x1b[{}m", color).as_bytes());
                            current_color = color;
                        }
                        ab.push(c);
                    }
                }
                ab.extend_from_slice(b"\x1b[39m");
            }
            ab.extend_from_slice(b"\x1b[0K\r\n");
        }

        // Status bar
        ab.extend_from_slice(b"\x1b[7m");
        let fname = self.filename.as_deref().unwrap_or("[No Name]");
        let dirty_str = if self.dirty > 0 { "(modified)" } else { "" };
        let status = format!("{:.20} - {} lines {}", fname, self.rows.len(), dirty_str);
        let rstatus = format!("{}/{}", self.rowoff + self.cy + 1, self.rows.len());
        let mut status_len = cmp::min(status.len(), self.screencols);
        ab.extend_from_slice(&status.as_bytes()[..status_len]);
        while status_len < self.screencols {
            if self.screencols - status_len == rstatus.len() {
                ab.extend_from_slice(rstatus.as_bytes());
                break;
            } else {
                ab.push(b' ');
                status_len += 1;
            }
        }
        ab.extend_from_slice(b"\x1b[0m\r\n");

        // Message bar
        ab.extend_from_slice(b"\x1b[0K");
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        if now - self.statusmsg_time < 5 {
            let mut msg = self.statusmsg.clone();
            if msg.len() > self.screencols {
                msg.truncate(self.screencols);
            }
            ab.extend_from_slice(msg.as_bytes());
        }

        // Cursor
        let mut rx: usize = 0;
        let filerow = self.cy + self.rowoff;
        if filerow < self.rows.len() {
            let row = &self.rows[filerow];
            for i in 0..cmp::min(self.cx + self.coloff, row.chars.len()) {
                if row.chars[i] == TAB as u8 {
                    rx += 7 - (rx % 8);
                }
                rx += 1;
            }
        }
        let term_cx = rx.saturating_sub(self.coloff) + 1;
        ab.extend_from_slice(format!("\x1b[{};{}H", self.cy + 1, term_cx).as_bytes());
        ab.extend_from_slice(b"\x1b[?25h");
        let _ = io::stdout().write_all(&ab);
        let _ = io::stdout().flush();
    }

    fn syntax_to_color(&self, hl: u8) -> i32 {
        match hl {
            HL_COMMENT | HL_MLCOMMENT => 36,
            HL_KEYWORD1 => 33,
            HL_KEYWORD2 => 32,
            HL_STRING => 35,
            HL_NUMBER => 31,
            HL_MATCH => 34,
            _ => 37,
        }
    }

    fn process_keypress(&mut self) -> bool {
        let c = self.read_key();
        match c {
            ENTER => self.insert_newline(),
            CTRL_Q => {
                if self.dirty > 0 && self.quit_times > 0 {
                    self.set_status_message("WARNING!!! File has unsaved changes. Press Ctrl-Q again to quit.");
                    self.quit_times -= 1;
                    return true;
                }
                return false;
            }
            CTRL_S => self.save(),
            CTRL_F => self.find(),
            BACKSPACE | CTRL_H | DEL_KEY => {
                if c == DEL_KEY { self.move_cursor(ARROW_RIGHT); }
                self.del_char();
            }
            PAGE_UP | PAGE_DOWN => {
                let mut times = self.screenrows;
                while times > 0 {
                    self.move_cursor(if c == PAGE_UP { ARROW_UP } else { ARROW_DOWN });
                    times -= 1;
                }
            }
            ARROW_UP | ARROW_DOWN | ARROW_LEFT | ARROW_RIGHT => self.move_cursor(c),
            CTRL_L | ESC | 0 => (),
            c if (c as u8).is_ascii_graphic() || (c as u8).is_ascii_whitespace() => self.insert_char(c as u8),
            _ => (),
        }
        self.quit_times = 1;
        true
    }

    fn read_key(&self) -> i32 {
        let mut buf = [0; 1];
        loop {
            match io::stdin().read(&mut buf) {
                Ok(1) => break,
                Ok(_) => continue,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => return 0,
                Err(_) => continue,
            }
        }

        if buf[0] == ESC as u8 {
            let mut seq = [0; 3];
            if io::stdin().read_exact(&mut seq[0..1]).is_err() { return ESC; }
            if io::stdin().read_exact(&mut seq[1..2]).is_err() { return ESC; }

            if seq[0] == b'[' {
                if seq[1] >= b'0' && seq[1] <= b'9' {
                    if io::stdin().read_exact(&mut seq[2..3]).is_err() { return ESC; }
                    if seq[2] == b'~' {
                        match seq[1] {
                            b'3' => return DEL_KEY,
                            b'5' => return PAGE_UP,
                            b'6' => return PAGE_DOWN,
                            _ => (),
                        }
                    }
                } else {
                    match seq[1] {
                        b'A' => return ARROW_UP,
                        b'B' => return ARROW_DOWN,
                        b'C' => return ARROW_RIGHT,
                        b'D' => return ARROW_LEFT,
                        b'H' => return HOME_KEY,
                        b'F' => return END_KEY,
                        _ => (),
                    }
                }
            } else if seq[0] == b'O' {
                match seq[1] {
                    b'H' => return HOME_KEY,
                    b'F' => return END_KEY,
                    _ => (),
                }
            }
            return ESC;
        }
        buf[0] as i32
    }

    fn move_cursor(&mut self, key: i32) {
        let filerow = self.cy + self.rowoff;
        let rowlen = if filerow < self.rows.len() { self.rows[filerow].chars.len() } else { 0 };

        match key {
            ARROW_LEFT => {
                if self.cx + self.coloff > 0 {
                    if self.cx > 0 { self.cx -= 1; } else { self.coloff -= 1; }
                } else if filerow > 0 {
                    if self.cy > 0 {
                        self.cy -= 1;
                    } else {
                        self.rowoff -= 1;
                    }
                    let prev_rowlen = self.rows[filerow-1].chars.len();
                    self.cx = prev_rowlen;
                    if self.cx >= self.screencols {
                        self.coloff = self.cx - self.screencols + 1;
                        self.cx = self.screencols - 1;
                    }
                }
            }
            ARROW_RIGHT => {
                if filerow < self.rows.len() && self.cx + self.coloff < rowlen {
                    if self.cx < self.screencols - 1 { self.cx += 1; } else { self.coloff += 1; }
                } else if filerow < self.rows.len() && self.cx + self.coloff == rowlen {
                    self.cy += 1;
                    self.cx = 0;
                    self.coloff = 0;
                    if self.cy >= self.screenrows {
                        self.rowoff += 1;
                        self.cy -= 1;
                    }
                }
            }
            ARROW_UP => {
                if self.cy > 0 { self.cy -= 1; } else if self.rowoff > 0 { self.rowoff -= 1; }
            }
            ARROW_DOWN => {
                if filerow < self.rows.len() {
                    self.cy += 1;
                    if self.cy >= self.screenrows {
                        self.rowoff += 1;
                        self.cy -= 1;
                    }
                }
            }
            _ => (),
        }

        let filerow = self.cy + self.rowoff;
        let rowlen = if filerow < self.rows.len() { self.rows[filerow].chars.len() } else { 0 };
        if self.cx + self.coloff > rowlen {
            let diff = (self.cx + self.coloff) - rowlen;
            if self.cx >= diff {
                self.cx -= diff;
            } else {
                self.coloff = self.coloff.saturating_sub(diff - self.cx);
                self.cx = 0;
            }
        }
    }

    fn insert_char(&mut self, c: u8) {
        let filerow = self.cy + self.rowoff;
        if filerow == self.rows.len() {
            self.insert_row(self.rows.len(), Vec::new());
        }
        let row = &mut self.rows[filerow];
        let at = self.cx + self.coloff;
        row.chars.insert(at, c);
        row.update_render();
        self.update_syntax_iterative(filerow);
        self.cx += 1;
        if self.cx >= self.screencols {
            self.coloff += 1;
            self.cx -= 1;
        }
        self.dirty += 1;
    }

    fn insert_newline(&mut self) {
        let filerow = self.cy + self.rowoff;
        let at = self.cx + self.coloff;
        if at == 0 {
            self.insert_row(filerow, Vec::new());
        } else {
            let row = &mut self.rows[filerow];
            let new_chars = row.chars.split_off(at);
            row.update_render();
            self.update_syntax_iterative(filerow);
            self.insert_row(filerow + 1, new_chars);
        }
        self.cy += 1;
        self.cx = 0;
        self.coloff = 0;
        if self.cy >= self.screenrows {
            self.rowoff += 1;
            self.cy -= 1;
        }
    }

    fn del_char(&mut self) {
        let filerow = self.cy + self.rowoff;
        let at = self.cx + self.coloff;
        if filerow == self.rows.len() || (at == 0 && filerow == 0) { return; }

        if at > 0 {
            let row = &mut self.rows[filerow];
            row.chars.remove(at - 1);
            row.update_render();
            self.update_syntax_iterative(filerow);
            self.cx -= 1;
            if self.cx == 0 && self.coloff > 0 {
                self.coloff -= 1;
                self.cx += 1;
            }
            self.dirty += 1;
        } else {
            let chars = self.rows[filerow].chars.clone();
            self.rows.remove(filerow);
            for i in filerow..self.rows.len() { self.rows[i].idx = i; }
            let prev_row = &mut self.rows[filerow - 1];
            self.cx = prev_row.chars.len();
            prev_row.chars.extend(chars);
            prev_row.update_render();
            self.update_syntax_iterative(filerow - 1);
            self.cy -= 1;
            if self.cy == 0 && self.rowoff > 0 {
                self.rowoff -= 1;
                self.cy += 1;
            }
            if self.cx >= self.screencols {
                self.coloff = self.cx - self.screencols + 1;
                self.cx = self.screencols - 1;
            }
            self.dirty += 1;
        }
    }

    fn save(&mut self) {
        if let Some(ref filename) = self.filename {
            let mut file = File::create(filename).unwrap_or_else(|e| {
                self.set_status_message(&format!("Can't save! I/O error: {}", e));
                return File::open("/dev/null").unwrap(); // Dummy
            });
            for row in &self.rows {
                let _ = file.write_all(&row.chars);
                let _ = file.write_all(b"\n");
            }
            self.dirty = 0;
            self.set_status_message("File saved to disk");
        }
    }

    fn find(&mut self) {
        let (saved_cx, saved_cy) = (self.cx, self.cy);
        let (saved_coloff, saved_rowoff) = (self.coloff, self.rowoff);
        let mut query = String::new();
        let mut last_match: i32 = -1;
        let mut direction = 1;

        loop {
            self.set_status_message(&format!("Search: {} (ESC/Arrows/Enter)", query));
            self.refresh_screen();

            let c = self.read_key();
            if c == DEL_KEY || c == CTRL_H || c == BACKSPACE {
                query.pop();
                last_match = -1;
            } else if c == ESC || c == ENTER {
                if c == ESC {
                    self.cx = saved_cx;
                    self.cy = saved_cy;
                    self.coloff = saved_coloff;
                    self.rowoff = saved_rowoff;
                }
                self.set_status_message("");
                return;
            } else if c == ARROW_RIGHT || c == ARROW_DOWN {
                direction = 1;
            } else if c == ARROW_LEFT || c == ARROW_UP {
                direction = -1;
            } else if (c as u8).is_ascii_graphic() || (c as u8).is_ascii_whitespace() {
                query.push(c as u8 as char);
                last_match = -1;
            }

            if last_match == -1 { direction = 1; }
            if !query.is_empty() && !self.rows.is_empty() {
                let mut current = last_match;
                for _ in 0..self.rows.len() {
                    current += direction;
                    if current == -1 { current = self.rows.len() as i32 - 1; }
                    else if current == self.rows.len() as i32 { current = 0; }

                    let row = &self.rows[current as usize];
                    if let Some(idx) = String::from_utf8_lossy(&row.render).find(&query) {
                        last_match = current;
                        self.rowoff = current as usize;
                        self.cy = 0;
                        
                        // Map render index back to char index (Fixes the tab bug!)
                        let mut char_idx = 0;
                        let mut render_idx = 0;
                        for &c in &row.chars {
                            if c == TAB as u8 {
                                render_idx += 8 - (render_idx % 8);
                            } else {
                                render_idx += 1;
                            }
                            if render_idx > idx { break; }
                            char_idx += 1;
                        }
                        
                        self.cx = char_idx;
                        self.coloff = 0;
                        if self.cx >= self.screencols {
                            self.coloff = self.cx - self.screencols + 1;
                            self.cx = self.screencols - 1;
                        }
                        break;
                    }
                }
            }
        }
    }
}

impl Erow {
    fn update_syntax(&mut self, syntax: Option<&EditorSyntax>, prev_oc: bool) -> bool {
        self.hl = vec![HL_NORMAL; self.render.len()];
        if syntax.is_none() { return false; }
        let s = syntax.unwrap();

        let mut in_string = 0u8;
        let mut in_comment = prev_oc;
        let mut prev_sep = true;

        let scs = s.singleline_comment_start;
        let mcs = s.multiline_comment_start;
        let mce = s.multiline_comment_end;

        let mut i = 0;
        while i < self.render.len() {
            let c = self.render[i];
            let prev_hl = if i > 0 { self.hl[i-1] } else { HL_NORMAL };

            if !scs.is_empty() && in_string == 0 && !in_comment && i + scs.len() <= self.render.len() {
                if &self.render[i..i+scs.len()] == scs.as_bytes() {
                    for j in i..self.hl.len() { self.hl[j] = HL_COMMENT; }
                    break;
                }
            }

            if in_comment {
                self.hl[i] = HL_MLCOMMENT;
                if !mce.is_empty() && i + mce.len() <= self.render.len() {
                    if &self.render[i..i+mce.len()] == mce.as_bytes() {
                        for _ in 0..mce.len() {
                            self.hl[i] = HL_MLCOMMENT;
                            i += 1;
                        }
                        in_comment = false;
                        prev_sep = true;
                        continue;
                    }
                }
                i += 1;
                continue;
            } else if !mcs.is_empty() && i + mcs.len() <= self.render.len() {
                if &self.render[i..i+mcs.len()] == mcs.as_bytes() {
                    for _ in 0..mcs.len() {
                        self.hl[i] = HL_MLCOMMENT;
                        i += 1;
                    }
                    in_comment = true;
                    continue;
                }
            }

            if in_string != 0 {
                self.hl[i] = HL_STRING;
                if c == b'\\' && i + 1 < self.render.len() {
                    self.hl[i+1] = HL_STRING;
                    i += 2;
                    continue;
                }
                if c == in_string { in_string = 0; }
                i += 1;
                prev_sep = true;
                continue;
            } else if c == b'"' || c == b'\'' {
                in_string = c;
                self.hl[i] = HL_STRING;
                i += 1;
                continue;
            }

            if !c.is_ascii_graphic() && !c.is_ascii_whitespace() {
                self.hl[i] = HL_NONPRINT;
                i += 1;
                prev_sep = false;
                continue;
            }

            if (s.flags & HL_HIGHLIGHT_NUMBERS) != 0 {
                if (c.is_ascii_digit() && (prev_sep || prev_hl == HL_NUMBER)) || (c == b'.' && prev_hl == HL_NUMBER) {
                    self.hl[i] = HL_NUMBER;
                    i += 1;
                    prev_sep = false;
                    continue;
                }
            }

            if prev_sep {
                let mut matched = false;
                for kw in s.keywords {
                    let mut klen = kw.len();
                    let kw2 = kw.ends_with('|');
                    if kw2 { klen -= 1; }

                    if i + klen <= self.render.len() && &self.render[i..i+klen] == kw[..klen].as_bytes() {
                        if i + klen == self.render.len() || is_separator(self.render[i + klen]) {
                            let color = if kw2 { HL_KEYWORD2 } else { HL_KEYWORD1 };
                            for j in 0..klen { self.hl[i+j] = color; }
                            i += klen;
                            matched = true;
                            break;
                        }
                    }
                }
                if matched {
                    prev_sep = false;
                    continue;
                }
            }

            prev_sep = is_separator(c);
            i += 1;
        }

        let oc = self.has_open_comment(mce);
        let changed = self.hl_oc != oc;
        self.hl_oc = oc;
        changed
    }

    fn has_open_comment(&self, mce: &str) -> bool {
        if !self.hl.is_empty() && self.hl[self.hl.len()-1] == HL_MLCOMMENT {
            if self.render.len() < mce.len() || &self.render[self.render.len()-mce.len()..] != mce.as_bytes() {
                return true;
            }
        }
        false
    }
}
