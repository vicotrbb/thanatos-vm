#![no_main]
use libfuzzer_sys::fuzz_target;
use thanatos_vmm::devices::input::{Input, Kind};

fuzz_target!(|data: &[u8]| {
    thanatos_fuzz::drive(Input::new(Kind::Tablet), data, 2);
});
