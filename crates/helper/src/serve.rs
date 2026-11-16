//! The helper's side: it locks itself down, then decodes the files the viewer passes in. On
//! Linux the lockdown is Landlock and seccomp; on macOS, Seatbelt with a deny-default profile.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, IoSliceMut, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::ptr;

use nsxiv_ng_decode::{DecodeError, Image, fontdb};
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};
use rustix::process::{Resource, Rlimit, setrlimit};

use crate::{IMAGE, MAX_TEXT, TOO_LARGE, UNREADABLE};
#[cfg(target_os = "macos")]
use std::ffi::{CStr, c_char, c_int};
#[cfg(target_os = "linux")]
use {
    crate::heif,
    landlock::{ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetStatus, Scope},
    nsxiv_ng_decode::Helped,
    rustix::net::{AddressFamily, SocketType},
    seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule, sock_filter,
    },
    std::collections::BTreeMap,
    std::os::unix::process::CommandExt,
};

/// Runs as a decoding helper on the socket its stdin is, with a memory cap of `cap` bytes,
/// which on Linux bounds its heap. macOS takes no cap on a process's memory, so there the
/// viewer's check of an answer's size and its timeout are what bound a helper. Without its
/// sandbox, every part of it, it decodes nothing: every file it gets answers
/// `no sandbox for decoding` and the reason.
pub fn serve(cap: u64) -> ExitCode {
    // first, while the helper holds no descriptor of its own beyond its standard three
    let closed = close_inherited();
    let Ok(socket) = io::stdin().as_fd().try_clone_to_owned() else {
        return ExitCode::FAILURE;
    };
    let mut socket = UnixStream::from(socket);
    // what a locked-down helper cannot open: fonts, and on Linux libheif with its codec plugins
    nsxiv_ng_decode::set_svg_fonts(mapped_fonts());
    #[cfg(target_os = "linux")]
    heif::load();
    let sandbox = closed.and_then(|()| lockdown(cap));
    if sandbox.is_ok() {
        end_with_viewer(&socket);
    }
    loop {
        match receive(&mut socket) {
            Ok(Some(request)) => {
                let locked = sandbox.as_ref().map(|_| ()).map_err(String::as_str);
                if send(&mut socket, &answer(locked, request, cap)).is_err() {
                    return ExitCode::FAILURE;
                }
            }
            // the viewer closed its end
            Ok(None) => return ExitCode::SUCCESS,
            Err(_) => return ExitCode::FAILURE,
        }
    }
}

// The answer to one request: a refusal, before a byte of the file is read, where the lockdown
// failed for the reason `locked` gives.
fn answer(
    locked: Result<(), &str>,
    #[cfg_attr(target_os = "macos", allow(unused_mut))] mut request: Request,
    cap: u64,
) -> Result<Image, DecodeError> {
    if let Err(why) = locked {
        return Err(DecodeError::Unreadable(format!(
            "no sandbox for decoding: {why}"
        )));
    }
    let path = Path::new(OsStr::from_bytes(&request.name));
    let max_bytes = request.max_bytes.min(cap);
    // macOS decodes HEIC and AVIF in the viewer; a file that became one after the viewer looked
    // reaches decode_file there, which refuses it, as it decodes no HEIF
    #[cfg(target_os = "linux")]
    if let Ok(Some(Helped::Heif)) = nsxiv_ng_decode::needs_helper(&mut request.file, path) {
        return heif::decode(request.file, max_bytes, request.side);
    }
    nsxiv_ng_decode::decode_file(request.file, path, max_bytes)
}

// Ends the helper once the viewer's end of `socket` closes, even in a decode that never
// finishes, which would otherwise run on to the CPU limit: a viewer that quits leaves the
// thread that holds a decoding helper as it is, and one killed leaves all of them. A thread
// of its own, started after the lockdown, as Landlock holds only the threads a locked-down
// thread starts; not PR_SET_PDEATHSIG, which follows the viewer's thread that started the
// helper, not the viewer.
#[cfg(target_os = "linux")]
fn end_with_viewer(socket: &UnixStream) {
    let Ok(socket) = socket.try_clone() else {
        return;
    };
    // a helper that cannot start the thread, as at the user's process limit, still decodes
    let _ = std::thread::Builder::new().spawn(move || {
        // no events asked for, so the main thread's data stays unread; a hang-up wakes it
        let mut watched = libc::pollfd {
            fd: socket.as_raw_fd(),
            events: 0,
            revents: 0,
        };
        loop {
            // SAFETY: one pollfd that lives on this thread's stack for the call
            let woke = unsafe { libc::poll(&mut watched, 1, -1) };
            // with no events asked for, only a hang-up, an error or a closed descriptor wakes it
            if woke > 0 {
                // SAFETY: ends the process with no destructors run, mid-decode as it may be
                unsafe { libc::_exit(0) };
            }
            if woke < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return;
            }
        }
    });
}

