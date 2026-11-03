//! Confines this process once the VM's disks and kernel are open, so a guest escape through a
//! device bug cannot read the user's files or start programs. Open descriptors keep working.
use std::{io, path::PathBuf};

/// What the running VMM may still open by path.
#[derive(Debug, Default)]
pub struct Paths {
    /// Directories it may read and change, such as where the API socket lives.
    pub directories: Vec<PathBuf>,
    /// Devices it may open for reading and writing.
    pub devices: Vec<PathBuf>,
    /// Files it may read.
    pub read: Vec<PathBuf>,
}

/// Paths that do not exist are skipped: there is nothing to allow.
pub fn enter(paths: &Paths) -> io::Result<()> {
    platform::enter(paths)
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::{
        ffi::{CStr, CString, c_char, c_int},
        ptr,
    };

    unsafe extern "C" {
        fn sandbox_init(profile: *const c_char, flags: u64, error: *mut *mut c_char) -> c_int;
        fn sandbox_free_error(error: *mut c_char);
    }

    /// Seatbelt uses real paths, so `/tmp` must be spelled `/private/tmp`.
    fn quoted(path: &std::path::Path) -> Option<String> {
        let path = path.canonicalize().ok()?;
        let text = path.to_str()?.replace('\\', "\\\\").replace('"', "\\\"");
        Some(format!("\"{text}\""))
    }

    pub(super) fn profile(paths: &Paths) -> String {
        // The last matching rule wins. Allowing by default keeps the hypervisor and the window
        // working; what matters to an attacker is denied below.
        let mut text = String::from(
            "(version 1)(allow default)(deny process-exec*)(deny process-fork)\
             (deny file-write* (subpath \"/\"))\
             (allow file-write* (literal \"/dev/null\") (literal \"/dev/dtracehelper\"))\
             (deny file-read* (subpath \"/Users\") (subpath \"/Volumes\") \
             (subpath \"/private/tmp\") (subpath \"/private/var/root\") \
             (subpath \"/private/var/folders\"))\
             (allow file-read-metadata)",
        );
        for path in paths.directories.iter().filter_map(|p| quoted(p)) {
            text += &format!("(allow file-read* file-write* (subpath {path}))");
        }
        for path in paths.devices.iter().filter_map(|p| quoted(p)) {
            text += &format!("(allow file-read* file-write* (literal {path}))");
        }
        for path in paths.read.iter().filter_map(|p| quoted(p)) {
            text += &format!("(allow file-read* (literal {path}))");
        }
        text
    }

    pub(super) fn enter(paths: &Paths) -> io::Result<()> {
        let profile = CString::new(profile(paths)).map_err(io::Error::other)?;
        let mut error = ptr::null_mut();
        // SAFETY: the profile is a live C string; on failure `error` is a C string we must free.
        if unsafe { sandbox_init(profile.as_ptr(), 0, &mut error) } == 0 {
            return Ok(());
        }
        // SAFETY: as above, and it is freed once, after the last read.
        let message = unsafe {
            let message = CStr::from_ptr(error).to_string_lossy().into_owned();
            sandbox_free_error(error);
            message
        };
        Err(io::Error::other(format!("sandbox: {message}")))
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::os::{
        fd::AsRawFd,
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    };

    const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: libc::c_int = 1;
    const FS_EXECUTE: u64 = 1 << 0;
    const FS_WRITE: u64 = 1 << 1;
    const FS_READ: u64 = 1 << 2;
    const FS_ALL_V1: u64 = (1 << 13) - 1;
    const FS_REFER: u64 = 1 << 13;
    const FS_TRUNCATE: u64 = 1 << 14;

    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
        handled_access_net: u64,
    }

    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }

    /// Everything this kernel's Landlock can restrict on files, or `None` without Landlock.
    fn handled() -> Option<u64> {
        // SAFETY: the version query takes a null attribute and a zero size by definition.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        match abi {
            ..=0 => None,
            1 => Some(FS_ALL_V1),
            2 => Some(FS_ALL_V1 | FS_REFER),
            _ => Some(FS_ALL_V1 | FS_REFER | FS_TRUNCATE),
        }
    }

    fn rule(ruleset: libc::c_long, path: &std::path::Path, access: u64) -> io::Result<()> {
        let Ok(opened) = std::fs::File::options()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
            .open(path)
        else {
            return Ok(());
        };
        let attr = PathBeneathAttr {
            allowed_access: access,
            parent_fd: opened.as_raw_fd(),
        };
        // SAFETY: the attribute is a live struct of the kernel's layout for this rule type.
        let result = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset,
                RULE_PATH_BENEATH,
                &raw const attr,
                0u32,
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            let shown = String::from_utf8_lossy(path.as_os_str().as_bytes()).into_owned();
            return Err(io::Error::other(format!("sandbox rule {shown}: {error}")));
        }
        Ok(())
    }

    /// Landlock binds this thread and those it starts, which is why it runs before the VM's.
    fn landlock(paths: &Paths) -> io::Result<bool> {
        let Some(handled) = handled() else {
            return Ok(false);
        };
        let attr = RulesetAttr {
            handled_access_fs: handled,
            handled_access_net: 0,
        };
        // SAFETY: the attribute is a live struct and its exact size is passed.
        let ruleset = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &raw const attr,
                std::mem::size_of::<RulesetAttr>(),
                0u32,
            )
        };
        if ruleset < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = FS_WRITE | FS_READ | (handled & FS_TRUNCATE);
        let added = (|| {
            for path in &paths.directories {
                rule(ruleset, path, handled & !FS_EXECUTE)?;
            }
            for path in &paths.devices {
                rule(ruleset, path, file)?;
            }
            for path in &paths.read {
                rule(ruleset, path, FS_READ)?;
            }
            // SAFETY: restrict_self takes a ruleset descriptor and no pointers.
            match unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) } {
                0 => Ok(()),
                _ => Err(io::Error::last_os_error()),
            }
        })();
        // SAFETY: the descriptor came from the kernel above and is closed once.
        unsafe { libc::close(ruleset as libc::c_int) };
        added.map(|()| true)
    }

    const fn statement(code: u16, jt: u8, jf: u8, k: u32) -> libc::sock_filter {
        libc::sock_filter { code, jt, jf, k }
    }

    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xc000_00b7;

    /// shortcut: a deny list rather than Firecracker's allow list, because the window's libraries
    /// share this process and their calls vary by desktop; move to an allow list for headless VMs
    /// once it can be tested across kernels and C libraries.
    fn denied() -> Vec<libc::c_long> {
        #[cfg_attr(target_arch = "aarch64", allow(unused_mut))]
        let mut list = vec![
            libc::SYS_execve,
            libc::SYS_execveat,
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
            libc::SYS_process_madvise,
            libc::SYS_pidfd_getfd,
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_pivot_root,
            libc::SYS_chroot,
            libc::SYS_setns,
            libc::SYS_unshare,
            libc::SYS_init_module,
            libc::SYS_finit_module,
            libc::SYS_delete_module,
            libc::SYS_kexec_load,
            libc::SYS_bpf,
            libc::SYS_perf_event_open,
            libc::SYS_userfaultfd,
            libc::SYS_keyctl,
            libc::SYS_add_key,
            libc::SYS_request_key,
            libc::SYS_io_uring_setup,
            libc::SYS_open_by_handle_at,
        ];
        #[cfg(target_arch = "x86_64")]
        list.extend([libc::SYS_fork, libc::SYS_vfork]);
        list
    }

    fn filter() -> Vec<libc::sock_filter> {
        const LOAD: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
        const EQUAL: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
        const SET: u16 = (libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K) as u16;
        const AT_LEAST: u16 = (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16;
        const RETURN: u16 = (libc::BPF_RET | libc::BPF_K) as u16;
        const ALLOW: u32 = 0x7fff_0000;
        const KILL: u32 = 0x8000_0000;
        const ENOSYS: u32 = 0x0005_0000 | libc::ENOSYS as u32;
        const CLONE_THREAD: u32 = libc::CLONE_THREAD as u32;
        const X32_BIT: u32 = 0x4000_0000;

        let mut program = vec![
            statement(LOAD, 0, 0, 4), // arch
            statement(EQUAL, 1, 0, AUDIT_ARCH),
            statement(RETURN, 0, 0, KILL),
            statement(LOAD, 0, 0, 0), // syscall number
            statement(AT_LEAST, 0, 1, X32_BIT),
            statement(RETURN, 0, 0, KILL),
            // clone3 hides its flags behind a pointer, so C libraries are sent back to clone.
            statement(EQUAL, 0, 1, libc::SYS_clone3 as u32),
            statement(RETURN, 0, 0, ENOSYS),
            // Threads only: a new process would escape nothing but is an attacker's first step.
            statement(EQUAL, 0, 3, libc::SYS_clone as u32),
            statement(LOAD, 0, 0, 16), // low half of the first argument, the flags
            statement(SET, 1, 0, CLONE_THREAD),
            statement(RETURN, 0, 0, KILL),
        ];
        // After a thread's clone the accumulator holds flags, so reload the number.
        program.push(statement(LOAD, 0, 0, 0));
        for call in denied() {
            program.push(statement(EQUAL, 0, 1, call as u32));
            program.push(statement(RETURN, 0, 0, KILL));
        }
        program.push(statement(RETURN, 0, 0, ALLOW));
        program
    }

    fn seccomp() -> io::Result<()> {
        const SECCOMP_SET_MODE_FILTER: libc::c_uint = 1;
        const SECCOMP_FILTER_FLAG_TSYNC: libc::c_ulong = 1;
        let mut program = filter();
        let fprog = libc::sock_fprog {
            len: program.len() as libc::c_ushort,
            filter: program.as_mut_ptr(),
        };
        // SAFETY: the program outlives the call, which copies it; TSYNC covers existing threads.
        let result = unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                SECCOMP_SET_MODE_FILTER,
                SECCOMP_FILTER_FLAG_TSYNC,
                &raw const fprog,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn enter(paths: &Paths) -> io::Result<()> {
        // SAFETY: prctl with these constant arguments touches no memory.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // A kernel or container without Landlock still gets the system call filter.
        match landlock(paths) {
            Ok(true) => (),
            Ok(false) => eprintln!("sandbox: this kernel has no Landlock, files are not confined"),
            Err(error) => eprintln!("sandbox: files are not confined: {error}"),
        }
        seccomp()
    }
}
