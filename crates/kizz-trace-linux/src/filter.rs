// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The seccomp-bpf filter in front of `ptrace`.
//!
//! The filter decides, inside the kernel and before each system call runs, whether
//! the tracer needs to see it:
//!
//! * a system call through a non-native interface, the 32-bit `int 0x80` interface or
//!   x32, stops for the tracer, which marks the run as incompletely traced;
//! * `io_uring_setup` fails with `ENOSYS`, so programs fall back to ordinary system
//!   calls, because `io_uring` operations would bypass the filter;
//! * each system call in [`TRACKED_SYSTEM_CALLS`] stops for the tracer;
//! * every other system call runs at full speed.

use std::num::TryFromIntError;

use libc::{c_long, sock_filter};

/// A system call that touches the file system, which the filter stops for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackedSystemCall {
    /// The system call's `x86_64` name.
    pub name: &'static str,
    /// The system call's `x86_64` number.
    pub number: c_long,
}

impl TrackedSystemCall {
    /// A tracked system call with its `x86_64` name and number.
    const fn new(
        name: &'static str,
        number: c_long,
    ) -> Self {
        Self { name, number }
    }
}

/// The `x86_64` numbers of the tracked system calls that the `libc` crate does not
/// define, from the kernel's `x86_64` table, `arch/x86/entry/syscalls/syscall_64.tbl`.
pub(crate) mod numbers {
    use libc::c_long;

    /// `fchmodat2`, number 452 in the kernel's `x86_64` table.
    pub(crate) const SYS_FCHMODAT2: c_long = 452;
    /// `setxattrat`, number 463 in the kernel's `x86_64` table.
    pub(crate) const SYS_SETXATTRAT: c_long = 463;
    /// `getxattrat`, number 464 in the kernel's `x86_64` table.
    pub(crate) const SYS_GETXATTRAT: c_long = 464;
    /// `listxattrat`, number 465 in the kernel's `x86_64` table.
    pub(crate) const SYS_LISTXATTRAT: c_long = 465;
    /// `removexattrat`, number 466 in the kernel's `x86_64` table.
    pub(crate) const SYS_REMOVEXATTRAT: c_long = 466;
    /// `file_getattr`, number 468 in the kernel's `x86_64` table.
    pub(crate) const SYS_FILE_GETATTR: c_long = 468;
    /// `file_setattr`, number 469 in the kernel's `x86_64` table.
    pub(crate) const SYS_FILE_SETATTR: c_long = 469;
}

