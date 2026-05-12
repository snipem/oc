//! oc — two-pane file manager
//! zero external crates; terminal I/O via x86-64 inline syscalls
//!
//! Usage: oc [left] [right]
//!   left/right: local path | ftp://host | bare host (resolved via ~/.netrc)
//!
//! Keys
//!   Tab      switch pane          ↑↓/PgUp PgDn/Home End  navigate
//!   Enter/→  enter directory      ←  back
//!   r  rename    c  copy→other    m  move→other
//!   d  delete    n  mkdir         g  goto path
//!   C  connect (netrc FTP)        q  quit

#![allow(clippy::all)]

use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::path::PathBuf;

// ─────────────────────────────────────────────────────────────────────────────
// Inline syscalls — x86-64 Linux, no libc
// ─────────────────────────────────────────────────────────────────────────────

#[inline(always)]
unsafe fn sys3(n: u64, a: u64, b: u64, c: u64) -> i64 {
    let r: i64;
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") n as i64 => r,
            in("rdi") a, in("rsi") b, in("rdx") c,
            out("rcx") _, out("r11") _,
            options(nostack)
        );
    }
    r
}

const SYS_READ:  u64 = 0;
const SYS_IOCTL: u64 = 16;

// ─────────────────────────────────────────────────────────────────────────────
// Terminal raw mode
// ─────────────────────────────────────────────────────────────────────────────

const TCGETS:     u64 = 0x5401;
const TCSETS:     u64 = 0x5402;
const TIOCGWINSZ: u64 = 0x5413;

const ICRNL:  u32 = 0x100;
const IXON:   u32 = 0x400;
const OPOST:  u32 = 0x1;
const ECHO:   u32 = 0x8;
const ICANON: u32 = 0x2;
const ISIG:   u32 = 0x1;
const IEXTEN: u32 = 0x8000;

const VTIME: usize = 5;
const VMIN:  usize = 6;

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line:  u8,
    c_cc:    [u8; 19],
}

static mut SAVED_TERM: Termios = Termios {
    c_iflag: 0, c_oflag: 0, c_cflag: 0, c_lflag: 0, c_line: 0, c_cc: [0; 19],
};

fn tget(t: &mut Termios) {
    unsafe { sys3(SYS_IOCTL, 0, TCGETS, t as *mut Termios as u64); }
}
fn tset(t: &Termios) {
    unsafe { sys3(SYS_IOCTL, 0, TCSETS, t as *const Termios as u64); }
}

fn raw_on() {
    let mut t = Termios::default();
    tget(&mut t);
    unsafe { SAVED_TERM = t; }
    t.c_iflag &= !(ICRNL | IXON);
    t.c_oflag &= !OPOST;
    t.c_lflag &= !(ECHO | ICANON | ISIG | IEXTEN);
    t.c_cc[VMIN]  = 1;
    t.c_cc[VTIME] = 0;
    tset(&t);
}

fn raw_off() {
    let t = unsafe { SAVED_TERM };
    tset(&t);
}

fn term_size() -> (u16, u16) {
    #[repr(C)]
    struct Ws { rows: u16, cols: u16, _xp: u16, _yp: u16 }
    let mut ws = Ws { rows: 24, cols: 80, _xp: 0, _yp: 0 };
    unsafe { sys3(SYS_IOCTL, 1, TIOCGWINSZ, &mut ws as *mut Ws as u64); }
    (ws.rows, ws.cols)
}

// ─────────────────────────────────────────────────────────────────────────────
// Keyboard
// ─────────────────────────────────────────────────────────────────────────────

fn read_byte() -> u8 {
    let mut b = 0u8;
    unsafe { sys3(SYS_READ, 0, &mut b as *mut u8 as u64, 1); }
    b
}

fn read_byte_timed(tenths: u8) -> Option<u8> {
    let mut t = Termios::default();
    tget(&mut t);
    let (vm, vt) = (t.c_cc[VMIN], t.c_cc[VTIME]);
    t.c_cc[VMIN] = 0; t.c_cc[VTIME] = tenths;
    tset(&t);
    let mut b = 0u8;
    let n = unsafe { sys3(SYS_READ, 0, &mut b as *mut u8 as u64, 1) };
    t.c_cc[VMIN] = vm; t.c_cc[VTIME] = vt;
    tset(&t);
    (n == 1).then_some(b)
}

#[derive(Debug, Clone)]
enum Key {
    Char(char), Up, Down, Left, Right,
    Enter, Backspace, Insert, Delete,
    PageUp, PageDown, Home, End,
    Esc, F(u8),
    Click  { btn: u8, col: u16, row: u16 },
    Scroll { up: bool, col: u16, row: u16 },
}

// Parse up to 3 semicolon-separated decimal numbers from a byte slice.
fn parse_nums(b: &[u8]) -> [u32; 3] {
    let mut out = [1u32; 3]; // default 1 (1-based coords)
    let mut i = 0usize;
    let mut cur = 0u32;
    let mut has = false;
    for &c in b {
        if c == b';' {
            if has { out[i] = cur; }
            i = (i + 1).min(2); cur = 0; has = false;
        } else if c.is_ascii_digit() {
            cur = cur * 10 + (c - b'0') as u32; has = true;
        }
    }
    if has { out[i] = cur; }
    out
}

// Classify a complete CSI payload (everything after ESC[ up to and including final byte).
fn parse_csi(buf: &[u8]) -> Option<Key> {
    let &fin = buf.last()?;

    // SGR mouse: ESC[<Pb;Px;PyM (press) or m (release)
    if buf.first() == Some(&b'<') {
        if fin == b'm' { return None; } // release — ignore
        let nums = parse_nums(&buf[1..buf.len() - 1]);
        let (btn, col, row) = (nums[0] as u8, nums[1] as u16, nums[2] as u16);
        return Some(match btn {
            64        => Key::Scroll { up: true,  col, row },
            65        => Key::Scroll { up: false, col, row },
            b if b < 4 => Key::Click { btn: b,   col, row },
            _         => return None, // motion/shift-click etc — ignore
        });
    }

    // Single-letter cursor keys
    if buf.len() == 1 {
        return Some(match fin {
            b'A' => Key::Up,    b'B' => Key::Down,
            b'C' => Key::Right, b'D' => Key::Left,
            b'H' => Key::Home,  b'F' => Key::End,
            b'Z' => Key::Char('\t'), // Shift+Tab — same action as Tab (toggle pane)
            _ => return None,
        });
    }

    // Numeric parameter sequences (n~ or 1;mods{letter})
    let n = parse_nums(&buf[..buf.len() - 1])[0];
    Some(match (n, fin) {
        (1|7, b'~') | (_, b'H') => Key::Home,
        (4|8, b'~') | (_, b'F') => Key::End,
        (2, b'~')  => Key::Insert,
        (3, b'~')  => Key::Delete,
        (5, b'~')  => Key::PageUp,
        (6, b'~')  => Key::PageDown,
        (11, b'~') => Key::F(1),  (12, b'~') => Key::F(2),
        (13, b'~') => Key::F(3),  (14, b'~') => Key::F(4),
        (15, b'~') => Key::F(5),  (17, b'~') => Key::F(6),
        (18, b'~') => Key::F(7),  (19, b'~') => Key::F(8),
        (20, b'~') => Key::F(9),  (21, b'~') => Key::F(10),
        (23, b'~') => Key::F(11), (24, b'~') => Key::F(12),
        _ => return None,
    })
}

