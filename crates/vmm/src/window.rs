//! The native window sink. macOS only allows windows on the main thread, so `host` owns that
//! thread and the rest of the VMM asks it for a window over a channel.
use crate::{
    devices::input::{ABS_MAX, InputPorts},
    screen::{Frame, Screen},
};
use softbuffer::{Context, Surface};
use std::{
    collections::HashSet,
    io,
    num::NonZeroU32,
    rc::Rc,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Sender},
    },
    time::Duration,
};
use winit::{
    application::ApplicationHandler,
    dpi::LogicalSize,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop},
    keyboard::{KeyCode, PhysicalKey},
    window::{Window as Native, WindowId},
};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// Trackpad pixels to one wheel click.
const PIXELS_PER_CLICK: f64 = 40.0;

static HOST: OnceLock<Sender<Command>> = OnceLock::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

struct Open {
    id: u64,
    title: String,
    screen: Arc<Screen>,
    input: InputPorts,
    closed: Box<dyn FnOnce() + Send>,
    reply: Sender<io::Result<()>>,
}

enum Command {
    Open(Box<Open>),
    Close(u64),
    Redraw(u64),
}

/// A running window: dropping it closes the window.
pub struct Window {
    id: u64,
    stop: Arc<AtomicBool>,
}

impl Drop for Window {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(host) = HOST.get() {
            let _ = host.send(Command::Close(self.id));
        }
    }
}

/// Asks the main thread for a window. `closed` runs if the person closes it.
pub fn open(
    title: String,
    screen: Arc<Screen>,
    input: InputPorts,
    closed: Box<dyn FnOnce() + Send>,
) -> io::Result<Window> {
    let host = HOST
        .get()
        .ok_or_else(|| io::Error::other("no window host in this process"))?;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let stop = Arc::new(AtomicBool::new(false));
    let (reply, opened) = mpsc::channel();
    let watched = screen.clone();
    host.send(Command::Open(Box::new(Open {
        id,
        title,
        screen,
        input,
        closed,
        reply,
    })))
    .map_err(|_| io::Error::other("the window host has gone"))?;
    opened
        .recv()
        .map_err(|_| io::Error::other("the window host has gone"))??;
    let window = Window { id, stop };
    let (stop, host) = (window.stop.clone(), host.clone());
    std::thread::spawn(move || {
        let mut seen = 0;
        while !stop.load(Ordering::Relaxed) {
            let now = watched.wait_newer(seen, Duration::from_secs(1));
            if now != seen {
                seen = now;
                let _ = host.send(Command::Redraw(id));
                std::thread::sleep(FRAME_INTERVAL);
            }
        }
    });
    Ok(window)
}

/// Runs on the main thread for the life of the process. The event loop starts at the first open,
/// so a VM that never asks for a window never touches the windowing system. `vm` starts on its
/// own thread once windows can be asked for, and ends the process.
pub fn host(vm: impl FnOnce() + Send + 'static) {
    let (sender, commands) = mpsc::channel();
    let _ = HOST.set(sender);
    std::thread::spawn(vm);
    while let Ok(command) = commands.recv() {
        let Command::Open(open) = command else {
            continue;
        };
        let event_loop = match build_event_loop() {
            Ok(event_loop) => event_loop,
            Err(error) => {
                let _ = open
                    .reply
                    .send(Err(io::Error::other(format!("no window: {error}"))));
                continue;
            }
        };
        let proxy = event_loop.create_proxy();
        std::thread::spawn(move || {
            while let Ok(command) = commands.recv() {
                if proxy.send_event(command).is_err() {
                    return;
                }
            }
        });
        let mut app = App {
            started: false,
            pending: vec![*open],
            shown: None,
        };
        if let Err(error) = event_loop.run_app(&mut app) {
            eprintln!("window: {error}");
        }
        break;
    }
    loop {
        std::thread::park();
    }
}