/// Every system call the filter stops for, in the order of the spec's list:
/// opening files, metadata and existence, file systems, watches, extended
/// attributes, links, directory listings, programs, the working directory, changes,
/// and changes through a descriptor.
pub const TRACKED_SYSTEM_CALLS: &[TrackedSystemCall] = &[
    // Opening files.
    TrackedSystemCall::new("open", libc::SYS_open),
    TrackedSystemCall::new("openat", libc::SYS_openat),
    TrackedSystemCall::new("openat2", libc::SYS_openat2),
    TrackedSystemCall::new("creat", libc::SYS_creat),
    // Metadata and existence.
    TrackedSystemCall::new("stat", libc::SYS_stat),
    TrackedSystemCall::new("lstat", libc::SYS_lstat),
    TrackedSystemCall::new("newfstatat", libc::SYS_newfstatat),
    TrackedSystemCall::new("statx", libc::SYS_statx),
    TrackedSystemCall::new("access", libc::SYS_access),
    TrackedSystemCall::new("faccessat", libc::SYS_faccessat),
    TrackedSystemCall::new("faccessat2", libc::SYS_faccessat2),
    TrackedSystemCall::new("file_getattr", numbers::SYS_FILE_GETATTR),
    TrackedSystemCall::new("name_to_handle_at", libc::SYS_name_to_handle_at),
    // File systems.
    TrackedSystemCall::new("statfs", libc::SYS_statfs),
    TrackedSystemCall::new("fstatfs", libc::SYS_fstatfs),
    // Watches.
    TrackedSystemCall::new("inotify_add_watch", libc::SYS_inotify_add_watch),
    TrackedSystemCall::new("fanotify_mark", libc::SYS_fanotify_mark),
    // Extended attributes.
    TrackedSystemCall::new("getxattr", libc::SYS_getxattr),
    TrackedSystemCall::new("lgetxattr", libc::SYS_lgetxattr),
    TrackedSystemCall::new("fgetxattr", libc::SYS_fgetxattr),
    TrackedSystemCall::new("getxattrat", numbers::SYS_GETXATTRAT),
    TrackedSystemCall::new("listxattr", libc::SYS_listxattr),
    TrackedSystemCall::new("llistxattr", libc::SYS_llistxattr),
    TrackedSystemCall::new("flistxattr", libc::SYS_flistxattr),
    TrackedSystemCall::new("listxattrat", numbers::SYS_LISTXATTRAT),
    TrackedSystemCall::new("setxattr", libc::SYS_setxattr),
    TrackedSystemCall::new("lsetxattr", libc::SYS_lsetxattr),
    TrackedSystemCall::new("fsetxattr", libc::SYS_fsetxattr),
    TrackedSystemCall::new("setxattrat", numbers::SYS_SETXATTRAT),
    TrackedSystemCall::new("removexattr", libc::SYS_removexattr),
    TrackedSystemCall::new("lremovexattr", libc::SYS_lremovexattr),
    TrackedSystemCall::new("fremovexattr", libc::SYS_fremovexattr),
    TrackedSystemCall::new("removexattrat", numbers::SYS_REMOVEXATTRAT),
    // Links.
    TrackedSystemCall::new("readlink", libc::SYS_readlink),
    TrackedSystemCall::new("readlinkat", libc::SYS_readlinkat),
    TrackedSystemCall::new("link", libc::SYS_link),
    TrackedSystemCall::new("linkat", libc::SYS_linkat),
    TrackedSystemCall::new("symlink", libc::SYS_symlink),
    TrackedSystemCall::new("symlinkat", libc::SYS_symlinkat),
    // Directory listings.
    TrackedSystemCall::new("getdents", libc::SYS_getdents),
    TrackedSystemCall::new("getdents64", libc::SYS_getdents64),
    // Programs.
    TrackedSystemCall::new("execve", libc::SYS_execve),
    TrackedSystemCall::new("execveat", libc::SYS_execveat),
    // The working directory.
    TrackedSystemCall::new("chdir", libc::SYS_chdir),
    TrackedSystemCall::new("fchdir", libc::SYS_fchdir),
    // Changes.
    TrackedSystemCall::new("rename", libc::SYS_rename),
    TrackedSystemCall::new("renameat", libc::SYS_renameat),
    TrackedSystemCall::new("renameat2", libc::SYS_renameat2),
    TrackedSystemCall::new("unlink", libc::SYS_unlink),
    TrackedSystemCall::new("unlinkat", libc::SYS_unlinkat),
    TrackedSystemCall::new("rmdir", libc::SYS_rmdir),
    TrackedSystemCall::new("mkdir", libc::SYS_mkdir),
    TrackedSystemCall::new("mkdirat", libc::SYS_mkdirat),
    TrackedSystemCall::new("mknod", libc::SYS_mknod),
    TrackedSystemCall::new("mknodat", libc::SYS_mknodat),
    TrackedSystemCall::new("truncate", libc::SYS_truncate),
    TrackedSystemCall::new("chmod", libc::SYS_chmod),
    TrackedSystemCall::new("fchmodat", libc::SYS_fchmodat),
    TrackedSystemCall::new("fchmodat2", numbers::SYS_FCHMODAT2),
    TrackedSystemCall::new("chown", libc::SYS_chown),
    TrackedSystemCall::new("lchown", libc::SYS_lchown),
    TrackedSystemCall::new("fchownat", libc::SYS_fchownat),
    TrackedSystemCall::new("utime", libc::SYS_utime),
    TrackedSystemCall::new("utimes", libc::SYS_utimes),
    TrackedSystemCall::new("utimensat", libc::SYS_utimensat),
    TrackedSystemCall::new("futimesat", libc::SYS_futimesat),
    TrackedSystemCall::new("file_setattr", numbers::SYS_FILE_SETATTR),
    // Changes through a descriptor, resolved to the path behind it.
    TrackedSystemCall::new("ftruncate", libc::SYS_ftruncate),
    TrackedSystemCall::new("fchmod", libc::SYS_fchmod),
    TrackedSystemCall::new("fchown", libc::SYS_fchown),
    TrackedSystemCall::new("fallocate", libc::SYS_fallocate),
];

