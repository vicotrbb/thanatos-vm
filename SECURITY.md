# Security

## Reporting

Report vulnerabilities through [GitHub private vulnerability reporting](https://github.com/vicotrbb/thanatos-vm/security/advisories/new), not a public issue.

## What Thanatos defends against

The guest is untrusted. Device emulation is written in safe Rust, with `unsafe` confined to `crates/hv`, and the guest cannot reach the host's services unless a network rule allows it.

If a device bug is exploited anyway, the sandbox limits the damage:

| | Linux | macOS |
|---|---|---|
| Mechanism | Landlock and a seccomp deny list | Seatbelt, through the deprecated `sandbox_init` |
| Start programs | Blocked; the process is killed | Blocked |
| Read your files | Blocked by Landlock, except what the VM needs | Blocked under `/Users`, `/Volumes` and temporary folders |
| Write files | Blocked by Landlock, except the API socket's directory | Blocked, except the API socket's directory |

## What it does not defend against

- **Network:** the VMM can open outbound connections, so code running inside a compromised VMM can bypass `[network]` policy.
- **Open files:** a compromised VMM keeps its disks, including their contents.
- **Hypervisor or kernel bugs:** KVM, Hypervisor.framework and the host kernel are trusted.
- **Local access as your user:** the API socket and the running process are yours. Anything running as you can control the VM.
- **Side channels between guest and host.**

## Known limits

- **Linux windows:** Landlock covers the VM's threads, not the main thread that draws the window. Those threads share memory, so an attacker who controls a device thread could reach it. Use `--display headless` or `vnc` for untrusted guests.
- **Older kernels and containers:** without Landlock, the VMM warns and files are not confined. The seccomp filter still applies.
- **Seccomp:** it is a deny list, because the window's libraries share the process.
- **macOS metadata:** file names and sizes stay readable; only contents are blocked.
- **VNC:** its authentication uses DES and traffic is not encrypted. Tunnel it over SSH.
- **Crash consistency:** `just crash` kills the VMM under write load on 4 KiB- and 1 KiB-block ext4 disks. A process kill keeps the host's cache, so it does not stand in for a power cut. The image's kernel carries a backport of upstream ext4 commit `ded2d726` (`image/kernel/ext4-partial-folio.patch`): without it, Linux 6.12 can leave a disk with blocks smaller than a page in a state `e2fsck -p` refuses after a crash, if ext4 runs out of free blocks mid-page. Other guest kernels based on 6.12 need the same fix.

## Status

Thanatos has had no independent security review.