fn build_event_loop() -> Result<EventLoop<Command>, winit::error::EventLoopError> {
    let mut builder = EventLoop::with_user_event();
    // The default menu's Cmd+Q would kill the VM; with no menu the key reaches the guest.
    #[cfg(target_os = "macos")]
    winit::platform::macos::EventLoopBuilderExtMacOS::with_default_menu(&mut builder, false);
    builder.build()
}

struct Shown {
    id: u64,
    native: Rc<Native>,
    surface: Surface<Rc<Native>, Rc<Native>>,
    screen: Arc<Screen>,
    input: InputPorts,
    closed: Option<Box<dyn FnOnce() + Send>>,
    held: HashSet<u16>,
    scrolled: f64,
}

impl Shown {
    fn release_keys(&mut self) {
        for code in self.held.drain() {
            self.input.key(code, false);
        }
    }

    fn draw(&mut self) -> Result<(), softbuffer::SoftBufferError> {
        let size = self.native.inner_size();
        let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return Ok(());
        };
        self.surface.resize(width, height)?;
        let mut buffer = self.surface.buffer_mut()?;
        self.screen
            .with(|frame| blit(frame, &mut buffer, size.width, size.height));
        self.native.pre_present_notify();
        buffer.present()
    }
}

struct App {
    started: bool,
    pending: Vec<Open>,
    shown: Option<Shown>,
}

impl App {
    fn show(&mut self, event_loop: &ActiveEventLoop, open: Open) {
        let (width, height) = open.screen.with(|frame| (frame.width, frame.height));
        let attributes = Native::default_attributes()
            .with_title(open.title)
            .with_inner_size(LogicalSize::new(width, height));
        let created = event_loop
            .create_window(attributes)
            .map_err(io::Error::other)
            .and_then(|native| {
                let native = Rc::new(native);
                let context = Context::new(native.clone()).map_err(softbuffer_error)?;
                let surface = Surface::new(&context, native.clone()).map_err(softbuffer_error)?;
                Ok((native, surface))
            });
        let result = created.map(|(native, surface)| {
            native.request_redraw();
            native.focus_window();
            // Replaces any earlier window; that one's owner is about to drop it.
            self.shown = Some(Shown {
                id: open.id,
                native,
                surface,
                screen: open.screen,
                input: open.input,
                closed: Some(open.closed),
                held: HashSet::new(),
                scrolled: 0.0,
            });
        });
        let _ = open.reply.send(result);
    }
}

fn softbuffer_error(error: softbuffer::SoftBufferError) -> io::Error {
    io::Error::other(error.to_string())
}

impl ApplicationHandler<Command> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.started = true;
        for open in std::mem::take(&mut self.pending) {
            self.show(event_loop, open);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, command: Command) {
        match command {
            Command::Open(open) if self.started => self.show(event_loop, *open),
            Command::Open(open) => self.pending.push(*open),
            Command::Close(id) => {
                if self.shown.as_ref().is_some_and(|shown| shown.id == id) {
                    let mut shown = self.shown.take().unwrap();
                    shown.release_keys();
                }
            }
            Command::Redraw(id) => {
                if let Some(shown) = self.shown.as_ref().filter(|shown| shown.id == id) {
                    shown.native.request_redraw();
                }
            }
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let Some(shown) = self.shown.as_mut() else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                let mut shown = self.shown.take().unwrap();
                shown.release_keys();
                if let Some(closed) = shown.closed.take() {
                    closed();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(error) = shown.draw() {
                    eprintln!("window: {error}");
                }
            }
            WindowEvent::Resized(_) => shown.native.request_redraw(),
            // A key let go while another app has focus would otherwise stay down in the guest.
            WindowEvent::Focused(false) => shown.release_keys(),
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(key) = event.physical_key else {
                    return;
                };
                let Some(code) = keycode(key) else { return };
                if event.repeat {
                    return;
                }
                let down = event.state == ElementState::Pressed;
                if down {
                    shown.held.insert(code);
                } else {
                    shown.held.remove(&code);
                }
                shown.input.key(code, down);
            }
            WindowEvent::CursorMoved { position, .. } => {
                let size = shown.native.inner_size();
                let frame = shown.screen.with(|frame| (frame.width, frame.height));
                let place = fit(frame, (size.width, size.height));
                if let Some((x, y)) = to_tablet(place, position.x, position.y) {
                    shown.input.pointer(x, y);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    MouseButton::Left => 0,
                    MouseButton::Middle => 1,
                    MouseButton::Right => 2,
                    _ => return,
                };
                shown.input.button(button, state == ElementState::Pressed);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let clicks = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(position) => position.y / PIXELS_PER_CLICK,
                };
                shown.scrolled += clicks;
                let whole = shown.scrolled.trunc();
                shown.scrolled -= whole;
                if whole != 0.0 {
                    shown.input.scroll(whole as i32);
                }
            }
            _ => (),
        }
    }
}

