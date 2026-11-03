bin := "target/release/thanatos"
default_arch := if arch() == "aarch64" { "aarch64" } else { "x86_64" }

check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

# Boots the current desktop-profile image on this host: agent, network, screen and input.
boot:
    cargo test --release -p thanatos-cli --test boot --test desktop -- --ignored --nocapture

# Kills the VMM repeatedly under write load and checks that flushed data survives.
crash:
    cargo test --release -p thanatos-cli --test crash -- --ignored --nocapture --test-threads=1

image arch=default_arch profile="desktop": _build
    {{bin}} image build --arch {{arch}} --profile {{profile}}

_build:
    cargo build --release -p thanatos-cli
    {{ if os() == "macos" { bin + " doctor --sign" } else { "true" } }}

# Runs each device and parser fuzzer for a minute.
fuzz:
    #!/bin/sh
    set -eu
    cd fuzz
    for target in $(cargo +nightly fuzz list); do
        cargo +nightly fuzz run "$target" -- -max_total_time=60
    done
