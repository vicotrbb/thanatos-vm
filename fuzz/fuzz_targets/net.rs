#![no_main]
use libfuzzer_sys::fuzz_target;
use thanatos_vmm::devices::net::Net;

fuzz_target!(|data: &[u8]| {
    thanatos_fuzz::drive(Net::new([2, 0, 0, 0, 0, 1], drop), data, 2);
});