/// The filter's return data for a tracked system call.
pub(crate) const DATA_TRACKED: u16 = 0;
/// The filter's return data for a system call through a foreign architecture's
/// interface, which on `x86_64` is the 32-bit `int 0x80` interface.
pub(crate) const DATA_FOREIGN_ARCH: u16 = 1;
/// The filter's return data for a system call through the x32 interface.
pub(crate) const DATA_X32: u16 = 2;

/// The bit that marks an x32 system call number.
pub(crate) const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// The audit architecture of native `x86_64` system calls, `AUDIT_ARCH_X86_64`:
/// `EM_X86_64` (62) with the 64-bit and little-endian flags.
const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;

/// The offset of the system call number, `nr`, in `struct seccomp_data`.
const NR_OFFSET: u32 = 0;
/// The offset of the audit architecture, `arch`, in `struct seccomp_data`.
const ARCH_OFFSET: u32 = 4;

/// `BPF_LD | BPF_W | BPF_ABS`: load a 32-bit word of `struct seccomp_data`.
const LOAD_WORD: u16 = 0x20;
/// `BPF_JMP | BPF_JEQ | BPF_K`: jump on equality with a constant.
const JUMP_IF_EQUAL: u16 = 0x15;
/// `BPF_JMP | BPF_JSET | BPF_K`: jump when any bit of a constant is set.
const JUMP_IF_SET: u16 = 0x45;
/// `BPF_RET | BPF_K`: return a constant action.
const RETURN: u16 = 0x06;

/// Load the word at `offset` of `struct seccomp_data`.
const fn load(offset: u32) -> sock_filter {
    sock_filter {
        code: LOAD_WORD,
        jt: 0,
        jf: 0,
        k: offset,
    }
}

/// Skip the next instruction unless the loaded word equals `value`.
const fn unless_equal_skip(value: u32) -> sock_filter {
    sock_filter {
        code: JUMP_IF_EQUAL,
        jt: 0,
        jf: 1,
        k: value,
    }
}

/// Skip the next instruction if the loaded word equals `value`.
const fn if_equal_skip(value: u32) -> sock_filter {
    sock_filter {
        code: JUMP_IF_EQUAL,
        jt: 1,
        jf: 0,
        k: value,
    }
}

/// Skip the next instruction unless the loaded word has a bit of `bits` set.
const fn unless_set_skip(bits: u32) -> sock_filter {
    sock_filter {
        code: JUMP_IF_SET,
        jt: 0,
        jf: 1,
        k: bits,
    }
}

/// Return `action` from the filter.
const fn action(action: u32) -> sock_filter {
    sock_filter {
        code: RETURN,
        jt: 0,
        jf: 0,
        k: action,
    }
}

/// Stop for the tracer, which reads `data` with `PTRACE_GETEVENTMSG`.
fn trace(data: u16) -> sock_filter {
    action(libc::SECCOMP_RET_TRACE | u32::from(data))
}

/// Build the filter program.
///
/// # Errors
///
/// Returns an error only if a system call number does not fit the filter's 32-bit
/// comparison, which no `x86_64` number does.
pub(crate) fn program() -> Result<Vec<sock_filter>, TryFromIntError> {
    let io_uring_setup = u32::try_from(libc::SYS_io_uring_setup)?;
    let mut program = vec![
        load(ARCH_OFFSET),
        if_equal_skip(AUDIT_ARCH_X86_64),
        trace(DATA_FOREIGN_ARCH),
        load(NR_OFFSET),
        unless_set_skip(X32_SYSCALL_BIT),
        trace(DATA_X32),
        unless_equal_skip(io_uring_setup),
        action(libc::SECCOMP_RET_ERRNO | libc::ENOSYS.unsigned_abs()),
    ];
    for call in TRACKED_SYSTEM_CALLS {
        program.push(unless_equal_skip(u32::try_from(call.number)?));
        program.push(trace(DATA_TRACKED));
    }
    program.push(action(libc::SECCOMP_RET_ALLOW));
    Ok(program)
}