fn read_key() -> Key {
    'outer: loop {
        return match read_byte() {
            0x1b => match read_byte_timed(1) {
                Some(b'[') => {
                    // Collect full CSI sequence: everything up to and including
                    // the final byte (0x40–0x7E).
                    let mut buf = Vec::with_capacity(16);
                    loop {
                        let b = read_byte_timed(1).unwrap_or(0);
                        if b == 0 { continue 'outer; }
                        buf.push(b);
                        if (0x40..=0x7E).contains(&b) { break; }
                    }
                    match parse_csi(&buf) {
                        Some(k) => k,
                        None    => continue 'outer,
                    }
                }
                Some(b'O') => match read_byte_timed(1).unwrap_or(0) {
                    b'P' => Key::F(1), b'Q' => Key::F(2),
                    b'R' => Key::F(3), b'S' => Key::F(4),
                    _    => continue 'outer,
                },
                None => Key::Esc,
                _    => continue 'outer,
            },
            b'\r' | b'\n'           => Key::Enter,
            0x7f | 0x08             => Key::Backspace,
            b if b.is_ascii()       => Key::Char(b as char),
            _ => continue 'outer,
        };
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ANSI output
// ─────────────────────────────────────────────────────────────────────────────

type Out = BufWriter<io::Stdout>;

macro_rules! esc { ($o:expr, $s:literal) => { write!($o, concat!("\x1b[", $s)).unwrap() } }
macro_rules! tc {
    ($o:expr, fg $r:expr, $g:expr, $b:expr) =>
        { write!($o, "\x1b[38;2;{};{};{}m", $r, $g, $b).unwrap() };
    ($o:expr, bg $r:expr, $g:expr, $b:expr) =>
        { write!($o, "\x1b[48;2;{};{};{}m", $r, $g, $b).unwrap() };
}

fn clr(o: &mut Out) {
    let (rows, cols) = term_size();
    tc!(o, bg 0x00,0x00,0xAA); tc!(o, fg 0x55,0xFF,0xFF);
    for r in 1..=rows {
        goto(o, r, 1);
        write!(o, "{:width$}", "", width = cols as usize).unwrap();
    }
    goto(o, 1, 1);
}
fn goto(o: &mut Out, r: u16, c: u16) { write!(o, "\x1b[{};{}H", r, c).unwrap(); }
fn hide_cur(o: &mut Out)       { esc!(o, "?25l"); }
fn show_cur(o: &mut Out)       { esc!(o, "?25h"); }
// CGA exact hex values — exact DOS CGA colours via 24-bit SGR
// so the terminal's own palette remapping cannot alter them.
//   blue  #0000AA   cyan  #00AAAA   bright-cyan  #55FFFF
//   white #FFFFFF   lgray #AAAAAA   dgray        #555555
//   black #000000   yellow #FFFF55

fn c_reset(o: &mut Out) { esc!(o, "0m"); }

// Active pane header / dialog box borders — yellow on blue
fn c_hdr_act(o: &mut Out) { tc!(o, fg 0xFF,0xFF,0x55); tc!(o, bg 0x00,0x00,0xAA); }

// Inactive pane header — light grey on blue
fn c_hdr(o: &mut Out) { tc!(o, fg 0xAA,0xAA,0xAA); tc!(o, bg 0x00,0x00,0xAA); }

// Selected item in the active pane — black on dark cyan
fn c_sel(o: &mut Out) { tc!(o, fg 0x00,0x00,0x00); tc!(o, bg 0x00,0xAA,0xAA); }

// Selected item in the inactive pane — light grey on dark grey
fn c_sel_inactive(o: &mut Out) { tc!(o, fg 0xAA,0xAA,0xAA); tc!(o, bg 0x55,0x55,0x55); }

// Directory entries — bright white on blue
fn c_dir(o: &mut Out) { tc!(o, fg 0xFF,0xFF,0xFF); tc!(o, bg 0x00,0x00,0xAA); }

// Normal files and dialog interior — bright cyan on blue
fn c_norm(o: &mut Out) { tc!(o, fg 0x55,0xFF,0xFF); tc!(o, bg 0x00,0x00,0xAA); }

// Status / function-key bar — black on dark cyan
fn c_status(o: &mut Out) { tc!(o, fg 0x00,0x00,0x00); tc!(o, bg 0x00,0xAA,0xAA); }

// Selected file (not cursor) — yellow on blue
fn c_sel_file(o: &mut Out) { tc!(o, fg 0xFF,0xFF,0x55); tc!(o, bg 0x00,0x00,0xAA); }

fn human_size(n: u64) -> String {
    let units = ["B","K","M","G","T"];
    let mut v = n as f64;
    for (i, &u) in units.iter().enumerate() {
        if v < 1024.0 || i == units.len() - 1 {
            return if v < 10.0 && i > 0 { format!("{:.1}{}", v, u) }
                   else                  { format!("{:.0}{}", v, u) };
        }
        v /= 1024.0;
    }
    unreachable!()
}

fn mouse_on(o: &mut Out)  { write!(o, "\x1b[?1000h\x1b[?1006h").unwrap(); o.flush().unwrap(); }
fn mouse_off(o: &mut Out) { write!(o, "\x1b[?1006l\x1b[?1000l").unwrap(); o.flush().unwrap(); }

// ─────────────────────────────────────────────────────────────────────────────
// Netrc parser
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct NetrcEntry { machine: String, login: String, password: String }

fn load_netrc() -> Vec<NetrcEntry> {
    let home = std::env::var("HOME").unwrap_or_default();
    let text  = fs::read_to_string(format!("{}/.netrc", home)).unwrap_or_default();
    let mut out: Vec<NetrcEntry> = Vec::new();
    let mut cur = NetrcEntry::default();
    let mut state: u8 = 0; // 0=key, 1=machine, 2=login, 3=password
    let mut in_entry = false;

    for tok in text.split_ascii_whitespace() {
        if state != 0 {
            match state {
                1 => { cur.machine  = tok.into(); }
                2 => { cur.login    = tok.into(); }
                3 => { cur.password = tok.into(); }
                _ => {}
            }
            state = 0;
            continue;
        }
        match tok {
            "machine" | "default" => {
                if in_entry { out.push(cur.clone()); }
                cur = NetrcEntry::default();
                if tok == "default" { cur.machine = "*".into(); } else { state = 1; }
                in_entry = true;
            }
            "login"    => state = 2,
            "password" => state = 3,
            _ => {}
        }
    }
    if in_entry { out.push(cur); }
    out
}

fn netrc_creds(host: &str) -> Option<(String, String)> {
    load_netrc().into_iter()
        .find(|e| e.machine == host || e.machine == "*")
        .map(|e| (e.login, e.password))
}

// ─────────────────────────────────────────────────────────────────────────────
// FTP client — stdlib TcpStream, no crates
// ─────────────────────────────────────────────────────────────────────────────

struct Ftp { ctrl: BufReader<TcpStream>, host: String }

impl Ftp {
    fn connect(host: &str, user: &str, pass: &str) -> Result<Self, String> {
        let addr = if host.contains(':') { host.to_string() } else { format!("{}:21", host) };
        let s = TcpStream::connect(&addr).map_err(|e| e.to_string())?;
        let mut f = Ftp { ctrl: BufReader::new(s), host: host.to_string() };
        f.expect(220)?;
        f.cmd(&format!("USER {}", user))?; f.expect(331)?;
        f.cmd(&format!("PASS {}", pass))?; f.expect(230)?;
        Ok(f)
    }

    fn cmd(&mut self, c: &str) -> Result<(), String> {
        write!(self.ctrl.get_mut(), "{}\r\n", c).map_err(|e| e.to_string())
    }

    fn response(&mut self) -> Result<(u16, String), String> {
        let mut line = String::new();
        self.ctrl.read_line(&mut line).map_err(|e| e.to_string())?;
        let line = line.trim_end().to_string();
        let code: u16 = line.get(..3).unwrap_or("0").parse().unwrap_or(0);
        if line.get(3..4) == Some("-") {
            let marker = format!("{} ", code);
            loop {
                let mut l = String::new();
                self.ctrl.read_line(&mut l).map_err(|e| e.to_string())?;
                if l.starts_with(&marker) { break; }
            }
        }
        Ok((code, line))
    }

    fn expect(&mut self, code: u16) -> Result<String, String> {
        let (c, msg) = self.response()?;
        if c == code { Ok(msg) } else { Err(format!("got {} expected {}: {}", c, code, msg)) }
    }

    fn pasv_addr(&mut self) -> Result<String, String> {
        self.cmd("PASV")?;
        let msg = self.expect(227)?;
        let s = msg.find('(').ok_or("no (")?;
        let e = msg.find(')').ok_or("no )")?;
        let n: Vec<u8> = msg[s+1..e].split(',').filter_map(|x| x.trim().parse().ok()).collect();
        if n.len() < 6 { return Err("bad PASV".into()); }
        Ok(format!("{}.{}.{}.{}:{}", n[0], n[1], n[2], n[3], (n[4] as u16)*256 + n[5] as u16))
    }

    fn list(&mut self) -> Vec<FtpEntry> {
        let addr = self.pasv_addr().unwrap_or_default();
        if self.cmd("LIST").is_err() { return vec![]; }
        let conn = match TcpStream::connect(&addr) { Ok(s) => s, Err(_) => return vec![] };
        let _ = self.expect(150);
        let mut v: Vec<FtpEntry> = BufReader::new(conn).lines().flatten()
            .filter_map(|l| parse_ls(&l)).collect();
        let _ = self.expect(226);
        v.sort_by(|a, b| (!a.is_dir).cmp(&(!b.is_dir)).then(a.name.cmp(&b.name)));
        v
    }

    fn cwd(&mut self, d: &str) -> bool {
        self.cmd(&format!("CWD {}", d)).is_ok() && self.expect(250).is_ok()
    }
    fn cdup(&mut self) -> bool {
        self.cmd("CDUP").is_ok() && self.expect(250).is_ok()
    }
    fn pwd(&mut self) -> String {
        if self.cmd("PWD").is_err() { return "/".into(); }
        let msg = self.expect(257).unwrap_or_default();
        if let (Some(s), Some(e)) = (msg.find('"'), msg.rfind('"')) {
            if s < e { return msg[s+1..e].to_string(); }
        }
        "/".into()
    }
}

#[derive(Clone)]
struct FtpEntry { name: String, is_dir: bool, size: u64 }

fn parse_ls(line: &str) -> Option<FtpEntry> {
    let parts: Vec<&str> = line.split_ascii_whitespace().collect();
    if parts.len() < 9 { return None; }
    let name = parts[8..].join(" ");
    if name == "." || name == ".." { return None; }
    Some(FtpEntry { name, is_dir: line.starts_with('d'), size: parts[4].parse().unwrap_or(0) })
}

// ─────────────────────────────────────────────────────────────────────────────
// Nerd Font file icons
// ─────────────────────────────────────────────────────────────────────────────

fn file_icon(name: &str, is_dir: bool) -> char {
    if is_dir {
        return match name {
            ".git"            => '\u{e702}', //
            "node_modules"    => '\u{e5fa}', //
            _                 => '\u{f07b}', //
        };
    }
    // full-name matches first
    match name.to_lowercase().as_str() {
        "makefile" | "gnumakefile"          => return '\u{e779}', //
        "dockerfile"                        => return '\u{e7b0}', //
        "cargo.toml" | "cargo.lock"         => return '\u{e7a8}', //
        ".gitignore" | ".gitconfig"
        | ".gitmodules" | ".gitattributes"  => return '\u{e702}', //
        ".env" | ".envrc"                   => return '\u{f462}', //
        _ => {}
    }
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "rs"                                         => '\u{e7a8}', //
        "py" | "pyw" | "pyc" | "pyi"                => '\u{e73c}', //
        "js" | "mjs" | "cjs"                         => '\u{e74e}', //
        "ts" | "mts" | "cts"                         => '\u{e628}', //
        "jsx" | "tsx"                                => '\u{e7ba}', //
        "html" | "htm"                               => '\u{e736}', //
        "css" | "scss" | "sass" | "less"             => '\u{e749}', //
        "json" | "jsonc" | "json5"                   => '\u{e60b}', //
        "yaml" | "yml" | "toml"                      => '\u{e6d7}', //
        "md" | "markdown" | "mdx"                    => '\u{e73e}', //
        "pdf"                                        => '\u{f1c1}', //
        "png" | "jpg" | "jpeg" | "gif" | "svg"
        | "webp" | "bmp" | "ico" | "tiff" | "avif"  => '\u{f1c5}', //
        "mp3" | "flac" | "wav" | "ogg" | "m4a"
        | "aac" | "opus"                             => '\u{f001}', //
        "mp4" | "mkv" | "avi" | "mov" | "webm"
        | "flv" | "wmv"                              => '\u{f03d}', //
        "zip" | "tar" | "gz" | "bz2" | "xz"
        | "7z" | "rar" | "zst" | "lz4" | "lzma"     => '\u{f1c6}', //
        "sh" | "bash" | "zsh" | "fish" | "ksh"      => '\u{f489}', //
        "c" | "h"                                    => '\u{e61e}', //
        "cpp" | "cc" | "cxx" | "hpp" | "hxx"        => '\u{e61d}', //
        "go"                                         => '\u{e626}', //
        "rb" | "erb"                                 => '\u{e739}', //
        "java" | "class" | "jar"                     => '\u{e738}', //
        "kt" | "kts"                                 => '\u{e634}', //
        "hs" | "lhs"                                 => '\u{e777}', //
        "lua"                                        => '\u{e620}', //
        "vim"                                        => '\u{e62b}', //
        "conf" | "config" | "cfg" | "ini" | "nvim"  => '\u{e615}', //
        "lock"                                       => '\u{f023}', //
        "log"                                        => '\u{f18d}', //
        "txt"                                        => '\u{f15c}', //
        "xml" | "xsl" | "xsd"                        => '\u{e619}', //
        "sql"                                        => '\u{f1c0}', //
        "db" | "sqlite" | "sqlite3"                  => '\u{f1c0}', //
        "php"                                        => '\u{e73d}', //
        "cs"                                         => '\u{f031}', //
        "swift"                                      => '\u{e755}', //
        "dart"                                       => '\u{e798}', //
        "ex" | "exs"                                 => '\u{e62d}', //
        "erl" | "hrl"                                => '\u{e7b1}', //
        "zig"                                        => '\u{e6a9}', //
        "nix"                                        => '\u{f313}', //
        "tf" | "tfvars"                              => '\u{e69a}', //
        "vimrc"                                      => '\u{e62b}', //
        _                                            => '\u{f15b}', //
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Panes
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Default)]
enum SortBy { #[default] Name, Size, Ext }

#[derive(Clone)]
struct FileEntry { name: String, is_dir: bool, size: u64 }

impl FileEntry {
    fn display(&self, w: usize) -> String {
        let icon  = file_icon(&self.name, self.is_dir);
        let tag   = if self.is_dir { "/" } else { " " };
        let sz    = if self.is_dir { "     ".to_string() } else { human_size(self.size) };
        let avail = w.saturating_sub(9);  // icon(1) + sp(1) + name + sp(1) + size(5) + 1
        let label = format!("{}{}", self.name, tag);
        let label = if label.len() > avail {
            format!("…{}", &label[label.len().saturating_sub(avail.saturating_sub(1))..])
        } else { label };
        format!("{} {:<avail$} {:>5}", icon, label, sz, avail = avail)
    }
}

enum Pane {
    Local(LocalPane),
    Remote(RemotePane),
}

impl Pane {
    fn entries(&self)        -> &[FileEntry]  { match self { Pane::Local(p)  => &p.entries, Pane::Remote(p) => &p.entries } }
    fn cursor(&self)         -> usize         { match self { Pane::Local(p)  => p.cursor,   Pane::Remote(p) => p.cursor } }
    fn cursor_mut(&mut self) -> &mut usize    { match self { Pane::Local(p)  => &mut p.cursor, Pane::Remote(p) => &mut p.cursor } }
    fn offset(&self)         -> usize         { match self { Pane::Local(p)  => p.offset,   Pane::Remote(p) => p.offset } }
    fn offset_mut(&mut self) -> &mut usize    { match self { Pane::Local(p)  => &mut p.offset, Pane::Remote(p) => &mut p.offset } }
    fn title(&mut self)      -> String        { match self { Pane::Local(p)  => p.title(),  Pane::Remote(p) => p.title() } }
    fn refresh(&mut self)                     { match self { Pane::Local(p)  => p.refresh(), Pane::Remote(p) => p.refresh() } }
    fn enter(&mut self)                       { match self { Pane::Local(p)  => p.enter(),  Pane::Remote(p) => p.enter() } }
    fn back(&mut self)                        { match self { Pane::Local(p)  => p.back(),   Pane::Remote(p) => p.back() } }
    fn is_remote(&self) -> bool               { matches!(self, Pane::Remote(_)) }
    fn current(&self)    -> Option<&FileEntry> { self.entries().get(self.cursor()) }
    fn as_local(&self)   -> Option<&LocalPane> { if let Pane::Local(p)  = self { Some(p) } else { None } }
    fn as_local_mut(&mut self) -> Option<&mut LocalPane> { if let Pane::Local(p) = self { Some(p) } else { None } }

    fn is_selected(&self, idx: usize) -> bool {
        match self { Pane::Local(p) => p.selected.contains(&idx), Pane::Remote(p) => p.selected.contains(&idx) }
    }
    fn toggle_sel(&mut self, idx: usize) {
        let sel = match self { Pane::Local(p) => &mut p.selected, Pane::Remote(p) => &mut p.selected };
        if !sel.insert(idx) { sel.remove(&idx); }
    }
    fn select_all(&mut self) {
        let n = self.entries().len();
        let sel = match self { Pane::Local(p) => &mut p.selected, Pane::Remote(p) => &mut p.selected };
        *sel = (0..n).collect();
    }
    fn clear_sel(&mut self) {
        match self { Pane::Local(p) => p.selected.clear(), Pane::Remote(p) => p.selected.clear() }
    }
    fn has_selection(&self) -> bool {
        match self { Pane::Local(p) => !p.selected.is_empty(), Pane::Remote(p) => !p.selected.is_empty() }
    }
    fn selected_names(&self) -> Vec<String> {
        let sel = match self { Pane::Local(p) => &p.selected, Pane::Remote(p) => &p.selected };
        sel.iter()
            .filter_map(|&i| self.entries().get(i))
            .filter(|e| e.name != "..")
            .map(|e| e.name.clone())
            .collect()
    }
}

struct LocalPane {
    path:     PathBuf,
    entries:  Vec<FileEntry>,
    cursor:   usize,
    offset:   usize,
    sort:     SortBy,
    reverse:  bool,
    selected: std::collections::BTreeSet<usize>,
}

impl LocalPane {
    fn new(path: &str) -> Self {
        let mut p = LocalPane {
            path: PathBuf::from(path).canonicalize().unwrap_or_else(|_| PathBuf::from(".")),
            entries: vec![], cursor: 0, offset: 0, sort: SortBy::Name, reverse: false,
            selected: std::collections::BTreeSet::new(),
        };
        p.refresh(); p
    }

    fn refresh(&mut self) {
        let mut items = Vec::new();
        if let Ok(rd) = fs::read_dir(&self.path) {
            for e in rd.flatten() {
                let name   = e.file_name().to_string_lossy().into_owned();
                let is_dir = e.path().is_dir();
                let size   = e.metadata().map(|m| m.len()).unwrap_or(0);
                items.push(FileEntry { name, is_dir, size });
            }
        }
        let sort = self.sort;
        let reverse = self.reverse;
        items.sort_by(|a, b| {
            // dirs always before files regardless of reverse
            let dir_ord = (!a.is_dir).cmp(&(!b.is_dir));
            let file_ord = match sort {
                SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                SortBy::Size => a.size.cmp(&b.size),
                SortBy::Ext  => {
                    let ea = a.name.rsplit('.').next().unwrap_or("").to_lowercase();
                    let eb = b.name.rsplit('.').next().unwrap_or("").to_lowercase();
                    ea.cmp(&eb).then(a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                }
            };
            let file_ord = if reverse { file_ord.reverse() } else { file_ord };
            dir_ord.then(file_ord)
        });
        if self.path.parent().is_some() {
            items.insert(0, FileEntry { name: "..".to_string(), is_dir: true, size: 0 });
        }
        self.entries  = items;
        self.cursor   = self.cursor.min(self.entries.len().saturating_sub(1));
        self.selected.clear();
    }

    fn enter(&mut self) {
        if let Some(e) = self.entries.get(self.cursor).cloned() {
            if e.is_dir {
                if e.name == ".." {
                    self.back();
                } else {
                    self.path = self.path.join(&e.name);
                    self.cursor = 0; self.offset = 0;
                    self.refresh();
                }
            }
        }
    }

    fn back(&mut self) {
        let parent = match self.path.parent() { Some(p) => p.to_path_buf(), None => return };
        let old    = self.path.file_name().map(|n| n.to_string_lossy().into_owned());
        self.path  = parent;
        self.cursor = 0; self.offset = 0;
        self.refresh();
        if let Some(name) = old {
            if let Some(i) = self.entries.iter().position(|e| e.name == name) {
                self.cursor = i;
            }
        }
    }

    fn title(&self) -> String { self.path.display().to_string() }
}

struct RemotePane {
    ftp:      Ftp,
    entries:  Vec<FileEntry>,
    cursor:   usize,
    offset:   usize,
    pwd:      String,
    selected: std::collections::BTreeSet<usize>,
}

impl RemotePane {
    fn new(mut ftp: Ftp) -> Self {
        let pwd = ftp.pwd();
        let entries = ftp.list().into_iter().map(|e| FileEntry { name: e.name, is_dir: e.is_dir, size: e.size }).collect();
        RemotePane { ftp, entries, cursor: 0, offset: 0, pwd, selected: std::collections::BTreeSet::new() }
    }

    fn refresh(&mut self) {
        self.pwd      = self.ftp.pwd();
        self.entries  = self.ftp.list().into_iter()
            .map(|e| FileEntry { name: e.name, is_dir: e.is_dir, size: e.size }).collect();
        self.cursor   = self.cursor.min(self.entries.len().saturating_sub(1));
        self.selected.clear();
    }

    fn enter(&mut self) {
        if let Some(e) = self.entries.get(self.cursor).cloned() {
            if e.is_dir && self.ftp.cwd(&e.name) {
                self.cursor = 0; self.offset = 0;
                self.refresh();
            }
        }
    }

    fn back(&mut self) {
        let old = self.pwd.trim_end_matches('/').split('/').last().map(str::to_string);
        if self.ftp.cdup() {
            self.cursor = 0; self.offset = 0;
            self.refresh();
            if let Some(name) = old {
                if let Some(i) = self.entries.iter().position(|e| e.name == name) { self.cursor = i; }
            }
        }
    }

    fn title(&self) -> String { format!("ftp://{}{}", self.ftp.host, self.pwd) }
}

// ─────────────────────────────────────────────────────────────────────────────
// Drawing
// ─────────────────────────────────────────────────────────────────────────────

// Truncate `s` to at most `max_cols` terminal columns.
// Nerd Font PUA codepoints (U+E000–U+F8FF) count as 2 columns each.
fn trunc_cols(s: &str, max_cols: usize) -> &str {
    let mut used = 0usize;
    for (i, c) in s.char_indices() {
        let w = if (c as u32) >= 0xE000 { 2 } else { 1 };
        if used + w > max_cols { return &s[..i]; }
        used += w;
    }
    s
}

// How many terminal columns does `s` occupy?
fn str_cols(s: &str) -> usize {
    s.chars().map(|c| if (c as u32) >= 0xE000 { 2 } else { 1 }).sum()
}

// Write `width` chars of ── with the title centred, active/inactive colour.
fn write_border_title(o: &mut Out, title: &str, width: usize, active: bool) {
    // truncate title from the left if it is too long
    let max_t = width.saturating_sub(4); // minimum 2 dashes + 2 spaces
    let t = if title.len() > max_t { &title[title.len() - max_t..] } else { title };
    let inner = format!(" {} ", t);
    let ilen  = inner.chars().count();
    let left  = if width > ilen { (width - ilen) / 2 } else { 0 };
    let right = width.saturating_sub(ilen + left);
    c_norm(o);
    for _ in 0..left  { write!(o, "═").unwrap(); }
    if active { c_hdr_act(o); } else { c_hdr(o); }
    write!(o, "{}", inner).unwrap();
    c_norm(o);
    for _ in 0..right { write!(o, "═").unwrap(); }
}

fn draw_top_border(o: &mut Out, cols: usize, half: usize,
                   left_title: &str, right_title: &str, active: usize) {
    goto(o, 1, 1);
    c_norm(o);
    write!(o, "╔").unwrap();
    write_border_title(o, left_title,  half - 1,       active == 0);
    c_norm(o); write!(o, "╦").unwrap();
    write_border_title(o, right_title, cols - half - 2, active == 1);
    c_norm(o); write!(o, "╗").unwrap();
    c_reset(o);
}

fn draw_bottom_border(o: &mut Out, row: u16, cols: usize, half: usize) {
    goto(o, row, 1);
    c_norm(o);
    write!(o, "╚").unwrap();
    for _ in 0..half - 1       { write!(o, "═").unwrap(); }
    write!(o, "╩").unwrap();
    for _ in 0..cols - half - 2 { write!(o, "═").unwrap(); }
    write!(o, "╝").unwrap();
    c_reset(o);
}

// Draw only the content cells of one pane (no title, no borders).
// col0 / inner_w are the inner dimensions (already excluding the │ chars).
fn draw_pane_content(o: &mut Out, pane: &mut Pane, active: bool,
                     row0: u16, n_rows: usize, col0: u16, inner_w: usize) {
    let cursor = pane.cursor();
    let offset = {
        let off = pane.offset();
        if cursor < off { cursor }
        else if cursor >= off + n_rows { cursor.saturating_sub(n_rows.saturating_sub(1)) }
        else { off }
    };
    *pane.offset_mut() = offset;

    let entries = pane.entries();
    for i in 0..n_rows {
        goto(o, row0 + i as u16, col0);
        let idx = offset + i;
        if idx >= entries.len() {
            c_norm(o);
            write!(o, "{:<w$}", "", w = inner_w).unwrap();
            c_reset(o);
            continue;
        }
        let e        = &entries[idx];
        let selected = pane.is_selected(idx);
        if idx == cursor  { if active { c_sel(o); } else { c_sel_inactive(o); } }
        else if selected  { c_sel_file(o); }
        else if e.is_dir  { c_dir(o); }
        else              { c_norm(o); }
        let line    = e.display(inner_w);
        let trimmed = trunc_cols(&line, inner_w);
        let padding = inner_w.saturating_sub(str_cols(trimmed));
        write!(o, "{}{:pad$}", trimmed, "", pad = padding).unwrap();
        c_reset(o);
    }
}

// (num_str, label_with_vim_hint)
const FKEYS: &[(&str, &str)] = &[
    ("1",  "Help(?)"),
    ("2",  "Rename(r)"),
    ("3",  "View"),
    ("4",  "Edit(e)"),
    ("5",  "Copy"),
    ("6",  "Move"),
    ("7",  "Mkdir"),
    ("8",  "Delete(dd)"),
    ("9",  "Menu"),
    ("10", "Quit"),
];

fn draw_fkey_bar(o: &mut Out, rows: u16, cols: usize) {
    goto(o, rows, 1);
    let mut used = 0usize;
    for &(num, label) in FKEYS {
        let entry_len = num.len() + label.len() + 1; // +1 trailing space
        if used + entry_len > cols { break; }
        // number: bright cyan on black  (exact classic look)
        tc!(o, fg 0x55,0xFF,0xFF); tc!(o, bg 0x00,0x00,0x00);
        write!(o, "{}", num).unwrap();
        // label: black on dark cyan
        tc!(o, fg 0x00,0x00,0x00); tc!(o, bg 0x00,0xAA,0xAA);
        write!(o, "{} ", label).unwrap();
        used += entry_len;
    }
    // fill remaining width with black
    tc!(o, fg 0x55,0xFF,0xFF); tc!(o, bg 0x00,0x00,0x00);
    if used < cols { write!(o, "{:width$}", "", width = cols - used).unwrap(); }
    c_reset(o);
}

fn flash(o: &mut Out, msg: &str) {
    let (rows, cols) = term_size();
    goto(o, rows, 1);
    c_status(o);
    let text = &msg[..msg.len().min(cols as usize)];
    write!(o, "{:<width$}", text, width = cols as usize).unwrap();
    c_reset(o);
    o.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500));
}

fn draw_info_row(o: &mut Out, row: u16, cols: usize, _pane: &Pane) {
    goto(o, row, 1);
    tc!(o, bg 0x00,0x00,0x00);
    write!(o, "{:<width$}", "", width = cols).unwrap();
    c_reset(o);
}

fn render(o: &mut Out, panes: &mut [Pane; 2], active: usize) {
    tc!(o, bg 0x00,0x00,0xAA);
    write!(o, "\x1b[2J").unwrap();
    let (rows, cols) = term_size();
    let cols = cols as usize;
    let half = cols / 2;
    // Layout: row 1 = top border, rows 2..rows-3 = content,
    //         row rows-2 = bottom border, row rows-1 = black line, row rows = fkeys
    let n_content = (rows as usize).saturating_sub(4);
    let inner_left  = half.saturating_sub(1);          // cols between ┌ and ┬
    let inner_right = cols.saturating_sub(half + 2);   // cols between ┬ and ┐

    // get titles before splitting the mutable borrow
    let lt = panes[0].title();
    let rt = panes[1].title();

    draw_top_border(o, cols, half, &lt, &rt, active);

    // pane content (inner columns, no │ chars)
    draw_pane_content(o, &mut panes[0], active == 0, 2, n_content, 2,                 inner_left);
    draw_pane_content(o, &mut panes[1], active == 1, 2, n_content, half as u16 + 2,   inner_right);

    // draw ║ side/middle borders over content rows (after content so colour is correct)
    c_norm(o);
    for r in 0..n_content as u16 {
        goto(o, 2 + r, 1);                write!(o, "║").unwrap();
        goto(o, 2 + r, half as u16 + 1); write!(o, "║").unwrap();
        goto(o, 2 + r, cols as u16);     write!(o, "║").unwrap();
    }
    c_reset(o);

    draw_bottom_border(o, rows - 2, cols, half);
    draw_info_row(o, rows - 1, cols, &panes[active]);
    draw_fkey_bar(o, rows, cols);
    o.flush().unwrap();
}

// ─────────────────────────────────────────────────────────────────────────────
// Input prompt — raw mode, ESC aborts (None), Enter confirms (Some)
// ─────────────────────────────────────────────────────────────────────────────

fn prompt(o: &mut Out, label: &str, default: &str) -> Option<String> {
    let mut buf: Vec<char> = default.chars().collect();
    show_cur(o);
    loop {
        let (rows, cols) = term_size();
        let header = format!(" {}: ", label);
        let avail  = (cols as usize).saturating_sub(header.len() + 1);
        goto(o, rows, 1);
        c_status(o);
        let display: String = buf.iter().collect();
        let disp = if display.len() > avail { &display[display.len() - avail..] } else { &display };
        write!(o, "{}{:<width$}", header, disp, width = avail).unwrap();
        goto(o, rows, (header.len() + disp.len() + 1).min(cols as usize) as u16);
        o.flush().unwrap();

        match read_key() {
            Key::Esc                              => { hide_cur(o); return None; }
            Key::Enter                            => { hide_cur(o); return Some(buf.iter().collect()); }
            Key::Char('\x7f') | Key::Char('\x08') => { buf.pop(); }
            Key::Char(c) if !c.is_control()      => buf.push(c),
            _ => {}
        }
    }
}


// ─────────────────────────────────────────────────────────────────────────────
// File operations (local only; remote panes show a notice)
// ─────────────────────────────────────────────────────────────────────────────

fn copy_rec(src: &std::path::Path, dst: &std::path::Path) -> io::Result<()> {
    if src.is_dir() {
        fs::create_dir_all(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_rec(&e.path(), &dst.join(e.file_name()))?;
        }
    } else {
        if let Some(p) = dst.parent() { fs::create_dir_all(p)?; }
        fs::copy(src, dst)?;
    }
    Ok(())
}

fn op_rename(o: &mut Out, p: &mut Pane) {
    if p.is_remote() { flash(o, " Remote rename not supported"); return; }
    let lp = p.as_local_mut().unwrap();
    let Some(e) = lp.entries.get(lp.cursor).cloned() else { return };
    if e.name == ".." { return; }
    let Some(new) = prompt(o, "Rename to", &e.name) else { return };
    if new.is_empty() || new == e.name { return; }
    if let Err(e) = fs::rename(lp.path.join(&e.name), lp.path.join(&new)) {
        flash(o, &format!(" Error: {}", e));
    }
    lp.refresh();
}

fn op_copy(o: &mut Out, src: &mut Pane, dst: &mut Pane) {
    if src.is_remote() || dst.is_remote() { flash(o, " Remote copy not supported"); return; }
    let names: Vec<String> = if src.has_selection() { src.selected_names() }
        else { src.current().filter(|e| e.name != "..").map(|e| vec![e.name.clone()]).unwrap_or_default() };
    if names.is_empty() { return; }
    let spath = src.as_local().unwrap().path.clone();
    let dpath = dst.as_local().unwrap().path.clone();
    let default = if names.len() == 1 { dpath.join(&names[0]).display().to_string() }
                  else { dpath.display().to_string() };
    let Some(target) = prompt(o, "Copy to", &default) else { return };
    if target.is_empty() { return; }
    let dst_root = std::path::PathBuf::from(&target);
    for name in &names {
        let dst_p = if names.len() == 1 { dst_root.clone() } else { dst_root.join(name) };
        if let Err(e) = copy_rec(&spath.join(name), &dst_p) { flash(o, &format!(" Error: {}", e)); }
    }
    src.refresh(); dst.refresh();
}

fn op_move(o: &mut Out, src: &mut Pane, dst: &mut Pane) {
    if src.is_remote() || dst.is_remote() { flash(o, " Remote move not supported"); return; }
    let names: Vec<String> = if src.has_selection() { src.selected_names() }
        else { src.current().filter(|e| e.name != "..").map(|e| vec![e.name.clone()]).unwrap_or_default() };
    if names.is_empty() { return; }
    let spath = src.as_local().unwrap().path.clone();
    let dpath = dst.as_local().unwrap().path.clone();
    let default = if names.len() == 1 { dpath.join(&names[0]).display().to_string() }
                  else { dpath.display().to_string() };
    let Some(target) = prompt(o, "Move to", &default) else { return };
    if target.is_empty() { return; }
    let dst_root = std::path::PathBuf::from(&target);
    for name in &names {
        let src_p = spath.join(name);
        let dst_p = if names.len() == 1 { dst_root.clone() } else { dst_root.join(name) };
        let result = fs::rename(&src_p, &dst_p).or_else(|_| {
            copy_rec(&src_p, &dst_p)?;
            if src_p.is_dir() { fs::remove_dir_all(&src_p) } else { fs::remove_file(&src_p) }
        });
        if let Err(e) = result { flash(o, &format!(" Error: {}", e)); }
    }
    src.refresh(); dst.refresh();
}

fn op_delete(o: &mut Out, p: &mut Pane) {
    if p.is_remote() { flash(o, " Remote delete not supported"); return; }
    let names: Vec<String> = if p.has_selection() {
        p.selected_names()
    } else {
        p.current().filter(|e| e.name != "..").map(|e| vec![e.name.clone()]).unwrap_or_default()
    };
    if names.is_empty() { return; }
    let label = if names.len() == 1 { format!("'{}'", names[0]) }
                else { format!("{} items", names.len()) };
    let Some(ans) = prompt(o, &format!("Delete {}? [y/N]", label), "") else { return };
    if ans.to_lowercase() != "y" { return; }
    let lp = p.as_local_mut().unwrap();
    for name in &names {
        let path = lp.path.join(name);
        if let Err(e) = if path.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) } {
            flash(o, &format!(" Error: {}", e));
        }
    }
    lp.refresh();
}

