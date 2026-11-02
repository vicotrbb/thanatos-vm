use std::process::Command;

#[test]
fn doctor_gives_a_verdict_and_says_what_to_fix() {
    let out = Command::new(env!("CARGO_BIN_EXE_thanatos"))
        .arg("doctor")
        .output()
        .unwrap();
    let all = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        assert!(all.contains("hypervisor: usable"), "{all}");
    } else {
        assert!(all.contains("unusable") && all.contains("fix:"), "{all}");
    }
}