/// Unit tests that pin the filter's system-call set and run its program.
#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, mem};

    use super::*;

    /// The 32-bit audit architecture, `AUDIT_ARCH_I386`.
    const AUDIT_ARCH_I386: u32 = 0x4000_0003;

    /// The kernel's limit on the length of a filter program, `BPF_MAXINSNS`.
    const MAX_INSTRUCTIONS: usize = 4096;

    /// Run the filter program on one system call, as the kernel does, and return
    /// the action it chooses.
    fn evaluate(
        program: &[sock_filter],
        arch: u32,
        nr: u32,
    ) -> u32 {
        let mut pc = 0_usize;
        let mut accumulator = 0_u32;
        loop {
            let instruction = program[pc];
            let next = pc.checked_add(1).unwrap();
            pc = match instruction.code {
                LOAD_WORD => {
                    accumulator = match instruction.k {
                        NR_OFFSET => nr,
                        ARCH_OFFSET => arch,
                        offset => panic!("unexpected load offset {offset}"),
                    };
                    next
                },
                JUMP_IF_EQUAL | JUMP_IF_SET => {
                    let taken = if instruction.code == JUMP_IF_EQUAL {
                        accumulator == instruction.k
                    } else {
                        accumulator & instruction.k != 0
                    };
                    let skip = if taken {
                        instruction.jt
                    } else {
                        instruction.jf
                    };
                    next.checked_add(usize::from(skip)).unwrap()
                },
                RETURN => return instruction.k,
                code => panic!("unexpected instruction {code:#x}"),
            };
        }
    }

    /// The `x86_64` number of a system call, as the filter compares it.
    fn number(nr: c_long) -> u32 {
        u32::try_from(nr).unwrap()
    }

    /// The tracked calls are the spec's 71, in the order of its list.
    #[test]
    fn tracked_set_is_the_spec_list() {
        let names: Vec<&str> = TRACKED_SYSTEM_CALLS.iter().map(|call| call.name).collect();
        assert_eq!(names, vec![
            "open",
            "openat",
            "openat2",
            "creat",
            "stat",
            "lstat",
            "newfstatat",
            "statx",
            "access",
            "faccessat",
            "faccessat2",
            "file_getattr",
            "name_to_handle_at",
            "statfs",
            "fstatfs",
            "inotify_add_watch",
            "fanotify_mark",
            "getxattr",
            "lgetxattr",
            "fgetxattr",
            "getxattrat",
            "listxattr",
            "llistxattr",
            "flistxattr",
            "listxattrat",
            "setxattr",
            "lsetxattr",
            "fsetxattr",
            "setxattrat",
            "removexattr",
            "lremovexattr",
            "fremovexattr",
            "removexattrat",
            "readlink",
            "readlinkat",
            "link",
            "linkat",
            "symlink",
            "symlinkat",
            "getdents",
            "getdents64",
            "execve",
            "execveat",
            "chdir",
            "fchdir",
            "rename",
            "renameat",
            "renameat2",
            "unlink",
            "unlinkat",
            "rmdir",
            "mkdir",
            "mkdirat",
            "mknod",
            "mknodat",
            "truncate",
            "chmod",
            "fchmodat",
            "fchmodat2",
            "chown",
            "lchown",
            "fchownat",
            "utime",
            "utimes",
            "utimensat",
            "futimesat",
            "file_setattr",
            "ftruncate",
            "fchmod",
            "fchown",
            "fallocate",
        ]);
        assert_eq!(names.len(), 71);
    }

    #[test]
    fn tracked_numbers_are_distinct_x86_64_numbers() {
        let numbers: BTreeSet<c_long> = TRACKED_SYSTEM_CALLS
            .iter()
            .map(|call| call.number)
            .collect();
        assert_eq!(numbers.len(), TRACKED_SYSTEM_CALLS.len());
        let known = [
            ("open", 2),
            ("stat", 4),
            ("access", 21),
            ("execve", 59),
            ("chdir", 80),
            ("getdents64", 217),
            ("openat", 257),
            ("execveat", 322),
            ("statx", 332),
            ("openat2", 437),
            ("faccessat2", 439),
            ("statfs", 137),
            ("fstatfs", 138),
            ("getxattr", 191),
            ("removexattr", 197),
            ("fremovexattr", 199),
            ("inotify_add_watch", 254),
            ("fallocate", 285),
            ("fanotify_mark", 301),
            ("name_to_handle_at", 303),
            ("fchmodat2", 452),
            ("setxattrat", 463),
            ("getxattrat", 464),
            ("listxattrat", 465),
            ("removexattrat", 466),
            ("file_getattr", 468),
            ("file_setattr", 469),
        ];
        for (name, nr) in known {
            let call = TRACKED_SYSTEM_CALLS
                .iter()
                .find(|call| call.name == name)
                .unwrap();
            assert_eq!(call.number, nr, "{name}");
        }
        assert_eq!(libc::SYS_io_uring_setup, 425);
    }

    #[test]
    fn instruction_codes_and_offsets_match_the_kernel() {
        assert_eq!(
            u32::from(LOAD_WORD),
            libc::BPF_LD | libc::BPF_W | libc::BPF_ABS
        );
        assert_eq!(
            u32::from(JUMP_IF_EQUAL),
            libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K
        );
        assert_eq!(
            u32::from(JUMP_IF_SET),
            libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K
        );
        assert_eq!(u32::from(RETURN), libc::BPF_RET | libc::BPF_K);
        assert_eq!(
            usize::try_from(NR_OFFSET).unwrap(),
            mem::offset_of!(libc::seccomp_data, nr)
        );
        assert_eq!(
            usize::try_from(ARCH_OFFSET).unwrap(),
            mem::offset_of!(libc::seccomp_data, arch)
        );
    }

    #[test]
    fn program_fits_the_kernel_limits() {
        let program = program().unwrap();
        assert!(program.len() <= MAX_INSTRUCTIONS);
        assert!(u16::try_from(program.len()).is_ok());
    }

    #[test]
    fn tracked_calls_stop_for_the_tracer() {
        let program = program().unwrap();
        let stop = libc::SECCOMP_RET_TRACE | u32::from(DATA_TRACKED);
        for call in TRACKED_SYSTEM_CALLS {
            assert_eq!(
                evaluate(&program, AUDIT_ARCH_X86_64, number(call.number)),
                stop,
                "{}",
                call.name
            );
        }
    }

    #[test]
    fn other_calls_run_at_full_speed() {
        let program = program().unwrap();
        let untracked = [
            libc::SYS_read,
            libc::SYS_write,
            libc::SYS_close,
            libc::SYS_fstat,
            libc::SYS_mmap,
            libc::SYS_brk,
            libc::SYS_getpid,
            libc::SYS_clone,
            libc::SYS_wait4,
            libc::SYS_exit_group,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
        ];
        for nr in untracked {
            assert_eq!(
                evaluate(&program, AUDIT_ARCH_X86_64, number(nr)),
                libc::SECCOMP_RET_ALLOW,
                "system call {nr}"
            );
        }
    }

    #[test]
    fn io_uring_setup_fails_with_enosys() {
        let program = program().unwrap();
        let action = evaluate(
            &program,
            AUDIT_ARCH_X86_64,
            number(libc::SYS_io_uring_setup),
        );
        assert_eq!(
            action & libc::SECCOMP_RET_ACTION_FULL,
            libc::SECCOMP_RET_ERRNO
        );
        assert_eq!(action & libc::SECCOMP_RET_DATA, libc::ENOSYS.unsigned_abs());
    }

    #[test]
    fn non_native_calls_stop_with_their_interface() {
        let program = program().unwrap();
        let foreign = libc::SECCOMP_RET_TRACE | u32::from(DATA_FOREIGN_ARCH);
        let x32 = libc::SECCOMP_RET_TRACE | u32::from(DATA_X32);
        for nr in [
            number(libc::SYS_getpid),
            number(libc::SYS_open),
            number(libc::SYS_io_uring_setup),
        ] {
            assert_eq!(evaluate(&program, AUDIT_ARCH_I386, nr), foreign);
            assert_eq!(
                evaluate(&program, AUDIT_ARCH_X86_64, nr | X32_SYSCALL_BIT),
                x32
            );
        }
    }
}