// Closes every descriptor from 3 up, before the helper opens any of its own.
#[cfg(target_os = "linux")]
fn close_inherited() -> Result<(), String> {
    // SAFETY: nothing in this process owns a descriptor from 3 up yet; any the viewer passed on
    // by mistake, as a C library's display connection, closes before the helper reads a file
    if unsafe { libc::syscall(libc::SYS_close_range, 3, u32::MAX, 0) } != 0 {
        // a kernel before 5.9 leaves them open, so no sandbox would hold
        return Err(format!("close_range: {}", io::Error::last_os_error()));
    }
    Ok(())
}

/// Locks down as a helper does, then tries what the sandbox must stop, and prints the status
/// line `-v` shows, as `sandbox: seccomp, landlock full; libheif 1.23.6`, or what got through.
#[cfg(target_os = "linux")]
pub fn probe() -> ExitCode {
    let libheif = heif::load().map_or("no libheif".to_owned(), |v| format!("libheif {v}"));
    let landlock = match close_inherited().and_then(|()| lockdown(1 << 30)) {
        Ok(status) => status,
        Err(why) => {
            println!("sandbox: none ({why})");
            return ExitCode::FAILURE;
        }
    };
    let mut through = Vec::new();
    if File::open("/etc/passwd").is_ok() {
        through.push("open");
    }
    // a fork of its own, as Command reports a child whose exec was refused as no child at all
    // SAFETY: the probe runs no other thread, and the child calls only _exit
    match unsafe { libc::fork() } {
        // SAFETY: _exit is async-signal-safe, so a forked child may call it
        0 => unsafe { libc::_exit(0) },
        -1 => {}
        child => {
            through.push("fork");
            // SAFETY: waits for the child just made, which ends at once
            unsafe { libc::waitpid(child, ptr::null_mut(), 0) };
        }
    }
    // in place: were exec allowed, this process would become true and print nothing at all
    let _ = Command::new("/bin/true").exec();
    // the socket itself: an unbound one could still send
    let udp = rustix::net::socket(AddressFamily::INET, SocketType::DGRAM, None);
    if udp.is_ok() {
        through.push("socket");
    }
    let mut params = rustix::io_uring::io_uring_params::default();
    // SAFETY: io_uring_setup only reads and fills the parameters it gets, which live here
    if unsafe { rustix::io_uring::io_uring_setup(1, &mut params) }.is_ok() {
        through.push("io_uring");
    }
    if !through.is_empty() {
        println!("sandbox: broken, {} got through", through.join(", "));
        return ExitCode::FAILURE;
    }
    let landlock = match landlock {
        RulesetStatus::FullyEnforced => "full",
        RulesetStatus::PartiallyEnforced => "partial",
        _ => "none",
    };
    println!("sandbox: seccomp, landlock {landlock}; {libheif}");
    ExitCode::SUCCESS
}

// The system's fonts, mapped for the helper's life, as it cannot open them once locked down.
fn mapped_fonts() -> fontdb::Database {
    let mut fonts = fontdb::Database::new();
    fonts.load_system_fonts();
    let faces: Vec<fontdb::ID> = fonts.faces().map(|face| face.id).collect();
    for face in faces {
        // SAFETY: a font file that changes on disk while mapped can crash only this helper,
        // which the viewer then replaces; nothing here writes to the mappings
        let _ = unsafe { fonts.make_shared_face_data(face) };
    }
    fonts
}

// Locks the process down for good: its limits, Landlock where the kernel has it, and the
// seccomp filter, which it needs. Returns how much of Landlock holds.
#[cfg(target_os = "linux")]
fn lockdown(cap: u64) -> Result<RulesetStatus, String> {
    // the heap and anonymous mappings; mapped fonts do not count
    let data = cap.saturating_mul(2).saturating_add(512 << 20);
    let limits = [
        (Resource::Data, data),
        // seconds over the helper's whole life, a backstop for the viewer's timeout
        (Resource::Cpu, 600),
        (Resource::Fsize, 0),
        (Resource::Core, 0),
        (Resource::Nofile, 64),
    ];
    for (resource, limit) in limits {
        let limit = Rlimit {
            current: Some(limit),
            maximum: Some(limit),
        };
        setrlimit(resource, limit).map_err(|e| format!("{resource:?}: {e}"))?;
    }
    // no file access at all, and no signals or abstract sockets beyond the helper itself
    let landlock = Ruleset::default()
        .handle_access(AccessFs::from_all(ABI::V9))
        .and_then(|ruleset| ruleset.scope(Scope::from_all(ABI::V9)))
        .and_then(|ruleset| ruleset.create())
        .and_then(|ruleset| ruleset.restrict_self())
        .map_or(RulesetStatus::NotEnforced, |status| status.ruleset);
    seccomp().map_err(|e| e.to_string())?;
    Ok(landlock)
}

