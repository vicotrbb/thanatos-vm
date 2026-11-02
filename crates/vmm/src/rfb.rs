//! A VNC server: RFB 3.8 with None or VNC authentication. Everything from the client is parsed
//! by [`read_message`]. shortcut: raw encoding only (CopyRect is optional for a server), add CopyRect or ZRLE if a remote link needs it.
use crate::{
    devices::input::{ABS_MAX, InputPorts},
    screen::Screen,
};
use std::{
    io::{self, Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

const MAX_CLIENTS: usize = 16;
const MAX_CUT_TEXT: usize = 1 << 20;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// The pixel format until the client picks one: the bytes as `Frame` stores them.
const NATIVE: PixelFormat = PixelFormat {
    bits: 32,
    big_endian: false,
    max: [255, 255, 255],
    shift: [0, 8, 16],
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixelFormat {
    bits: u8,
    big_endian: bool,
    max: [u16; 3],
    shift: [u8; 3],
}

#[derive(Debug, PartialEq)]
pub enum Message {
    SetPixelFormat(PixelFormat),
    SetEncodings,
    Update {
        incremental: bool,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    Key {
        down: bool,
        keysym: u32,
    },
    Pointer {
        buttons: u8,
        x: u16,
        y: u16,
    },
    CutText,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

/// One client message. Lengths are checked before anything is allocated.
pub fn read_message(input: &mut impl Read) -> io::Result<Message> {
    let mut kind = [0];
    input.read_exact(&mut kind)?;
    match kind[0] {
        0 => {
            let mut body = [0; 19];
            input.read_exact(&mut body)?;
            let f = &body[3..];
            let bits = f[0];
            let big = f[2] != 0;
            let true_color = f[3] != 0;
            let u16_at = |at: usize| u16::from_be_bytes([f[at], f[at + 1]]);
            if !matches!(bits, 8 | 16 | 32) || !true_color {
                return Err(invalid("only true colour at 8, 16 or 32 bits"));
            }
            let shift = [f[10], f[11], f[12]];
            if shift.iter().any(|&s| u32::from(s) >= u32::from(bits)) {
                return Err(invalid("pixel shift outside the pixel"));
            }
            Ok(Message::SetPixelFormat(PixelFormat {
                bits,
                big_endian: big,
                max: [u16_at(4), u16_at(6), u16_at(8)],
                shift,
            }))
        }
        2 => {
            let mut head = [0; 3];
            input.read_exact(&mut head)?;
            let count = u16::from_be_bytes([head[1], head[2]]);
            // Raw is always allowed, so what the client lists does not matter.
            io::copy(&mut input.take(u64::from(count) * 4), &mut io::sink())?;
            Ok(Message::SetEncodings)
        }
        3 => {
            let mut body = [0; 9];
            input.read_exact(&mut body)?;
            let u16_at = |at: usize| u16::from_be_bytes([body[at], body[at + 1]]);
            Ok(Message::Update {
                incremental: body[0] != 0,
                x: u16_at(1),
                y: u16_at(3),
                width: u16_at(5),
                height: u16_at(7),
            })
        }
        4 => {
            let mut body = [0; 7];
            input.read_exact(&mut body)?;
            Ok(Message::Key {
                down: body[0] != 0,
                keysym: u32::from_be_bytes([body[3], body[4], body[5], body[6]]),
            })
        }
        5 => {
            let mut body = [0; 5];
            input.read_exact(&mut body)?;
            Ok(Message::Pointer {
                buttons: body[0],
                x: u16::from_be_bytes([body[1], body[2]]),
                y: u16::from_be_bytes([body[3], body[4]]),
            })
        }
        6 => {
            let mut body = [0; 7];
            input.read_exact(&mut body)?;
            let length = u32::from_be_bytes([body[3], body[4], body[5], body[6]]) as usize;
            if length > MAX_CUT_TEXT {
                return Err(invalid("cut text too long"));
            }
            io::copy(&mut input.take(length as u64), &mut io::sink())?;
            Ok(Message::CutText)
        }
        _ => Err(invalid("unknown client message")),
    }
}

/// X11 keysym to Linux keycode. Shifted symbols share their base key: the client sends Shift.
fn keycode(keysym: u32) -> Option<u16> {
    const ROWS: [(&str, u16); 8] = [
        ("1234567890", 2),
        ("qwertyuiop", 16),
        ("asdfghjkl", 30),
        ("zxcvbnm", 44),
        ("!@#$%^&*()", 2),
        ("QWERTYUIOP", 16),
        ("ASDFGHJKL", 30),
        ("ZXCVBNM", 44),
    ];
    if let Some(symbol) = char::from_u32(keysym).filter(|c| c.is_ascii_graphic()) {
        for (row, first) in ROWS {
            if let Some(index) = row.find(symbol) {
                return Some(first + index as u16);
            }
        }
        return match symbol {
            '-' | '_' => Some(12),
            '=' | '+' => Some(13),
            '[' | '{' => Some(26),
            ']' | '}' => Some(27),
            ';' | ':' => Some(39),
            '\'' | '"' => Some(40),
            '`' | '~' => Some(41),
            '\\' | '|' => Some(43),
            ',' | '<' => Some(51),
            '.' | '>' => Some(52),
            '/' | '?' => Some(53),
            _ => None,
        };
    }
    match keysym {
        0x20 => Some(57),
        0xff08 => Some(14),
        0xff09 => Some(15),
        0xff0d => Some(28),
        0xff1b => Some(1),
        0xff50 => Some(102),
        0xff51 => Some(105),
        0xff52 => Some(103),
        0xff53 => Some(106),
        0xff54 => Some(108),
        0xff55 => Some(104),
        0xff56 => Some(109),
        0xff57 => Some(107),
        0xff63 => Some(110),
        0xffff => Some(111),
        0xffbe..=0xffc7 => Some(59 + (keysym - 0xffbe) as u16),
        0xffc8 => Some(87),
        0xffc9 => Some(88),
        0xffe1 => Some(42),
        0xffe2 => Some(54),
        0xffe3 => Some(29),
        0xffe4 => Some(97),
        0xffe5 => Some(58),
        0xffe9 | 0xffe7 => Some(56),
        0xffea | 0xffe8 => Some(100),
        0xffeb => Some(125),
        0xffec => Some(126),
        _ => None,
    }
}

/// Encodes a run of RGBA pixels in the client's format.
fn encode(pixels: &[u8], format: &PixelFormat, out: &mut Vec<u8>) {
    if *format == NATIVE {
        // The usual case, and alpha is ignored by the shifts.
        out.extend_from_slice(pixels);
        return;
    }
    for pixel in pixels.chunks_exact(4) {
        let mut value = 0u32;
        for ((&byte, &max), &shift) in pixel.iter().zip(&format.max).zip(&format.shift) {
            value |= (u32::from(byte) * u32::from(max) / 255) << shift;
        }
        match (format.bits, format.big_endian) {
            (32, false) => out.extend_from_slice(&value.to_le_bytes()),
            (32, true) => out.extend_from_slice(&value.to_be_bytes()),
            (16, false) => out.extend_from_slice(&(value as u16).to_le_bytes()),
            (16, true) => out.extend_from_slice(&(value as u16).to_be_bytes()),
            _ => out.push(value as u8),
        }
    }
}

/// What the reader thread leaves for the writer thread.
#[derive(Default)]
struct Wanted {
    update: bool,
    full: bool,
    format: Option<PixelFormat>,
    closed: bool,
}

type Shared = (Mutex<Wanted>, Condvar);

/// A running server. Dropping it closes the listener and every client.
pub struct Vnc {
    closing: Arc<AtomicBool>,
    bound: SocketAddr,
    clients: Arc<Mutex<Vec<TcpStream>>>,
}

impl Drop for Vnc {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::SeqCst);
        // accept has no timeout; one last connection wakes it to see the flag.
        let _ = TcpStream::connect(self.bound);
        for client in self.clients.lock().unwrap().drain(..) {
            let _ = client.shutdown(Shutdown::Both);
        }
    }
}

impl Vnc {
    /// A listener reachable from elsewhere must have a password, whatever the caller asked for.
    pub fn start(
        bind: &str,
        password: Option<String>,
        screen: Arc<Screen>,
        input: InputPorts,
    ) -> io::Result<Self> {
        let address: SocketAddr = bind
            .parse()
            .map_err(|_| io::Error::other(format!("bad VNC address {bind:?}")))?;
        if !address.ip().is_loopback() && password.is_none() {
            return Err(io::Error::other(format!(
                "VNC on {bind} is reachable from other machines and needs a password"
            )));
        }
        let listener = TcpListener::bind(address)?;
        let bound = listener.local_addr()?;
        let closing = Arc::new(AtomicBool::new(false));
        let clients: Arc<Mutex<Vec<TcpStream>>> = Arc::default();
        let count = Arc::new(AtomicUsize::new(0));
        let (flag, list) = (closing.clone(), clients.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                if flag.load(Ordering::SeqCst) {
                    return;
                }
                if count.load(Ordering::SeqCst) >= MAX_CLIENTS {
                    continue;
                }
                count.fetch_add(1, Ordering::SeqCst);
                if let Ok(clone) = stream.try_clone() {
                    let mut list = list.lock().unwrap();
                    // Finished clients are shut already; only their entries remain to clear.
                    list.retain(|client: &TcpStream| client.peer_addr().is_ok());
                    list.push(clone);
                }
                let (screen, input, password, count) = (
                    screen.clone(),
                    input.clone(),
                    password.clone(),
                    count.clone(),
                );
                std::thread::spawn(move || {
                    let _ = client(&stream, password.as_deref(), &screen, &input);
                    let _ = stream.shutdown(Shutdown::Both);
                    count.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Ok(Self {
            closing,
            bound,
            clients,
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.bound
    }
}

fn handshake(stream: &TcpStream, password: Option<&str>) -> io::Result<()> {
    let (mut reader, mut writer) = (stream, stream);
    writer.write_all(b"RFB 003.008\n")?;
    let mut version = [0; 12];
    reader.read_exact(&mut version)?;
    if &version[..4] != b"RFB " {
        return Err(invalid("not an RFB client"));
    }
    writer.write_all(&[1, if password.is_some() { 2 } else { 1 }])?;
    let mut choice = [0];
    reader.read_exact(&mut choice)?;
    if let Some(password) = password {
        if choice[0] != 2 {
            return Err(invalid("client refused authentication"));
        }
        let mut challenge = [0; 16];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut challenge)?;
        writer.write_all(&challenge)?;
        let mut answer = [0; 16];
        reader.read_exact(&mut answer)?;
        if answer != vnc_response(password, &challenge) {
            writer.write_all(&1u32.to_be_bytes())?;
            let reason = b"wrong password";
            writer.write_all(&(reason.len() as u32).to_be_bytes())?;
            writer.write_all(reason)?;
            return Err(invalid("wrong password"));
        }
    } else if choice[0] != 1 {
        return Err(invalid("client refused no authentication"));
    }
    writer.write_all(&0u32.to_be_bytes())?;
    let mut shared = [0];
    reader.read_exact(&mut shared)?;
    Ok(())
}

fn client(
    stream: &TcpStream,
    password: Option<&str>,
    screen: &Screen,
    input: &InputPorts,
) -> io::Result<()> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let _ = stream.set_nodelay(true);
    handshake(stream, password)?;
    stream.set_read_timeout(None)?;
    let (width, height) = screen.with(|frame| (frame.width, frame.height));
    let mut init = Vec::new();
    init.extend_from_slice(&(width as u16).to_be_bytes());
    init.extend_from_slice(&(height as u16).to_be_bytes());
    init.extend_from_slice(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0]);
    init.extend_from_slice(&8u32.to_be_bytes());
    init.extend_from_slice(b"thanatos");
    let mut writer = stream;
    writer.write_all(&init)?;

    let shared: Shared = Default::default();
    std::thread::scope(|scope| {
        let sender = scope.spawn(|| send_updates(stream, screen, &shared));
        let result = read_client(stream, input, &shared, (width, height));
        shared.0.lock().unwrap().closed = true;
        shared.1.notify_all();
        let _ = stream.shutdown(Shutdown::Both);
        let _ = sender.join();
        result
    })
}

fn read_client(
    stream: &TcpStream,
    input: &InputPorts,
    shared: &Shared,
    (width, height): (u32, u32),
) -> io::Result<()> {
    let mut reader = stream;
    let scale = |value: u16, size: u32| {
        u32::from(value).min(size.saturating_sub(1)) * ABS_MAX / size.saturating_sub(1).max(1)
    };
    let mut buttons = 0u8;
    loop {
        match read_message(&mut reader)? {
            Message::SetPixelFormat(format) => {
                let mut wanted = shared.0.lock().unwrap();
                wanted.format = Some(format);
                wanted.full = true;
            }
            Message::Update { incremental, .. } => {
                let mut wanted = shared.0.lock().unwrap();
                wanted.update = true;
                wanted.full |= !incremental;
                shared.1.notify_all();
            }
            Message::Key { down, keysym } => {
                if let Some(code) = keycode(keysym) {
                    input.key(code, down);
                }
            }
            Message::Pointer { buttons: now, x, y } => {
                input.pointer(scale(x, width), scale(y, height));
                for button in 0..3 {
                    if (now ^ buttons) & (1 << button) != 0 {
                        input.button(button, now & (1 << button) != 0);
                    }
                }
                // Buttons 4 and 5 are the wheel, one click per press.
                if (now & !buttons) & 8 != 0 {
                    input.scroll(1);
                }
                if (now & !buttons) & 16 != 0 {
                    input.scroll(-1);
                }
                buttons = now;
            }
            Message::SetEncodings | Message::CutText => (),
        }
    }
}

/// Sends the rows that differ from what this client last got, when it has asked for an update.
fn send_updates(stream: &TcpStream, screen: &Screen, shared: &Shared) -> io::Result<()> {
    let mut writer = stream;
    let mut format = NATIVE;
    let mut last: Vec<u8> = Vec::new();
    let mut seen = 0;
    loop {
        let full = {
            let mut wanted = shared.0.lock().unwrap();
            while !wanted.update && !wanted.closed {
                wanted = shared.1.wait(wanted).unwrap();
            }
            if wanted.closed {
                return Ok(());
            }
            if let Some(new) = wanted.format.take() {
                format = new;
            }
            wanted.update = false;
            std::mem::take(&mut wanted.full)
        };
        loop {
            if shared.0.lock().unwrap().closed {
                return Ok(());
            }
            if !full && screen.with(|f| f.generation) <= seen {
                seen = screen.wait_newer(seen, Duration::from_millis(200));
            }
            let frame = screen.with(|frame| {
                seen = frame.generation;
                frame.clone()
            });
            let stride = frame.width as usize * 4;
            if last.len() != frame.pixels.len() || full {
                last = vec![0; frame.pixels.len()];
                last.iter_mut().for_each(|b| *b = !0);
            }
            let rows = changed_rows(&frame.pixels, &last, stride);
            let Some((first, end)) = rows else {
                // Nothing changed yet: an incremental request waits, without an empty reply.
                continue;
            };
            let mut message = vec![0, 0, 0, 1];
            for value in [0, first, frame.width as usize, end - first] {
                message.extend_from_slice(&(value as u16).to_be_bytes());
            }
            message.extend_from_slice(&0i32.to_be_bytes());
            encode(
                &frame.pixels[first * stride..end * stride],
                &format,
                &mut message,
            );
            writer.write_all(&message)?;
            last[first * stride..end * stride]
                .copy_from_slice(&frame.pixels[first * stride..end * stride]);
            break;
        }
    }
}

fn changed_rows(now: &[u8], last: &[u8], stride: usize) -> Option<(usize, usize)> {
    if stride == 0 {
        return None;
    }
    let differs = |row: &(usize, (&[u8], &[u8]))| row.1.0 != row.1.1;
    let rows = || now.chunks(stride).zip(last.chunks(stride)).enumerate();
    let first = rows().find(|row| differs(row))?.0;
    let last_row = rows().rev().find(|row| differs(row))?.0;
    Some((first, last_row + 1))
}

/// The answer to a VNC challenge: DES-ECB under the password, its bytes bit-reversed as RFB says.
pub fn vnc_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0; 8];
    for (slot, byte) in key.iter_mut().zip(password.bytes()) {
        *slot = byte.reverse_bits();
    }
    let schedule = des_schedule(u64::from_be_bytes(key));
    let mut out = [0; 16];
    for (input, output) in challenge.chunks_exact(8).zip(out.chunks_exact_mut(8)) {
        let block = u64::from_be_bytes(input.try_into().unwrap());
        output.copy_from_slice(&des_encrypt(block, &schedule).to_be_bytes());
    }
    out
}

/// Takes the bits `table` names (1-based from the top of `width`) from `value`.
fn permute(value: u64, width: u32, table: &[u8]) -> u64 {
    table.iter().fold(0, |out, &bit| {
        out << 1 | (value >> (width - u32::from(bit))) & 1
    })
}

fn des_schedule(key: u64) -> [u64; 16] {
    const PC1: [u8; 56] = [
        57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3,
        60, 52, 44, 36, 63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45,
        37, 29, 21, 13, 5, 28, 20, 12, 4,
    ];
    const PC2: [u8; 48] = [
        14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2, 41,
        52, 31, 37, 47, 55, 30, 40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
    ];
    const SHIFTS: [u32; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
    let permuted = permute(key, 64, &PC1);
    let (mut c, mut d) = ((permuted >> 28) as u32, (permuted & 0xfff_ffff) as u32);
    let mut keys = [0; 16];
    for (round, shift) in SHIFTS.iter().enumerate() {
        c = (c << shift | c >> (28 - shift)) & 0xfff_ffff;
        d = (d << shift | d >> (28 - shift)) & 0xfff_ffff;
        keys[round] = permute(u64::from(c) << 28 | u64::from(d), 56, &PC2);
    }
    keys
}

fn des_encrypt(block: u64, keys: &[u64; 16]) -> u64 {
    const IP: [u8; 64] = [
        58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14,
        6, 64, 56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11,
        3, 61, 53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
    ];
    const FP: [u8; 64] = [
        40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62,
        30, 37, 5, 45, 13, 53, 21, 61, 29, 36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19,
        59, 27, 34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
    ];
    const E: [u8; 48] = [
        32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17, 16, 17,
        18, 19, 20, 21, 20, 21, 22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
    ];
    const P: [u8; 32] = [
        16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9,
        19, 13, 30, 6, 22, 11, 4, 25,
    ];
    const S: [[u8; 64]; 8] = [
        [
            14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6,
            12, 11, 9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2,
            4, 9, 1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
        ],
        [
            15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 14, 12, 0,
            1, 10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1,
            3, 15, 4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
        ],
        [
            10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8,
            5, 14, 12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13,
            0, 6, 9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12,
        ],
        [
            7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7,
            2, 12, 1, 10, 14, 9, 10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6,
            10, 1, 13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
        ],
        [
            2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0,
            15, 10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7,
            1, 14, 2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
        ],
        [
            12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1,
            13, 14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12,
            9, 5, 15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
        ],
        [
            4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3,
            5, 12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8,
            1, 4, 10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
        ],
        [
            13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5,
            6, 11, 0, 14, 9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7,
            4, 10, 8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
        ],
    ];
    let permuted = permute(block, 64, &IP);
    let (mut left, mut right) = ((permuted >> 32) as u32, permuted as u32);
    for key in keys {
        let expanded = permute(u64::from(right), 32, &E) ^ key;
        let mut substituted = 0u32;
        for (index, box_) in S.iter().enumerate() {
            let six = (expanded >> (42 - index * 6)) & 0x3f;
            let row = (six >> 4 & 2 | six & 1) as usize;
            let column = (six >> 1 & 0xf) as usize;
            substituted = substituted << 4 | u32::from(box_[row * 16 + column]);
        }
        let mixed = permute(u64::from(substituted), 32, &P) as u32;
        (left, right) = (right, left ^ mixed);
    }
    permute(u64::from(right) << 32 | u64::from(left), 64, &FP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn des_matches_the_published_vector() {
        let keys = des_schedule(0x1334_5779_9BBC_DFF1);
        assert_eq!(
            des_encrypt(0x0123_4567_89AB_CDEF, &keys),
            0x85E8_1354_0F0A_B405
        );
    }

    #[test]
    fn messages_parse_and_hostile_ones_fail_cleanly() {
        let key = [4, 1, 0, 0, 0, 0, 0xff, 0x0d];
        assert_eq!(
            read_message(&mut key.as_slice()).unwrap(),
            Message::Key {
                down: true,
                keysym: 0xff0d
            }
        );
        let update = [3, 1, 0, 0, 0, 0, 5, 0, 3, 0];
        assert!(matches!(
            read_message(&mut update.as_slice()).unwrap(),
            Message::Update {
                incremental: true,
                width: 1280,
                height: 768,
                ..
            }
        ));
        for hostile in [
            &[9u8][..],                            // unknown type
            &[6, 0, 0, 0, 0xff, 0xff, 0xff, 0xff], // 4 GiB of cut text
            &[4, 1],                               // truncated
            &[
                0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 200, 8, 16, 0, 0, 0,
            ], // shift past the pixel
        ] {
            assert!(read_message(&mut { hostile }).is_err());
        }
    }

    #[test]
    fn keysyms_map_to_linux_keycodes() {
        assert_eq!(keycode('a' as u32), Some(30));
        assert_eq!(keycode('A' as u32), Some(30));
        assert_eq!(keycode('1' as u32), Some(2));
        assert_eq!(keycode('!' as u32), Some(2));
        assert_eq!(keycode(0xff0d), Some(28));
        assert_eq!(keycode(0xffbe), Some(59));
        assert_eq!(keycode(0xffeb), Some(125));
        assert_eq!(keycode(0x1234_5678), None);
    }

    #[test]
    fn pixels_convert_to_the_clients_format() {
        let mut out = Vec::new();
        let rgb565 = PixelFormat {
            bits: 16,
            big_endian: false,
            max: [31, 63, 31],
            shift: [11, 5, 0],
        };
        encode(&[255, 0, 0, 255], &rgb565, &mut out);
        assert_eq!(out, 0xf800u16.to_le_bytes());
    }

    #[test]
    fn only_changed_rows_are_sent() {
        let now = [0, 0, 1, 1, 0, 0];
        assert_eq!(changed_rows(&now, &[0; 6], 2), Some((1, 2)));
        assert_eq!(changed_rows(&now, &now, 2), None);
    }
}
