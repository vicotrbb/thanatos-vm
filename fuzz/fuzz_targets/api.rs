#![no_main]
use libfuzzer_sys::fuzz_target;
use thanatos_vmm::api::{AgentFrame, Request, read_frame};

fuzz_target!(|data: &[u8]| {
    let _ = serde_json::from_slice::<Request>(data);
    let _ = read_frame::<AgentFrame>(&mut { data });
});