/// fchmodat2's number on every architecture, which libc names on x86-64 alone.
#[cfg(target_os = "linux")]
const FCHMODAT2: i64 = 452;

// A denylist, as libheif's plugins make calls no allowlist could foresee. It refuses what a
// decoder has no use for and an attacker would want: opening, making and changing files,
// running programs, starting processes, networks, reaching into other processes, io_uring,
// mounts and namespaces, keys, shared memory past the data limit, typing into a terminal,
// and loading code into the kernel. Signals and limits reach only the helper itself.
#[cfg(target_os = "linux")]
fn seccomp() -> Result<(), seccompiler::Error> {
    #[rustfmt::skip]
    let denied = vec![
        // files: opening, making and changing them
        libc::SYS_openat, libc::SYS_openat2, libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at, libc::SYS_mknodat, libc::SYS_mkdirat, libc::SYS_unlinkat,
        libc::SYS_renameat, libc::SYS_renameat2, libc::SYS_linkat, libc::SYS_symlinkat,
        libc::SYS_fchmod, libc::SYS_fchmodat, FCHMODAT2, libc::SYS_fchown,
        libc::SYS_fchownat, libc::SYS_truncate, libc::SYS_ftruncate, libc::SYS_utimensat,
        libc::SYS_setxattr, libc::SYS_lsetxattr, libc::SYS_fsetxattr, libc::SYS_removexattr,
        libc::SYS_lremovexattr, libc::SYS_fremovexattr,
        // programs, and networks
        libc::SYS_execve, libc::SYS_execveat, libc::SYS_socket, libc::SYS_socketpair,
        libc::SYS_connect, libc::SYS_bind, libc::SYS_listen, libc::SYS_accept,
        libc::SYS_accept4,
        // other processes
        libc::SYS_ptrace, libc::SYS_process_vm_readv, libc::SYS_process_vm_writev,
        libc::SYS_process_madvise, libc::SYS_pidfd_open, libc::SYS_pidfd_getfd,
        libc::SYS_pidfd_send_signal, libc::SYS_tkill, libc::SYS_kcmp,
        libc::SYS_perf_event_open, libc::SYS_bpf, libc::SYS_userfaultfd,
        // io_uring, mounts and namespaces, keys
        libc::SYS_io_uring_setup, libc::SYS_io_uring_enter, libc::SYS_io_uring_register,
        libc::SYS_mount, libc::SYS_umount2, libc::SYS_pivot_root, libc::SYS_chroot,
        libc::SYS_unshare, libc::SYS_setns, libc::SYS_fsopen, libc::SYS_fsconfig,
        libc::SYS_fsmount, libc::SYS_fspick, libc::SYS_move_mount, libc::SYS_open_tree,
        libc::SYS_mount_setattr, libc::SYS_keyctl, libc::SYS_add_key, libc::SYS_request_key,
        // shared memory, which the data limit does not count and System V keeps after exit
        libc::SYS_memfd_create, libc::SYS_memfd_secret, libc::SYS_shmget, libc::SYS_shmat, libc::SYS_shmctl,
        libc::SYS_shmdt, libc::SYS_msgget, libc::SYS_msgsnd, libc::SYS_msgrcv,
        libc::SYS_msgctl, libc::SYS_semget, libc::SYS_semop, libc::SYS_semctl,
        libc::SYS_semtimedop,
        // the kernel itself
        libc::SYS_init_module, libc::SYS_finit_module, libc::SYS_delete_module,
        libc::SYS_kexec_load, libc::SYS_kexec_file_load, libc::SYS_reboot,
        libc::SYS_swapon, libc::SYS_swapoff, libc::SYS_acct,
    ];
    // and on x86-64 the calls of the same kinds older than the *at ones, which arm64 lacks
    #[cfg(target_arch = "x86_64")]
    #[rustfmt::skip]
    let denied = [denied, vec![
        libc::SYS_open, libc::SYS_creat, libc::SYS_fork, libc::SYS_vfork, libc::SYS_mknod,
        libc::SYS_mkdir, libc::SYS_rmdir, libc::SYS_unlink, libc::SYS_rename, libc::SYS_link,
        libc::SYS_symlink, libc::SYS_chmod, libc::SYS_chown, libc::SYS_lchown,
        libc::SYS_utime, libc::SYS_utimes, libc::SYS_futimesat,
    ]].concat();
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> =
        denied.into_iter().map(|call| (call, Vec::new())).collect();
    let when = |arg, len, op, value| SeccompCondition::new(arg, len, op, value);
    let me = u64::from(std::process::id());
    // clone may start threads, never processes, whose flags lack CLONE_THREAD
    let process = when(
        0,
        SeccompCmpArgLen::Qword,
        SeccompCmpOp::MaskedEq(libc::CLONE_THREAD as u64),
        0,
    )?;
    rules.insert(libc::SYS_clone, vec![SeccompRule::new(vec![process])?]);
    // signals for any process but this one, and limits set on another
    for call in [
        libc::SYS_kill,
        libc::SYS_tgkill,
        libc::SYS_rt_sigqueueinfo,
        libc::SYS_rt_tgsigqueueinfo,
    ] {
        let other = when(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, me)?;
        rules.insert(call, vec![SeccompRule::new(vec![other])?]);
    }
    let elsewhere = vec![
        when(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, 0)?,
        when(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, me)?,
    ];
    rules.insert(libc::SYS_prlimit64, vec![SeccompRule::new(elsewhere)?]);
    // ioctl does nothing a decoder needs and much an attacker would want, from typing into a
    // terminal to marking the passed file immutable, so it goes whole
    rules.insert(libc::SYS_ioctl, Vec::new());
    // signals to another process through a file's owner, which no signal call shows
    // F_SETOWN_EX and F_SETSIG, which libc leaves out, are 15 and 10 on both architectures
    let owner = [libc::F_SETOWN, 15, 10]
        .into_iter()
        .map(|cmd| {
            let cmd = when(1, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, cmd as u64)?;
            SeccompRule::new(vec![cmd])
        })
        .collect::<Result<Vec<_>, _>>()?;
    rules.insert(libc::SYS_fcntl, owner);
    // shared anonymous and grow-down mappings, which the data limit does not count
    let unlimited = [
        (libc::MAP_SHARED | libc::MAP_ANONYMOUS) as u64,
        libc::MAP_GROWSDOWN as u64,
    ]
    .into_iter()
    .map(|flags| {
        let flags = when(
            3,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::MaskedEq(flags),
            flags,
        )?;
        SeccompRule::new(vec![flags])
    })
    .collect::<Result<Vec<_>, _>>()?;
    rules.insert(libc::SYS_mmap, unlimited);
    let arch = std::env::consts::ARCH.try_into()?;
    let calls: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )?
    .try_into()?;
    // clone3 keeps its flags in memory a filter cannot read; ENOSYS sends glibc back to clone
    let clone3: BpfProgram = SeccompFilter::new(
        BTreeMap::from([(libc::SYS_clone3, Vec::new())]),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
        arch,
    )?
    .try_into()?;
    seccompiler::apply_filter_all_threads(&calls)?;
    seccompiler::apply_filter_all_threads(&clone3)?;
    refuse_unknown()
}

