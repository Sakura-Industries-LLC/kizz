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
//! * a native system call outside [`KNOWN_SYSTEM_CALLS`] stops for the tracer, which
//!   reports its number as one that it does not know and lets it run;
//! * each system call in [`UNTRACEABLE_SYSTEM_CALLS`] stops for the tracer, which marks
//!   the run as incompletely traced, with the checks of its arguments that the filter can
//!   make itself: `clone` and `unshare` only with `CLONE_NEWNS` in their flags, and
//!   `setns` only when its namespace type is 0 or includes `CLONE_NEWNS`; `clone3` always
//!   stops, and the tracer reads the flags in its `struct clone_args`;
//! * each system call in [`TRACKED_SYSTEM_CALLS`] stops for the tracer, `sendto` only
//!   with a destination address: a null address names none, so the call, which a
//!   connected socket makes for every `send`, runs at full speed;
//! * every other system call runs at full speed.

use std::{num::TryFromIntError, ops::RangeInclusive};

use libc::{c_long, sock_filter};

/// A system call that the filter stops for, with its `x86_64` name and number.
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

/// The `x86_64` numbers of the system calls that the filter stops for and that the
/// `libc` crate does not define, from the kernel's `x86_64` table,
/// `arch/x86/entry/syscalls/syscall_64.tbl`.
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
    /// `open_tree_attr`, number 467 in the kernel's `x86_64` table.
    pub(crate) const SYS_OPEN_TREE_ATTR: c_long = 467;
    /// `fchroot`, number 472 in the kernel's `x86_64` table.
    pub(crate) const SYS_FCHROOT: c_long = 472;
}

/// Every system call the filter stops for, in the order of the spec's list:
/// opening files, metadata and existence, file systems, watches, extended
/// attributes, links, directory listings, programs, the working directory, changes,
/// changes through a descriptor, and sockets.
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
    // Sockets.
    TrackedSystemCall::new("bind", libc::SYS_bind),
    TrackedSystemCall::new("connect", libc::SYS_connect),
    TrackedSystemCall::new("sendto", libc::SYS_sendto),
    TrackedSystemCall::new("sendmsg", libc::SYS_sendmsg),
    TrackedSystemCall::new("sendmmsg", libc::SYS_sendmmsg),
];

/// Every system call that marks a run as incompletely traced, in the order of the
/// spec's list: opening by handle, changing the root, mounting, mount namespaces,
/// and the other uses of a path. The first four groups can change what a path
/// means; the last uses a path in ways that the tracer does not record. `unshare`,
/// `clone` and `clone3` mark the run only with `CLONE_NEWNS`, and `setns` only when
/// it can enter a mount namespace.
pub const UNTRACEABLE_SYSTEM_CALLS: &[TrackedSystemCall] = &[
    // Opening by handle.
    TrackedSystemCall::new("open_by_handle_at", libc::SYS_open_by_handle_at),
    // Changing the root.
    TrackedSystemCall::new("chroot", libc::SYS_chroot),
    TrackedSystemCall::new("fchroot", numbers::SYS_FCHROOT),
    TrackedSystemCall::new("pivot_root", libc::SYS_pivot_root),
    // Mounting.
    TrackedSystemCall::new("mount", libc::SYS_mount),
    TrackedSystemCall::new("umount2", libc::SYS_umount2),
    TrackedSystemCall::new("fsopen", libc::SYS_fsopen),
    TrackedSystemCall::new("fsconfig", libc::SYS_fsconfig),
    TrackedSystemCall::new("fsmount", libc::SYS_fsmount),
    TrackedSystemCall::new("fspick", libc::SYS_fspick),
    TrackedSystemCall::new("move_mount", libc::SYS_move_mount),
    TrackedSystemCall::new("open_tree", libc::SYS_open_tree),
    TrackedSystemCall::new("open_tree_attr", numbers::SYS_OPEN_TREE_ATTR),
    TrackedSystemCall::new("mount_setattr", libc::SYS_mount_setattr),
    // Mount namespaces.
    TrackedSystemCall::new("unshare", libc::SYS_unshare),
    TrackedSystemCall::new("setns", libc::SYS_setns),
    TrackedSystemCall::new("clone", libc::SYS_clone),
    TrackedSystemCall::new("clone3", libc::SYS_clone3),
    // Other uses of a path.
    TrackedSystemCall::new("uselib", libc::SYS_uselib),
    TrackedSystemCall::new("acct", libc::SYS_acct),
    TrackedSystemCall::new("quotactl", libc::SYS_quotactl),
    TrackedSystemCall::new("quotactl_fd", libc::SYS_quotactl_fd),
    TrackedSystemCall::new("swapon", libc::SYS_swapon),
    TrackedSystemCall::new("swapoff", libc::SYS_swapoff),
];

