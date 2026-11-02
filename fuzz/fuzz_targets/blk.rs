#![no_main]
use libfuzzer_sys::fuzz_target;
use std::{io::Write, sync::OnceLock};
use thanatos_vmm::devices::blk::Blk;

fn disk() -> std::fs::File {
    static PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
    let path = PATH.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("thanatos-fuzz-{}", std::process::id()));
        std::fs::File::create(&path).unwrap().write_all(&[0; 1 << 16]).unwrap();
        path
    });
    std::fs::File::options().read(true).write(true).open(path).unwrap()
}

fuzz_target!(|data: &[u8]| {
    thanatos_fuzz::drive(Blk::new(disk(), false, "fuzz").unwrap(), data, 1);
});