fn op_mkdir(o: &mut Out, p: &mut Pane) {
    if p.is_remote() { flash(o, " Remote mkdir not supported"); return; }
    let lp  = p.as_local_mut().unwrap();
    let Some(name) = prompt(o, "New directory", "") else { return };
    if name.is_empty() { return; }
    if let Err(e) = fs::create_dir_all(lp.path.join(&name)) {
        flash(o, &format!(" Error: {}", e));
    }
    lp.refresh();
}

fn op_goto(o: &mut Out, p: &mut Pane) {
    if p.is_remote() { flash(o, " Use ← → to navigate remote pane"); return; }
    let lp   = p.as_local_mut().unwrap();
    let curr = lp.path.display().to_string();
    let Some(tgt) = prompt(o, "Go to", &curr) else { return };
    if tgt.is_empty() { return; }
    let path = std::path::PathBuf::from(&tgt);
    if path.is_dir() {
        lp.path   = path.canonicalize().unwrap_or(path);
        lp.cursor = 0; lp.offset = 0;
        lp.refresh();
    } else {
        flash(o, &format!(" Not a directory: {}", tgt));
    }
}

fn op_sort(o: &mut Out, p: &mut Pane) {
    let lp = match p { Pane::Local(lp) => lp, _ => return };
    let current = match lp.sort { SortBy::Name => 0, SortBy::Size => 1, SortBy::Ext => 2 };
    let rev0    = lp.reverse;
    let Some(idx) = ({
        let mut cur = current;
        let mut rev = rev0;
        let n = 3usize;
        loop {
            let (rows, cols) = term_size();
            let (rows, cols) = (rows as usize, cols as usize);
            let box_w   = 28usize.min(cols.saturating_sub(4));
            let total_h = n + 2;
            let row0    = ((rows.saturating_sub(total_h)) / 2 + 1) as u16;
            let col0    = ((cols.saturating_sub(box_w))   / 2 + 1) as u16;
            let title   = if rev { "Sort by  ↓ desc" } else { "Sort by  ↑ asc" };
            let inner_w = draw_nc_box(o, row0, col0, box_w, n, title);
            let labels  = ["Name", "Size", "Extension"];
            for i in 0..n {
                goto(o, row0 + 1 + i as u16, col0 + 1);
                let marker = if i == current { if rev { "◆↓ " } else { "◆↑ " } } else { "   " };
                if i == cur { c_sel(o); } else { c_norm(o); }
                let lbl = &labels[i][..labels[i].len().min(inner_w.saturating_sub(3))];
                write!(o, "{}{:<w$}", marker, lbl, w = inner_w.saturating_sub(3)).unwrap();
                c_reset(o);
            }
            o.flush().unwrap();
            match read_key() {
                Key::Esc              => break None,
                Key::Enter            => break Some((cur, rev)),
                Key::Char('r') | Key::Char('R') => rev = !rev,
                Key::Up   if cur > 0     => cur -= 1,
                Key::Down if cur + 1 < n => cur += 1,
                _ => {}
            }
        }
    }) else { return };
    let (idx, rev) = idx;
    let new_sort = match idx { 1 => SortBy::Size, 2 => SortBy::Ext, _ => SortBy::Name };
    // selecting the already-active criterion also toggles direction
    lp.reverse = if new_sort == lp.sort && rev == rev0 { !rev0 } else { rev };
    lp.sort    = new_sort;
    lp.refresh();
}

