#![no_main]
use libfuzzer_sys::fuzz_target;
use thanatos_vmm::rfb::read_message;

fuzz_target!(|data: &[u8]| {
    let mut input = data;
    while read_message(&mut input).is_ok() {}
});
