#![no_main]
use libfuzzer_sys::fuzz_target;
use std::sync::Arc;
use thanatos_vmm::{devices::gpu::Gpu, screen::Screen};

fuzz_target!(|data: &[u8]| {
    thanatos_fuzz::drive(Gpu::new(Arc::new(Screen::new(16, 16))), data, 2);
});