/// Where the frame sits in the window: as large as fits, aspect kept. (x, y, width, height).
fn fit(frame: (u32, u32), window: (u32, u32)) -> (u32, u32, u32, u32) {
    let (fw, fh, ww, wh) = (
        u64::from(frame.0.max(1)),
        u64::from(frame.1.max(1)),
        u64::from(window.0),
        u64::from(window.1),
    );
    let (w, h) = if ww * fh <= wh * fw {
        (ww, ww * fh / fw)
    } else {
        (wh * fw / fh, wh)
    };
    (
        ((ww - w) / 2) as u32,
        ((wh - h) / 2) as u32,
        w as u32,
        h as u32,
    )
}

/// Draws the frame into a 0RGB window buffer, nearest pixel, black around it.
fn blit(frame: &Frame, out: &mut [u32], width: u32, height: u32) {
    out.fill(0);
    if frame.width == 0 || frame.height == 0 {
        return;
    }
    let (x0, y0, w, h) = fit((frame.width, frame.height), (width, height));
    if w == 0 || h == 0 {
        return;
    }
    let columns: Vec<usize> = (0..u64::from(w))
        .map(|x| (x * u64::from(frame.width) / u64::from(w)) as usize * 4)
        .collect();
    let stride = frame.width as usize * 4;
    for y in 0..h as usize {
        let source = y * frame.height as usize / h as usize * stride;
        let row = &frame.pixels[source..source + stride];
        let start = (y + y0 as usize) * width as usize + x0 as usize;
        for (pixel, &column) in out[start..start + w as usize].iter_mut().zip(&columns) {
            let p = &row[column..column + 3];
            *pixel = u32::from(p[0]) << 16 | u32::from(p[1]) << 8 | u32::from(p[2]);
        }
    }
}

/// A window position in tablet units; `None` over the bars around the frame.
fn to_tablet(place: (u32, u32, u32, u32), x: f64, y: f64) -> Option<(u32, u32)> {
    let axis = |at: f64, start: u32, size: u32| {
        let inside = at - f64::from(start);
        (size > 1 && inside >= 0.0 && inside < f64::from(size))
            .then(|| (inside * f64::from(ABS_MAX) / f64::from(size - 1)) as u32)
            .map(|value| value.min(ABS_MAX))
    };
    Some((axis(x, place.0, place.2)?, axis(y, place.1, place.3)?))
}

