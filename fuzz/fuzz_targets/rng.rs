#![no_main]
use libfuzzer_sys::fuzz_target;
use thanatos_vmm::devices::rng::Rng;

fuzz_target!(|data: &[u8]| {
    thanatos_fuzz::drive(Rng::new().unwrap(), data, 1);
});
