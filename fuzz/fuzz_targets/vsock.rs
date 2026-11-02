#![no_main]
use libfuzzer_sys::fuzz_target;
use thanatos_vmm::devices::vsock::Vsock;

fuzz_target!(|data: &[u8]| {
    thanatos_fuzz::drive(Vsock::default(), data, 3);
});
