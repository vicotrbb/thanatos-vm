Invoke the ponytail skill at level `full` (`/ponytail:ponytail full`) before anything else and keep it active for the whole session: the smallest change that fully works, nothing speculative, every non-trivial piece gets one small test.

# Build Thanatos VM

You are implementing Thanatos, a standalone Rust virtual machine monitor, from the design in `DESIGN.md` in this repository. Read `DESIGN.md` in full before doing anything. It is the specification: intent, goals, non-goals, platform matrix, architecture, crates, devices, networking, API, guest images, security model, testing, and the order of work in section 10. Do not redesign it. Where it says "(my call)" the default stands unless it proves wrong in practice; if it does, change it, say why in a dated one-line amendment in `DESIGN.md`, and move on.

The open questions in section 12 are resolved as follows:

1. No emulation fallback, ever. Keep the hypervisor trait as small as the three real backends need; do not shape it for a hypothetical interpreter.
2. License: Apache-2.0 OR MIT. Add both license files and the SPDX line in `Cargo.toml`.
3. Debian trixie is the default and the only tested image. Alpine is not built.
4. Windows is 1.1, out of scope for this session. Leave the `whp` module absent, not stubbed.
5. No Apple Developer ID yet. `thanatos doctor --sign` ad-hoc signs the local build with the hypervisor entitlement; that is the macOS install path for now.

## Deliverable

Everything in section 10, steps 1 through 9, ending with a tagged `v1.0.0` that boots the Debian image on Linux x86_64 KVM, Linux aarch64 KVM and macOS arm64 Hypervisor.framework, with every device, the user-mode network, the screen, the agent, the API and the CLI working as section 4 describes, and the tests in section 8 green.

## How to work

- First turn `DESIGN.md` section 10 into a plan with `superpowers:writing-plans`: one task per step, each ending in something that runs and the one test that proves it. Then execute the plan step by step. Use `superpowers:test-driven-development` for every device and parser: write the failing test against the fake guest memory first, then the code.
- One step at a time, in order. A step is done when its test passes on the hardware you have, `just check` is green, and it is committed. Then the next step. Never start step N+1 with step N red.
- Verify before you claim. "Boots" means you ran it and saw the login prompt over the serial console. If you cannot run a platform here (this machine is an Apple M5 Pro on macOS 26 with Hypervisor.framework; it has no KVM), say so in the commit message and let CI prove it. GitHub's Linux runners expose `/dev/kvm`.
- Commit after every green step with a short message that says what now works. Do not squash the history.
- When something only the user could decide comes up and the design does not answer it, pick the simplest option that fully works, write one line under "Builder's calls" at the end of `DESIGN.md`, and continue. Do not stop to ask.

## Engineering rules

These are the whole quality bar. Nothing else is required, and nothing heavier is wanted.

- **Gates: exactly three.** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`. A `justfile` with `check` (all three), `boot` (boots the current image on this host), `image` (builds the guest image), `fuzz` (runs the device fuzzers for a minute). CI is one GitHub Actions workflow running `just check` on Linux and macOS and `just boot` on Linux. No coverage thresholds, no complexity tools, no pre-commit hooks, no release automation beyond `cargo build --release` and the ad-hoc signing on macOS.
- **Dependencies: only those in `DESIGN.md` section 11.** If a task seems to need another crate, it needs a smaller solution instead. `std` errors: one `Error` enum per crate where a caller must distinguish cases, `io::Error::other` where it need not. No `thiserror`, `anyhow`, `tokio`, `clap`; argument parsing is a `match` on `std::env::args`.
- **Modules deep, interfaces small.** Each device is one file that implements the one device trait and exposes nothing else. Each hypervisor backend is one module that implements the three traits and exposes nothing else. If two callers need the same thing, write a function; if one does, inline it. No abstraction before its second use.
- **`unsafe` only in `thanatos-hv`**, with `unsafe_code = "forbid"` in every other crate and `deny` with per-module `allow` in `hv`. Every `unsafe` block gets a one-line comment naming the invariant it relies on. Guest memory is only ever touched through `vm-memory`'s checked accessors.
- **No options nobody asked for.** The configuration has the fields in `DESIGN.md` 4.12 and no more. The API has the requests in 4.9 and no more. No feature flags except `window`. No plugin seams, no trait objects where an enum of three will do, no builder patterns.
- **Tests are small and real.** One unit test per device against the fake queue, one per boot loader against a golden blob, one boot test per platform. A test that takes more than a few seconds is a boot test and lives in `tests/`, not in a unit test. No mocks of the hypervisor; the trait's fake is the boot test on real hardware.
- **Comments say why, never what.** A shortcut with a known limit is marked `shortcut: <limit>, <when to upgrade>` on one line so it can be found later.
- **Documentation is the README and the API types.** The README shows install, `doctor`, `image build`, `run`, `exec`, `screenshot` and `vnc` in that order, with the exact commands. Every API request type has a doc comment; the JSON schema is generated from them in a test and checked in.
- **Delete before you add.** When a step makes earlier code unnecessary, remove it in the same commit.

## What done looks like

From a clean checkout on a Mac with Docker and on a Linux x86_64 box with `/dev/kvm`:

```sh
just check
just image            # ~/.thanatos/images/<arch>/{Image|bzImage,root.ext4}
thanatos doctor       # says the hypervisor is usable, or exactly what to fix
thanatos vm create dev
thanatos run ~/.thanatos/vms/dev/vm.toml &
thanatos exec dev -- uname -a
thanatos screenshot dev > shot.png
thanatos display dev vnc   # then any VNC client on 127.0.0.1:5901 shows sway
```

Each command does what `DESIGN.md` says and nothing more, the repository has no file that a reader cannot explain the purpose of in one sentence, and the whole thing is something a careful engineer would be proud to have written in as few lines as it took.