/// Physical key to Linux keycode.
fn keycode(key: KeyCode) -> Option<u16> {
    use KeyCode::*;
    Some(match key {
        Escape => 1,
        Digit1 => 2,
        Digit2 => 3,
        Digit3 => 4,
        Digit4 => 5,
        Digit5 => 6,
        Digit6 => 7,
        Digit7 => 8,
        Digit8 => 9,
        Digit9 => 10,
        Digit0 => 11,
        Minus => 12,
        Equal => 13,
        Backspace => 14,
        Tab => 15,
        KeyQ => 16,
        KeyW => 17,
        KeyE => 18,
        KeyR => 19,
        KeyT => 20,
        KeyY => 21,
        KeyU => 22,
        KeyI => 23,
        KeyO => 24,
        KeyP => 25,
        BracketLeft => 26,
        BracketRight => 27,
        Enter => 28,
        ControlLeft => 29,
        KeyA => 30,
        KeyS => 31,
        KeyD => 32,
        KeyF => 33,
        KeyG => 34,
        KeyH => 35,
        KeyJ => 36,
        KeyK => 37,
        KeyL => 38,
        Semicolon => 39,
        Quote => 40,
        Backquote => 41,
        ShiftLeft => 42,
        Backslash => 43,
        KeyZ => 44,
        KeyX => 45,
        KeyC => 46,
        KeyV => 47,
        KeyB => 48,
        KeyN => 49,
        KeyM => 50,
        Comma => 51,
        Period => 52,
        Slash => 53,
        ShiftRight => 54,
        NumpadMultiply => 55,
        AltLeft => 56,
        Space => 57,
        CapsLock => 58,
        F1 => 59,
        F2 => 60,
        F3 => 61,
        F4 => 62,
        F5 => 63,
        F6 => 64,
        F7 => 65,
        F8 => 66,
        F9 => 67,
        F10 => 68,
        NumLock => 69,
        ScrollLock => 70,
        Numpad7 => 71,
        Numpad8 => 72,
        Numpad9 => 73,
        NumpadSubtract => 74,
        Numpad4 => 75,
        Numpad5 => 76,
        Numpad6 => 77,
        NumpadAdd => 78,
        Numpad1 => 79,
        Numpad2 => 80,
        Numpad3 => 81,
        Numpad0 => 82,
        NumpadDecimal => 83,
        IntlBackslash => 86,
        F11 => 87,
        F12 => 88,
        NumpadEnter => 96,
        ControlRight => 97,
        NumpadDivide => 98,
        AltRight => 100,
        Home => 102,
        ArrowUp => 103,
        PageUp => 104,
        ArrowLeft => 105,
        ArrowRight => 106,
        End => 107,
        ArrowDown => 108,
        PageDown => 109,
        Insert => 110,
        Delete => 111,
        SuperLeft => 125,
        SuperRight => 126,
        ContextMenu => 127,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Frame {
        let mut frame = Frame::default();
        frame.resize(2, 1);
        frame.pixels[..4].copy_from_slice(&[0x11, 0x22, 0x33, 255]);
        frame.pixels[4..8].copy_from_slice(&[0x44, 0x55, 0x66, 255]);
        frame
    }

    #[test]
    fn the_frame_scales_up_with_bars_and_keeps_its_colors() {
        let mut out = vec![9; 16];
        blit(&frame(), &mut out, 4, 4);
        let (a, b) = (0x112233, 0x445566);
        #[rustfmt::skip]
        assert_eq!(out, [0, 0, 0, 0, a, a, b, b, a, a, b, b, 0, 0, 0, 0]);
    }

    #[test]
    fn hostile_sizes_draw_nothing_rather_than_panic() {
        let mut out = vec![9; 1];
        blit(&Frame::default(), &mut out, 1, 1);
        assert_eq!(out, [0]);
        blit(&frame(), &mut out, 1, 1); // scales to zero height
        assert_eq!(out, [0]);
    }

    #[test]
    fn the_pointer_maps_inside_the_frame_only() {
        let place = fit((2, 1), (4, 4));
        assert_eq!(place, (0, 1, 4, 2));
        assert_eq!(to_tablet(place, 0.0, 0.0), None);
        assert_eq!(to_tablet(place, 0.0, 1.0), Some((0, 0)));
        assert_eq!(to_tablet(place, 3.0, 2.0), Some((ABS_MAX, ABS_MAX)));
    }

    #[test]
    fn keys_use_linux_codes() {
        assert_eq!(keycode(KeyCode::KeyA), Some(30));
        assert_eq!(keycode(KeyCode::SuperLeft), Some(125));
        assert_eq!(keycode(KeyCode::Enter), Some(28));
        assert_eq!(keycode(KeyCode::F24), None);
    }
}