// Calls no rule above can name: x32 ones and ones newer than this filter. A call through
// the x32 ABI carries the x86-64 audit arch and a number with the x32 bit set, and x32
// numbers some calls apart, execve among them; a decoder has no use for x32, so EPERM. A
// call newer than this filter knows gets ENOSYS, as from an older kernel, which glibc falls
// back from; among the newest, the ones that change files or mounts are refused by number.
#[cfg(target_os = "linux")]
fn refuse_unknown() -> Result<(), seccompiler::Error> {
    // numbers every architecture shares: setxattrat, removexattrat, open_tree_attr and
    // file_setattr, too new for libc's constants; file_setattr, 469, is the newest known
    const CHANGING: [u32; 4] = [463, 466, 467, 469];
    const NEWEST: u32 = 469;
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xc000_003e; // AUDIT_ARCH_X86_64
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xc000_00b7; // AUDIT_ARCH_AARCH64
    const LOAD: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const EQUAL: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const ABOVE: u16 = 0x25; // BPF_JMP | BPF_JGT | BPF_K
    const AT_LEAST: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
    const RETURN: u16 = 0x06; // BPF_RET | BPF_K
    const ERRNO: u32 = 0x0005_0000; // SECCOMP_RET_ERRNO
    const ALLOW: u32 = 0x7fff_0000; // SECCOMP_RET_ALLOW
    let step = |code, jt, jf, k| sock_filter { code, jt, jf, k };
    let program = [
        // seccomp_data.arch: another one goes on to the allow at the end
        step(LOAD, 0, 0, 4),
        step(EQUAL, 0, 9, ARCH),
        // seccomp_data.nr
        step(LOAD, 0, 0, 0),
        // from __X32_SYSCALL_BIT up, and the newest calls that change files: EPERM
        step(AT_LEAST, 6, 0, 0x4000_0000),
        step(EQUAL, 5, 0, CHANGING[0]),
        step(EQUAL, 4, 0, CHANGING[1]),
        step(EQUAL, 3, 0, CHANGING[2]),
        step(EQUAL, 2, 0, CHANGING[3]),
        // newer than this filter: ENOSYS
        step(ABOVE, 0, 2, NEWEST),
        step(RETURN, 0, 0, ERRNO | libc::ENOSYS as u32),
        step(RETURN, 0, 0, ERRNO | libc::EPERM as u32),
        step(RETURN, 0, 0, ALLOW),
    ];
    seccompiler::apply_filter_all_threads(&program)
}