/// The native system calls that the tracer knows, as ranges of numbers: those of
/// the `x86_64` table of Linux 7.3, `arch/x86/entry/syscalls/syscall_64.tbl`, which
/// uses 0 through 336 and 424 through 472 for native calls. Any other native
/// number is a system call that the tracer does not know: 337 through 423, which
/// the table leaves unused, and every number above 472.
pub const KNOWN_SYSTEM_CALLS: [RangeInclusive<u32>; 2] = [0..=336, 424..=472];

/// The filter's return data for a tracked system call.
pub(crate) const DATA_TRACKED: u16 = 0;
/// The filter's return data for a system call through a foreign architecture's
/// interface, which on `x86_64` is the 32-bit `int 0x80` interface.
pub(crate) const DATA_FOREIGN_ARCH: u16 = 1;
/// The filter's return data for a system call through the x32 interface.
pub(crate) const DATA_X32: u16 = 2;
/// The filter's return data for a system call of [`UNTRACEABLE_SYSTEM_CALLS`] that
/// marks the run, or that the tracer checks itself.
pub(crate) const DATA_UNTRACEABLE: u16 = 3;
/// The filter's return data for a native system call outside
/// [`KNOWN_SYSTEM_CALLS`].
pub(crate) const DATA_UNKNOWN: u16 = 4;

/// The bit that marks an x32 system call number.
pub(crate) const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// The audit architecture of native `x86_64` system calls, `AUDIT_ARCH_X86_64`:
/// `EM_X86_64` (62) with the 64-bit and little-endian flags.
const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;

/// The offset of the system call number, `nr`, in `struct seccomp_data`.
const NR_OFFSET: u32 = 0;
/// The offset of the audit architecture, `arch`, in `struct seccomp_data`.
const ARCH_OFFSET: u32 = 4;
/// The offset of the low 32 bits of the first argument, `args[0]`, in
/// `struct seccomp_data` on a little-endian machine.
const FIRST_ARGUMENT_OFFSET: u32 = 16;
/// The offset of the low 32 bits of the second argument, `args[1]`, in
/// `struct seccomp_data` on a little-endian machine.
const SECOND_ARGUMENT_OFFSET: u32 = 24;
/// The offset of the low 32 bits of the fifth argument, `args[4]`, in
/// `struct seccomp_data` on a little-endian machine: the destination address of
/// `sendto`.
const FIFTH_ARGUMENT_OFFSET: u32 = 48;
/// The offset of the high 32 bits of the fifth argument, `args[4]`, in
/// `struct seccomp_data` on a little-endian machine.
const FIFTH_ARGUMENT_HIGH_OFFSET: u32 = 52;

/// The `CLONE_NEWNS` flag, which asks for a new mount namespace, as the filter
/// compares it with the low 32 bits of an argument.
const NEW_MOUNT_NAMESPACE: u32 = libc::CLONE_NEWNS.cast_unsigned();