// ─────────────────────────────────────────────────────────────────────────────
// Double-line box helper
// ─────────────────────────────────────────────────────────────────────────────

// Draw a ╔══╡ Title ╞══╗ … ╚══════╝ frame.
// box_h = number of content rows (not counting top/bottom border lines).
// Returns inner_w (usable columns inside the box).
fn draw_nc_box(o: &mut Out, row0: u16, col0: u16, box_w: usize, box_h: usize, title: &str) -> usize {
    let inner_w = box_w.saturating_sub(2);
    let title_seg = format!("╡ {} ╞", title);
    let tlen = title_seg.chars().count();
    let left  = if inner_w > tlen { (inner_w - tlen) / 2 } else { 0 };
    let right = inner_w.saturating_sub(tlen + left);

    // top border
    goto(o, row0, col0);
    c_hdr_act(o);
    write!(o, "╔").unwrap();
    for _ in 0..left  { write!(o, "═").unwrap(); }
    write!(o, "{}", title_seg).unwrap();
    for _ in 0..right { write!(o, "═").unwrap(); }
    write!(o, "╗").unwrap();

    // side borders (clear interior)
    for r in 0..box_h {
        goto(o, row0 + 1 + r as u16, col0);
        c_hdr_act(o); write!(o, "║").unwrap();
        c_norm(o);    write!(o, "{:<w$}", "", w = inner_w).unwrap();
        c_hdr_act(o); write!(o, "║").unwrap();
    }

    // bottom border
    goto(o, row0 + 1 + box_h as u16, col0);
    write!(o, "╚").unwrap();
    for _ in 0..inner_w { write!(o, "═").unwrap(); }
    write!(o, "╝").unwrap();
    c_reset(o);
    inner_w
}