// libSystem's Seatbelt, which no header of the SDK still declares but `sandbox_init`, there
// marked deprecated since macOS 10.8; a profile string of the caller's own, with flags 0, is
// what it takes on macOS 27
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn sandbox_init(profile: *const c_char, flags: u64, errorbuf: *mut *mut c_char) -> c_int;
    fn sandbox_free_error(errorbuf: *mut c_char);
    static bootstrap_port: libc::mach_port_t;
    static mach_task_self_: libc::mach_port_t;
    fn bootstrap_look_up(
        port: libc::mach_port_t,
        name: *const c_char,
        service: *mut libc::mach_port_t,
    ) -> libc::kern_return_t;
}

// IOKit, for the probe's attempt to open a device's user client
#[cfg(target_os = "macos")]
#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut std::ffi::c_void;
    fn IOServiceGetMatchingService(
        main_port: libc::mach_port_t,
        matching: *mut std::ffi::c_void,
    ) -> libc::mach_port_t;
    fn IOServiceOpen(
        service: libc::mach_port_t,
        task: libc::mach_port_t,
        kind: u32,
        connect: *mut libc::mach_port_t,
    ) -> libc::kern_return_t;
}

/// The helper's Seatbelt profile: nothing allowed beyond what the kernel lets every process
/// do. A helper decodes with the fonts it mapped and the file it was passed.
#[cfg(target_os = "macos")]
const PROFILE: &CStr = c"(version 1)(deny default)";

// Closes every descriptor from 3 up, before the helper opens any of its own, from the list of
// its own descriptors the kernel gives, as macOS has no close_range.
#[cfg(target_os = "macos")]
fn close_inherited() -> Result<(), String> {
    let failed = || format!("PROC_PIDLISTFDS: {}", io::Error::last_os_error());
    let pid = std::process::id() as c_int;
    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    // SAFETY: with no buffer, proc_pidinfo gives the bytes the list takes and writes nothing
    let bytes = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Err(failed());
    }
    let empty = libc::proc_fdinfo {
        proc_fd: 0,
        proc_fdtype: 0,
    };
    // a list that fills the room it was given may have more: asked again with more room, a few
    // times, then given up, which ends in a refusal
    let mut fds = vec![empty; bytes as usize / entry + 1];
    let listed = (0..4).find_map(|_| {
        let room = (fds.len() * entry) as c_int;
        // SAFETY: the buffer holds `room` bytes of proc_fdinfo entries
        let bytes = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), room)
        };
        if bytes > 0 && bytes < room {
            return Some(Ok(bytes as usize / entry));
        }
        if bytes <= 0 {
            return Some(Err(failed()));
        }
        fds.resize(fds.len() * 2, empty);
        None
    });
    let listed =
        listed.unwrap_or_else(|| Err("PROC_PIDLISTFDS: the list kept growing".to_owned()))?;
    for fd in &fds[..listed] {
        if fd.proc_fd > 2 {
            // SAFETY: nothing in this process owns a descriptor from 3 up yet; any the viewer
            // passed on by mistake closes before the helper reads a file
            unsafe { libc::close(fd.proc_fd) };
        }
    }
    Ok(())
}

// Locks the process down for good: its limits, then the profile, which holds every thread,
// those started before as well. macOS takes no cap on a process's memory: it refuses any
// RLIMIT_DATA, RLIMIT_AS or RLIMIT_RSS below the address space a process already has.
#[cfg(target_os = "macos")]
fn lockdown(_cap: u64) -> Result<(), String> {
    let limits = [
        // seconds over the helper's whole life, a backstop for the viewer's timeout
        (Resource::Cpu, 600),
        (Resource::Fsize, 0),
        (Resource::Core, 0),
        (Resource::Nofile, 64),
    ];
    for (resource, limit) in limits {
        let limit = Rlimit {
            current: Some(limit),
            maximum: Some(limit),
        };
        setrlimit(resource, limit).map_err(|e| format!("{resource:?}: {e}"))?;
    }
    let mut error = ptr::null_mut();
    // SAFETY: a NUL-terminated profile, and a pointer libsandbox fills with its message
    if unsafe { sandbox_init(PROFILE.as_ptr(), 0, &mut error) } != 0 {
        // a profile that does not parse returns here too, and leaves no sandbox at all
        let why = if error.is_null() {
            io::Error::last_os_error().to_string()
        } else {
            // SAFETY: libsandbox's NUL-terminated message, freed once copied
            let why = unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned();
            // SAFETY: the message sandbox_init gave, freed once
            unsafe { sandbox_free_error(error) };
            why
        };
        // its first line; the rest is a backtrace into the profile
        return Err(format!(
            "sandbox_init: {}",
            why.lines().next().unwrap_or_default()
        ));
    }
    Ok(())
}

