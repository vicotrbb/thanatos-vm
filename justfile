bin := "target/release/thanatos"
default_arch := if arch() == "aarch64" { "aarch64" } else { "x86_64" }

check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

# Boots the current image on this host and shows the serial console.
boot:
    cargo test --release -p thanatos-cli --test boot -- --ignored --nocapture

image arch=default_arch profile="minimal": _build
    {{bin}} image build --arch {{arch}} --profile {{profile}}

_build:
    cargo build --release -p thanatos-cli
    {{ if os() == "macos" { bin + " doctor --sign" } else { "true" } }}
