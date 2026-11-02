//! The serial console's two ends on the host: bytes the guest wrote and bytes waiting for it.
use std::{
    collections::VecDeque,
    io::Write,
    sync::{Arc, Mutex},
};

/// Set by the machine: raises the UART's interrupt when input arrives.
type Kick = Box<dyn Fn() + Send>;

#[derive(Clone, Default)]
pub struct Console(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    input: VecDeque<u8>,
    outputs: Vec<Box<dyn Write + Send>>,
    kick: Option<Kick>,
}

impl Console {
    pub fn add_output(&self, output: impl Write + Send + 'static) {
        self.0.lock().unwrap().outputs.push(Box::new(output));
    }

    pub fn set_kick(&self, kick: impl Fn() + Send + 'static) {
        self.0.lock().unwrap().kick = Some(Box::new(kick));
    }

    /// A client that went away is dropped; the guest never blocks on it.
    pub fn output(&self, byte: u8) {
        let mut inner = self.0.lock().unwrap();
        inner
            .outputs
            .retain_mut(|out| out.write_all(&[byte]).and_then(|()| out.flush()).is_ok());
    }

    pub fn input(&self, bytes: &[u8]) {
        let mut inner = self.0.lock().unwrap();
        inner.input.extend(bytes);
        if let Some(kick) = &inner.kick {
            kick();
        }
    }

    pub fn read(&self) -> Option<u8> {
        self.0.lock().unwrap().input.pop_front()
    }

    pub fn has_input(&self) -> bool {
        !self.0.lock().unwrap().input.is_empty()
    }
}