// Ends the helper once the viewer's end of `socket` closes, even in a decode that never
// finishes, which would otherwise run on to the CPU limit. poll with no events asked reports
// no hang-up of a UNIX stream socket on macOS, so a kqueue does: its read filter, cleared once
// reported, wakes once a request comes, which this leaves to the main thread to read, and
// marks the hang-up EV_EOF.
#[cfg(target_os = "macos")]
fn end_with_viewer(socket: &UnixStream) {
    let Ok(socket) = socket.try_clone() else {
        return;
    };
    // a helper that cannot start the thread, as at the user's process limit, still decodes
    let _ = std::thread::Builder::new().spawn(move || {
        // SAFETY: kqueue takes no argument and gives a new descriptor
        let queue = unsafe { libc::kqueue() };
        if queue < 0 {
            return;
        }
        let watch = libc::kevent {
            ident: socket.as_raw_fd() as usize,
            filter: libc::EVFILT_READ,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: 0,
            data: 0,
            udata: ptr::null_mut(),
        };
        // SAFETY: one change from this thread's stack, and no event list
        if unsafe { libc::kevent(queue, &watch, 1, ptr::null_mut(), 0, ptr::null()) } < 0 {
            return;
        }
        let mut got = watch;
        loop {
            // SAFETY: room for one event on this thread's stack, and no change
            let woke = unsafe { libc::kevent(queue, ptr::null(), 0, &mut got, 1, ptr::null()) };
            if woke > 0 && got.flags & libc::EV_EOF != 0 {
                // SAFETY: ends the process with no destructors run, mid-decode as it may be
                unsafe { libc::_exit(0) };
            }
            if woke < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return;
            }
        }
    });
}

