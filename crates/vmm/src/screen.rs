//! The host's copy of what the guest last presented, and the PNG of it.
use std::{
    io,
    sync::{Condvar, Mutex},
    time::Duration,
};

#[derive(Clone, Default)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// RGBA, opaque.
    pub pixels: Vec<u8>,
    /// Counts presentations, so a sink can sleep until there is something new.
    pub generation: u64,
}

#[derive(Default)]
pub struct Screen {
    frame: Mutex<Frame>,
    changed: Condvar,
}

impl Screen {
    pub fn new(width: u32, height: u32) -> Self {
        let screen = Self::default();
        screen.update(|frame| frame.resize(width, height));
        screen
    }

    /// Changes the frame and wakes the sinks.
    pub fn update(&self, change: impl FnOnce(&mut Frame)) {
        let mut frame = self.frame.lock().unwrap();
        change(&mut frame);
        frame.generation += 1;
        self.changed.notify_all();
    }

    pub fn with<R>(&self, read: impl FnOnce(&Frame) -> R) -> R {
        read(&self.frame.lock().unwrap())
    }

    /// Blocks until the frame is newer than `seen` or the timeout passes; returns the generation.
    pub fn wait_newer(&self, seen: u64, timeout: Duration) -> u64 {
        let frame = self.frame.lock().unwrap();
        let (frame, _) = self
            .changed
            .wait_timeout_while(frame, timeout, |frame| frame.generation <= seen)
            .unwrap();
        frame.generation
    }

    pub fn png(&self) -> io::Result<Vec<u8>> {
        self.with(|frame| {
            let mut out = Vec::new();
            let mut encoder = png::Encoder::new(&mut out, frame.width, frame.height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            // Speed over size: a screenshot loop must not stall on the screen's lock.
            encoder.set_compression(png::Compression::Fast);
            let mut writer = encoder.write_header().map_err(io::Error::other)?;
            writer
                .write_image_data(&frame.pixels)
                .map_err(io::Error::other)?;
            writer.finish().map_err(io::Error::other)?;
            Ok(out)
        })
    }
}

impl Frame {
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.pixels.clear();
        self.pixels.resize(width as usize * height as usize * 4, 0);
        self.pixels.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_screenshot_is_a_png_of_the_last_frame() {
        let screen = Screen::new(2, 1);
        screen.update(|frame| frame.pixels[..4].copy_from_slice(&[1, 2, 3, 255]));
        let png = screen.png().unwrap();
        let mut reader = png::Decoder::new(io::Cursor::new(png)).read_info().unwrap();
        let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut decoded).unwrap();
        assert_eq!(decoded, [1, 2, 3, 255, 0, 0, 0, 255]);
    }

    #[test]
    fn waiting_returns_at_a_new_frame_or_the_timeout() {
        let screen = Screen::new(1, 1);
        let seen = screen.with(|frame| frame.generation);
        assert_eq!(screen.wait_newer(seen, Duration::from_millis(1)), seen);
        screen.update(|_| ());
        assert_eq!(screen.wait_newer(seen, Duration::from_secs(5)), seen + 1);
    }
}