/// `BPF_LD | BPF_W | BPF_ABS`: load a 32-bit word of `struct seccomp_data`.
const LOAD_WORD: u16 = 0x20;
/// `BPF_JMP | BPF_JEQ | BPF_K`: jump on equality with a constant.
const JUMP_IF_EQUAL: u16 = 0x15;
/// `BPF_JMP | BPF_JSET | BPF_K`: jump when any bit of a constant is set.
const JUMP_IF_SET: u16 = 0x45;
/// `BPF_JMP | BPF_JGT | BPF_K`: jump when the loaded word is greater than a
/// constant.
const JUMP_IF_GREATER: u16 = 0x25;
/// `BPF_JMP | BPF_JGE | BPF_K`: jump when the loaded word is at least a constant.
const JUMP_IF_AT_LEAST: u16 = 0x35;
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

/// Compare the loaded word with `value` by `code`, and skip `taken` instructions
/// when the comparison holds, or `not_taken` when it does not.
const fn jump(
    code: u16,
    value: u32,
    taken: u8,
    not_taken: u8,
) -> sock_filter {
    sock_filter {
        code,
        jt: taken,
        jf: not_taken,
        k: value,
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

/// Allow the system call.
const fn allow() -> sock_filter {
    action(libc::SECCOMP_RET_ALLOW)
}

/// The instructions for `call`, one of [`UNTRACEABLE_SYSTEM_CALLS`]: stop for the
/// tracer when the call can mark the run, and otherwise allow it. `clone` and
/// `unshare` stop only with `CLONE_NEWNS` in their first argument, and `setns` only
/// when its second argument, the namespace type, is 0 or includes `CLONE_NEWNS`;
/// every other call, `clone3` included, always stops. The instructions that load an
/// argument end in an action on every path, so the number that the instructions
/// after them compare stays loaded.
///
/// # Errors
///
/// Returns an error only if the call's number does not fit the filter's 32-bit
/// comparison.
fn untraceable(call: &TrackedSystemCall) -> Result<Vec<sock_filter>, TryFromIntError> {
    let number = u32::try_from(call.number)?;
    Ok(match call.number {
        libc::SYS_clone | libc::SYS_unshare => {
            vec![
                jump(JUMP_IF_EQUAL, number, 0, 4),
                load(FIRST_ARGUMENT_OFFSET),
                jump(JUMP_IF_SET, NEW_MOUNT_NAMESPACE, 0, 1),
                trace(DATA_UNTRACEABLE),
                allow(),
            ]
        },
        libc::SYS_setns => {
            vec![
                jump(JUMP_IF_EQUAL, number, 0, 5),
                load(SECOND_ARGUMENT_OFFSET),
                jump(JUMP_IF_EQUAL, 0, 1, 0),
                jump(JUMP_IF_SET, NEW_MOUNT_NAMESPACE, 0, 1),
                trace(DATA_UNTRACEABLE),
                allow(),
            ]
        },
        _ => vec![unless_equal_skip(number), trace(DATA_UNTRACEABLE)],
    })
}

/// The instructions for `call`, one of [`TRACKED_SYSTEM_CALLS`]: stop for the
/// tracer. `sendto` stops only with a destination address, a fifth argument that
/// is not null, which the filter compares in its two 32-bit halves; with a null
/// one, it runs. The instructions that load an argument end in an action on every
/// path, so the number that the instructions after them compare stays loaded.
///
/// # Errors
///
/// Returns an error only if the call's number does not fit the filter's 32-bit
/// comparison.
fn tracked(call: &TrackedSystemCall) -> Result<Vec<sock_filter>, TryFromIntError> {
    let number = u32::try_from(call.number)?;
    Ok(if call.number == libc::SYS_sendto {
        vec![
            jump(JUMP_IF_EQUAL, number, 0, 7),
            load(FIFTH_ARGUMENT_OFFSET),
            jump(JUMP_IF_EQUAL, 0, 1, 0),
            trace(DATA_TRACKED),
            load(FIFTH_ARGUMENT_HIGH_OFFSET),
            jump(JUMP_IF_EQUAL, 0, 1, 0),
            trace(DATA_TRACKED),
            allow(),
        ]
    } else {
        vec![unless_equal_skip(number), trace(DATA_TRACKED)]
    })
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
    // Every native number outside the known table, which starts at 0: above its
    // last number, and in the gap between its two ranges.
    let [low, high] = KNOWN_SYSTEM_CALLS;
    program.extend([
        jump(JUMP_IF_GREATER, *high.end(), 0, 1),
        trace(DATA_UNKNOWN),
        jump(JUMP_IF_GREATER, *low.end(), 0, 2),
        jump(JUMP_IF_AT_LEAST, *high.start(), 1, 0),
        trace(DATA_UNKNOWN),
    ]);
    for call in UNTRACEABLE_SYSTEM_CALLS {
        program.extend(untraceable(call)?);
    }
    for call in TRACKED_SYSTEM_CALLS {
        program.extend(tracked(call)?);
    }
    program.push(allow());
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

    /// Run the filter program on one system call without arguments, as the kernel
    /// does, and return the action it chooses.
    fn evaluate(
        program: &[sock_filter],
        arch: u32,
        nr: u32,
    ) -> u32 {
        evaluate_with(program, arch, nr, [0; 2])
    }

    /// Run the filter program on one system call whose first two arguments have
    /// the low 32 bits `arguments`, and whose other arguments are 0, as the kernel
    /// does, and return the action it chooses.
    fn evaluate_with(
        program: &[sock_filter],
        arch: u32,
        nr: u32,
        arguments: [u32; 2],
    ) -> u32 {
        let [first, second] = arguments;
        evaluate_all(program, arch, nr, [
            u64::from(first),
            u64::from(second),
            0,
            0,
            0,
            0,
        ])
    }

    /// Run the filter program on one system call with the six arguments
    /// `arguments`, as the kernel does, and return the action it chooses.
    fn evaluate_all(
        program: &[sock_filter],
        arch: u32,
        nr: u32,
        arguments: [u64; 6],
    ) -> u32 {
        let low = |argument: u64| u32::try_from(argument & u64::from(u32::MAX)).unwrap();
        let high = |argument: u64| u32::try_from(argument >> 32).unwrap();
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
                        FIRST_ARGUMENT_OFFSET => low(arguments[0]),
                        SECOND_ARGUMENT_OFFSET => low(arguments[1]),
                        FIFTH_ARGUMENT_OFFSET => low(arguments[4]),
                        FIFTH_ARGUMENT_HIGH_OFFSET => high(arguments[4]),
                        offset => panic!("unexpected load offset {offset}"),
                    };
                    next
                },
                JUMP_IF_EQUAL | JUMP_IF_SET | JUMP_IF_GREATER | JUMP_IF_AT_LEAST => {
                    let taken = match instruction.code {
                        JUMP_IF_EQUAL => accumulator == instruction.k,
                        JUMP_IF_SET => accumulator & instruction.k != 0,
                        JUMP_IF_GREATER => accumulator > instruction.k,
                        _ => accumulator >= instruction.k,
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

    /// The tracked calls are the spec's 76, in the order of its list.
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
            "bind",
            "connect",
            "sendto",
            "sendmsg",
            "sendmmsg",
        ]);
        assert_eq!(names.len(), 76);
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
            ("connect", 42),
            ("sendto", 44),
            ("sendmsg", 46),
            ("bind", 49),
            ("sendmmsg", 307),
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
        assert_eq!(
            u32::from(JUMP_IF_GREATER),
            libc::BPF_JMP | libc::BPF_JGT | libc::BPF_K
        );
        assert_eq!(
            u32::from(JUMP_IF_AT_LEAST),
            libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K
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
        // The low 32 bits of each argument, on a little-endian machine.
        let arguments = mem::offset_of!(libc::seccomp_data, args);
        assert_eq!(usize::try_from(FIRST_ARGUMENT_OFFSET).unwrap(), arguments);
        assert_eq!(
            usize::try_from(SECOND_ARGUMENT_OFFSET).unwrap(),
            arguments.checked_add(mem::size_of::<u64>()).unwrap()
        );
        // The fifth argument, `args[4]`, in its low and high halves.
        let fifth = arguments
            .checked_add(mem::size_of::<u64>().checked_mul(4).unwrap())
            .unwrap();
        assert_eq!(usize::try_from(FIFTH_ARGUMENT_OFFSET).unwrap(), fifth);
        assert_eq!(
            usize::try_from(FIFTH_ARGUMENT_HIGH_OFFSET).unwrap(),
            fifth.checked_add(mem::size_of::<u32>()).unwrap()
        );
        assert_eq!(NEW_MOUNT_NAMESPACE, 0x0002_0000);
    }

    #[test]
    fn program_fits_the_kernel_limits() {
        let program = program().unwrap();
        assert!(program.len() <= MAX_INSTRUCTIONS);
        assert!(u16::try_from(program.len()).is_ok());
    }

    /// Every tracked call stops for the tracer, `sendto` with a destination address
    /// in its fifth argument.
    #[test]
    fn tracked_calls_stop_for_the_tracer() {
        let program = program().unwrap();
        let stop = libc::SECCOMP_RET_TRACE | u32::from(DATA_TRACKED);
        let destination = [0, 0, 0, 0, 0x7FFF_0000_1000, 16];
        for call in TRACKED_SYSTEM_CALLS {
            assert_eq!(
                evaluate_all(
                    &program,
                    AUDIT_ARCH_X86_64,
                    number(call.number),
                    destination
                ),
                stop,
                "{}",
                call.name
            );
        }
    }

    /// `sendto` stops for the tracer when its destination address is not null, in
    /// either half of the register, and runs at full speed with a null one, as a
    /// `send` on a connected socket makes it; the calls after it in the program are
    /// still compared.
    #[test]
    fn sendto_stops_only_with_a_destination() {
        let program = program().unwrap();
        let stop = libc::SECCOMP_RET_TRACE | u32::from(DATA_TRACKED);
        let sendto = number(libc::SYS_sendto);
        for address in [
            1,
            0x1000,
            0xFFFF_FFFF,
            0x1_0000_0000,
            0x7FFF_0000_1000,
            u64::MAX,
        ] {
            assert_eq!(
                evaluate_all(&program, AUDIT_ARCH_X86_64, sendto, [
                    3, 0, 5, 0, address, 16
                ]),
                stop,
                "{address:#x}"
            );
        }
        for length in [0, 16] {
            assert_eq!(
                evaluate_all(&program, AUDIT_ARCH_X86_64, sendto, [3, 0, 5, 0, 0, length]),
                libc::SECCOMP_RET_ALLOW,
                "{length}"
            );
        }
        for call in ["sendmsg", "sendmmsg"] {
            let nr = TRACKED_SYSTEM_CALLS
                .iter()
                .find(|tracked| tracked.name == call)
                .map(|tracked| number(tracked.number))
                .unwrap();
            assert_eq!(evaluate(&program, AUDIT_ARCH_X86_64, nr), stop, "{call}");
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

    /// The calls that mark a run are the spec's 24, in the order of its list, with
    /// their numbers in the kernel's `x86_64` table.
    #[test]
    fn untraceable_set_is_the_spec_list() {
        let calls: Vec<(&str, c_long)> = UNTRACEABLE_SYSTEM_CALLS
            .iter()
            .map(|call| (call.name, call.number))
            .collect();
        assert_eq!(calls, vec![
            ("open_by_handle_at", 304),
            ("chroot", 161),
            ("fchroot", 472),
            ("pivot_root", 155),
            ("mount", 165),
            ("umount2", 166),
            ("fsopen", 430),
            ("fsconfig", 431),
            ("fsmount", 432),
            ("fspick", 433),
            ("move_mount", 429),
            ("open_tree", 428),
            ("open_tree_attr", 467),
            ("mount_setattr", 442),
            ("unshare", 272),
            ("setns", 308),
            ("clone", 56),
            ("clone3", 435),
            ("uselib", 134),
            ("acct", 163),
            ("quotactl", 179),
            ("quotactl_fd", 443),
            ("swapon", 167),
            ("swapoff", 168),
        ]);
        let tracked: BTreeSet<c_long> = TRACKED_SYSTEM_CALLS
            .iter()
            .map(|call| call.number)
            .collect();
        let untraceable: BTreeSet<c_long> = UNTRACEABLE_SYSTEM_CALLS
            .iter()
            .map(|call| call.number)
            .collect();
        assert_eq!(untraceable.len(), 24);
        assert!(tracked.is_disjoint(&untraceable));
    }

    /// The known calls are those of the `x86_64` table of Linux 7.3,
    /// `arch/x86/entry/syscalls/syscall_64.tbl`: 0 through 336, from `read` to
    /// `uprobe`, and 424 through 472, from `pidfd_send_signal` to `fchroot`; every
    /// tracked call and every call that marks a run is among them.
    #[test]
    fn known_system_calls_are_the_linux_7_3_table() {
        assert_eq!(KNOWN_SYSTEM_CALLS, [0..=336, 424..=472]);
        assert_eq!(libc::SYS_read, 0);
        assert_eq!(libc::SYS_rseq, 334);
        assert_eq!(libc::SYS_pidfd_send_signal, 424);
        let known = |nr: c_long| {
            KNOWN_SYSTEM_CALLS
                .iter()
                .any(|range| range.contains(&number(nr)))
        };
        for call in TRACKED_SYSTEM_CALLS.iter().chain(UNTRACEABLE_SYSTEM_CALLS) {
            assert!(known(call.number), "{}", call.name);
        }
    }

    /// Each call that marks a run stops for the tracer with the arguments that let
    /// it change what a path means; `clone` and `unshare` without `CLONE_NEWNS`, and
    /// `setns` with another namespace type, run at full speed, while `clone3` always
    /// stops, because the filter cannot read its flags.
    #[test]
    fn untraceable_calls_stop_for_the_tracer() {
        let program = program().unwrap();
        let stop = libc::SECCOMP_RET_TRACE | u32::from(DATA_UNTRACEABLE);
        let allow = libc::SECCOMP_RET_ALLOW;
        let new_namespace = NEW_MOUNT_NAMESPACE;
        let other_namespace = libc::CLONE_NEWUTS.cast_unsigned();
        let child_signal = libc::SIGCHLD.cast_unsigned();
        for call in UNTRACEABLE_SYSTEM_CALLS {
            let nr = number(call.number);
            let (marking, running): (Vec<[u32; 2]>, Vec<[u32; 2]>) = match call.name {
                "clone" => {
                    (
                        vec![[new_namespace | child_signal, 0], [new_namespace, 0]],
                        vec![[child_signal, 0], [libc::CLONE_FILES.cast_unsigned(), 0], [
                            0,
                            new_namespace,
                        ]],
                    )
                },
                "unshare" => {
                    (
                        vec![[new_namespace, 0], [new_namespace | other_namespace, 0]],
                        vec![[0, 0], [libc::CLONE_FILES.cast_unsigned(), 0], [
                            other_namespace,
                            new_namespace,
                        ]],
                    )
                },
                "setns" => {
                    (vec![[3, 0], [3, new_namespace], [u32::MAX, 0]], vec![
                        [3, other_namespace],
                        [new_namespace, libc::CLONE_NEWNET.cast_unsigned()],
                    ])
                },
                _ => {
                    (
                        vec![[0, 0], [u32::MAX, u32::MAX], [new_namespace, 0]],
                        Vec::new(),
                    )
                },
            };
            for arguments in marking {
                assert_eq!(
                    evaluate_with(&program, AUDIT_ARCH_X86_64, nr, arguments),
                    stop,
                    "{} {arguments:?}",
                    call.name
                );
            }
            for arguments in running {
                assert_eq!(
                    evaluate_with(&program, AUDIT_ARCH_X86_64, nr, arguments),
                    allow,
                    "{} {arguments:?}",
                    call.name
                );
            }
        }
    }

    /// The numbers around the bounds of the known table: 336 runs; 337, 400 and 423
    /// stop as unknown; 424, 470 and 471 run; 472, `fchroot`, stops as a call that
    /// marks the run; 473 and 1,000,000 stop as unknown; an x32 number and a 32-bit
    /// call keep their own stops.
    #[test]
    fn numbers_outside_the_known_table_stop_as_unknown() {
        let program = program().unwrap();
        let unknown = libc::SECCOMP_RET_TRACE | u32::from(DATA_UNKNOWN);
        let untraceable = libc::SECCOMP_RET_TRACE | u32::from(DATA_UNTRACEABLE);
        let allow = libc::SECCOMP_RET_ALLOW;
        for (nr, expected) in [
            (336, allow),
            (337, unknown),
            (400, unknown),
            (423, unknown),
            (424, allow),
            (470, allow),
            (471, allow),
            (472, untraceable),
            (473, unknown),
            (1_000_000, unknown),
        ] {
            assert_eq!(
                evaluate(&program, AUDIT_ARCH_X86_64, nr),
                expected,
                "system call {nr}"
            );
        }
        let foreign = libc::SECCOMP_RET_TRACE | u32::from(DATA_FOREIGN_ARCH);
        let x32 = libc::SECCOMP_RET_TRACE | u32::from(DATA_X32);
        for nr in [400, 1_000_000] {
            assert_eq!(evaluate(&program, AUDIT_ARCH_I386, nr), foreign);
            assert_eq!(
                evaluate(&program, AUDIT_ARCH_X86_64, nr | X32_SYSCALL_BIT),
                x32
            );
        }
    }

    /// Every native number up to 1,000 gets exactly its stop: `io_uring_setup`
    /// fails, a tracked call and a call that marks the run stop for the tracer
    /// (`sendto` without a destination, and `clone` and `unshare` without
    /// arguments, run, and `setns` with type 0 stops), every other known number
    /// runs, and every number outside the table stops as unknown.
    #[test]
    fn every_native_number_gets_its_stop() {
        let program = program().unwrap();
        let stop = |data: u16| libc::SECCOMP_RET_TRACE | u32::from(data);
        let failure = libc::SECCOMP_RET_ERRNO | libc::ENOSYS.unsigned_abs();
        for nr in 0..=1_000_u32 {
            let number = c_long::from(nr);
            let expected = if number == libc::SYS_io_uring_setup {
                failure
            } else if number == libc::SYS_sendto {
                libc::SECCOMP_RET_ALLOW
            } else if TRACKED_SYSTEM_CALLS
                .iter()
                .any(|call| call.number == number)
            {
                stop(DATA_TRACKED)
            } else if [libc::SYS_clone, libc::SYS_unshare].contains(&number) {
                libc::SECCOMP_RET_ALLOW
            } else if UNTRACEABLE_SYSTEM_CALLS
                .iter()
                .any(|call| call.number == number)
            {
                stop(DATA_UNTRACEABLE)
            } else if KNOWN_SYSTEM_CALLS.iter().any(|range| range.contains(&nr)) {
                libc::SECCOMP_RET_ALLOW
            } else {
                stop(DATA_UNKNOWN)
            };
            assert_eq!(
                evaluate(&program, AUDIT_ARCH_X86_64, nr),
                expected,
                "system call {nr}"
            );
        }
    }
}