/// Locks down as a helper does, then tries what the profile must stop, and prints the status
/// line `-v` shows, `sandbox: seatbelt`, or what got through. An attempt that cannot work on
/// this machine shows nothing: the window server's lookup and the signal to the process that
/// ran the probe, which need no sandbox to fail, as with nobody logged in or launchd for a
/// parent, are tried once before the lockdown and count only where they worked then.
#[cfg(target_os = "macos")]
pub fn probe() -> ExitCode {
    let (mach, signal) = (window_server(), signal_parent());
    if let Err(why) = close_inherited().and_then(|()| lockdown(1 << 30)) {
        println!("sandbox: none ({why})");
        return ExitCode::FAILURE;
    }
    // refused shows as EPERM; any other failure means the attempt reached what it asked for
    let refused = |e: io::Error| e.kind() == io::ErrorKind::PermissionDenied;
    let mut through = Vec::new();
    if File::open("/etc/passwd").is_ok() {
        through.push("read");
    }
    let written = std::env::temp_dir().join(format!("nsxiv-ng-probe-{}", std::process::id()));
    if File::create(&written).is_ok() {
        through.push("write");
        let _ = std::fs::remove_file(&written);
    }
    // SAFETY: the probe runs no other thread, and the child calls only _exit
    match unsafe { libc::fork() } {
        // SAFETY: _exit is async-signal-safe, so a forked child may call it
        0 => unsafe { libc::_exit(0) },
        -1 => {}
        child => {
            through.push("fork");
            // SAFETY: waits for the child just made, which ends at once
            unsafe { libc::waitpid(child, ptr::null_mut(), 0) };
        }
    }
    // posix_spawn, which Command uses here, answers a refused program with its error
    if Command::new("/usr/bin/true").status().is_ok() {
        through.push("exec");
    }
    // a short wait, as a firewall that drops it would hold an unsandboxed connect for long; a
    // connect that runs out of time reached the network
    let local = std::net::SocketAddr::from(([127, 0, 0, 1], 1));
    let tcp = std::net::TcpStream::connect_timeout(&local, std::time::Duration::from_millis(500));
    if !tcp.map_or_else(refused, |_| false) {
        through.push("tcp");
    }
    let unix = std::os::unix::net::UnixDatagram::unbound()
        .and_then(|socket| socket.connect("/var/run/syslog"));
    if !unix.map_or_else(refused, |()| false) {
        through.push("unix");
    }
    if mach && window_server() {
        through.push("mach");
    }
    // SAFETY: a matching dictionary IOKit makes, which the lookup then takes; the service, if
    // any, opened from this task
    let opened = unsafe {
        let service = IOServiceGetMatchingService(0, IOServiceMatching(c"IOSurfaceRoot".as_ptr()));
        let mut connect = 0;
        service != 0 && IOServiceOpen(service, mach_task_self_, 0, &mut connect) == 0
    };
    if opened {
        through.push("iokit");
    }
    let mut value: c_int = 0;
    let mut size = std::mem::size_of::<c_int>();
    // SAFETY: a NUL-terminated name, and room for the int it reads
    let read = unsafe {
        libc::sysctlbyname(
            c"kern.maxproc".as_ptr(),
            (&mut value as *mut c_int).cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if read == 0 {
        through.push("sysctl");
    }
    if signal && signal_parent() {
        through.push("signal");
    }
    if !through.is_empty() {
        println!("sandbox: broken, {} got through", through.join(", "));
        return ExitCode::FAILURE;
    }
    println!("sandbox: seatbelt");
    ExitCode::SUCCESS
}

// Whether a lookup of the window server answers.
#[cfg(target_os = "macos")]
fn window_server() -> bool {
    let mut port = 0;
    // SAFETY: the bootstrap port this process was given, a NUL-terminated name and an out port
    let looked = unsafe {
        bootstrap_look_up(
            bootstrap_port,
            c"com.apple.windowserver.active".as_ptr(),
            &mut port,
        )
    };
    looked == 0
}

// Whether a signal could go to the process that ran the probe.
#[cfg(target_os = "macos")]
fn signal_parent() -> bool {
    // SAFETY: signal 0 only checks that one could be sent
    unsafe { libc::kill(libc::getppid(), 0) == 0 }
}

// The next request: the file, its name and the cap; None once the viewer has gone.
struct Request {
    file: File,
    name: Vec<u8>,
    max_bytes: u64,
    // the side a thumbnail must cover, or 0 for the image itself; read for HEIF, which only
    // Linux's helpers decode
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    side: u32,
}

fn receive(socket: &mut UnixStream) -> io::Result<Option<Request>> {
    let mut head = [0; 16];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    // Darwin has no MSG_CMSG_CLOEXEC, so the flag is set once the descriptor is in
    #[cfg(target_os = "linux")]
    let flags = RecvFlags::CMSG_CLOEXEC;
    #[cfg(target_os = "macos")]
    let flags = RecvFlags::empty();
    let got = rustix::net::recvmsg(
        &*socket,
        &mut [IoSliceMut::new(&mut head)],
        &mut control,
        flags,
    )?
    .bytes;
    if got == 0 {
        return Ok(None);
    }
    // the first file passed counts; any more close as they drop
    let mut file = None;
    for message in control.drain() {
        if let RecvAncillaryMessage::ScmRights(fds) = message {
            for fd in fds {
                #[cfg(target_os = "macos")]
                rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
                file.get_or_insert(File::from(fd));
            }
        }
    }
    socket.read_exact(&mut head[got..])?;
    let max_bytes = u64::from_le_bytes(head[..8].try_into().unwrap());
    let side = u32::from_le_bytes(head[8..12].try_into().unwrap());
    let length = u32::from_le_bytes(head[12..].try_into().unwrap()) as usize;
    if length > MAX_TEXT {
        return Err(io::Error::other("a name too long"));
    }
    let mut name = vec![0; length];
    socket.read_exact(&mut name)?;
    let file = file.ok_or_else(|| io::Error::other("a request without a file"))?;
    Ok(Some(Request {
        file,
        name,
        max_bytes,
        side,
    }))
}

fn send(socket: &mut UnixStream, answer: &Result<Image, DecodeError>) -> io::Result<()> {
    let mut head = Vec::with_capacity(16);
    match answer {
        Ok(image) => {
            head.push(IMAGE);
            head.extend_from_slice(&image.width.to_le_bytes());
            head.extend_from_slice(&image.height.to_le_bytes());
            head.push(u8::from(image.may_animate));
            socket.write_all(&head)?;
            socket.write_all(&image.rgba)
        }
        Err(DecodeError::TooLarge { size }) => {
            let (width, height) = size.unwrap_or_default();
            head.push(TOO_LARGE);
            head.push(u8::from(size.is_some()));
            head.extend_from_slice(&width.to_le_bytes());
            head.extend_from_slice(&height.to_le_bytes());
            socket.write_all(&head)
        }
        // RAW never decodes in a helper, so this arm is there for the match
        Err(DecodeError::NoPreview) => {
            let why = DecodeError::NoPreview.to_string();
            head.push(UNREADABLE);
            head.extend_from_slice(&(why.len() as u32).to_le_bytes());
            head.extend_from_slice(why.as_bytes());
            socket.write_all(&head)
        }
        Err(DecodeError::Unreadable(why)) => {
            let why = &why[..why.floor_char_boundary(MAX_TEXT)];
            head.push(UNREADABLE);
            head.extend_from_slice(&(why.len() as u32).to_le_bytes());
            head.extend_from_slice(why.as_bytes());
            socket.write_all(&head)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::OwnedFd;

    /// The calls the filter test makes, by name and number on this architecture: each one that
    /// the filter lets through answers otherwise than EPERM, given null pointers and zeros.
    #[cfg(target_os = "linux")]
    const REFUSED: &[(&str, i64)] = &[
        ("openat", libc::SYS_openat),
        ("openat2", libc::SYS_openat2),
        ("renameat", libc::SYS_renameat),
        ("renameat2", libc::SYS_renameat2),
        ("fchmodat2", FCHMODAT2),
        ("mkdirat", libc::SYS_mkdirat),
        ("unlinkat", libc::SYS_unlinkat),
        ("linkat", libc::SYS_linkat),
        ("symlinkat", libc::SYS_symlinkat),
        ("execve", libc::SYS_execve),
        ("memfd_create", libc::SYS_memfd_create),
        ("memfd_secret", libc::SYS_memfd_secret),
        ("ioctl", libc::SYS_ioctl),
        ("io_uring_setup", libc::SYS_io_uring_setup),
        ("socket", libc::SYS_socket),
    ];

    /// The variable that makes this test's own run the child that applies the filter.
    #[cfg(target_os = "linux")]
    const CHILD: &str = "NSXIV_NG_SECCOMP_CHILD";

    #[cfg(target_os = "linux")]
    #[test]
    fn the_filter_refuses_each_call_it_names_on_this_architecture() {
        if std::env::var_os(CHILD).is_some() {
            let call = |number| {
                // SAFETY: null pointers and zeros, which the filter refuses before the kernel
                // reads them, and which the kernel refuses with EFAULT or EINVAL where it
                // does not; a descriptor one gives is closed below
                let result = unsafe { libc::syscall(number, 0, 0, 0, 0, 0, 0) };
                (result, io::Error::last_os_error().raw_os_error())
            };
            // each call before the filter: one refused with EPERM already is refused by what
            // surrounds this process, a container's filter or a sysctl, as Docker's refuses
            // io_uring_setup, whose answer cannot be told from the filter's: it is left out,
            // and said so
            let mut checked = Vec::new();
            for &(name, number) in REFUSED {
                let (result, errno) = call(number);
                if result > 2 {
                    // a descriptor, as memfd_secret gives
                    // SAFETY: the descriptor the call just made, which nothing else holds
                    unsafe { libc::close(result as libc::c_int) };
                }
                if result == -1 && errno == Some(libc::EPERM) {
                    println!(
                        "{name} ({number}) left unchecked: refused with EPERM before the \
                         filter, by the container or the system this test runs in"
                    );
                } else {
                    checked.push((name, number));
                }
            }
            assert!(
                !checked.is_empty(),
                "every call was refused before the filter"
            );
            // the filter, then each call left, which must meet EPERM
            seccomp().unwrap();
            let mut through = Vec::new();
            for (name, number) in checked {
                let (result, errno) = call(number);
                if result != -1 || errno != Some(libc::EPERM) {
                    through.push(format!("{name} ({number}): {result}, {errno:?}"));
                }
            }
            assert!(through.is_empty(), "got through: {through:?}");
            return;
        }
        // in a process of its own, as a filter cannot be taken off again
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "serve::tests::the_filter_refuses_each_call_it_names_on_this_architecture",
            ])
            // its lines on stdout whether it passes or not, for the calls it left unchecked
            .args(["--test-threads=1", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        // the calls the child could not check, said again where this test's output shows
        for line in stdout
            .lines()
            .filter(|line| line.contains("left unchecked"))
        {
            println!("{line}");
        }
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn without_a_sandbox_a_request_is_refused_with_no_byte_of_its_file_read() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>"#;
        let (reader, mut writer) = io::pipe().unwrap();
        writer.write_all(svg).unwrap();
        drop(writer);
        let mut kept = reader.try_clone().unwrap();
        let request = Request {
            file: File::from(OwnedFd::from(reader)),
            name: b"a.svg".to_vec(),
            max_bytes: 1 << 20,
            side: 0,
        };
        let refused = answer(Err("close_range: ENOSYS"), request, 1 << 20);
        assert_eq!(
            refused.map(|image| image.width),
            Err(DecodeError::Unreadable(
                "no sandbox for decoding: close_range: ENOSYS".to_owned()
            ))
        );
        let mut left = Vec::new();
        kept.read_to_end(&mut left).unwrap();
        assert_eq!(left, svg, "the file's bytes are all still there");
    }
}