// ─────────────────────────────────────────────────────────────────────────────
// Generic arrow-key picker overlay
// ─────────────────────────────────────────────────────────────────────────────

fn picker(o: &mut Out, title: &str, items: &[String]) -> Option<usize> {
    if items.is_empty() { return None; }
    let mut cursor = 0usize;
    let n = items.len();
    loop {
        let (rows, cols) = term_size();
        let (rows, cols) = (rows as usize, cols as usize);
        let visible = n.min(rows.saturating_sub(6));
        let box_w   = items.iter().map(|s| s.len()).max().unwrap_or(10)
                          .max(title.len() + 4) + 4;
        let box_w   = box_w.clamp(24, cols.saturating_sub(4));
        // +2: top border + bottom border
        let total_h = visible + 2;
        let row0    = ((rows.saturating_sub(total_h)) / 2 + 1) as u16;
        let col0    = ((cols.saturating_sub(box_w))   / 2 + 1) as u16;
        let inner_w = draw_nc_box(o, row0, col0, box_w, visible, title);

        let offset = if cursor >= visible { cursor - visible + 1 } else { 0 };
        for i in 0..visible {
            goto(o, row0 + 1 + i as u16, col0 + 1);
            let idx = offset + i;
            if idx >= n {
                write!(o, "{:<w$}", "", w = inner_w).unwrap();
                continue;
            }
            if idx == cursor { c_sel(o); } else { c_norm(o); }
            let label = &items[idx];
            let label = &label[..label.len().min(inner_w.saturating_sub(2))];
            write!(o, " {:<w$} ", label, w = inner_w.saturating_sub(2)).unwrap();
            c_reset(o);
        }
        o.flush().unwrap();

        match read_key() {
            Key::Esc                    => return None,
            Key::Enter                  => return Some(cursor),
            Key::Up   if cursor > 0      => cursor -= 1,
            Key::Down if cursor + 1 < n  => cursor += 1,
            _ => {}
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// rclone remote mounting
// ─────────────────────────────────────────────────────────────────────────────

fn rclone_config_flag() -> Option<String> {
    if let Ok(p) = std::env::var("RCLONE_CONFIG") {
        if std::path::Path::new(&p).exists() { return Some(p); }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    for candidate in [
        format!("{}/git/conf/rclone.conf", home),
        format!("{}/.config/rclone/rclone.conf", home),
        format!("{}/.rclone.conf", home),
    ] {
        if std::path::Path::new(&candidate).exists() { return Some(candidate); }
    }
    None
}

fn list_rclone_remotes() -> Vec<String> {
    let mut cmd = std::process::Command::new("rclone");
    cmd.arg("listremotes");
    if let Some(cfg) = rclone_config_flag() { cmd.args(["--config", &cfg]); }
    match cmd.output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim_end_matches(':').to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        Err(_) => vec![],
    }
}

fn is_mounted(mp: &str) -> bool {
    fs::read_to_string("/proc/mounts")
        .map(|s| s.lines().any(|l| l.split_whitespace().nth(1) == Some(mp)))
        .unwrap_or(false)
}

fn op_rclone_mount(o: &mut Out, panes: &mut [Pane; 2], mounts: &mut Vec<String>) {
    let remotes = list_rclone_remotes();
    if remotes.is_empty() {
        flash(o, " No rclone remotes found (is rclone installed and configured?)");
        return;
    }
    let Some(idx) = picker(o, "rclone Remotes", &remotes) else { return };
    let remote = &remotes[idx];
    let mp = format!("/tmp/oc-{}", remote);

    // already mounted — just navigate
    if is_mounted(&mp) {
        navigate_right_to(panes, &mp);
        return;
    }

    if let Err(e) = fs::create_dir_all(&mp) {
        flash(o, &format!(" Cannot create mountpoint: {}", e)); return;
    }

    flash(o, &format!(" Mounting {}...", remote));

    let mut cmd = std::process::Command::new("rclone");
    cmd.args(["mount", &format!("{}:", remote), &mp, "--vfs-cache-mode", "writes"]);
    if let Some(cfg) = rclone_config_flag() { cmd.args(["--config", &cfg]); }
    if let Err(e) = cmd.spawn() {
        flash(o, &format!(" rclone spawn failed: {}", e)); return;
    }

    // poll /proc/mounts until FUSE is ready (up to 10 s)
    let mut ready = false;
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if is_mounted(&mp) { ready = true; break; }
    }
    if !ready {
        flash(o, &format!(" Timeout waiting for {} to mount", remote));
        return;
    }

    mounts.push(mp.clone());
    navigate_right_to(panes, &mp);
}

fn navigate_right_to(panes: &mut [Pane; 2], path: &str) {
    if let Pane::Local(lp) = &mut panes[1] {
        lp.path = std::path::PathBuf::from(path);
        lp.cursor = 0; lp.offset = 0;
        lp.refresh();
    } else {
        panes[1] = Pane::Local(LocalPane::new(path));
    }
}

fn unmount_all(mounts: &[String]) {
    for mp in mounts {
        for prog in &["fusermount3 -u", "fusermount -u", "umount"] {
            let mut parts = prog.split_whitespace();
            let exe  = parts.next().unwrap();
            let args: Vec<&str> = parts.collect();
            let ok = std::process::Command::new(exe)
                .args(&args).arg(mp)
                .status().map(|s| s.success()).unwrap_or(false);
            if ok { break; }
        }
    }
}

fn op_connect_right(o: &mut Out, panes: &mut [Pane; 2]) {
    let hosts: Vec<String> = load_netrc().into_iter().map(|e| e.machine).collect();
    if hosts.is_empty() { flash(o, " No ~/.netrc entries found"); return; }
    let Some(idx) = picker(o, "FTP (netrc)", &hosts) else { return };
    let host = &hosts[idx];
    flash(o, &format!(" Connecting to {}...", host));
    let (user, pass) = netrc_creds(host).unwrap_or_default();
    match Ftp::connect(host, &user, &pass) {
        Ok(ftp) => panes[1] = Pane::Remote(RemotePane::new(ftp)),
        Err(e)  => flash(o, &format!(" Connection failed: {}", e)),
    }
}

fn spawn_external(o: &mut Out, prog: &str, arg: &str) {
    show_cur(o);
    clr(o);
    o.flush().unwrap();
    raw_off();
    let _ = std::process::Command::new(prog).arg(arg).status();
    raw_on();
    hide_cur(o);
    clr(o);
    o.flush().unwrap();
}

fn op_view(o: &mut Out, p: &Pane) {
    let Some(e) = p.current() else { return };
    if e.is_dir || p.is_remote() { return; }
    let path  = p.as_local().unwrap().path.join(&e.name).display().to_string();
    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".into());
    spawn_external(o, &pager, &path);
}

fn op_edit(o: &mut Out, p: &Pane) {
    let Some(e) = p.current() else { return };
    if e.is_dir || p.is_remote() { return; }
    let path   = p.as_local().unwrap().path.join(&e.name).display().to_string();
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    spawn_external(o, &editor, &path);
}

const HELP_LINES: &[&str] = &[
    "  Navigation                              ",
    "  Tab           switch active pane        ",
    "  ↑/↓           move cursor               ",
    "  PgUp/PgDn     scroll page               ",
    "  Home/End       first/last entry         ",
    "  gg             go to top (vim)           ",
    "  G              go to bottom (vim)        ",
    "  /              incremental search        ",
    "  Enter/→        enter dir or open file   ",
    "  ←/Backspace    go up (parent dir)       ",
    "                                          ",
    "  Selection                               ",
    "  Insert         toggle + move down       ",
    "  Space          toggle selection         ",
    "  Ctrl+A         select all               ",
    "  Esc            deselect all             ",
    "                                          ",
    "  File operations                         ",
    "  F1 / ?         this help                ",
    "  F2 / r         rename                   ",
    "  F3             view file (pager)        ",
    "  F4 / e         edit file                ",
    "  F5             copy to other pane       ",
    "  F6             move to other pane       ",
    "  F7             create directory         ",
    "  F8 / dd / Del  delete                   ",
    "  F9             context menu             ",
    "  F10 / q        quit                     ",
    "                                          ",
    "  Misc                                    ",
    "  S              sync other pane here     ",
    "  '              goto path                 ",
    "  s              sort by …               ",
    "  R              refresh                  ",
    "  C              rclone remote mount      ",
    "  f              FTP (netrc) connect      ",
    "                                          ",
    "  Mouse                                   ",
    "  click          switch pane / cursor     ",
    "  dbl-click      enter dir or open file   ",
    "  scroll         scroll 3 rows            ",
];

fn op_help(o: &mut Out) {
    let (rows, cols) = term_size();
    let n     = HELP_LINES.len();
    let box_w = (HELP_LINES[0].len() + 2).min(cols as usize);
    let total_h = n + 2;
    let row0  = ((rows as usize).saturating_sub(total_h) / 2 + 1) as u16;
    let col0  = ((cols as usize).saturating_sub(box_w)   / 2 + 1) as u16;
    let inner_w = draw_nc_box(o, row0, col0, box_w, n, "Keybindings  (any key to close)");
    for (i, line) in HELP_LINES.iter().enumerate() {
        goto(o, row0 + 1 + i as u16, col0 + 1);
        c_norm(o);
        write!(o, "{:<w$}", line, w = inner_w).unwrap();
    }
    o.flush().unwrap();
    read_key();
}

fn op_search(o: &mut Out, panes: &mut [Pane; 2], active: usize) {
    let original = panes[active].cursor();
    let mut query = String::new();
    show_cur(o);
    loop {
        render(o, panes, active);
        let (rows, cols) = term_size();
        // overwrite info row with the search prompt
        goto(o, rows - 1, 1);
        c_status(o);
        let prompt = format!("/{}", query);
        write!(o, "{:<width$}", prompt, width = cols as usize).unwrap();
        c_reset(o);
        goto(o, rows - 1, prompt.chars().count() as u16 + 1);
        o.flush().unwrap();

        match read_key() {
            Key::Esc => {
                *panes[active].cursor_mut() = original;
                hide_cur(o);
                return;
            }
            Key::Enter => { hide_cur(o); return; }
            Key::Backspace => { query.pop(); }
            Key::Char(c) if !c.is_control() => query.push(c),
            _ => {}
        }

        if !query.is_empty() {
            let q = query.to_lowercase();
            if let Some(idx) = panes[active].entries().iter()
                .position(|e| e.name.to_lowercase().contains(&q))
            {
                *panes[active].cursor_mut() = idx;
            }
        }
    }
}

fn op_context_menu(o: &mut Out, panes: &mut [Pane; 2], active: usize, mounts: &mut Vec<String>) {
    let items: Vec<String> = vec![
        "View (F3)".into(), "Edit (F4/e)".into(), "Copy (F5)".into(),
        "Move (F6)".into(), "Rename (F2/r)".into(), "Mkdir (F7)".into(),
        "Delete (F8/dd)".into(), "Select All (Ctrl+A)".into(),
        "Deselect All (Esc)".into(), "Sort…(s)".into(),
        "Go to…(g)".into(), "Sync panes (S)".into(),
        "rclone mount (C)".into(), "FTP connect (f)".into(),
        "Refresh (R)".into(),
    ];
    let Some(idx) = picker(o, "Menu", &items) else { return };
    match idx {
        0  => { let (a, _) = split(panes, active); op_view(o, a); }
        1  => { let (a, _) = split(panes, active); op_edit(o, a); }
        2  => { let (a, b) = split(panes, active); op_copy(o, a, b); }
        3  => { let (a, b) = split(panes, active); op_move(o, a, b); }
        4  => { let (a, _) = split(panes, active); op_rename(o, a); }
        5  => { let (a, _) = split(panes, active); op_mkdir(o, a); }
        6  => { let (a, _) = split(panes, active); op_delete(o, a); }
        7  => panes[active].select_all(),
        8  => panes[active].clear_sel(),
        9  => { let (a, _) = split(panes, active); op_sort(o, a); }
        10 => { let (a, _) = split(panes, active); op_goto(o, a); }
        11 => op_sync_panes(panes, active),
        12 => op_rclone_mount(o, panes, mounts),
        13 => op_connect_right(o, panes),
        14 => panes[active].refresh(),
        _  => {}
    }
}

fn op_sync_panes(panes: &mut [Pane; 2], active: usize) {
    let src_path = match &panes[active] {
        Pane::Local(p) => p.path.clone(),
        Pane::Remote(_) => return,
    };
    let other = 1 - active;
    if let Pane::Local(op) = &mut panes[other] {
        op.path = src_path;
        op.cursor = 0; op.offset = 0;
        op.refresh();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

fn make_pane(arg: &str) -> Pane {
    if arg.starts_with("ftp://") {
        let host = &arg[6..].split('/').next().unwrap_or(arg);
        let (u, p) = netrc_creds(host).unwrap_or_default();
        if let Ok(ftp) = Ftp::connect(host, &u, &p) { return Pane::Remote(RemotePane::new(ftp)); }
    }
    if !std::path::Path::new(arg).exists() {
        let (u, p) = netrc_creds(arg).unwrap_or_default();
        if !u.is_empty() {
            if let Ok(ftp) = Ftp::connect(arg, &u, &p) { return Pane::Remote(RemotePane::new(ftp)); }
        }
    }
    Pane::Local(LocalPane::new(arg))
}

// Borrow active and inactive panes as (&mut active, &mut other)
fn split(panes: &mut [Pane; 2], active: usize) -> (&mut Pane, &mut Pane) {
    let [a, b] = panes;
    if active == 0 { (a, b) } else { (b, a) }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let left  = args.get(0).map(String::as_str).unwrap_or(".");
    let right = args.get(1).map(String::as_str).unwrap_or(".");

    raw_on();
    let mut o      = BufWriter::new(io::stdout());
    let mut panes  = [make_pane(left), make_pane(right)];
    let mut active = 0usize;
    let mut mounts: Vec<String> = Vec::new();

    hide_cur(&mut o);
    mouse_on(&mut o);
    clr(&mut o);
    o.flush().unwrap();

    let mut last_char = '\0';

    loop {
        render(&mut o, &mut panes, active);
        let (rows, cols) = term_size();
        let visible = rows as usize - 4; // top border + content + bottom border + black + fkeys
        let half    = (cols / 2) as u16;

        let key = read_key();
        let key_char = if let Key::Char(c) = key { c } else { '\0' };
        match key {
            Key::Char('\t') => active ^= 1,

            // ── Navigation ──────────────────────────────────────────────────
            Key::Up       => { let c = panes[active].cursor_mut(); *c = c.saturating_sub(1); }
            Key::Down     => {
                let len = panes[active].entries().len();
                let c   = panes[active].cursor_mut();
                if *c + 1 < len { *c += 1; }
            }
            Key::PageUp   => { let c = panes[active].cursor_mut(); *c = c.saturating_sub(visible); }
            Key::PageDown => {
                let len = panes[active].entries().len();
                let c   = panes[active].cursor_mut();
                *c = (*c + visible).min(len.saturating_sub(1));
            }
            Key::Home => { *panes[active].cursor_mut() = 0; }
            Key::End  => {
                let last = panes[active].entries().len().saturating_sub(1);
                *panes[active].cursor_mut() = last;
            }
            Key::Enter | Key::Right => panes[active].enter(),
            Key::Left | Key::Backspace => panes[active].back(),

            // ── Selection ───────────────────────────────────────────────────
            Key::Insert => {
                let idx = panes[active].cursor();
                panes[active].toggle_sel(idx);
                let len = panes[active].entries().len();
                let c   = panes[active].cursor_mut();
                if *c + 1 < len { *c += 1; }
            }
            Key::Char(' ') => {
                let idx = panes[active].cursor();
                panes[active].toggle_sel(idx);
            }
            Key::Char('\x01') => panes[active].select_all(),    // Ctrl+A
            Key::Esc          => panes[active].clear_sel(),
            Key::Delete       => { let (a, _) = split(&mut panes, active); op_delete(&mut o, a); }

            // ── F-keys ──────────────────────────────────────────
            Key::F(1)  => op_help(&mut o),
            Key::F(2)  => { let (a, _) = split(&mut panes, active); op_rename(&mut o, a); }
            Key::F(3)  => { let (a, _) = split(&mut panes, active); op_view(&mut o, a); }
            Key::F(4)  => { let (a, _) = split(&mut panes, active); op_edit(&mut o, a); }
            Key::F(5)  => { let (a, b) = split(&mut panes, active); op_copy(&mut o, a, b); }
            Key::F(6)  => { let (a, b) = split(&mut panes, active); op_move(&mut o, a, b); }
            Key::F(7)  => { let (a, _) = split(&mut panes, active); op_mkdir(&mut o, a); }
            Key::F(8)  => { let (a, _) = split(&mut panes, active); op_delete(&mut o, a); }
            Key::F(9)  => op_context_menu(&mut o, &mut panes, active, &mut mounts),
            Key::F(10) => break,

            // ── Vim-style letter bindings ───────────────────────────────────
            Key::Char('j') => {
                let len = panes[active].entries().len();
                let c   = panes[active].cursor_mut();
                if *c + 1 < len { *c += 1; }
            }
            Key::Char('k') => { let c = panes[active].cursor_mut(); *c = c.saturating_sub(1); }
            Key::Char('/') => op_search(&mut o, &mut panes, active),
            Key::Char('e') => { let (a, _) = split(&mut panes, active); op_edit(&mut o, a); }
            Key::Char('d') if last_char == 'd' => { let (a, _) = split(&mut panes, active); op_delete(&mut o, a); }
            Key::Char('g') if last_char == 'g' => { *panes[active].cursor_mut() = 0; }
            Key::Char('G') => {
                let last = panes[active].entries().len().saturating_sub(1);
                *panes[active].cursor_mut() = last;
            }
            Key::Char('g') => { /* first half of gg — wait for second g */ }
            Key::Char('\'') => { let (a, _) = split(&mut panes, active); op_goto(&mut o, a); }
            Key::Char('r') => { let (a, _) = split(&mut panes, active); op_rename(&mut o, a); }
            Key::Char('s') => { let (a, _) = split(&mut panes, active); op_sort(&mut o, a); }
            Key::Char('S') => op_sync_panes(&mut panes, active),
            Key::Char('C') => op_rclone_mount(&mut o, &mut panes, &mut mounts),
            Key::Char('f') => op_connect_right(&mut o, &mut panes),
            Key::Char('R') => panes[active].refresh(),
            Key::Char('?') => op_help(&mut o),
            Key::Char('q') | Key::Char('Q') => break,

            // ── Mouse ────────────────────────────────────────────────────────
            Key::Click { btn: 0, col, row } => {
                let clicked = if col <= half { 0usize } else { 1usize };
                if row >= 2 {
                    let idx = panes[clicked].offset() + (row as usize - 2);
                    if idx < panes[clicked].entries().len() {
                        if clicked == active && idx == panes[clicked].cursor() {
                            panes[clicked].enter();
                        } else {
                            active = clicked;
                            *panes[clicked].cursor_mut() = idx;
                        }
                    } else { active = clicked; }
                } else { active = clicked; }
            }
            Key::Click { btn: 2, col, .. } => {
                let clicked = if col <= half { 0usize } else { 1usize };
                active = clicked;
                panes[active].back();
            }
            Key::Scroll { up, col, .. } => {
                let p = if col <= half { 0usize } else { 1usize };
                let len = panes[p].entries().len();
                let c   = panes[p].cursor_mut();
                if up { *c = c.saturating_sub(3); }
                else  { *c = (*c + 3).min(len.saturating_sub(1)); }
            }

            _ => {}
        }
        last_char = key_char;
    }

    unmount_all(&mounts);
    mouse_off(&mut o);
    clr(&mut o);
    show_cur(&mut o);
    o.flush().unwrap();
    raw_off();
}
