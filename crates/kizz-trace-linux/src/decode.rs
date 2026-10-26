// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Decoding of the tracked system calls into the paths they touch.
//!
//! At the seccomp stop before a tracked call runs, [`enter`] reads the call's path
//! arguments from the traced thread's memory, its directory descriptors and its
//! flags, and resolves every path to an absolute one: a relative path against the
//! thread's working directory or the directory descriptor the call used, and a call
//! that acts on a descriptor to the path behind that descriptor. At the stop after
//! the call returns, [`Call::finish_observed`] records each path with its access kinds:
//!
//! * a call that succeeded records the kinds of its rule in [`SYSTEM_CALLS`];
//! * a call that failed records a probe for each path: it checked the path's existence or
//!   metadata, including a lookup that found nothing (`ENOENT`), and it read, wrote and
//!   ran nothing;
//! * a call that reads or lists extended attributes and fails because the attribute does
//!   not exist (`ENODATA`), the buffer is too small (`ERANGE`), or the file system has
//!   none (`EOPNOTSUPP`) records the xattr kind rather than a probe, because each of
//!   those answers depends on the attributes;
//! * a call whose result was never seen, because its thread was killed, records the kinds
//!   of its rule and a probe, since it may or may not have taken effect;
//! * a call that a signal interrupts fails like any other, with `EINTR` or with one of
//!   the kernel's internal restart errors, so it records a probe for each path: its
//!   lookup happened. A call that the kernel restarts after the signal stops again at its
//!   entry, so it is decoded again and records the kinds of its final result too.
//!
//! Paths are absolute, with `.` components and repeated separators removed and
//! `..` components kept, as [`AbsolutePath`] normalizes them. A descriptor or a
//! working directory that is not a file system path, such as a pipe, or whose file
//! or directory was removed, names no path, so nothing is recorded for it. Linux
//! marks a removed object by adding ` (deleted)` to its path, which a live name may
//! also end with, so the object is compared with the one at that name.
//!
//! `openat2` with `RESOLVE_IN_ROOT` looks its path up with the directory descriptor
//! as the root: `..` at the root stays there, and an absolute path or symbolic-link
//! target starts at the root. The decoder repeats that lookup in the file system, so
//! the reported path names the object the call reached, or the component where the
//! kernel stops, such as one that is not a directory, and records each symbolic
//! link it followed. When the call succeeds, the descriptor it returns names the
//! object it opened, and a different name there makes the trace incomplete.
//!
//! At the exit of `execve` and `execveat`, [`Call::loaded`] also records what the
//! kernel loads to run the program without another system call, as it records the
//! program's own path: the interpreter that a `#!` line names, the interpreters of
//! interpreters that are scripts themselves, as deep as the kernel follows them, and
//! the loader that an `x86_64` ELF executable names in its `PT_INTERP` program
//! header. The tracer reads the start of each file as the kernel does, and resolves
//! a relative interpreter or loader path against the working directory that the
//! thread had when it entered the call. A program that ran although the tracer
//! could not read it, or whose last file is neither a `#!` script nor an `x86_64`
//! ELF executable, makes the trace incomplete, because a handler such as
//! `binfmt_misc` may have run it with an interpreter that the tracer cannot see.
//!
//! A call that opens a regular file for reading or runs one is a read whose
//! file the command may change later, so [`Call::first_reads`] gives the file's
//! metadata at the call's exit, for the tracer to note when the run had not
//! written the path before: an opening call with read access through the
//! descriptor that it returned, unless it truncated the file, or created it, which
//! an opening call with `O_CREAT` tells by whether a regular file was at the path
//! at its entry; and `execve` and `execveat` through the path of the program and of
//! each interpreter and loader that the kernel loaded. Reading a link opens no
//! file, so `readlink` and `readlinkat` give nothing.
//!
//! The socket calls `bind`, `connect`, `sendto`, `sendmsg` and `sendmmsg` name a
//! socket address: the one that their arguments point to, the `msg_name` of the
//! `struct msghdr` of `sendmsg`, and the `msg_name` of every `struct mmsghdr` of
//! `sendmmsg`. The decoder reads each address as the kernel does. A Unix socket
//! path, neither abstract nor unnamed, is resolved like any path against the
//! working directory: `bind` writes it, and the other calls probe it. Every call
//! but `bind` also notes its destination as a connection, which
//! [`Call::connections`] gives at the exit: an IPv4 or IPv6 address and port
//! whatever the outcome of the call, and a Unix socket, by path or abstract name,
//! unless the call failed with `ENOENT`, `ENOTDIR`, `ELOOP`, `ENAMETOOLONG` or
//! `EACCES`, which say that its path was not found. A null address, a family other
//! than these, and an address that the kernel rejects for its length name nothing.
//!
//! When the request asks for the process tree, [`Call::finish_observed`] also
//! records each path into the observations of the program that made the call, each
//! kind with the call's [`Outcome`]: `ok` for a success, the name of its error for a
//! failure, as [`errors::name`] gives it, `EINTR` and the kernel's restart errors
//! included, and `unknown` when the result was never seen. At the entry of
//! `execve` and `execveat`, [`argument_vector`] reads the arguments that the call
//! passes, which name the program that it starts with [`Call::program`]. Without
//! the process tree, as in a build, no outcome is built and no error is named.
//!
//! Decoding never panics ("Kizz never panics"): the bytes of a file that the
//! kernel loads are read into a buffer reserved with `Vec::try_reserve_exact`, so
//! a size that cannot be allocated is an error of the decoding, and the program
//! headers of an ELF file are taken one by one with `slice::get`, never with
//! `as_chunks`, which panics on a chunk size of zero.

use std::{
    ffi::{OsStr, OsString},
    io,
    net::{SocketAddr, SocketAddrV6},
    os::unix::ffi::{OsStrExt as _, OsStringExt as _},
    path::{Path, PathBuf},
};

use kizz_trace::{
    AbsolutePath, AccessKind, AccessKinds, Accesses, Connection, FileMetadata, Observations,
    Outcome,
};
use libc::{c_int, c_long};

use crate::{
    errors,
    filter::{UNTRACEABLE_SYSTEM_CALLS, numbers},
};

/// The six argument registers of a system call, in order.
pub(crate) type Arguments = [u64; 6];

/// The longest path the kernel reads, with its terminating NUL byte: `PATH_MAX`.
const PATH_MAX: usize = 4096;
/// The size of the chunks in which memory is read, so that no read crosses a page
/// boundary: the smallest `x86_64` page.
const CHUNK: usize = 4096;
/// [`CHUNK`] as an address offset.
const CHUNK_ADDRESS: u64 = 4096;
/// The suffix Linux gives the link of a file or directory that was removed.
const DELETED: &[u8] = b" (deleted)";
/// The lowest result of a failed system call: the largest error number, negated.
const LOWEST_ERROR_RESULT: i64 = -4095;
/// The `RESOLVE_IN_ROOT` flag of `openat2`: the directory descriptor is the root
/// of the lookup.
const RESOLVE_IN_ROOT: u64 = 0x10;
/// The `FAN_MARK_FLUSH` flag of `fanotify_mark`, from the kernel's
/// `include/uapi/linux/fanotify.h`: the call removes marks and names no path.
const FAN_MARK_FLUSH: u64 = 0x80;
/// The errors with which a call that reads or lists extended attributes answers
/// from the attributes themselves: no such attribute (`ENODATA`), a buffer too
/// small for the answer (`ERANGE`), or a file system without attributes
/// (`EOPNOTSUPP`). Such a failure records the xattr kind, not a probe.
const ATTRIBUTE_ANSWERS: [c_int; 3] = [libc::ENODATA, libc::ERANGE, libc::EOPNOTSUPP];
/// The most symbolic links one lookup follows: the kernel's `MAXSYMLINKS`.
const MAX_SYMBOLIC_LINKS: usize = 40;
/// The size of `struct open_how`, the third argument of `openat2`.
const OPEN_HOW_SIZE: usize = 24;
/// The offset of `resolve` in `struct open_how`.
const OPEN_HOW_RESOLVE: usize = 16;
/// How many bytes of a program the kernel reads to tell its format, and in which
/// it looks for a `#!` line: `BINPRM_BUF_SIZE`.
const HEADER_SIZE: usize = 256;
/// The index of the last byte of [`HEADER_SIZE`], which the kernel's `#!` parser
/// never takes as part of the line.
const HEADER_LAST: usize = 255;
/// The deepest file whose format the kernel reads in one `exec`: the program is at
/// depth 0, each interpreter one deeper, and `exec_binprm` fails with `ELOOP`
/// beyond this depth, after opening one more interpreter.
const LAST_HANDLER_DEPTH: usize = 5;
/// The magic bytes that start an ELF file.
const ELF_MAGIC: &[u8] = b"\x7fELF";
/// The offset of the class byte in an ELF header.
const ELF_CLASS: usize = 4;
/// The class byte of a 64-bit ELF file.
const ELF_CLASS_64: u8 = 2;
/// The offset of the byte order byte in an ELF header.
const ELF_DATA: usize = 5;
/// The byte order byte of a little-endian ELF file.
const ELF_DATA_LITTLE: u8 = 1;
/// The size of a 64-bit ELF header.
const ELF_HEADER_SIZE: usize = 64;
/// The offset of `e_type` in a 64-bit ELF header.
const ELF_TYPE: usize = 16;
/// The `e_type` of an executable that is loaded at a fixed address.
const ELF_TYPE_EXEC: u16 = 2;
/// The `e_type` of a position-independent executable or shared object.
const ELF_TYPE_DYN: u16 = 3;
/// The offset of `e_machine` in a 64-bit ELF header.
const ELF_MACHINE: usize = 18;
/// The `e_machine` of `x86_64`: `EM_X86_64`.
const ELF_MACHINE_X86_64: u16 = 62;
/// The offset of `e_phoff`, where the program headers start, in a 64-bit ELF header.
const ELF_PROGRAM_HEADERS: usize = 32;
/// The offset of `e_phentsize` in a 64-bit ELF header.
const ELF_PROGRAM_HEADER_SIZE: usize = 54;
/// The offset of `e_phnum` in a 64-bit ELF header.
const ELF_PROGRAM_HEADER_COUNT: usize = 56;
/// The size of a 64-bit program header, the only `e_phentsize` the kernel accepts.
const PROGRAM_HEADER_SIZE: usize = 56;
/// The most bytes of program headers the kernel reads: one page.
const PROGRAM_HEADERS_LIMIT: usize = 4096;
/// The `p_type` of the program header that names the loader.
const PT_INTERP: u32 = 3;
/// The offset of `p_offset` in a 64-bit program header.
const PROGRAM_HEADER_OFFSET: usize = 8;
/// The offset of `p_filesz` in a 64-bit program header.
const PROGRAM_HEADER_FILE_SIZE: usize = 32;
/// The size of `struct sockaddr_storage`: the longest socket address that `bind`,
/// `connect` and `sendto` accept, and the length to which `sendmsg` and `sendmmsg`
/// cut a longer `msg_namelen`.
const SOCKADDR_STORAGE_SIZE: usize = 128;
/// The size of `struct sockaddr_in`, the shortest IPv4 address the kernel accepts.
const SOCKADDR_IN_SIZE: usize = 16;
/// The shortest IPv6 address the kernel accepts, `SIN6_LEN_RFC2133`: a
/// `struct sockaddr_in6` without its `sin6_scope_id`.
const SOCKADDR_IN6_MINIMUM: usize = 24;
/// The size of `struct sockaddr_in6`, with its `sin6_scope_id`.
const SOCKADDR_IN6_SIZE: usize = 28;
/// The offset of `sun_path` in `struct sockaddr_un`: an address no longer than
/// this is unnamed.
const SUN_PATH_OFFSET: usize = 2;
/// The size of `struct sockaddr_un`, the longest Unix socket address the kernel
/// accepts.
const SOCKADDR_UN_SIZE: usize = 110;
/// The size of the start of `struct msghdr` that the decoder reads: `msg_name`, a
/// pointer, then `msg_namelen`.
const MSGHDR_NAME_SIZE: usize = 12;
/// The offset of `msg_namelen` in `struct msghdr`.
const MSGHDR_NAMELEN: usize = 8;
/// The size of `struct mmsghdr`, the elements of the array of `sendmmsg`: a
/// `struct msghdr` of 56 bytes, then `msg_len` and padding.
const MMSGHDR_SIZE: u64 = 64;
/// The most messages that one `sendmmsg` sends, `UIO_MAXIOV`: the kernel cuts a
/// longer count to it.
const UIO_MAXIOV: u32 = 1024;
/// The errors with which a call on a Unix socket path says that the path was not
/// found, so it made no connection.
const NOT_FOUND: [c_int; 5] = [
    libc::ENOENT,
    libc::ENOTDIR,
    libc::ELOOP,
    libc::ENAMETOOLONG,
    libc::EACCES,
];

/// A file system object, as `stat` identifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Object {
    /// The device that holds the object.
    pub device: u64,
    /// The object's inode number on its device.
    pub inode: u64,
    /// How many names the object has; a removed object has none.
    pub links: u64,
    /// Whether the object is a symbolic link.
    pub symbolic_link: bool,
    /// Whether the object is a directory, which a lookup can continue through.
    pub directory: bool,
}

impl Object {
    /// Whether `other` is the same object.
    fn same(
        self,
        other: Self,
    ) -> bool {
        (self.device, self.inode) == (other.device, other.inode)
    }
}

/// What decoding reads from a traced thread stopped at a system call, and from the
/// file system as the tracer sees it.
pub(crate) trait Inspect {
    /// Copy the thread's memory at `address` into `buffer`, and return the number
    /// of bytes copied.
    ///
    /// # Errors
    ///
    /// Returns the error of the read, such as `EFAULT` for an unmapped address.
    fn memory(
        &self,
        address: u64,
        buffer: &mut [u8],
    ) -> io::Result<usize>;

    /// The target of the thread's working directory link, `/proc/<tid>/cwd`.
    ///
    /// # Errors
    ///
    /// Returns the error of reading the link.
    fn working_directory(&self) -> io::Result<Vec<u8>>;

    /// The target of the link of the thread's descriptor `fd`,
    /// `/proc/<tid>/fd/<fd>`.
    ///
    /// # Errors
    ///
    /// Returns the error of reading the link, such as `ENOENT` for a descriptor
    /// that is not open.
    fn descriptor(
        &self,
        fd: c_int,
    ) -> io::Result<Vec<u8>>;

    /// The object behind the thread's descriptor `fd`, or behind its working
    /// directory for `AT_FDCWD`, which `stat` reaches through the link in `/proc`
    /// even when the object was removed.
    ///
    /// # Errors
    ///
    /// Returns the error of `stat`, such as `ENOENT` for a descriptor that is not
    /// open.
    fn link_object(
        &self,
        fd: c_int,
    ) -> io::Result<Object>;

    /// The object at `path`, without following a symbolic link in its last
    /// component, as `lstat` reports it.
    ///
    /// # Errors
    ///
    /// Returns the error of `lstat`, such as `ENOENT` for a path that names
    /// nothing.
    fn path_object(
        &self,
        path: &Path,
    ) -> io::Result<Object>;

    /// The target of the symbolic link at `path`.
    ///
    /// # Errors
    ///
    /// Returns the error of reading the link.
    fn link_target(
        &self,
        path: &Path,
    ) -> io::Result<Vec<u8>>;

    /// Read the regular file at `path`, following symbolic links, from `offset`
    /// into `buffer`, and return the number of bytes read, which is less than the
    /// buffer only at the end of the file.
    ///
    /// # Errors
    ///
    /// Returns the error of opening or reading the file, or an error for a path
    /// that does not name a regular file, which is never opened.
    fn read_file(
        &self,
        path: &Path,
        offset: u64,
        buffer: &mut [u8],
    ) -> io::Result<usize>;

    /// The metadata of the regular file behind the thread's descriptor `fd`, which
    /// `stat` reaches through the link in `/proc`, or `None` when the descriptor
    /// names no regular file or its metadata cannot be read.
    fn descriptor_file(
        &self,
        fd: c_int,
    ) -> Option<FileMetadata>;

    /// The metadata of the regular file at `path`, following symbolic links, or
    /// `None` when `path` names no regular file or its metadata cannot be read.
    fn path_file(
        &self,
        path: &Path,
    ) -> Option<FileMetadata>;
}

/// When an empty or null path argument names the directory descriptor itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Empty {
    /// Never: an empty path fails without a lookup, and a null path is a fault.
    Never,
    /// An empty path always names the descriptor, as for `readlinkat`.
    Always,
    /// With `AT_EMPTY_PATH` in the flags argument at this index, an empty or null
    /// path names the descriptor.
    Flag(usize),
    /// A null path names the descriptor, as for `futimesat`.
    Null,
    /// A null path names the descriptor, and with `AT_EMPTY_PATH` in the flags
    /// argument at this index so does an empty path, as for `utimensat`.
    NullOrFlag(usize),
}

/// How a path argument is looked up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lookup {
    /// As most calls look a path up: a relative path from the directory descriptor,
    /// and an absolute path from `/`.
    Ordinary,
    /// As `openat2` with `RESOLVE_IN_ROOT` looks it up, with the directory
    /// descriptor as the root.
    InRoot {
        /// Whether a symbolic link in the last component is followed.
        follow_last: bool,
    },
}

/// Where a call finds one of the paths it touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operand {
    /// The path in the argument at `path`, relative to the directory descriptor in
    /// the argument at `dirfd`, or to the working directory when `dirfd` is `None`.
    Path {
        /// The index of the directory descriptor argument, if the call has one.
        dirfd: Option<usize>,
        /// The index of the path argument.
        path: usize,
        /// When an empty or null path names the directory descriptor.
        empty: Empty,
    },
    /// The path behind the descriptor in the argument at this index.
    Descriptor(usize),
    /// The path of `openat2`, relative to its directory descriptor, with its flags
    /// and resolution in the `struct open_how` that the third argument points to.
    OpenHow,
    /// The path that `fanotify_mark` marks: its fifth argument, relative to the
    /// directory descriptor in its fourth, where a null path names the descriptor
    /// itself, and no path at all with `FAN_MARK_FLUSH` in the flags of its second.
    FanotifyMark,
    /// The socket address that the argument at `address` points to, as long as the
    /// argument at `length` says: a Unix socket path in it has the rule's kinds,
    /// and with `connects`, its destination is a connection.
    SocketAddress {
        /// The index of the address argument.
        address: usize,
        /// The index of the length argument.
        length: usize,
        /// Whether the destination is a connection: for every call but `bind`.
        connects: bool,
    },
    /// The destination of `sendmsg`: the `msg_name` of the `struct msghdr` that its
    /// second argument points to.
    MessageName,
    /// The destinations of `sendmmsg`: the `msg_name` of each `struct mmsghdr` of
    /// the array that its second argument points to, as many as its third argument
    /// says.
    MessageNames,
}

/// The access kinds a call records for a path when it succeeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kinds {
    /// Always these kinds.
    Fixed(AccessKinds),
    /// The kinds of the open flags in the argument at this index.
    OpenFlags(usize),
    /// The kinds of the open flags in `openat2`'s `struct open_how`.
    OpenHow,
}

/// One path that a call touches, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rule {
    /// Where the call finds the path.
    operand: Operand,
    /// The kinds it records when it succeeds.
    kinds: Kinds,
}

/// How to decode one tracked system call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SystemCall {
    /// The `x86_64` number of the call.
    pub number: c_long,
    /// The `x86_64` name of the call.
    pub name: &'static str,
    /// Every path the call touches, in argument order.
    rules: &'static [Rule],
}

/// A read when the call succeeds.
const READ: Kinds = Kinds::Fixed(AccessKinds::only(AccessKind::Read));
/// A probe when the call succeeds.
const PROBE: Kinds = Kinds::Fixed(AccessKinds::only(AccessKind::Probe));
/// A listing when the call succeeds.
const LIST: Kinds = Kinds::Fixed(AccessKinds::only(AccessKind::List));
/// A write when the call succeeds.
const WRITE: Kinds = Kinds::Fixed(AccessKinds::only(AccessKind::Write));
/// A program run when the call succeeds.
const EXEC: Kinds = Kinds::Fixed(AccessKinds::only(AccessKind::Exec));
/// Extended attributes read or listed when the call succeeds, and also when it
/// fails with one of [`ATTRIBUTE_ANSWERS`].
const XATTR: Kinds = Kinds::Fixed(AccessKinds::only(AccessKind::Xattr));

/// The rule for the path in argument `path`, relative to the working directory.
const fn path(
    path: usize,
    kinds: Kinds,
) -> Rule {
    Rule {
        operand: Operand::Path {
            dirfd: None,
            path,
            empty: Empty::Never,
        },
        kinds,
    }
}

/// The rule for the path in argument `path`, relative to the directory descriptor
/// in argument `dirfd`.
const fn at(
    dirfd: usize,
    path: usize,
    empty: Empty,
    kinds: Kinds,
) -> Rule {
    Rule {
        operand: Operand::Path {
            dirfd: Some(dirfd),
            path,
            empty,
        },
        kinds,
    }
}

/// The rule for the path behind the descriptor in argument `fd`.
const fn descriptor(
    fd: usize,
    kinds: Kinds,
) -> Rule {
    Rule {
        operand: Operand::Descriptor(fd),
        kinds,
    }
}

/// The rule for the socket address of `bind` or `connect`, which the second
/// argument points to, as long as the third says; with `connects`, its destination
/// is a connection.
const fn socket_address(
    connects: bool,
    kinds: Kinds,
) -> Rule {
    Rule {
        operand: Operand::SocketAddress {
            address: 1,
            length: 2,
            connects,
        },
        kinds,
    }
}

/// A tracked system call and its rules.
const fn call(
    number: c_long,
    name: &'static str,
    rules: &'static [Rule],
) -> SystemCall {
    SystemCall {
        number,
        name,
        rules,
    }
}

/// How every tracked system call is decoded, in the order of the filter's list.
///
/// Arguments are counted from zero. The kinds are those of a call that succeeded;
/// a failed call records a probe for each path instead.
pub(crate) const SYSTEM_CALLS: &[SystemCall] = &[
    // Opening files: the kinds follow the open flags.
    call(libc::SYS_open, "open", &[path(0, Kinds::OpenFlags(1))]),
    call(libc::SYS_openat, "openat", &[at(
        0,
        1,
        Empty::Never,
        Kinds::OpenFlags(2),
    )]),
    call(libc::SYS_openat2, "openat2", &[Rule {
        operand: Operand::OpenHow,
        kinds: Kinds::OpenHow,
    }]),
    call(libc::SYS_creat, "creat", &[path(0, WRITE)]),
    // Metadata and existence.
    call(libc::SYS_stat, "stat", &[path(0, PROBE)]),
    call(libc::SYS_lstat, "lstat", &[path(0, PROBE)]),
    call(libc::SYS_newfstatat, "newfstatat", &[at(
        0,
        1,
        Empty::Flag(3),
        PROBE,
    )]),
    call(libc::SYS_statx, "statx", &[at(0, 1, Empty::Flag(2), PROBE)]),
    call(libc::SYS_access, "access", &[path(0, PROBE)]),
    call(libc::SYS_faccessat, "faccessat", &[at(
        0,
        1,
        Empty::Never,
        PROBE,
    )]),
    call(libc::SYS_faccessat2, "faccessat2", &[at(
        0,
        1,
        Empty::Flag(3),
        PROBE,
    )]),
    // `file_getattr` reads the file attributes of a path, such as the immutable
    // flag, and `name_to_handle_at` its file handle: both look the path up.
    call(numbers::SYS_FILE_GETATTR, "file_getattr", &[at(
        0,
        1,
        Empty::Flag(4),
        PROBE,
    )]),
    call(libc::SYS_name_to_handle_at, "name_to_handle_at", &[at(
        0,
        1,
        Empty::Flag(4),
        PROBE,
    )]),
    // File systems: the figures of the file system that holds a path, or the path
    // behind a descriptor.
    call(libc::SYS_statfs, "statfs", &[path(0, PROBE)]),
    call(libc::SYS_fstatfs, "fstatfs", &[descriptor(0, PROBE)]),
    // Watches: a watch looks up the path it watches.
    call(libc::SYS_inotify_add_watch, "inotify_add_watch", &[path(
        1, PROBE,
    )]),
    call(libc::SYS_fanotify_mark, "fanotify_mark", &[Rule {
        operand: Operand::FanotifyMark,
        kinds: PROBE,
    }]),
    // Extended attributes: reading or listing them records the xattr kind, and
    // setting or removing one is a write. The `at` calls take their flags third.
    call(libc::SYS_getxattr, "getxattr", &[path(0, XATTR)]),
    call(libc::SYS_lgetxattr, "lgetxattr", &[path(0, XATTR)]),
    call(libc::SYS_fgetxattr, "fgetxattr", &[descriptor(0, XATTR)]),
    call(numbers::SYS_GETXATTRAT, "getxattrat", &[at(
        0,
        1,
        Empty::Flag(2),
        XATTR,
    )]),
    call(libc::SYS_listxattr, "listxattr", &[path(0, XATTR)]),
    call(libc::SYS_llistxattr, "llistxattr", &[path(0, XATTR)]),
    call(libc::SYS_flistxattr, "flistxattr", &[descriptor(0, XATTR)]),
    call(numbers::SYS_LISTXATTRAT, "listxattrat", &[at(
        0,
        1,
        Empty::Flag(2),
        XATTR,
    )]),
    call(libc::SYS_setxattr, "setxattr", &[path(0, WRITE)]),
    call(libc::SYS_lsetxattr, "lsetxattr", &[path(0, WRITE)]),
    call(libc::SYS_fsetxattr, "fsetxattr", &[descriptor(0, WRITE)]),
    call(numbers::SYS_SETXATTRAT, "setxattrat", &[at(
        0,
        1,
        Empty::Flag(2),
        WRITE,
    )]),
    call(libc::SYS_removexattr, "removexattr", &[path(0, WRITE)]),
    call(libc::SYS_lremovexattr, "lremovexattr", &[path(0, WRITE)]),
    call(libc::SYS_fremovexattr, "fremovexattr", &[descriptor(
        0, WRITE,
    )]),
    call(numbers::SYS_REMOVEXATTRAT, "removexattrat", &[at(
        0,
        1,
        Empty::Flag(2),
        WRITE,
    )]),
    // Links: reading a link reads its content, the target; creating a link writes
    // the new name, and a hard link also changes the existing file's link count.
    // The target of a symbolic link is only text, so it is not a path touched.
    call(libc::SYS_readlink, "readlink", &[path(0, READ)]),
    call(libc::SYS_readlinkat, "readlinkat", &[at(
        0,
        1,
        Empty::Always,
        READ,
    )]),
    call(libc::SYS_link, "link", &[path(0, WRITE), path(1, WRITE)]),
    call(libc::SYS_linkat, "linkat", &[
        at(0, 1, Empty::Flag(4), WRITE),
        at(2, 3, Empty::Never, WRITE),
    ]),
    call(libc::SYS_symlink, "symlink", &[path(1, WRITE)]),
    call(libc::SYS_symlinkat, "symlinkat", &[at(
        1,
        2,
        Empty::Never,
        WRITE,
    )]),
    // Directory listings, of the directory behind the descriptor.
    call(libc::SYS_getdents, "getdents", &[descriptor(0, LIST)]),
    call(libc::SYS_getdents64, "getdents64", &[descriptor(0, LIST)]),
    // Programs.
    call(libc::SYS_execve, "execve", &[path(0, EXEC)]),
    call(libc::SYS_execveat, "execveat", &[at(
        0,
        1,
        Empty::Flag(4),
        EXEC,
    )]),
    // The working directory: entering a directory checks that it exists and is a
    // directory.
    call(libc::SYS_chdir, "chdir", &[path(0, PROBE)]),
    call(libc::SYS_fchdir, "fchdir", &[descriptor(0, PROBE)]),
    // Changes.
    call(libc::SYS_rename, "rename", &[
        path(0, WRITE),
        path(1, WRITE),
    ]),
    call(libc::SYS_renameat, "renameat", &[
        at(0, 1, Empty::Never, WRITE),
        at(2, 3, Empty::Never, WRITE),
    ]),
    call(libc::SYS_renameat2, "renameat2", &[
        at(0, 1, Empty::Never, WRITE),
        at(2, 3, Empty::Never, WRITE),
    ]),
    call(libc::SYS_unlink, "unlink", &[path(0, WRITE)]),
    call(libc::SYS_unlinkat, "unlinkat", &[at(
        0,
        1,
        Empty::Never,
        WRITE,
    )]),
    call(libc::SYS_rmdir, "rmdir", &[path(0, WRITE)]),
    call(libc::SYS_mkdir, "mkdir", &[path(0, WRITE)]),
    call(libc::SYS_mkdirat, "mkdirat", &[at(
        0,
        1,
        Empty::Never,
        WRITE,
    )]),
    call(libc::SYS_mknod, "mknod", &[path(0, WRITE)]),
    call(libc::SYS_mknodat, "mknodat", &[at(
        0,
        1,
        Empty::Never,
        WRITE,
    )]),
    call(libc::SYS_truncate, "truncate", &[path(0, WRITE)]),
    call(libc::SYS_chmod, "chmod", &[path(0, WRITE)]),
    call(libc::SYS_fchmodat, "fchmodat", &[at(
        0,
        1,
        Empty::Never,
        WRITE,
    )]),
    call(numbers::SYS_FCHMODAT2, "fchmodat2", &[at(
        0,
        1,
        Empty::Flag(3),
        WRITE,
    )]),
    call(libc::SYS_chown, "chown", &[path(0, WRITE)]),
    call(libc::SYS_lchown, "lchown", &[path(0, WRITE)]),
    call(libc::SYS_fchownat, "fchownat", &[at(
        0,
        1,
        Empty::Flag(4),
        WRITE,
    )]),
    call(libc::SYS_utime, "utime", &[path(0, WRITE)]),
    call(libc::SYS_utimes, "utimes", &[path(0, WRITE)]),
    call(libc::SYS_utimensat, "utimensat", &[at(
        0,
        1,
        Empty::NullOrFlag(3),
        WRITE,
    )]),
    call(libc::SYS_futimesat, "futimesat", &[at(
        0,
        1,
        Empty::Null,
        WRITE,
    )]),
    // `file_setattr` sets the file attributes that `file_getattr` reads.
    call(numbers::SYS_FILE_SETATTR, "file_setattr", &[at(
        0,
        1,
        Empty::Flag(4),
        WRITE,
    )]),
    // Changes through a descriptor, to the path behind it.
    call(libc::SYS_ftruncate, "ftruncate", &[descriptor(0, WRITE)]),
    call(libc::SYS_fchmod, "fchmod", &[descriptor(0, WRITE)]),
    call(libc::SYS_fchown, "fchown", &[descriptor(0, WRITE)]),
    call(libc::SYS_fallocate, "fallocate", &[descriptor(0, WRITE)]),
    // Sockets: binding a Unix socket path writes it, and connecting or sending to
    // one probes it.
    call(libc::SYS_bind, "bind", &[socket_address(false, WRITE)]),
    call(libc::SYS_connect, "connect", &[socket_address(true, PROBE)]),
    call(libc::SYS_sendto, "sendto", &[Rule {
        operand: Operand::SocketAddress {
            address: 4,
            length: 5,
            connects: true,
        },
        kinds: PROBE,
    }]),
    call(libc::SYS_sendmsg, "sendmsg", &[Rule {
        operand: Operand::MessageName,
        kinds: PROBE,
    }]),
    call(libc::SYS_sendmmsg, "sendmmsg", &[Rule {
        operand: Operand::MessageNames,
        kinds: PROBE,
    }]),
];

/// A tracked call decoded at its entry, waiting for its result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    /// The `x86_64` name of the call.
    name: &'static str,
    /// Every path the call touches, with the kinds it records when it succeeds.
    operands: Vec<(AbsolutePath, AccessKinds)>,
    /// The first thing that could not be decoded, if any.
    problem: Option<String>,
    /// The index in `operands` of the object that a rooted `openat2` lookup was
    /// repeated to find, which the descriptor it returns names.
    reached: Option<usize>,
    /// For `execve` and `execveat`, the working directory the thread had when it
    /// entered the call, against which the kernel resolves a relative interpreter
    /// or loader path; `None` for every other call.
    directory: Option<Directory>,
    /// Why the tracer could not follow the program that the call ran, which makes
    /// the trace incomplete.
    unfollowed: Option<String>,
    /// What the call reads or runs first, whose metadata [`Call::first_reads`]
    /// gives when the call succeeds.
    first_read: FirstRead,
    /// The destinations that the call connects or sends to, which
    /// [`Call::connections`] gives at its exit.
    connections: Vec<Connection>,
}

/// The files whose metadata a call gives when it succeeds, as reads or runs that
/// may come before any write of their paths in the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstRead {
    /// None: the call opens nothing for reading and runs nothing, or it created or
    /// truncated what it opened, which writes it first.
    Nothing,
    /// The regular file that an opening call opened for reading, at this index of
    /// its operands, through the descriptor that the call returns.
    Opened(usize),
    /// The program that an `exec` ran, and each interpreter and loader that the
    /// kernel loaded to run it, through their paths: every operand of the call.
    Ran,
}

/// The working directory against which the kernel resolves a relative interpreter
/// or loader path.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Directory {
    /// The directory at this path.
    Path(AbsolutePath),
    /// A directory that was removed, in which every lookup fails, or that names no
    /// path.
    Removed,
    /// A directory that could not be read, with the reason.
    Unknown(String),
}

impl Directory {
    /// The path of the interpreter or loader `name`, resolved as the kernel's
    /// `open_exec` resolves it: an absolute path as it is, and a relative one
    /// against this working directory, or `None` when the directory was removed.
    ///
    /// # Errors
    ///
    /// Returns why a relative path cannot be resolved: the directory is unknown.
    fn resolve(
        &self,
        name: &[u8],
    ) -> Result<Option<AbsolutePath>, String> {
        if name.starts_with(b"/") {
            return Ok(AbsolutePath::new(OsStr::from_bytes(name)).ok());
        }
        match self {
            Self::Path(base) => Ok(Some(base.join(OsStr::from_bytes(name)))),
            Self::Removed => Ok(None),
            Self::Unknown(problem) => {
                Err(format!(
                    "it names the relative path {}, and {problem}",
                    String::from_utf8_lossy(name)
                ))
            },
        }
    }
}

impl Call {
    /// Check the path of a rooted `openat2` against the descriptor the call
    /// returned as `result`, which names the object the kernel actually opened.
    ///
    /// The thread is at the call's exit stop, so the descriptor is open. When it
    /// names another path than the repeated lookup found, its path replaces the
    /// lookup's, and the trace is incomplete, because the other components of the
    /// lookup may differ too. A descriptor that cannot be read, or whose path
    /// cannot be told, also makes the trace incomplete rather than kept as if
    /// certain. Any other call, or one that failed, is left as it is.
    pub(crate) fn opened(
        &mut self,
        result: Option<i64>,
        inspect: &(impl Inspect + ?Sized),
    ) {
        let Some(index) = self.reached else {
            return;
        };
        let Some(fd) = result
            .filter(|result| *result >= 0)
            .and_then(|result| c_int::try_from(result).ok())
        else {
            return;
        };
        let Some((walked, _)) = self.operands.get_mut(index) else {
            return;
        };
        let problem = match inspect.descriptor(fd) {
            Ok(link) => {
                match link_path(&link) {
                    Some(opened) if opened == *walked => return,
                    Some(opened) if !link.ends_with(DELETED) => {
                        let problem =
                            format!("the descriptor it returned names {opened}, not {walked}");
                        *walked = opened;
                        problem
                    },
                    _ => {
                        format!(
                            "the descriptor it returned names {}, not {walked}",
                            String::from_utf8_lossy(&link)
                        )
                    },
                }
            },
            Err(error) => format!("could not read the descriptor {fd} it returned: {error}"),
        };
        if self.problem.is_none() {
            self.problem = Some(problem);
        }
    }

    /// Record what the kernel loads to run the program of an `execve` or
    /// `execveat` that returned `result`, or whose result was never seen, with the
    /// kinds of the program's own path, so [`Call::finish_observed`] records them as it
    /// records that path. Any other call is left as it is.
    ///
    /// The tracer reads the program's files as the kernel does: a `#!` line names
    /// an interpreter, which is read the same way as deep as the kernel follows
    /// interpreters, and an `x86_64` ELF executable names its loader in a
    /// `PT_INTERP` program header. When the call succeeded, a file that the tracer
    /// cannot read, or whose format it does not know, makes the trace incomplete;
    /// when it failed, or its result is unknown, the program never ran, and only
    /// the paths that were found are recorded.
    pub(crate) fn loaded(
        &mut self,
        result: Option<i64>,
        inspect: &(impl Inspect + ?Sized),
    ) {
        let Some(directory) = self.directory.take() else {
            return;
        };
        let ran = result.is_some_and(|result| !(LOWEST_ERROR_RESULT..0).contains(&result));
        let Some((program, kinds)) = self.operands.first().cloned() else {
            if ran && self.problem.is_none() {
                self.unfollowed = Some(format!(
                    "{} ran a program whose path the tracer cannot tell, so it cannot read it",
                    self.name
                ));
            }
            return;
        };
        let mut loads = Vec::new();
        let failure = follow_program(inspect, &directory, &program, &mut loads).err();
        self.operands
            .extend(loads.into_iter().map(|path| (path, kinds)));
        if ran && let Some(detail) = failure {
            self.unfollowed = Some(format!("{} ran {program}, but {detail}", self.name));
        }
    }

    /// The metadata of each regular file that the call, which returned `result`,
    /// opened for reading or ran, with its path, in operand order: for an opening
    /// call with read access that neither created nor truncated the file, the file
    /// behind the descriptor it returned; for `execve` and `execveat`, after
    /// [`Call::loaded`], the program and each interpreter and loader, through their
    /// paths. A call that failed, or whose result was never seen, gives nothing,
    /// and so does a path that names no regular file.
    pub(crate) fn first_reads(
        &self,
        result: Option<i64>,
        inspect: &(impl Inspect + ?Sized),
    ) -> Vec<(AbsolutePath, FileMetadata)> {
        let Some(result) = result.filter(|result| !(LOWEST_ERROR_RESULT..0).contains(result))
        else {
            return Vec::new();
        };
        match self.first_read {
            FirstRead::Nothing => Vec::new(),
            FirstRead::Opened(index) => {
                let (Some((path, _)), Ok(fd)) = (self.operands.get(index), c_int::try_from(result))
                else {
                    return Vec::new();
                };
                inspect
                    .descriptor_file(fd)
                    .map(|metadata| (path.clone(), metadata))
                    .into_iter()
                    .collect()
            },
            FirstRead::Ran => {
                self.operands
                    .iter()
                    .filter_map(|(path, _)| {
                        inspect
                            .path_file(path.as_path())
                            .map(|metadata| (path.clone(), metadata))
                    })
                    .collect()
            },
        }
    }

    /// The connections of the call, now that it returned `result`, or `None` when
    /// its result was never seen: each IPv4 or IPv6 destination whatever the
    /// outcome, and each Unix socket, by path or abstract name, unless the call
    /// failed with one of [`NOT_FOUND`], which say that its path was not found.
    /// `bind`, and every call that is not a socket call, gives nothing.
    pub(crate) fn connections(
        &self,
        result: Option<i64>,
    ) -> Vec<Connection> {
        let not_found = result
            .filter(|result| (LOWEST_ERROR_RESULT..0).contains(result))
            .and_then(i64::checked_neg)
            .is_some_and(|errno| NOT_FOUND.into_iter().any(|code| i64::from(code) == errno));
        self.connections
            .iter()
            .filter(|connection| matches!(connection, Connection::Ip(_)) || !not_found)
            .cloned()
            .collect()
    }

    /// Record the paths of the call, now that it returned `result`, or `None` when
    /// its result was never seen, and return a description of what could not be
    /// decoded, or of a program that ran but that [`Call::loaded`] could not
    /// follow, when that makes the trace incomplete.
    ///
    /// A call that failed records a probe for each path. That includes a call that a
    /// signal interrupted, which returns `EINTR`, or one of the kernel's internal
    /// restart errors when it restarts: either way its lookup happened, and a call
    /// that restarts is decoded again at its next entry. A call that reads or lists
    /// extended attributes and fails with one of [`ATTRIBUTE_ANSWERS`] records the
    /// xattr kind instead, because that answer depends on the attributes.
    ///
    /// A part of the call that could not be decoded makes the trace incomplete,
    /// unless the call failed with the error the kernel gives that same input
    /// before it looks anything up: a bad address, a bad descriptor, or a path that
    /// is too long.
    ///
    /// With `observations`, each path is also recorded into them, with the same
    /// kinds, each kind with the call's outcome, which [`Call::outcome`] gives.
    /// Without them, as for a build, the outcome is never built, so its error name
    /// is neither looked up nor allocated, and the call costs what it did before
    /// the process tree existed.
    pub(crate) fn finish_observed(
        self,
        result: Option<i64>,
        accesses: &mut Accesses,
        observations: Option<&mut Observations>,
    ) -> Option<String> {
        let errno = result
            .filter(|result| (LOWEST_ERROR_RESULT..0).contains(result))
            .and_then(i64::checked_neg);
        if let Some(errno) = errno {
            let rejected = [libc::EFAULT, libc::EBADF, libc::ENAMETOOLONG]
                .into_iter()
                .any(|code| i64::from(code) == errno);
            if self.problem.is_some() && rejected {
                return None;
            }
        }
        let answered = errno.is_some_and(|errno| {
            ATTRIBUTE_ANSWERS
                .into_iter()
                .any(|code| i64::from(code) == errno)
        });
        let mut observed = observations.map(|observations| (observations, Self::outcome(result)));
        for (path, kinds) in self.operands {
            let recorded = match (result, errno) {
                (Some(_), Some(_)) if answered && kinds.contains(AccessKind::Xattr) => {
                    AccessKinds::only(AccessKind::Xattr)
                },
                (Some(_), Some(_)) => AccessKinds::only(AccessKind::Probe),
                (Some(_), None) => kinds,
                (None, _) => kinds.with(AccessKind::Probe),
            };
            if let Some((observations, outcome)) = observed.as_mut() {
                observations.record_kinds(path.clone(), recorded, outcome);
            }
            accesses.record_kinds(path, recorded);
        }
        if let Some(problem) = self.problem {
            return Some(format!("could not decode {}: {problem}", self.name));
        }
        self.unfollowed
    }

    /// Record the paths of the call as [`Call::finish_observed`] does, without
    /// observations, as a build's tracer records them.
    #[cfg(test)]
    pub(crate) fn finish(
        self,
        result: Option<i64>,
        accesses: &mut Accesses,
    ) -> Option<String> {
        self.finish_observed(result, accesses, None)
    }

    /// The outcome of a call that returned `result`, or whose result was never
    /// seen: `ok` for a success, the name of the error for a failure, and `unknown`
    /// for a result never seen.
    pub(crate) fn outcome(result: Option<i64>) -> Outcome {
        #[cfg(test)]
        OUTCOMES_BUILT.with(|built| built.set(built.get().saturating_add(1)));
        match result {
            None => Outcome::Unknown,
            Some(result) if (LOWEST_ERROR_RESULT..0).contains(&result) => {
                Outcome::Error(errors::name(result.saturating_neg()))
            },
            Some(_) => Outcome::Ok,
        }
    }

    /// The path of the program that an `execve` or `execveat` names, resolved as
    /// its observation is, or `None` for any other call, or when the path could not
    /// be decoded.
    pub(crate) fn program(&self) -> Option<&AbsolutePath> {
        self.operands
            .first()
            .filter(|_| self.first_read == FirstRead::Ran)
            .map(|(path, _)| path)
    }
}

#[cfg(test)]
thread_local! {
    /// The number of outcomes that [`Call::outcome`] has built on this thread, so
    /// a test can prove that a call recorded without observations builds none.
    static OUTCOMES_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The most bytes of one argument, its terminating NUL byte included, that the
/// kernel copies for an `exec`: `MAX_ARG_STRLEN`, 32 pages.
const MAX_ARGUMENT: usize = 131_072;

/// The most arguments that the kernel accepts in one vector: `MAX_ARG_STRINGS`.
const MAX_ARGUMENTS: usize = 0x7FFF_FFFF;

/// The size of a pointer in an argument vector.
const POINTER_SIZE: u64 = 8;

/// The argument vector that the `execve` or `execveat` call `number` with
/// `arguments` passes, read from the stopped thread at the call's entry, as the
/// kernel reads it: a null-terminated array of pointers to NUL-terminated strings,
/// and a null array is empty. A vector that cannot be read, or that the kernel
/// would refuse for an argument that is too long, is left empty, and so is any
/// other call's.
pub(crate) fn argument_vector(
    number: c_long,
    arguments: Arguments,
    inspect: &(impl Inspect + ?Sized),
) -> Vec<OsString> {
    let address = match number {
        libc::SYS_execve => arguments[1],
        libc::SYS_execveat => arguments[2],
        _ => return Vec::new(),
    };
    read_argument_vector(inspect, address).unwrap_or_default()
}

/// The argument vector at `address`, as [`argument_vector`] describes it, or `None`
/// when it cannot be read.
fn read_argument_vector(
    inspect: &(impl Inspect + ?Sized),
    address: u64,
) -> Option<Vec<OsString>> {
    let mut vector = Vec::new();
    if address == 0 {
        return Some(vector);
    }
    let mut buffer = vec![0_u8; MAX_ARGUMENT];
    let mut slot = address;
    loop {
        let mut pointer = [0_u8; 8];
        if read_memory(inspect, slot, &mut pointer, false).ok()? != pointer.len() {
            return None;
        }
        let pointer = u64::from_ne_bytes(pointer);
        if pointer == 0 {
            return Some(vector);
        }
        if vector.len() >= MAX_ARGUMENTS {
            return None;
        }
        let filled = read_memory(inspect, pointer, &mut buffer, true).ok()?;
        let bytes = buffer.get(..filled)?;
        let end = bytes.iter().position(|byte| *byte == 0)?;
        vector.push(OsString::from_vec(bytes.get(..end)?.to_vec()));
        slot = slot.checked_add(POINTER_SIZE)?;
    }
}

/// What the kernel finds at the start of a file that it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Format {
    /// A `#!` script, with the interpreter path that its first line names.
    Script(Vec<u8>),
    /// An `x86_64` ELF executable, with the fields of its header that locate its
    /// program headers.
    Elf(ElfHeader),
    /// Neither, which the kernel runs only through another handler, such as
    /// `binfmt_misc`, or not at all.
    Unknown,
}

/// The fields of a 64-bit ELF header that locate its program headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ElfHeader {
    /// Where the program headers start in the file: `e_phoff`.
    offset: u64,
    /// The size of one program header: `e_phentsize`.
    entry_size: u16,
    /// How many program headers there are: `e_phnum`.
    count: u16,
}

/// The little-endian `u16` at `offset` in `bytes`.
fn u16_at(
    bytes: &[u8],
    offset: usize,
) -> Option<u16> {
    bytes
        .get(offset..)
        .and_then(<[u8]>::first_chunk::<2>)
        .map(|field| u16::from_le_bytes(*field))
}

/// The little-endian `u32` at `offset` in `bytes`.
fn u32_at(
    bytes: &[u8],
    offset: usize,
) -> Option<u32> {
    bytes
        .get(offset..)
        .and_then(<[u8]>::first_chunk::<4>)
        .map(|field| u32::from_le_bytes(*field))
}

/// The little-endian `u64` at `offset` in `bytes`.
fn u64_at(
    bytes: &[u8],
    offset: usize,
) -> Option<u64> {
    bytes
        .get(offset..)
        .and_then(<[u8]>::first_chunk::<8>)
        .map(|field| u64::from_le_bytes(*field))
}

/// The interpreter path that the `#!` line at the start of `header` names, parsed
/// as the kernel's `load_script` parses it, or `None` when the kernel finds none,
/// or finds an empty one, which it fails to look up.
///
/// The kernel looks only at the first [`HEADER_SIZE`] bytes, padded with NUL bytes.
/// The line ends at the first newline before any NUL byte. Without one, the line
/// is cut at the end of the buffer, unless the buffer holds only spaces and tabs
/// after `#!`, or the text after them has no space, tab or NUL byte, which means
/// that the interpreter path itself was cut. Spaces and tabs at the end of the
/// line are dropped, and the interpreter is the text after the spaces and tabs
/// that follow `#!`, up to the next space, tab or NUL byte; any text after that
/// is an argument, which the kernel does not load.
fn script_interpreter(header: &[u8]) -> Option<Vec<u8>> {
    let mut buffer = [0_u8; HEADER_SIZE];
    for (slot, byte) in buffer.iter_mut().zip(header) {
        *slot = *byte;
    }
    if !buffer.starts_with(b"#!") {
        return None;
    }
    let byte = |index: usize| buffer.get(index).copied().unwrap_or_default();
    let blank = |index: usize| matches!(byte(index), b' ' | b'\t');
    let terminator = |index: usize| blank(index) || byte(index) == 0;
    let newline = buffer
        .iter()
        .position(|value| matches!(*value, b'\n' | 0))
        .filter(|index| byte(*index) == b'\n');
    let mut end = if let Some(newline) = newline {
        newline
    } else {
        let start = (2..HEADER_LAST).find(|index| !blank(*index))?;
        (start..HEADER_LAST).find(|index| terminator(*index))?;
        HEADER_LAST
    };
    while let Some(before) = end.checked_sub(1)
        && blank(before)
    {
        end = before;
    }
    let start = (2..end).find(|index| !blank(*index))?;
    let stop = (start..end).find(|index| terminator(*index)).unwrap_or(end);
    buffer
        .get(start..stop)
        .filter(|name| !name.is_empty())
        .map(<[u8]>::to_vec)
}

/// The header of the `x86_64` ELF executable that `header` starts, or `None` when
/// it starts no such file: a 64-bit, little-endian ELF executable or shared object
/// for `x86_64`, with its whole ELF header.
fn elf_header(header: &[u8]) -> Option<ElfHeader> {
    let executable = header.len() >= ELF_HEADER_SIZE
        && header.starts_with(ELF_MAGIC)
        && header.get(ELF_CLASS) == Some(&ELF_CLASS_64)
        && header.get(ELF_DATA) == Some(&ELF_DATA_LITTLE)
        && matches!(u16_at(header, ELF_TYPE), Some(ELF_TYPE_EXEC | ELF_TYPE_DYN))
        && u16_at(header, ELF_MACHINE) == Some(ELF_MACHINE_X86_64);
    if !executable {
        return None;
    }
    Some(ElfHeader {
        offset: u64_at(header, ELF_PROGRAM_HEADERS)?,
        entry_size: u16_at(header, ELF_PROGRAM_HEADER_SIZE)?,
        count: u16_at(header, ELF_PROGRAM_HEADER_COUNT)?,
    })
}

/// The format of the file whose first bytes are `header`.
fn format(header: &[u8]) -> Format {
    if let Some(interpreter) = script_interpreter(header) {
        Format::Script(interpreter)
    } else if let Some(elf) = elf_header(header) {
        Format::Elf(elf)
    } else {
        Format::Unknown
    }
}

/// Read exactly `length` bytes of the file `file` from `offset`.
///
/// # Errors
///
/// Returns a description of a file that cannot be read, or that ends sooner.
fn read_exactly(
    inspect: &(impl Inspect + ?Sized),
    file: &AbsolutePath,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|error| format!("could not read {length} bytes of {file}: {error}"))?;
    bytes.resize(length, 0_u8);
    let read = inspect
        .read_file(file.as_path(), offset, &mut bytes)
        .map_err(|error| format!("could not read {file}: {error}"))?;
    if read != length {
        return Err(format!(
            "{file} ends before the {length} bytes at offset {offset}"
        ));
    }
    Ok(bytes)
}

/// The loader path that the first `PT_INTERP` program header of the `x86_64` ELF
/// executable `file`, whose header is `elf`, names, read as the kernel's
/// `load_elf_binary` reads it, or `None` when it has no such header.
///
/// # Errors
///
/// Returns a description of program headers that the kernel would reject or that
/// the tracer cannot read.
fn elf_loader(
    inspect: &(impl Inspect + ?Sized),
    file: &AbsolutePath,
    elf: ElfHeader,
) -> Result<Option<Vec<u8>>, String> {
    let size = usize::from(elf.count)
        .checked_mul(PROGRAM_HEADER_SIZE)
        .filter(|size| (1..=PROGRAM_HEADERS_LIMIT).contains(size));
    let (PROGRAM_HEADER_SIZE, Some(size)) = (usize::from(elf.entry_size), size) else {
        return Err(format!(
            "{file} has {} program headers of {} bytes, which the kernel does not load",
            elf.count, elf.entry_size
        ));
    };
    let table = read_exactly(inspect, file, elf.offset, size)?;
    // Each header in turn, taken with `slice::get`, where `as_chunks` would panic on
    // a chunk size of zero.
    let headers = (0..usize::from(elf.count)).filter_map(|index| {
        let start = index.saturating_mul(PROGRAM_HEADER_SIZE);
        table.get(start..start.saturating_add(PROGRAM_HEADER_SIZE))
    });
    for header in headers {
        if u32_at(header, 0) != Some(PT_INTERP) {
            continue;
        }
        let (Some(offset), Some(length)) = (
            u64_at(header, PROGRAM_HEADER_OFFSET),
            u64_at(header, PROGRAM_HEADER_FILE_SIZE)
                .and_then(|length| usize::try_from(length).ok())
                .filter(|length| (2..=PATH_MAX).contains(length)),
        ) else {
            return Err(format!(
                "the PT_INTERP header of {file} has a size that the kernel does not load"
            ));
        };
        let path = read_exactly(inspect, file, offset, length)?;
        if path.last() != Some(&0) {
            return Err(format!(
                "the PT_INTERP path of {file} does not end with a NUL byte"
            ));
        }
        let name = path.split(|byte| *byte == 0).next().unwrap_or_default();
        return Ok(Some(name.to_vec()));
    }
    Ok(None)
}

/// Follow the program at `program` as the kernel runs it, and push onto `loads`
/// every path that the kernel loads besides it, in order: each interpreter that a
/// `#!` line names, resolved against `directory`, and the loader of the last file
/// when that is an `x86_64` ELF executable with a `PT_INTERP` header.
///
/// # Errors
///
/// Returns what kept the tracer from following the program to its end: a file that
/// it cannot read, a file whose format it does not know, an ELF executable whose
/// program headers it cannot read, a relative path it cannot resolve, or more
/// interpreters than the kernel follows. The paths found until then are pushed.
fn follow_program(
    inspect: &(impl Inspect + ?Sized),
    directory: &Directory,
    program: &AbsolutePath,
    loads: &mut Vec<AbsolutePath>,
) -> Result<(), String> {
    let mut file = program.clone();
    for _depth in 0..=LAST_HANDLER_DEPTH {
        let mut header = vec![0_u8; HEADER_SIZE];
        let read = inspect
            .read_file(file.as_path(), 0, &mut header)
            .map_err(|error| format!("the tracer could not read {file}: {error}"))?;
        header.truncate(read);
        match format(&header) {
            Format::Script(name) => {
                let Some(interpreter) = directory.resolve(&name).map_err(|problem| {
                    format!("the interpreter of {file} is unknown: {problem}")
                })?
                else {
                    return Ok(());
                };
                loads.push(interpreter.clone());
                file = interpreter;
            },
            Format::Elf(elf) => {
                let loader = match elf_loader(inspect, &file, elf)? {
                    Some(name) => {
                        directory.resolve(&name).map_err(|problem| {
                            format!("the loader of {file} is unknown: {problem}")
                        })?
                    },
                    None => None,
                };
                loads.extend(loader);
                return Ok(());
            },
            Format::Unknown => {
                return Err(format!(
                    "{file} is neither a `#!` script nor an x86_64 ELF executable"
                ));
            },
        }
    }
    Err(format!(
        "its interpreters are nested deeper than the kernel follows, down to {file}"
    ))
}

/// The `int` in the low 32 bits of a register, as the kernel reads an `int`
/// argument.
fn int(value: u64) -> c_int {
    let low = u32::try_from(value & u64::from(u32::MAX)).unwrap_or_default();
    c_int::from_ne_bytes(low.to_ne_bytes())
}

/// The bits of an open flag constant, as they appear in a flags register.
fn flag(bits: c_int) -> u64 {
    u64::from(bits.cast_unsigned())
}

/// The access kinds of a successful open with `flags`.
///
/// Opening for reading reads the content, except that opening a directory with
/// `O_DIRECTORY` reads nothing, because its listing is recorded when it is read.
/// Opening for writing, creating or truncating writes. `O_PATH` and `O_TMPFILE`,
/// which creates an unnamed file in a directory, only look the path up.
fn open_kinds(flags: u64) -> AccessKinds {
    let tmpfile = flag(libc::O_TMPFILE);
    if flags & flag(libc::O_PATH) != 0 || flags & tmpfile == tmpfile {
        return AccessKinds::only(AccessKind::Probe);
    }
    let mode = flags & flag(libc::O_ACCMODE);
    let reads = mode == flag(libc::O_RDONLY) || mode == flag(libc::O_RDWR);
    let writes = mode == flag(libc::O_WRONLY)
        || mode == flag(libc::O_RDWR)
        || flags & flag(libc::O_CREAT | libc::O_TRUNC) != 0;
    let mut kinds = AccessKinds::EMPTY;
    if reads {
        kinds |= if flags & flag(libc::O_DIRECTORY) == 0 {
            AccessKind::Read
        } else {
            AccessKind::Probe
        };
    }
    if writes {
        kinds |= AccessKind::Write;
    }
    if kinds.is_empty() {
        // Access mode 3 opens for neither reading nor writing.
        kinds |= AccessKind::Probe;
    }
    kinds
}

/// The absolute path a link in `/proc` names, or `None` when it names no path in
/// the file system: a pipe, a socket or another object without a path. The path of
/// a removed file or directory ends in [`DELETED`].
fn link_path(link: &[u8]) -> Option<AbsolutePath> {
    if !link.starts_with(b"/") {
        return None;
    }
    AbsolutePath::new(OsStr::from_bytes(link)).ok()
}

/// Push the components of `path` onto `pending`, last first, so that popping gives
/// them in order: every component except the empty ones of repeated or trailing
/// separators. `.` is kept, because the component before it is then not the last
/// one, so it must be a directory and a symbolic link there is followed.
fn push_components(
    pending: &mut Vec<Vec<u8>>,
    path: &[u8],
) {
    pending.extend(
        path.split(|byte| *byte == b'/')
            .filter(|component| !component.is_empty())
            .rev()
            .map(<[u8]>::to_vec),
    );
}

/// The paths that a lookup in a root touches.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolution {
    /// Every symbolic link the lookup followed, in order.
    links: Vec<PathBuf>,
    /// Every directory the lookup passed through to a later component, in order,
    /// including one that a later `..` left again, and one that a link target
    /// passed through. A change to any of them, such as its removal or its
    /// replacement by a file, can change the lookup's result.
    directories: Vec<PathBuf>,
    /// The object the lookup reached, or the component where it stopped: the first
    /// one that names nothing, or the first one that is not a directory but has
    /// components after it.
    reached: PathBuf,
}

/// Whether a failed `lstat` of a component means that the kernel's lookup stops
/// at that same component: it names nothing, or its name is too long.
fn stops_lookup(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ENAMETOOLONG)
}

/// Look `path` up with `root` as its root, as `openat2` with `RESOLVE_IN_ROOT`
/// does, in the file system as the tracer sees it.
///
/// `..` at the root stays at the root, and an absolute path or symbolic-link target
/// starts at the root. Every other component is checked with `lstat`. A symbolic
/// link is replaced by its target, except in the last component when `follow_last`
/// is false and the path has no trailing separator; `.` after a link makes the link
/// an intermediate component, which is always followed. A component that is not a
/// directory ends the lookup when anything follows it, `.` and `..` included, as
/// the kernel fails with `ENOTDIR` there. Because every directory in the path it
/// builds is then a real directory, `..` goes to its lexical parent. The lookup
/// also stops at the first component that names nothing. Every directory it
/// passes through is kept, even when a later `..` leaves it, because the result
/// depends on it.
///
/// # Errors
///
/// Returns a description of what could not be repeated: a component the tracer
/// cannot check, a link it cannot read, or more links than the kernel follows.
fn resolve_in_root(
    inspect: &(impl Inspect + ?Sized),
    root: &Path,
    path: &[u8],
    follow_last: bool,
) -> Result<Resolution, String> {
    let check = |candidate: &Path| {
        inspect.path_object(candidate).map_err(|error| {
            if stops_lookup(&error) {
                None
            } else {
                Some(format!("could not check {}: {error}", candidate.display()))
            }
        })
    };
    let mut links = Vec::new();
    let mut directories = Vec::new();
    // A root that is not a directory fails before any component is looked up.
    match check(root) {
        Ok(object) if object.directory => {},
        Ok(_) | Err(None) => {
            return Ok(Resolution {
                links,
                directories,
                reached: root.to_path_buf(),
            });
        },
        Err(Some(problem)) => return Err(problem),
    }
    let follow_last = follow_last || path.ends_with(b"/");
    let mut pending = Vec::new();
    push_components(&mut pending, path);
    let mut current = root.to_path_buf();
    while let Some(component) = pending.pop() {
        // `current` is a directory here, so `.` stays in it and `..` goes to its
        // parent, or stays at the root.
        if component == b"." {
            continue;
        }
        if component == b".." {
            if current != root {
                current.pop();
            }
            continue;
        }
        let candidate = current.join(OsStr::from_bytes(&component));
        let object = match check(&candidate) {
            Ok(object) => object,
            Err(None) => {
                current = candidate;
                break;
            },
            Err(Some(problem)) => return Err(problem),
        };
        let last = pending.is_empty();
        if !object.symbolic_link || (last && !follow_last) {
            current = candidate;
            if !last {
                if !object.directory {
                    // The kernel fails with `ENOTDIR` at this component.
                    break;
                }
                directories.push(current.clone());
            }
            continue;
        }
        if links.len() >= MAX_SYMBOLIC_LINKS {
            return Err(format!(
                "more than {MAX_SYMBOLIC_LINKS} symbolic links in a lookup in {}",
                root.display()
            ));
        }
        let target = inspect.link_target(&candidate).map_err(|error| {
            format!(
                "could not read the symbolic link {}: {error}",
                candidate.display()
            )
        })?;
        if target.starts_with(b"/") {
            root.clone_into(&mut current);
        }
        push_components(&mut pending, &target);
        links.push(candidate);
    }
    Ok(Resolution {
        links,
        directories,
        reached: current,
    })
}

/// Fill `buffer` from the thread's memory at `address`, reading in chunks that do
/// not cross a page boundary, so a readable start is read even when the memory
/// after it is not mapped. With `until_nul`, stop after the chunk that holds a NUL
/// byte. Return how many bytes were filled.
fn read_memory(
    inspect: &(impl Inspect + ?Sized),
    address: u64,
    buffer: &mut [u8],
    until_nul: bool,
) -> Result<usize, String> {
    let mut filled = 0_usize;
    let mut next = address;
    while let Some(rest) = buffer.get_mut(filled..)
        && !rest.is_empty()
    {
        let offset = usize::try_from(next.checked_rem(CHUNK_ADDRESS).unwrap_or_default())
            .unwrap_or_default();
        let room = CHUNK.saturating_sub(offset).min(rest.len());
        let Some(chunk) = rest.get_mut(..room) else {
            break;
        };
        let read = inspect
            .memory(next, chunk)
            .map_err(|error| format!("could not read memory at {next:#x}: {error}"))?;
        if read == 0 {
            break;
        }
        let Some(after) = filled.checked_add(read) else {
            break;
        };
        filled = after;
        let Some(advanced) = u64::try_from(read)
            .ok()
            .and_then(|read| next.checked_add(read))
        else {
            break;
        };
        next = advanced;
        if until_nul && chunk.get(..read).is_some_and(|bytes| bytes.contains(&0)) {
            break;
        }
    }
    Ok(filled)
}

/// Read the NUL-terminated path at `address`, without its NUL byte.
fn read_path(
    inspect: &(impl Inspect + ?Sized),
    address: u64,
) -> Result<Vec<u8>, String> {
    if address == 0 {
        return Err("a path argument is null".to_owned());
    }
    let mut buffer = vec![0_u8; PATH_MAX];
    let filled = read_memory(inspect, address, &mut buffer, true)?;
    let bytes = buffer.get(..filled).unwrap_or_default();
    let Some(end) = bytes.iter().position(|byte| *byte == 0) else {
        return Err(if filled == PATH_MAX {
            "a path is longer than PATH_MAX".to_owned()
        } else {
            format!("could not read the path at {address:#x}")
        });
    };
    Ok(bytes.get(..end).unwrap_or_default().to_vec())
}

/// Decodes the operands of one call at its entry.
struct Decoder<'a, I: Inspect + ?Sized> {
    /// The stopped thread.
    inspect: &'a I,
    /// The call's arguments.
    arguments: Arguments,
    /// The call decoded so far.
    call: Call,
    /// The open flags of an opening call, once its rule is decoded.
    open_flags: Option<u64>,
}

impl<I: Inspect + ?Sized> Decoder<'_, I> {
    /// The argument at `index`.
    fn argument(
        &self,
        index: usize,
    ) -> u64 {
        self.arguments.get(index).copied().unwrap_or_default()
    }

    /// Note the first thing that could not be decoded.
    fn problem(
        &mut self,
        problem: String,
    ) {
        if self.call.problem.is_none() {
            self.call.problem = Some(problem);
        }
    }

    /// Record `path` with `kinds`, unless it names no path.
    fn record(
        &mut self,
        path: Option<AbsolutePath>,
        kinds: AccessKinds,
    ) {
        if let Some(path) = path {
            self.call.operands.push((path, kinds));
        }
    }

    /// The path behind the directory descriptor `dirfd`, or the working directory
    /// for `AT_FDCWD`, or `None` when it names no path or its object was removed.
    fn base(
        &self,
        dirfd: c_int,
    ) -> Result<Option<AbsolutePath>, String> {
        let link = if dirfd == libc::AT_FDCWD {
            self.inspect
                .working_directory()
                .map_err(|error| format!("could not read the working directory: {error}"))?
        } else {
            self.inspect
                .descriptor(dirfd)
                .map_err(|error| format!("could not read descriptor {dirfd}: {error}"))?
        };
        let Some(path) = link_path(&link) else {
            return Ok(None);
        };
        if link.ends_with(DELETED) && self.removed(dirfd, &path)? {
            return Ok(None);
        }
        Ok(Some(path))
    }

    /// Whether the object behind `dirfd`, whose link in `/proc` names `path`, which
    /// ends in [`DELETED`], was removed.
    ///
    /// Linux adds that suffix to the path of a removed file or directory, but a live
    /// one may have it in its name. The object is live when `path` names that same
    /// object, and removed when it has no names left. An object that has other
    /// names, but not `path`, cannot be told apart, so it is reported as a problem
    /// rather than skipped.
    fn removed(
        &self,
        dirfd: c_int,
        path: &AbsolutePath,
    ) -> Result<bool, String> {
        let object = self
            .inspect
            .link_object(dirfd)
            .map_err(|error| format!("could not read the object behind {path}: {error}"))?;
        let named = self.inspect.path_object(path.as_path()).ok();
        if named.is_some_and(|named| named.same(object)) {
            return Ok(false);
        }
        if object.links == 0 {
            return Ok(true);
        }
        Err(format!(
            "could not tell whether {path} was removed: its object has other names"
        ))
    }

    /// Record the path behind the descriptor `fd`.
    fn descriptor(
        &mut self,
        fd: c_int,
        kinds: AccessKinds,
    ) {
        match self.base(fd) {
            Ok(path) => self.record(path, kinds),
            Err(problem) => self.problem(problem),
        }
    }

    /// Record the path at `address`, relative to `dirfd`, looked up as `lookup`
    /// says.
    fn path(
        &mut self,
        dirfd: c_int,
        address: u64,
        empty: Empty,
        lookup: Lookup,
        kinds: AccessKinds,
    ) {
        let empty_flag = |index: usize| self.argument(index) & flag(libc::AT_EMPTY_PATH) != 0;
        let (null_is_descriptor, empty_is_descriptor) = match empty {
            Empty::Never => (false, false),
            Empty::Always => (false, true),
            Empty::Flag(index) => (empty_flag(index), empty_flag(index)),
            Empty::Null => (true, false),
            Empty::NullOrFlag(index) => (true, empty_flag(index)),
        };
        if address == 0 && null_is_descriptor {
            self.descriptor(dirfd, kinds);
            return;
        }
        let path = match read_path(self.inspect, address) {
            Ok(path) => path,
            Err(problem) => {
                self.problem(problem);
                return;
            },
        };
        if path.is_empty() {
            // Without a descriptor to act on, an empty path fails without a
            // lookup, so it touches nothing.
            if empty_is_descriptor {
                self.descriptor(dirfd, kinds);
            }
            return;
        }
        if lookup == Lookup::Ordinary && path.starts_with(b"/") {
            self.record(AbsolutePath::new(OsStr::from_bytes(&path)).ok(), kinds);
            return;
        }
        let base = match self.base(dirfd) {
            Ok(Some(base)) => base,
            Ok(None) => return,
            Err(problem) => {
                self.problem(problem);
                return;
            },
        };
        match lookup {
            Lookup::Ordinary => self.record(Some(base.join(OsStr::from_bytes(&path))), kinds),
            Lookup::InRoot { follow_last } => self.in_root(&base, &path, follow_last, kinds),
        }
    }

    /// Record the paths that the lookup of `path` with `root` as its root touches,
    /// as [`resolve_in_root`] finds them: each symbolic link it followed as a probe,
    /// because the lookup read its target; each directory it passed through as a
    /// probe, because the lookup depends on it being a directory, even one that a
    /// later `..` left; and the object it reached, or the component where it
    /// stopped, with `kinds`, which already depend on that path. The reached object
    /// is noted, so that [`Call::opened`] can check it against the descriptor the
    /// call returns.
    fn in_root(
        &mut self,
        root: &AbsolutePath,
        path: &[u8],
        follow_last: bool,
        kinds: AccessKinds,
    ) {
        match resolve_in_root(self.inspect, root.as_path(), path, follow_last) {
            Ok(resolution) => {
                for link in resolution.links {
                    self.record(
                        AbsolutePath::new(link).ok(),
                        AccessKinds::only(AccessKind::Probe),
                    );
                }
                let mut directories: Vec<PathBuf> = Vec::new();
                for directory in resolution.directories {
                    if directory != resolution.reached && !directories.contains(&directory) {
                        directories.push(directory);
                    }
                }
                for directory in directories {
                    self.record(
                        AbsolutePath::new(directory).ok(),
                        AccessKinds::only(AccessKind::Probe),
                    );
                }
                if let Ok(reached) = AbsolutePath::new(resolution.reached) {
                    self.call.reached = Some(self.call.operands.len());
                    self.record(Some(reached), kinds);
                }
            },
            Err(problem) => self.problem(problem),
        }
    }

    /// Decode `openat2`, whose flags and resolution are in the `struct open_how`
    /// that its third argument points to.
    fn open_how(&mut self) {
        let mut how = [0_u8; OPEN_HOW_SIZE];
        let filled = match read_memory(self.inspect, self.argument(2), &mut how, false) {
            Ok(filled) => filled,
            Err(problem) => {
                self.problem(problem);
                return;
            },
        };
        let word = |offset: usize| {
            how.get(offset..)
                .and_then(|rest| rest.first_chunk::<8>())
                .map(|bytes| u64::from_ne_bytes(*bytes))
        };
        let (Some(flags), Some(resolve), OPEN_HOW_SIZE) = (word(0), word(OPEN_HOW_RESOLVE), filled)
        else {
            self.problem("could not read the open_how of openat2".to_owned());
            return;
        };
        let lookup = if resolve & RESOLVE_IN_ROOT == 0 {
            Lookup::Ordinary
        } else {
            // `O_NOFOLLOW`, and an exclusive create, do not follow a symbolic link
            // in the last component.
            let exclusive = flag(libc::O_CREAT | libc::O_EXCL);
            Lookup::InRoot {
                follow_last: flags & flag(libc::O_NOFOLLOW) == 0 && flags & exclusive != exclusive,
            }
        };
        self.path(
            int(self.argument(0)),
            self.argument(1),
            Empty::Never,
            lookup,
            open_kinds(flags),
        );
        self.open_flags = Some(flags);
        let tmpfile = flag(libc::O_TMPFILE);
        if flags & tmpfile == tmpfile {
            // The descriptor names a new unnamed file, not the directory the
            // lookup reached.
            self.call.reached = None;
        }
    }

    /// Decode one rule of the call.
    fn rule(
        &mut self,
        rule: Rule,
    ) {
        let kinds = match rule.kinds {
            Kinds::Fixed(kinds) => kinds,
            Kinds::OpenFlags(index) => {
                self.open_flags = Some(self.argument(index));
                open_kinds(self.argument(index))
            },
            Kinds::OpenHow => AccessKinds::EMPTY,
        };
        match rule.operand {
            Operand::Path { dirfd, path, empty } => {
                let dirfd = dirfd.map_or(libc::AT_FDCWD, |index| int(self.argument(index)));
                self.path(dirfd, self.argument(path), empty, Lookup::Ordinary, kinds);
            },
            Operand::Descriptor(index) => self.descriptor(int(self.argument(index)), kinds),
            Operand::OpenHow => self.open_how(),
            Operand::FanotifyMark => {
                // Flushing marks looks no path up.
                if self.argument(1) & FAN_MARK_FLUSH == 0 {
                    self.path(
                        int(self.argument(3)),
                        self.argument(4),
                        Empty::Null,
                        Lookup::Ordinary,
                        kinds,
                    );
                }
            },
            Operand::SocketAddress {
                address,
                length,
                connects,
            } => {
                self.socket_address(
                    self.argument(address),
                    int(self.argument(length)),
                    false,
                    kinds,
                    connects,
                );
            },
            Operand::MessageName => self.message_name(self.argument(1), kinds),
            Operand::MessageNames => {
                let count =
                    u32::try_from(self.argument(2) & u64::from(u32::MAX)).unwrap_or_default();
                self.message_names(self.argument(1), count, kinds);
            },
        }
    }

    /// Decode the socket address at `address`, `length` bytes long as the call
    /// gave it, or cut to [`SOCKADDR_STORAGE_SIZE`] when `cut` is set, as `sendmsg`
    /// and `sendmmsg` cut `msg_namelen`: record a Unix socket path in it with
    /// `kinds`, and with `connects`, note its destination as a connection. A null
    /// address, a length of 0 or less, a longer length than `bind`, `connect` and
    /// `sendto` accept, and what [`socket_name`] finds no name in name nothing; an
    /// address that cannot be read is a problem.
    fn socket_address(
        &mut self,
        address: u64,
        length: c_int,
        cut: bool,
        kinds: AccessKinds,
        connects: bool,
    ) {
        let Ok(length) = usize::try_from(length) else {
            return;
        };
        if address == 0 || length == 0 || (!cut && length > SOCKADDR_STORAGE_SIZE) {
            return;
        }
        let length = length.min(SOCKADDR_STORAGE_SIZE);
        // A fixed buffer of the largest size, cut with `get_mut`, which cannot panic,
        // where `vec![0; length]` panics on a length that overflows.
        let mut storage = [0_u8; SOCKADDR_STORAGE_SIZE];
        let Some(bytes) = storage.get_mut(..length) else {
            return;
        };
        match read_memory(self.inspect, address, bytes, false) {
            Ok(filled) if filled == length => {},
            Ok(_) => {
                self.problem(format!("could not read the socket address at {address:#x}"));
                return;
            },
            Err(problem) => {
                self.problem(problem);
                return;
            },
        }
        let connection = match socket_name(bytes) {
            None => return,
            Some(SocketName::Ip(destination)) => Connection::Ip(destination),
            Some(SocketName::Abstract(name)) => Connection::UnixAbstract(name),
            Some(SocketName::Path(path)) => {
                let Some(resolved) = self.socket_path(&path, kinds) else {
                    return;
                };
                Connection::UnixPath(resolved)
            },
        };
        if connects {
            self.call.connections.push(connection);
        }
    }

    /// Record the Unix socket path `path` with `kinds`, resolved against the working
    /// directory when it is relative, as the kernel resolves it, and return it; or
    /// `None` when the working directory names no path, or cannot be read, which is
    /// a problem.
    fn socket_path(
        &mut self,
        path: &[u8],
        kinds: AccessKinds,
    ) -> Option<AbsolutePath> {
        let resolved = if path.starts_with(b"/") {
            AbsolutePath::new(OsStr::from_bytes(path)).ok()
        } else {
            match self.base(libc::AT_FDCWD) {
                Ok(base) => base.map(|base| base.join(OsStr::from_bytes(path))),
                Err(problem) => {
                    self.problem(problem);
                    None
                },
            }
        };
        self.record(resolved.clone(), kinds);
        resolved
    }

    /// Decode the `msg_name` of the `struct msghdr` at `header`, with its
    /// `msg_namelen`, as the destination of a send call; a header that cannot be
    /// read is a problem.
    fn message_name(
        &mut self,
        header: u64,
        kinds: AccessKinds,
    ) {
        let mut bytes = [0_u8; MSGHDR_NAME_SIZE];
        match read_memory(self.inspect, header, &mut bytes, false) {
            Ok(MSGHDR_NAME_SIZE) => {},
            Ok(_) => {
                self.problem(format!("could not read the message header at {header:#x}"));
                return;
            },
            Err(problem) => {
                self.problem(problem);
                return;
            },
        }
        if let (Some(name), Some(length)) = (u64_at(&bytes, 0), u32_at(&bytes, MSGHDR_NAMELEN)) {
            self.socket_address(name, int(u64::from(length)), true, kinds, true);
        }
    }

    /// Decode the `msg_name` of each `struct mmsghdr` of the array at `array`, the
    /// first `count` of them, cut to [`UIO_MAXIOV`] as the kernel cuts it, until one
    /// cannot be read.
    fn message_names(
        &mut self,
        array: u64,
        count: u32,
        kinds: AccessKinds,
    ) {
        for index in 0..count.min(UIO_MAXIOV) {
            let Some(header) = u64::from(index)
                .checked_mul(MMSGHDR_SIZE)
                .and_then(|offset| array.checked_add(offset))
            else {
                break;
            };
            self.message_name(header, kinds);
            if self.call.problem.is_some() {
                break;
            }
        }
    }

    /// What an opening call with `flags` reads first, now that its rule is decoded:
    /// the operand it opens for reading, unless it truncates the file, or creates it,
    /// which a call with `O_CREAT` does when no regular file is at the path now, at
    /// its entry.
    fn opened_for_reading(
        &self,
        flags: u64,
    ) -> FirstRead {
        if flags & flag(libc::O_TRUNC) != 0 {
            return FirstRead::Nothing;
        }
        let Some(index) = self
            .call
            .operands
            .iter()
            .position(|(_, kinds)| kinds.contains(AccessKind::Read))
        else {
            return FirstRead::Nothing;
        };
        if flags & flag(libc::O_CREAT) != 0 {
            let existed = self
                .call
                .operands
                .get(index)
                .is_some_and(|(path, _)| self.inspect.path_file(path.as_path()).is_some());
            if !existed {
                return FirstRead::Nothing;
            }
        }
        FirstRead::Opened(index)
    }
}

/// What a socket address names, as the kernel reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SocketName {
    /// An IPv4 or IPv6 address and port, with the scope of an IPv6 address.
    Ip(SocketAddr),
    /// A Unix socket path, up to its first NUL byte.
    Path(Vec<u8>),
    /// A Unix socket in the abstract namespace, by its name after the zero byte
    /// that starts it.
    Abstract(Vec<u8>),
}

/// What the socket address `bytes`, as long as the call gave it, names: an IPv4
/// address and port in a `struct sockaddr_in`; an IPv6 address and port in a
/// `struct sockaddr_in6`, with its scope when the address is long enough to hold
/// one; or a Unix socket path or abstract name in a `struct sockaddr_un`. `None`
/// for another family, an unnamed Unix socket, and an address that is shorter or
/// longer than the kernel accepts for its family, which it rejects with `EINVAL`.
fn socket_name(bytes: &[u8]) -> Option<SocketName> {
    let port = || {
        bytes
            .get(2..)?
            .first_chunk::<2>()
            .map(|port| u16::from_be_bytes(*port))
    };
    match c_int::from(u16_at(bytes, 0)?) {
        libc::AF_INET if bytes.len() >= SOCKADDR_IN_SIZE => {
            let address = *bytes.get(4..)?.first_chunk::<4>()?;
            Some(SocketName::Ip(SocketAddr::from((address, port()?))))
        },
        libc::AF_INET6 if bytes.len() >= SOCKADDR_IN6_MINIMUM => {
            let address = *bytes.get(8..)?.first_chunk::<16>()?;
            let scope = if bytes.len() >= SOCKADDR_IN6_SIZE {
                u32_at(bytes, SOCKADDR_IN6_MINIMUM)?
            } else {
                0
            };
            Some(SocketName::Ip(SocketAddr::V6(SocketAddrV6::new(
                address.into(),
                port()?,
                0,
                scope,
            ))))
        },
        libc::AF_UNIX if bytes.len() > SUN_PATH_OFFSET && bytes.len() <= SOCKADDR_UN_SIZE => {
            let name = bytes.get(SUN_PATH_OFFSET..)?;
            if let Some((0, rest)) = name.split_first() {
                return Some(SocketName::Abstract(rest.to_vec()));
            }
            let end = name
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(name.len());
            Some(SocketName::Path(name.get(..end)?.to_vec()))
        },
        _ => None,
    }
}

/// The decoding of the tracked system call `number`, if it is tracked.
pub(crate) fn system_call(number: c_long) -> Option<&'static SystemCall> {
    SYSTEM_CALLS.iter().find(|call| call.number == number)
}

/// Decode the tracked call `number` with `arguments` at its entry, reading what it
/// needs from the stopped thread. Return `None` when the call is not tracked.
///
/// For `execve` and `execveat`, the thread's working directory is kept too, so that
/// [`Call::loaded`] can resolve a relative interpreter or loader path at the exit.
pub(crate) fn enter(
    number: c_long,
    arguments: Arguments,
    inspect: &(impl Inspect + ?Sized),
) -> Option<Call> {
    let system_call = system_call(number)?;
    let mut decoder = Decoder {
        inspect,
        arguments,
        call: Call {
            name: system_call.name,
            operands: Vec::new(),
            problem: None,
            reached: None,
            directory: None,
            unfollowed: None,
            first_read: FirstRead::Nothing,
            connections: Vec::new(),
        },
        open_flags: None,
    };
    for rule in system_call.rules {
        decoder.rule(*rule);
    }
    if let Some(flags) = decoder.open_flags {
        decoder.call.first_read = decoder.opened_for_reading(flags);
    }
    if matches!(number, libc::SYS_execve | libc::SYS_execveat) {
        decoder.call.first_read = FirstRead::Ran;
        decoder.call.directory = Some(match decoder.base(libc::AT_FDCWD) {
            Ok(Some(path)) => Directory::Path(path),
            Ok(None) => Directory::Removed,
            Err(problem) => Directory::Unknown(problem),
        });
    }
    Some(decoder.call)
}

/// The size of the `flags` field at the start of `struct clone_args`, the first
/// argument of `clone3`.
const CLONE_ARGS_FLAGS_SIZE: usize = 8;

/// Whether the call `number`, one of [`UNTRACEABLE_SYSTEM_CALLS`], with
/// `arguments`, marks the run as incompletely traced, and if so what the process
/// made, for the reason, with the call's name in backticks.
///
/// `clone` and `unshare` mark the run only with `CLONE_NEWNS` in their flags, and
/// `setns` only when it can enter a mount namespace: with a namespace type of 0, or
/// one that includes `CLONE_NEWNS`. For `clone3` the tracer reads the flags at the
/// start of its `struct clone_args` from the thread's memory, and marks the run
/// when they include `CLONE_NEWNS` or cannot be read. Every other call marks the
/// run. Returns `None` for a call that does not mark it, and for a number that is
/// not in the list.
pub(crate) fn untraceable(
    number: c_long,
    arguments: Arguments,
    inspect: &(impl Inspect + ?Sized),
) -> Option<String> {
    let call = UNTRACEABLE_SYSTEM_CALLS
        .iter()
        .find(|call| call.number == number)?;
    let new_namespace = flag(libc::CLONE_NEWNS);
    let flagged = || format!("made `{}` with `CLONE_NEWNS`", call.name);
    match number {
        libc::SYS_clone | libc::SYS_unshare => (arguments[0] & new_namespace != 0).then(flagged),
        libc::SYS_setns => {
            let namespace = int(arguments[1]);
            (namespace == 0 || flag(namespace) & new_namespace != 0)
                .then(|| "made `setns`, which can enter a mount namespace".to_owned())
        },
        libc::SYS_clone3 => {
            let mut bytes = [0_u8; CLONE_ARGS_FLAGS_SIZE];
            let unreadable = |problem: &str| {
                Some(format!(
                    "made `clone3`, whose flags the tracer could not read: {problem}"
                ))
            };
            if arguments[0] == 0 {
                return unreadable("its `struct clone_args` is null");
            }
            match read_memory(inspect, arguments[0], &mut bytes, false) {
                Ok(CLONE_ARGS_FLAGS_SIZE) => {
                    (u64::from_le_bytes(bytes) & new_namespace != 0).then(flagged)
                },
                Ok(_) => unreadable("its `struct clone_args` ends early"),
                Err(problem) => unreadable(&problem),
            }
        },
        _ => Some(format!("made `{}`", call.name)),
    }
}

/// Unit tests of the decoding of every tracked call, with a fake thread.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::Path,
    };

    use super::*;
    use crate::TRACKED_SYSTEM_CALLS;

    /// The address of the relative path `rel`.
    const REL: u64 = 0x1000;
    /// The address of the absolute path `/abs/./x//y/`.
    const ABS: u64 = 0x2000;
    /// The address of an empty path.
    const EMPTY: u64 = 0x3000;
    /// The address of the relative path `other`.
    const OTHER: u64 = 0x4000;
    /// The address of a `struct open_how` that creates a file for writing, with
    /// `RESOLVE_IN_ROOT`.
    const HOW: u64 = 0x5000;
    /// The address of a `struct open_how` for `O_PATH | O_NOFOLLOW`, with
    /// `RESOLVE_IN_ROOT`.
    const HOW_NOFOLLOW: u64 = 0x5100;
    /// The address of a `struct open_how` for reading, without `RESOLVE_IN_ROOT`.
    const HOW_PLAIN: u64 = 0x5200;
    /// The address of a `struct open_how` for `O_RDONLY | O_NOFOLLOW`, with
    /// `RESOLVE_IN_ROOT`.
    const HOW_READ_NOFOLLOW: u64 = 0x5300;
    /// The address of a `struct open_how` for `O_TMPFILE | O_RDWR`, with
    /// `RESOLVE_IN_ROOT`.
    const HOW_TMPFILE: u64 = 0x5400;
    /// The address of a path that starts two bytes before a page boundary.
    const CROSSING: u64 = 0x6FFE;
    /// The address of the relative path `./a//b/../c/.`.
    const DOTTED: u64 = 0x8000;
    /// The address of the relative path `../../rel`.
    const PARENT: u64 = 0xA000;
    /// The address of the relative path `top/x/y`, through the absolute link `top`.
    const LINKED: u64 = 0xB000;
    /// The address of the relative path `up/y`, through the relative link `up`.
    const UP: u64 = 0xC000;
    /// The address of the relative path `nope/../rel`, through a missing directory.
    const MISSING: u64 = 0xD000;
    /// The address of the relative path `loop`, a link to itself.
    const LOOP: u64 = 0xE000;
    /// The address of the relative path `top`, a link to `/abs`.
    const TOP: u64 = 0xF000;
    /// The address of the absolute path `/../abs/x/../x/y`.
    const ABS_PARENT: u64 = 0xF800;
    /// The address of the relative path `plain/../rel`, through the regular file
    /// `plain`.
    const PLAIN_UP: u64 = 0x9000;
    /// The address of the relative path `plain/.`, after the regular file `plain`.
    const PLAIN_DOT: u64 = 0x9100;
    /// The address of the relative path `top/.`, after the link `top` to `/abs`.
    const TOP_DOT: u64 = 0x9200;
    /// The address of the relative path `file.link/x`, through the link `file.link`
    /// to the regular file `plain`.
    const FILE_LINK: u64 = 0x9300;
    /// The address of the relative path `locked/x`, beneath a directory that the
    /// tracer cannot search.
    const LOCKED: u64 = 0x9400;
    /// The address of the relative path `abs/../rel`, through the directory `abs`.
    const SUB_UP: u64 = 0x9500;
    /// The address of the relative path `abs/x/../x/..`, which passes through the
    /// directory `abs/x` twice and ends at the directory `abs`.
    const SUB_UP_TWICE: u64 = 0x9600;
    /// The address of more than `PATH_MAX` bytes without a NUL byte.
    const LONG: u64 = 0x10000;
    /// An address that is not mapped.
    const UNMAPPED: u64 = 0x9999_0000;

    /// A directory descriptor for `/dir`.
    const DIR: u64 = 3;
    /// A descriptor for `/dir/file`.
    const FILE: u64 = 4;
    /// A descriptor for a pipe.
    const PIPE: u64 = 5;
    /// A descriptor for a removed file.
    const GONE: u64 = 6;
    /// A descriptor for the live file `/dir/live (deleted)`.
    const LIVE: u64 = 7;
    /// A descriptor for a file whose name `/dir/renamed (deleted)` is gone, but
    /// that has another name.
    const RENAMED: u64 = 8;
    /// A descriptor that is not open.
    const CLOSED: u64 = 9;
    /// A directory descriptor for the live directory `/dir/sub (deleted)`.
    const LIVE_DIR: u64 = 10;
    /// A descriptor for `/dir/rel`, as a rooted open of `rel` in `/dir` returns it.
    const OPENED: u64 = 11;

    /// `AT_FDCWD`, -100, sign-extended in a register as the C library passes it.
    const CWD: u64 = 0xFFFF_FFFF_FFFF_FF9C;
    /// `AT_EMPTY_PATH` as a register holds it.
    const EMPTY_PATH: u64 = 0x1000;

    /// The object with `inode` and `links` names on the fake device, which is not
    /// a directory.
    fn object(
        inode: u64,
        links: u64,
        symbolic_link: bool,
    ) -> Object {
        Object {
            device: 1,
            inode,
            links,
            symbolic_link,
            directory: false,
        }
    }

    /// A thread with fixed memory, working directory and descriptors, in a fixed
    /// file system.
    struct Fake {
        /// Memory regions by their start address.
        memory: BTreeMap<u64, Vec<u8>>,
        /// The link in `/proc` of the working directory, under `AT_FDCWD`, and of
        /// each open descriptor, with the object behind it.
        links: BTreeMap<c_int, (&'static str, Object)>,
        /// Every path in the file system, with its inode number and, for a
        /// symbolic link, its target.
        files: BTreeMap<&'static str, (u64, Option<&'static str>)>,
        /// The paths in `files` that are directories.
        directories: BTreeSet<&'static str>,
        /// A directory that the tracer cannot search: checking a path beneath it
        /// fails with `EACCES`.
        locked: &'static str,
        /// The content of each file that [`Inspect::read_file`] reads, by path, or
        /// `None` for a file that the tracer cannot read.
        contents: BTreeMap<String, Option<Vec<u8>>>,
        /// The metadata of each regular file, by path, which
        /// [`Inspect::path_file`] gives, and [`Inspect::descriptor_file`] for a
        /// descriptor whose link names the path.
        regular: BTreeMap<String, FileMetadata>,
    }

    /// The bytes of a `struct open_how` with `flags` and `resolve`.
    fn open_how(
        flags: c_int,
        resolve: u64,
    ) -> Vec<u8> {
        [flag(flags), 0o644, resolve]
            .iter()
            .flat_map(|word| word.to_ne_bytes())
            .collect()
    }

    impl Fake {
        /// The fake thread of every test.
        fn new() -> Self {
            let memory = BTreeMap::from([
                (REL, b"rel\0".to_vec()),
                (ABS, b"/abs/./x//y/\0".to_vec()),
                (EMPTY, b"\0".to_vec()),
                (OTHER, b"other\0".to_vec()),
                (
                    HOW,
                    open_how(libc::O_WRONLY | libc::O_CREAT, RESOLVE_IN_ROOT),
                ),
                (
                    HOW_NOFOLLOW,
                    open_how(libc::O_PATH | libc::O_NOFOLLOW, RESOLVE_IN_ROOT),
                ),
                (HOW_PLAIN, open_how(libc::O_RDONLY, 0)),
                (
                    HOW_READ_NOFOLLOW,
                    open_how(libc::O_RDONLY | libc::O_NOFOLLOW, RESOLVE_IN_ROOT),
                ),
                (
                    HOW_TMPFILE,
                    open_how(libc::O_TMPFILE | libc::O_RDWR, RESOLVE_IN_ROOT),
                ),
                (CROSSING, b"cross\0".to_vec()),
                (DOTTED, b"./a//b/../c/.\0".to_vec()),
                (PARENT, b"../../rel\0".to_vec()),
                (LINKED, b"top/x/y\0".to_vec()),
                (UP, b"up/y\0".to_vec()),
                (MISSING, b"nope/../rel\0".to_vec()),
                (LOOP, b"loop\0".to_vec()),
                (TOP, b"top\0".to_vec()),
                (ABS_PARENT, b"/../abs/x/../x/y\0".to_vec()),
                (PLAIN_UP, b"plain/../rel\0".to_vec()),
                (PLAIN_DOT, b"plain/.\0".to_vec()),
                (TOP_DOT, b"top/.\0".to_vec()),
                (FILE_LINK, b"file.link/x\0".to_vec()),
                (LOCKED, b"locked/x\0".to_vec()),
                (SUB_UP, b"abs/../rel\0".to_vec()),
                (SUB_UP_TWICE, b"abs/x/../x/..\0".to_vec()),
                (LONG, vec![b'a'; PATH_MAX.checked_add(10).unwrap()]),
            ]);
            let links = BTreeMap::from([
                (libc::AT_FDCWD, ("/work", object(2, 1, false))),
                (3, ("/dir", object(3, 2, false))),
                (4, ("/dir/file", object(4, 1, false))),
                (5, ("pipe:[7]", object(5, 1, false))),
                (6, ("/gone (deleted)", object(6, 0, false))),
                (7, ("/dir/live (deleted)", object(7, 1, false))),
                (8, ("/dir/renamed (deleted)", object(8, 1, false))),
                (10, ("/dir/sub (deleted)", object(10, 2, false))),
                (11, ("/dir/rel", object(11, 1, false))),
            ]);
            let files = BTreeMap::from([
                ("/work", (2, None)),
                ("/dir", (3, None)),
                ("/dir/file", (4, None)),
                ("/dir/live (deleted)", (7, None)),
                ("/dir/sub (deleted)", (10, None)),
                ("/dir/rel", (11, None)),
                ("/dir/abs", (12, None)),
                ("/dir/abs/x", (13, None)),
                ("/dir/abs/x/y", (14, None)),
                ("/dir/top", (15, Some("/abs"))),
                ("/dir/up", (16, Some("../../abs/x"))),
                ("/dir/loop", (17, Some("loop"))),
                ("/dir/plain", (18, None)),
                ("/dir/file.link", (19, Some("plain"))),
                ("/dir/locked", (20, None)),
            ]);
            let directories = BTreeSet::from([
                "/work",
                "/dir",
                "/dir/sub (deleted)",
                "/dir/abs",
                "/dir/abs/x",
                "/dir/locked",
            ]);
            Self {
                memory,
                links,
                files,
                directories,
                locked: "/dir/locked",
                contents: BTreeMap::new(),
                regular: BTreeMap::new(),
            }
        }

        /// The fake thread with its working directory at `link`, whose object is
        /// `object`.
        fn in_directory(
            link: &'static str,
            object: Object,
        ) -> Self {
            let mut fake = Self::new();
            fake.links.insert(libc::AT_FDCWD, (link, object));
            fake
        }

        /// The file at `path`, if there is one.
        fn file(
            &self,
            path: &Path,
        ) -> io::Result<(u64, Option<&'static str>)> {
            path.to_str()
                .and_then(|path| self.files.get(path))
                .copied()
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
        }

        /// The link in `/proc` of descriptor `fd`, with its object.
        fn link(
            &self,
            fd: c_int,
        ) -> io::Result<(&'static str, Object)> {
            self.links
                .get(&fd)
                .copied()
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
        }
    }

    impl Inspect for Fake {
        fn memory(
            &self,
            address: u64,
            buffer: &mut [u8],
        ) -> io::Result<usize> {
            let fault = || io::Error::from_raw_os_error(libc::EFAULT);
            let (start, bytes) = self
                .memory
                .range(..=address)
                .next_back()
                .ok_or_else(fault)?;
            let offset = usize::try_from(address.checked_sub(*start).unwrap()).unwrap();
            let available = bytes
                .get(offset..)
                .filter(|rest| !rest.is_empty())
                .ok_or_else(fault)?;
            let count = available.len().min(buffer.len());
            buffer[..count].copy_from_slice(&available[..count]);
            Ok(count)
        }

        fn working_directory(&self) -> io::Result<Vec<u8>> {
            self.descriptor(libc::AT_FDCWD)
        }

        fn descriptor(
            &self,
            fd: c_int,
        ) -> io::Result<Vec<u8>> {
            Ok(self.link(fd)?.0.as_bytes().to_vec())
        }

        fn link_object(
            &self,
            fd: c_int,
        ) -> io::Result<Object> {
            Ok(self.link(fd)?.1)
        }

        fn path_object(
            &self,
            path: &Path,
        ) -> io::Result<Object> {
            if path.parent() == Some(Path::new(self.locked)) {
                return Err(io::Error::from_raw_os_error(libc::EACCES));
            }
            let (inode, target) = self.file(path)?;
            Ok(Object {
                directory: path
                    .to_str()
                    .is_some_and(|path| self.directories.contains(path)),
                ..object(inode, 1, target.is_some())
            })
        }

        fn link_target(
            &self,
            path: &Path,
        ) -> io::Result<Vec<u8>> {
            let target = self.file(path)?.1;
            target
                .map(|target| target.as_bytes().to_vec())
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))
        }

        fn read_file(
            &self,
            path: &Path,
            offset: u64,
            buffer: &mut [u8],
        ) -> io::Result<usize> {
            let contents = path
                .to_str()
                .and_then(|path| self.contents.get(path))
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?
                .as_ref()
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EACCES))?;
            let start = usize::try_from(offset).unwrap().min(contents.len());
            let available = &contents[start..];
            let count = available.len().min(buffer.len());
            buffer[..count].copy_from_slice(&available[..count]);
            Ok(count)
        }

        fn descriptor_file(
            &self,
            fd: c_int,
        ) -> Option<FileMetadata> {
            let (link, _) = self.link(fd).ok()?;
            self.regular.get(link).copied()
        }

        fn path_file(
            &self,
            path: &Path,
        ) -> Option<FileMetadata> {
            path.to_str()
                .and_then(|path| self.regular.get(path))
                .copied()
        }
    }

    /// The number of a tracked call by name.
    fn number(name: &str) -> c_long {
        SYSTEM_CALLS
            .iter()
            .find(|call| call.name == name)
            .unwrap_or_else(|| panic!("{name} is not tracked"))
            .number
    }

    /// Decode the call `name` with `arguments`, padded with zeros, in `fake`.
    fn decode(
        fake: &Fake,
        name: &str,
        arguments: &[u64],
    ) -> Call {
        let mut all = [0_u64; 6];
        all[..arguments.len()].copy_from_slice(arguments);
        enter(number(name), all, fake).unwrap()
    }

    /// The accesses and incomplete-trace report of the call `name` with
    /// `arguments` in `fake` when it returns `result`.
    fn finish_in(
        fake: &Fake,
        name: &str,
        arguments: &[u64],
        result: Option<i64>,
    ) -> (Vec<(String, String)>, Option<String>) {
        let mut accesses = Accesses::new();
        let problem = decode(fake, name, arguments).finish(result, &mut accesses);
        let paths = accesses
            .iter()
            .map(|(path, kinds)| (path.to_string(), kinds.to_string()))
            .collect();
        (paths, problem)
    }

    /// The accesses and incomplete-trace report of the call `name` with
    /// `arguments` when it returns `result`.
    fn finish(
        name: &str,
        arguments: &[u64],
        result: Option<i64>,
    ) -> (Vec<(String, String)>, Option<String>) {
        finish_in(&Fake::new(), name, arguments, result)
    }

    /// The accesses of the call `name` with `arguments` when it succeeds, which
    /// must decode completely.
    fn succeed(
        name: &str,
        arguments: &[u64],
    ) -> Vec<(String, String)> {
        let (paths, problem) = finish(name, arguments, Some(0));
        assert_eq!(problem, None, "{name}");
        paths
    }

    /// The expected accesses, from pairs of a path and its kinds.
    fn expect(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(path, kinds)| ((*path).to_owned(), (*kinds).to_owned()))
            .collect()
    }

    /// The result of a call that failed with `errno`.
    fn failed(errno: c_int) -> Option<i64> {
        0_i64.checked_sub(i64::from(errno))
    }

    /// An `int` argument as a register holds it.
    fn reg(value: c_int) -> u64 {
        u64::from(value.cast_unsigned())
    }

    #[test]
    fn every_tracked_call_has_a_decoding() {
        let decoded: Vec<(&str, c_long)> = SYSTEM_CALLS
            .iter()
            .map(|call| (call.name, call.number))
            .collect();
        let tracked: Vec<(&str, c_long)> = TRACKED_SYSTEM_CALLS
            .iter()
            .map(|call| (call.name, call.number))
            .collect();
        assert_eq!(decoded, tracked);
        for call in SYSTEM_CALLS {
            assert!(!call.rules.is_empty(), "{}", call.name);
        }
        assert_eq!(enter(libc::SYS_read, [0; 6], &Fake::new()), None);
    }

    #[test]
    fn opening_files_records_the_kinds_of_the_flags() {
        let flags = |name: &str, bits: c_int| succeed(name, &[REL, reg(bits)]);
        assert_eq!(
            flags("open", libc::O_RDONLY),
            expect(&[("/work/rel", "read")])
        );
        assert_eq!(
            flags("open", libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC),
            expect(&[("/work/rel", "write")])
        );
        assert_eq!(
            flags("open", libc::O_RDWR),
            expect(&[("/work/rel", "read+write")])
        );
        assert_eq!(
            flags("open", libc::O_RDONLY | libc::O_CREAT),
            expect(&[("/work/rel", "read+write")])
        );
        assert_eq!(
            flags("open", libc::O_PATH),
            expect(&[("/work/rel", "probe")])
        );
        assert_eq!(
            flags("open", libc::O_RDONLY | libc::O_DIRECTORY),
            expect(&[("/work/rel", "probe")])
        );
        assert_eq!(
            flags("open", libc::O_TMPFILE | libc::O_RDWR),
            expect(&[("/work/rel", "probe")])
        );
        assert_eq!(
            flags("open", libc::O_ACCMODE),
            expect(&[("/work/rel", "probe")])
        );
        assert_eq!(
            succeed("openat", &[DIR, REL, reg(libc::O_RDONLY)]),
            expect(&[("/dir/rel", "read")])
        );
        assert_eq!(
            succeed("openat", &[CWD, ABS, reg(libc::O_WRONLY)]),
            expect(&[("/abs/x/y", "write")])
        );
        assert_eq!(succeed("creat", &[REL]), expect(&[("/work/rel", "write")]));
    }

    #[test]
    fn openat2_reads_its_flags_and_resolution_from_open_how() {
        // `RESOLVE_IN_ROOT` makes an absolute path start at the descriptor, and
        // each directory the lookup passes through is a probe.
        assert_eq!(
            succeed("openat2", &[DIR, ABS, HOW, 24]),
            expect(&[
                ("/dir/abs", "probe"),
                ("/dir/abs/x", "probe"),
                ("/dir/abs/x/y", "write")
            ])
        );
        assert_eq!(
            succeed("openat2", &[DIR, REL, HOW, 24]),
            expect(&[("/dir/rel", "write")])
        );
        // Without it, the path is looked up as usual, and `..` is kept.
        assert_eq!(
            succeed("openat2", &[DIR, PARENT, HOW_PLAIN, 24]),
            expect(&[("/dir/../../rel", "read")])
        );
        assert_eq!(
            succeed("openat2", &[DIR, ABS, HOW_PLAIN, 24]),
            expect(&[("/abs/x/y", "read")])
        );
        let (paths, problem) = finish("openat2", &[DIR, REL, UNMAPPED, 24], Some(3));
        assert_eq!(paths, expect(&[]));
        assert!(problem.unwrap().contains("openat2"));
    }

    #[test]
    fn openat2_in_root_resolves_paths_as_the_kernel_does() {
        // `..` at the root stays at the root, and after a real directory goes to
        // its parent.
        assert_eq!(
            succeed("openat2", &[DIR, PARENT, HOW, 24]),
            expect(&[("/dir/rel", "write")])
        );
        assert_eq!(
            succeed("openat2", &[DIR, ABS_PARENT, HOW, 24]),
            expect(&[
                ("/dir/abs", "probe"),
                ("/dir/abs/x", "probe"),
                ("/dir/abs/x/y", "write")
            ])
        );
        // An absolute link target starts at the root, a relative one's `..`
        // stops there, and each link followed is a probe, as is each directory
        // its target passes through.
        assert_eq!(
            succeed("openat2", &[DIR, LINKED, HOW, 24]),
            expect(&[
                ("/dir/abs", "probe"),
                ("/dir/abs/x", "probe"),
                ("/dir/abs/x/y", "write"),
                ("/dir/top", "probe")
            ])
        );
        assert_eq!(
            succeed("openat2", &[DIR, UP, HOW, 24]),
            expect(&[
                ("/dir/abs", "probe"),
                ("/dir/abs/x", "probe"),
                ("/dir/abs/x/y", "write"),
                ("/dir/up", "probe")
            ])
        );
        // A link in the last component is followed unless `O_NOFOLLOW` says not to.
        assert_eq!(
            succeed("openat2", &[DIR, TOP, HOW, 24]),
            expect(&[("/dir/abs", "write"), ("/dir/top", "probe")])
        );
        assert_eq!(
            succeed("openat2", &[DIR, TOP, HOW_NOFOLLOW, 24]),
            expect(&[("/dir/top", "probe")])
        );
        // The lookup stops at the first component it does not find.
        assert_eq!(
            finish("openat2", &[DIR, MISSING, HOW, 24], failed(libc::ENOENT)),
            (expect(&[("/dir/nope", "probe")]), None)
        );
        // A lookup that follows too many links cannot be reported.
        let (paths, problem) = finish("openat2", &[DIR, LOOP, HOW, 24], failed(libc::ELOOP));
        assert_eq!(paths, expect(&[]));
        assert!(problem.unwrap().contains("symbolic links"));
    }

    #[test]
    fn openat2_in_root_stops_at_components_that_are_not_directories() {
        // `plain` is a regular file, so the kernel fails with `ENOTDIR` there,
        // before it looks at the `..` or `.` after it.
        for path in [PLAIN_UP, PLAIN_DOT] {
            assert_eq!(
                finish("openat2", &[DIR, path, HOW, 24], failed(libc::ENOTDIR)),
                (expect(&[("/dir/plain", "probe")]), None),
                "{path:#x}"
            );
        }
        // A link to a regular file is followed, and the lookup stops at the file.
        assert_eq!(
            finish("openat2", &[DIR, FILE_LINK, HOW, 24], failed(libc::ENOTDIR)),
            (
                expect(&[("/dir/file.link", "probe"), ("/dir/plain", "probe")]),
                None
            )
        );
        // Directories are traversed, and a regular file may be the last component.
        assert_eq!(
            succeed("openat2", &[DIR, REL, HOW, 24]),
            expect(&[("/dir/rel", "write")])
        );
        // A root that is not a directory fails before any component.
        assert_eq!(
            finish("openat2", &[FILE, REL, HOW, 24], failed(libc::ENOTDIR)),
            (expect(&[("/dir/file", "probe")]), None)
        );
        // A component that the tracer cannot check makes the trace incomplete
        // rather than reported as a guess.
        let (paths, problem) = finish("openat2", &[DIR, LOCKED, HOW, 24], failed(libc::EACCES));
        assert_eq!(paths, expect(&[]));
        assert!(problem.unwrap().contains("could not check /dir/locked/x"));
    }

    #[test]
    fn openat2_in_root_records_directories_that_parent_components_leave() {
        // `abs` is a directory that the lookup passes through and `..` leaves, so
        // removing it or replacing it with a file changes the result: it is a
        // probe, beside the object reached.
        assert_eq!(
            succeed("openat2", &[DIR, SUB_UP, HOW, 24]),
            expect(&[("/dir/abs", "probe"), ("/dir/rel", "write")])
        );
        // Each directory is recorded once, and one that the lookup also reaches
        // is recorded with the kinds of the call alone.
        assert_eq!(
            succeed("openat2", &[DIR, SUB_UP_TWICE, HOW_READ_NOFOLLOW, 24]),
            expect(&[("/dir/abs", "read"), ("/dir/abs/x", "probe")])
        );
        // A missing directory, `nope`, or a regular file, `plain`, in its place
        // stops the lookup, and the component where it stops is the probe.
        assert_eq!(
            finish("openat2", &[DIR, MISSING, HOW, 24], failed(libc::ENOENT)),
            (expect(&[("/dir/nope", "probe")]), None)
        );
        assert_eq!(
            finish("openat2", &[DIR, PLAIN_UP, HOW, 24], failed(libc::ENOTDIR)),
            (expect(&[("/dir/plain", "probe")]), None)
        );
        // Without `RESOLVE_IN_ROOT`, the path is reported as written.
        assert_eq!(
            succeed("openat2", &[DIR, SUB_UP, HOW_PLAIN, 24]),
            expect(&[("/dir/abs/../rel", "read")])
        );
    }

    #[test]
    fn openat2_in_root_follows_intermediate_links_despite_o_nofollow() {
        // `.` after `top` makes the link an intermediate component, so it is
        // followed and the directory it names is opened.
        assert_eq!(
            succeed("openat2", &[DIR, TOP_DOT, HOW_READ_NOFOLLOW, 24]),
            expect(&[("/dir/abs", "read"), ("/dir/top", "probe")])
        );
        // In the last component the link is not followed, and opening it fails.
        assert_eq!(
            finish(
                "openat2",
                &[DIR, TOP, HOW_READ_NOFOLLOW, 24],
                failed(libc::ELOOP)
            ),
            (expect(&[("/dir/top", "probe")]), None)
        );
    }

    /// The accesses and incomplete-trace report of `openat2` with `arguments`
    /// when it returns `result`, after checking the descriptor it returned.
    fn opened(
        arguments: &[u64],
        result: i64,
    ) -> (Vec<(String, String)>, Option<String>) {
        let fake = Fake::new();
        let mut call = decode(&fake, "openat2", arguments);
        call.opened(Some(result), &fake);
        let mut accesses = Accesses::new();
        let problem = call.finish(Some(result), &mut accesses);
        let paths = accesses
            .iter()
            .map(|(path, kinds)| (path.to_string(), kinds.to_string()))
            .collect();
        (paths, problem)
    }

    /// A descriptor as a system call returns it.
    fn returned(fd: u64) -> i64 {
        i64::try_from(fd).unwrap()
    }

    #[test]
    fn the_returned_descriptor_checks_the_rooted_lookup() {
        // The descriptor names the object the lookup reached.
        assert_eq!(
            opened(&[DIR, REL, HOW, 24], returned(OPENED)),
            (expect(&[("/dir/rel", "write")]), None)
        );
        // Another object replaces the lookup's, and the trace is incomplete.
        let (paths, problem) = opened(&[DIR, REL, HOW, 24], returned(FILE));
        assert_eq!(paths, expect(&[("/dir/file", "write")]));
        assert!(problem.unwrap().contains("names /dir/file, not /dir/rel"));
        // A removed object, or a descriptor that cannot be read, is not certain.
        let (paths, problem) = opened(&[DIR, REL, HOW, 24], returned(GONE));
        assert_eq!(paths, expect(&[("/dir/rel", "write")]));
        assert!(
            problem
                .unwrap()
                .contains("names /gone (deleted), not /dir/rel")
        );
        let (paths, problem) = opened(&[DIR, REL, HOW, 24], returned(CLOSED));
        assert_eq!(paths, expect(&[("/dir/rel", "write")]));
        assert!(problem.unwrap().contains("could not read the descriptor 9"));
        // A failed call, an unnamed temporary file, and an ordinary lookup are not
        // checked.
        assert_eq!(
            opened(&[DIR, REL, HOW, 24], -2),
            (expect(&[("/dir/rel", "probe")]), None)
        );
        assert_eq!(
            opened(&[DIR, TOP_DOT, HOW_TMPFILE, 24], returned(FILE)),
            (
                expect(&[("/dir/abs", "probe"), ("/dir/top", "probe")]),
                None
            )
        );
        assert_eq!(
            opened(&[DIR, REL, HOW_PLAIN, 24], returned(FILE)),
            (expect(&[("/dir/rel", "read")]), None)
        );
    }

    #[test]
    fn metadata_and_existence_checks_are_probes() {
        for name in ["stat", "lstat", "access", "chdir"] {
            assert_eq!(
                succeed(name, &[REL]),
                expect(&[("/work/rel", "probe")]),
                "{name}"
            );
        }
        assert_eq!(
            succeed("newfstatat", &[DIR, REL, 0, 0]),
            expect(&[("/dir/rel", "probe")])
        );
        assert_eq!(
            succeed("newfstatat", &[DIR, EMPTY, 0, EMPTY_PATH]),
            expect(&[("/dir", "probe")])
        );
        assert_eq!(
            succeed("newfstatat", &[FILE, 0, 0, EMPTY_PATH]),
            expect(&[("/dir/file", "probe")])
        );
        assert_eq!(
            succeed("statx", &[FILE, EMPTY, EMPTY_PATH, 0, 0]),
            expect(&[("/dir/file", "probe")])
        );
        assert_eq!(
            succeed("statx", &[CWD, REL, 0, 0, 0]),
            expect(&[("/work/rel", "probe")])
        );
        assert_eq!(
            succeed("faccessat", &[DIR, REL, 0]),
            expect(&[("/dir/rel", "probe")])
        );
        assert_eq!(
            succeed("faccessat2", &[CWD, EMPTY, 0, EMPTY_PATH]),
            expect(&[("/work", "probe")])
        );
        assert_eq!(
            succeed("faccessat2", &[DIR, OTHER, 0, 0]),
            expect(&[("/dir/other", "probe")])
        );
        assert_eq!(succeed("fchdir", &[DIR]), expect(&[("/dir", "probe")]));
    }

    #[test]
    fn links_record_their_names() {
        assert_eq!(
            succeed("readlink", &[REL]),
            expect(&[("/work/rel", "read")])
        );
        assert_eq!(
            succeed("readlinkat", &[FILE, EMPTY]),
            expect(&[("/dir/file", "read")])
        );
        assert_eq!(
            succeed("readlinkat", &[DIR, REL]),
            expect(&[("/dir/rel", "read")])
        );
        assert_eq!(
            succeed("link", &[REL, OTHER]),
            expect(&[("/work/other", "write"), ("/work/rel", "write")])
        );
        assert_eq!(
            succeed("linkat", &[FILE, EMPTY, DIR, OTHER, EMPTY_PATH]),
            expect(&[("/dir/file", "write"), ("/dir/other", "write")])
        );
        // The target of a symbolic link is text, not a path the call touches.
        assert_eq!(
            succeed("symlink", &[ABS, REL]),
            expect(&[("/work/rel", "write")])
        );
        assert_eq!(
            succeed("symlinkat", &[ABS, DIR, REL]),
            expect(&[("/dir/rel", "write")])
        );
    }

    #[test]
    fn listings_and_programs_are_recorded() {
        for name in ["getdents", "getdents64"] {
            assert_eq!(succeed(name, &[DIR]), expect(&[("/dir", "list")]), "{name}");
        }
        assert_eq!(succeed("execve", &[ABS]), expect(&[("/abs/x/y", "exec")]));
        assert_eq!(
            succeed("execveat", &[FILE, EMPTY, 0, 0, EMPTY_PATH]),
            expect(&[("/dir/file", "exec")])
        );
        assert_eq!(
            succeed("execveat", &[DIR, REL, 0, 0, 0]),
            expect(&[("/dir/rel", "exec")])
        );
    }

    #[test]
    fn changes_are_writes() {
        for name in [
            "unlink", "rmdir", "mkdir", "mknod", "truncate", "chmod", "chown", "lchown", "utime",
            "utimes",
        ] {
            assert_eq!(
                succeed(name, &[REL]),
                expect(&[("/work/rel", "write")]),
                "{name}"
            );
        }
        for name in ["unlinkat", "mkdirat", "mknodat", "fchmodat", "fchownat"] {
            assert_eq!(
                succeed(name, &[DIR, REL]),
                expect(&[("/dir/rel", "write")]),
                "{name}"
            );
        }
        assert_eq!(
            succeed("rename", &[REL, OTHER]),
            expect(&[("/work/other", "write"), ("/work/rel", "write")])
        );
        for name in ["renameat", "renameat2"] {
            assert_eq!(
                succeed(name, &[DIR, REL, CWD, OTHER]),
                expect(&[("/dir/rel", "write"), ("/work/other", "write")]),
                "{name}"
            );
        }
        assert_eq!(
            succeed("fchownat", &[FILE, EMPTY, 0, 0, EMPTY_PATH]),
            expect(&[("/dir/file", "write")])
        );
        assert_eq!(
            succeed("utimensat", &[CWD, REL, 0, 0]),
            expect(&[("/work/rel", "write")])
        );
        assert_eq!(
            succeed("utimensat", &[FILE, 0, 0, 0]),
            expect(&[("/dir/file", "write")])
        );
        assert_eq!(
            succeed("utimensat", &[FILE, EMPTY, 0, EMPTY_PATH]),
            expect(&[("/dir/file", "write")])
        );
        assert_eq!(
            succeed("futimesat", &[DIR, REL, 0]),
            expect(&[("/dir/rel", "write")])
        );
        assert_eq!(
            succeed("futimesat", &[FILE, 0, 0]),
            expect(&[("/dir/file", "write")])
        );
    }

    #[test]
    fn descriptor_changes_resolve_to_the_path_behind_the_descriptor() {
        for name in ["ftruncate", "fchmod", "fchown"] {
            assert_eq!(
                succeed(name, &[FILE]),
                expect(&[("/dir/file", "write")]),
                "{name}"
            );
            // A pipe and a removed file name no path.
            assert_eq!(succeed(name, &[PIPE]), expect(&[]), "{name}");
            assert_eq!(succeed(name, &[GONE]), expect(&[]), "{name}");
        }
    }

    #[test]
    fn live_names_with_the_removed_suffix_are_kept() {
        // A live file and directory whose names end in ` (deleted)`.
        assert_eq!(
            succeed("fchmod", &[LIVE]),
            expect(&[("/dir/live (deleted)", "write")])
        );
        assert_eq!(
            succeed("getdents64", &[LIVE_DIR]),
            expect(&[("/dir/sub (deleted)", "list")])
        );
        assert_eq!(
            succeed("openat", &[LIVE_DIR, REL, reg(libc::O_RDONLY)]),
            expect(&[("/dir/sub (deleted)/rel", "read")])
        );
        assert_eq!(
            succeed("openat2", &[LIVE_DIR, PARENT, HOW, 24]),
            expect(&[("/dir/sub (deleted)/rel", "write")])
        );
        // A working directory with the suffix.
        let live = Fake::in_directory("/dir/sub (deleted)", object(10, 2, false));
        assert_eq!(
            finish_in(&live, "stat", &[REL], Some(0)),
            (expect(&[("/dir/sub (deleted)/rel", "probe")]), None)
        );
        // A removed working directory names no path.
        let removed = Fake::in_directory("/gone.d (deleted)", object(18, 0, false));
        assert_eq!(
            finish_in(&removed, "stat", &[REL], failed(libc::ENOENT)),
            (expect(&[]), None)
        );
        // An object that lost the name but has another cannot be told apart.
        let (paths, problem) = finish("fchmod", &[RENAMED], Some(0));
        assert_eq!(paths, expect(&[]));
        assert!(problem.unwrap().contains("could not tell whether"));
    }

    #[test]
    fn failed_calls_are_probes() {
        let enoent = failed(libc::ENOENT);
        assert_eq!(
            finish("stat", &[REL], enoent),
            (expect(&[("/work/rel", "probe")]), None)
        );
        let flags = reg(libc::O_WRONLY | libc::O_CREAT);
        assert_eq!(
            finish("open", &[REL, flags], failed(libc::EACCES)),
            (expect(&[("/work/rel", "probe")]), None)
        );
        assert_eq!(
            finish("rename", &[REL, OTHER], enoent),
            (
                expect(&[("/work/other", "probe"), ("/work/rel", "probe")]),
                None
            )
        );
        assert_eq!(
            finish("execve", &[ABS], enoent),
            (expect(&[("/abs/x/y", "probe")]), None)
        );
    }

    #[test]
    fn interrupted_and_unfinished_calls() {
        // A call that a signal interrupts looked its path up, whether it returns
        // `EINTR` or one of the kernel's restart errors; a call that restarts is
        // decoded again at its next entry.
        for result in [
            failed(libc::EINTR),
            Some(-512),
            Some(-513),
            Some(-514),
            Some(-516),
        ] {
            assert_eq!(
                finish("stat", &[REL], result),
                (expect(&[("/work/rel", "probe")]), None),
                "{result:?}"
            );
        }
        assert_eq!(
            finish("open", &[REL, reg(libc::O_RDONLY)], Some(-512)),
            (expect(&[("/work/rel", "probe")]), None)
        );
        // A call whose result was never seen may or may not have taken effect.
        assert_eq!(
            finish("unlink", &[REL], None),
            (expect(&[("/work/rel", "probe+write")]), None)
        );
    }

    #[test]
    fn undecodable_arguments_make_the_trace_incomplete_unless_the_call_rejected_them() {
        let bad_address = failed(libc::EFAULT);
        assert_eq!(finish("stat", &[0], bad_address), (expect(&[]), None));
        assert_eq!(
            finish("stat", &[UNMAPPED], bad_address),
            (expect(&[]), None)
        );
        let (paths, problem) = finish("stat", &[UNMAPPED], Some(0));
        assert_eq!(paths, expect(&[]));
        assert!(problem.unwrap().starts_with("could not decode stat: "));
        let (_, problem) = finish("stat", &[0], failed(libc::ENOENT));
        assert!(problem.unwrap().contains("null"));

        let bad_descriptor = failed(libc::EBADF);
        assert_eq!(
            finish("fchmod", &[CLOSED], bad_descriptor),
            (expect(&[]), None)
        );
        assert!(finish("fchmod", &[CLOSED], Some(0)).1.is_some());
        assert_eq!(
            finish("openat", &[CLOSED, REL, 0], bad_descriptor),
            (expect(&[]), None)
        );

        let too_long = failed(libc::ENAMETOOLONG);
        assert_eq!(finish("stat", &[LONG], too_long), (expect(&[]), None));
        let (_, problem) = finish("stat", &[LONG], Some(0));
        assert!(problem.unwrap().contains("PATH_MAX"));

        // The decodable path of a call is kept when another part is not.
        let (paths, problem) = finish("rename", &[REL, UNMAPPED], Some(0));
        assert_eq!(paths, expect(&[("/work/rel", "write")]));
        assert!(problem.is_some());
    }

    #[test]
    fn empty_paths_without_a_descriptor_touch_nothing() {
        assert_eq!(succeed("stat", &[EMPTY]), expect(&[]));
        assert_eq!(succeed("newfstatat", &[DIR, EMPTY, 0, 0]), expect(&[]));
        assert_eq!(succeed("open", &[EMPTY, 0]), expect(&[]));
    }

    #[test]
    fn paths_are_read_across_page_boundaries() {
        assert_eq!(
            succeed("stat", &[CROSSING]),
            expect(&[("/work/cross", "probe")])
        );
    }

    #[test]
    fn paths_are_normalized_and_keep_parent_components() {
        // `.` components and repeated separators go, `..` stays.
        assert_eq!(
            succeed("stat", &[DOTTED]),
            expect(&[("/work/a/b/../c", "probe")])
        );
        assert_eq!(succeed("stat", &[ABS]), expect(&[("/abs/x/y", "probe")]));
        assert_eq!(
            link_path(b"/one//two/./three/../four/"),
            Some(AbsolutePath::new("/one/two/three/../four").unwrap())
        );
        assert_eq!(
            AbsolutePath::new("/base")
                .unwrap()
                .join(Path::new(".//x/./../y"))
                .as_path(),
            Path::new("/base/x/../y")
        );
    }

    #[test]
    fn links_that_name_no_path_are_skipped() {
        assert_eq!(link_path(b"pipe:[12]"), None);
        assert_eq!(link_path(b"anon_inode:[eventfd]"), None);
        // The suffix of a removed object is decided by comparing objects, because a
        // live name may end with it too.
        assert_eq!(
            link_path(b"/tmp/file (deleted)"),
            Some(AbsolutePath::new("/tmp/file (deleted)").unwrap())
        );
        assert_eq!(
            link_path(b"/tmp/file"),
            Some(AbsolutePath::new("/tmp/file").unwrap())
        );
    }

    #[test]
    fn int_arguments_use_the_low_32_bits() {
        assert_eq!(int(CWD), libc::AT_FDCWD);
        assert_eq!(int(0xFFFF_FFFF_0000_0003), 3);
        assert_eq!(int(reg(libc::AT_FDCWD)), libc::AT_FDCWD);
        assert_eq!(reg(libc::AT_EMPTY_PATH), EMPTY_PATH);
    }

    /// The address of the program path of the `exec` tests.
    const PROGRAM: u64 = 0x2_0000;

    /// The fake thread, whose memory holds `program` at [`PROGRAM`], in a file
    /// system that holds the readable `files`, each with its content.
    fn with_files(
        program: &str,
        files: &[(&str, Vec<u8>)],
    ) -> Fake {
        let mut fake = Fake::new();
        fake.memory
            .insert(PROGRAM, format!("{program}\0").into_bytes());
        for (path, content) in files {
            fake.contents
                .insert((*path).to_owned(), Some(content.clone()));
        }
        fake
    }

    /// The accesses and incomplete-trace report of an `exec` call `name` with
    /// `arguments` in `fake` that returns `result`, with what the kernel loaded.
    fn exec_in(
        fake: &Fake,
        name: &str,
        arguments: &[u64],
        result: Option<i64>,
    ) -> (Vec<(String, String)>, Option<String>) {
        let mut call = decode(fake, name, arguments);
        call.loaded(result, fake);
        let mut accesses = Accesses::new();
        let problem = call.finish(result, &mut accesses);
        let paths = accesses
            .iter()
            .map(|(path, kinds)| (path.to_string(), kinds.to_string()))
            .collect();
        (paths, problem)
    }

    /// The accesses and report of `execve` of the program at [`PROGRAM`] in
    /// `fake` that returns `result`.
    fn execve(
        fake: &Fake,
        result: Option<i64>,
    ) -> (Vec<(String, String)>, Option<String>) {
        exec_in(fake, "execve", &[PROGRAM, 0, 0], result)
    }

    /// A file that starts with `text`.
    fn text(text: &str) -> Vec<u8> {
        text.as_bytes().to_vec()
    }

    /// A 64-bit ELF executable with the class byte `class` for the machine
    /// `machine`, whose program headers are a `PT_LOAD` header and, with
    /// `loader`, a `PT_INTERP` header that names it.
    fn elf(
        class: u8,
        machine: u16,
        loader: Option<&str>,
    ) -> Vec<u8> {
        let count: u16 = if loader.is_some() { 2 } else { 1 };
        let mut bytes = vec![0_u8; ELF_HEADER_SIZE];
        bytes[..4].copy_from_slice(ELF_MAGIC);
        bytes[ELF_CLASS] = class;
        bytes[ELF_DATA] = ELF_DATA_LITTLE;
        bytes[6] = 1;
        bytes[ELF_TYPE..ELF_TYPE + 2].copy_from_slice(&ELF_TYPE_DYN.to_le_bytes());
        bytes[ELF_MACHINE..ELF_MACHINE + 2].copy_from_slice(&machine.to_le_bytes());
        bytes[ELF_PROGRAM_HEADERS..ELF_PROGRAM_HEADERS + 8].copy_from_slice(&64_u64.to_le_bytes());
        bytes[ELF_PROGRAM_HEADER_SIZE..ELF_PROGRAM_HEADER_SIZE + 2]
            .copy_from_slice(&56_u16.to_le_bytes());
        bytes[ELF_PROGRAM_HEADER_COUNT..ELF_PROGRAM_HEADER_COUNT + 2]
            .copy_from_slice(&count.to_le_bytes());
        let mut load = vec![0_u8; PROGRAM_HEADER_SIZE];
        load[..4].copy_from_slice(&1_u32.to_le_bytes());
        bytes.extend(load);
        if let Some(loader) = loader {
            let offset = u64::try_from(ELF_HEADER_SIZE + 2 * PROGRAM_HEADER_SIZE).unwrap();
            let size = u64::try_from(loader.len().checked_add(1).unwrap()).unwrap();
            let mut interp = vec![0_u8; PROGRAM_HEADER_SIZE];
            interp[..4].copy_from_slice(&PT_INTERP.to_le_bytes());
            interp[PROGRAM_HEADER_OFFSET..PROGRAM_HEADER_OFFSET + 8]
                .copy_from_slice(&offset.to_le_bytes());
            interp[PROGRAM_HEADER_FILE_SIZE..PROGRAM_HEADER_FILE_SIZE + 8]
                .copy_from_slice(&size.to_le_bytes());
            bytes.extend(interp);
            bytes.extend(loader.as_bytes());
            bytes.push(0);
        }
        bytes
    }

    /// A dynamically linked `x86_64` executable whose loader is `loader`.
    fn dynamic(loader: &str) -> Vec<u8> {
        elf(ELF_CLASS_64, ELF_MACHINE_X86_64, Some(loader))
    }

    /// The interpreter that the `#!` line of `header` names, as text.
    fn interpreter(header: &[u8]) -> Option<String> {
        script_interpreter(header).map(|name| String::from_utf8(name).unwrap())
    }

    /// The interpreter of a `#!` line is found as the kernel finds it, within
    /// the bytes that the kernel reads, and nothing else is taken for one.
    #[test]
    fn script_lines_are_parsed_as_the_kernel_parses_them() {
        let named = |name: &str| Some(name.to_owned());
        assert_eq!(interpreter(b"#!/bin/sh\necho\n"), named("/bin/sh"));
        // Spaces and tabs before and after the interpreter, and an argument.
        assert_eq!(interpreter(b"#! \t/bin/sh \t\n"), named("/bin/sh"));
        assert_eq!(
            interpreter(b"#!/usr/bin/env python3 -u\n"),
            named("/usr/bin/env")
        );
        assert_eq!(interpreter(b"#!/bin/sh\t-e\n"), named("/bin/sh"));
        // A relative path, as written.
        assert_eq!(interpreter(b"#!rel/interp arg\n"), named("rel/interp"));
        // A file without a newline, and a NUL byte that ends the line.
        assert_eq!(interpreter(b"#!/bin/sh"), named("/bin/sh"));
        assert_eq!(interpreter(b"#!/bin/sh\0\n"), named("/bin/sh"));
        // No interpreter.
        for header in [&b"#!\n"[..], b"#!  \t \n", b"#!", b"#! \0/bin/sh\n"] {
            assert_eq!(interpreter(header), None, "{header:?}");
        }
        // Not a `#!` line.
        for header in [&b"#/bin/sh\n"[..], b" #!/bin/sh\n", b"", b"#"] {
            assert_eq!(interpreter(header), None, "{header:?}");
        }
        // More than the kernel reads: an interpreter path that runs past the end
        // of the buffer was cut, so the kernel finds none, but an argument that
        // does only loses its end.
        let long_name = format!("#!/{}\n", "a".repeat(300));
        assert_eq!(interpreter(long_name.as_bytes()), None);
        let blanks = format!("#!{}/bin/sh\n", " ".repeat(300));
        assert_eq!(interpreter(blanks.as_bytes()), None);
        let long_argument = format!("#!/bin/sh {}\n", "x".repeat(300));
        assert_eq!(interpreter(long_argument.as_bytes()), named("/bin/sh"));
        // A newline beyond what the kernel reads does not count.
        let late_newline = format!("#!/{}/sh\n", "a".repeat(260));
        assert_eq!(interpreter(late_newline.as_bytes()), None);
        // A name that ends exactly where the kernel stops reading is cut too.
        let exact = format!("#!{}", "b".repeat(HEADER_SIZE - 2));
        assert_eq!(interpreter(exact.as_bytes()), None);
    }

    /// Only a complete little-endian 64-bit `x86_64` executable has its program
    /// headers read.
    #[test]
    fn elf_headers_are_recognized_only_for_x86_64_executables() {
        let header = |bytes: Vec<u8>| elf_header(&bytes);
        let static_program = header(elf(ELF_CLASS_64, ELF_MACHINE_X86_64, None)).unwrap();
        assert_eq!(static_program, ElfHeader {
            offset: 64,
            entry_size: 56,
            count: 1,
        });
        assert_eq!(
            header(elf(ELF_CLASS_64, ELF_MACHINE_X86_64, Some("/ld.so")))
                .unwrap()
                .count,
            2
        );
        // A 32-bit ELF and an aarch64 ELF are not x86_64 executables.
        assert_eq!(header(elf(1, ELF_MACHINE_X86_64, None)), None);
        assert_eq!(header(elf(1, 3, None)), None);
        assert_eq!(header(elf(ELF_CLASS_64, 183, None)), None);
        // A truncated header, a big-endian one and an object file are not either.
        let full = elf(ELF_CLASS_64, ELF_MACHINE_X86_64, None);
        assert_eq!(elf_header(&full[..40]), None);
        assert_eq!(elf_header(&full[..ELF_HEADER_SIZE - 1]), None);
        let mut big_endian = full.clone();
        big_endian[ELF_DATA] = 2;
        assert_eq!(elf_header(&big_endian), None);
        let mut relocatable = full;
        relocatable[ELF_TYPE] = 1;
        assert_eq!(elf_header(&relocatable), None);
    }

    /// A script run through a script records both scripts, the executable at
    /// the end of the chain and its loader, as run or as probed by the result.
    #[test]
    fn a_chain_of_scripts_records_each_interpreter_and_the_loader() {
        // A script whose interpreter is a script, whose relative interpreter is
        // resolved against the working directory, and is a dynamic executable.
        let fake = with_files("/p/outer", &[
            ("/p/outer", text("#!/p/inner first\necho\n")),
            ("/p/inner", text("#! \tbin/program second\n")),
            ("/work/bin/program", dynamic("/lib64/ld.so")),
        ]);
        assert_eq!(
            execve(&fake, Some(0)),
            (
                expect(&[
                    ("/lib64/ld.so", "exec"),
                    ("/p/inner", "exec"),
                    ("/p/outer", "exec"),
                    ("/work/bin/program", "exec")
                ]),
                None
            )
        );
        // A failed run probes each path, and one whose result is unknown may have
        // run them.
        assert_eq!(
            execve(&fake, failed(libc::E2BIG)),
            (
                expect(&[
                    ("/lib64/ld.so", "probe"),
                    ("/p/inner", "probe"),
                    ("/p/outer", "probe"),
                    ("/work/bin/program", "probe")
                ]),
                None
            )
        );
        assert_eq!(
            execve(&fake, None),
            (
                expect(&[
                    ("/lib64/ld.so", "probe+exec"),
                    ("/p/inner", "probe+exec"),
                    ("/p/outer", "probe+exec"),
                    ("/work/bin/program", "probe+exec")
                ]),
                None
            )
        );
    }

    /// A dynamically linked executable records its loader, a static one none,
    /// whether it is run by path or through a descriptor.
    #[test]
    fn elf_executables_record_their_loader_if_they_have_one() {
        let fake = with_files("/p/dynamic", &[("/p/dynamic", dynamic("/lib64/ld.so"))]);
        assert_eq!(
            execve(&fake, Some(0)),
            (
                expect(&[("/lib64/ld.so", "exec"), ("/p/dynamic", "exec")]),
                None
            )
        );
        // A relative loader path is resolved against the working directory.
        let fake = with_files("/p/dynamic", &[("/p/dynamic", dynamic("ld/rel.so"))]);
        assert_eq!(
            execve(&fake, Some(0)),
            (
                expect(&[("/p/dynamic", "exec"), ("/work/ld/rel.so", "exec")]),
                None
            )
        );
        // A static executable has no loader.
        let fake = with_files("/p/static", &[(
            "/p/static",
            elf(ELF_CLASS_64, ELF_MACHINE_X86_64, None),
        )]);
        assert_eq!(
            execve(&fake, Some(0)),
            (expect(&[("/p/static", "exec")]), None)
        );
        // `execveat` on a descriptor reads the file behind it.
        let fake = with_files("/dir/file", &[("/dir/file", dynamic("/lib64/ld.so"))]);
        assert_eq!(
            exec_in(&fake, "execveat", &[FILE, EMPTY, 0, 0, EMPTY_PATH], Some(0)),
            (
                expect(&[("/dir/file", "exec"), ("/lib64/ld.so", "exec")]),
                None
            )
        );
    }

    /// A `#!` interpreter that does not exist, at any depth, is recorded as
    /// probed when the `exec` fails.
    #[test]
    fn a_missing_interpreter_is_probed() {
        let fake = with_files("/p/broken", &[("/p/broken", text("#!/p/missing\n"))]);
        assert_eq!(
            execve(&fake, failed(libc::ENOENT)),
            (
                expect(&[("/p/broken", "probe"), ("/p/missing", "probe")]),
                None
            )
        );
        // A missing interpreter of an interpreter too.
        let fake = with_files("/p/outer", &[
            ("/p/outer", text("#!/p/inner\n")),
            ("/p/inner", text("#!missing\n")),
        ]);
        assert_eq!(
            execve(&fake, failed(libc::ENOENT)),
            (
                expect(&[
                    ("/p/inner", "probe"),
                    ("/p/outer", "probe"),
                    ("/work/missing", "probe")
                ]),
                None
            )
        );
    }

    /// Interpreters are read only as deep as the kernel reads them, and a run
    /// deeper than that cannot be followed.
    #[test]
    fn interpreters_are_followed_as_deep_as_the_kernel_follows_them() {
        // The program and five interpreters are read; the sixth interpreter is
        // only looked up, before the kernel fails with `ELOOP`.
        let names: Vec<String> = (0..=8).map(|index| format!("/p/s{index}")).collect();
        let files: Vec<(String, Vec<u8>)> = names
            .windows(2)
            .map(|pair| (pair[0].clone(), text(&format!("#!{}\n", pair[1]))))
            .collect();
        let borrowed: Vec<(&str, Vec<u8>)> = files
            .iter()
            .map(|(path, content)| (path.as_str(), content.clone()))
            .collect();
        let fake = with_files("/p/s0", &borrowed);
        let (paths, problem) = execve(&fake, failed(libc::ELOOP));
        let expected: Vec<(String, String)> = (0..=6)
            .map(|index| (format!("/p/s{index}"), "probe".to_owned()))
            .collect();
        assert_eq!((paths, problem), (expected, None));
        // A run that the kernel reports as successful cannot be followed.
        let (_, problem) = execve(&fake, Some(0));
        assert!(problem.unwrap().contains("deeper than the kernel follows"));
    }

    /// A program that is neither a script nor an `x86_64` executable makes a
    /// successful run incomplete, and a failed one only probes it.
    #[test]
    fn programs_of_unknown_formats_make_a_successful_run_incomplete() {
        let truncated = elf(ELF_CLASS_64, ELF_MACHINE_X86_64, None)[..40].to_vec();
        let unknown = [
            ("32-bit", elf(1, 3, Some("/lib/ld-linux.so.2"))),
            (
                "aarch64",
                elf(ELF_CLASS_64, 183, Some("/lib/ld-aarch64.so")),
            ),
            ("truncated", truncated),
            ("text", text("plain text\n")),
            ("no interpreter", text("#!\n")),
            ("empty", Vec::new()),
        ];
        for (label, content) in unknown {
            let fake = with_files("/p/program", &[("/p/program", content)]);
            // The kernel runs such a file only through another handler, such as
            // `binfmt_misc`, whose interpreter the tracer cannot see.
            let (paths, problem) = execve(&fake, Some(0));
            assert_eq!(paths, expect(&[("/p/program", "exec")]), "{label}");
            let problem = problem.unwrap();
            assert!(
                problem.contains("/p/program") && problem.contains("neither"),
                "{label}: {problem}"
            );
            // Without such a handler, it fails with `ENOEXEC` and ran nothing.
            assert_eq!(
                execve(&fake, failed(libc::ENOEXEC)),
                (expect(&[("/p/program", "probe")]), None),
                "{label}"
            );
        }
        // The last file of a chain counts, not the program.
        let fake = with_files("/p/script", &[
            ("/p/script", text("#!/p/data\n")),
            ("/p/data", text("data\n")),
        ]);
        let (paths, problem) = execve(&fake, Some(0));
        assert_eq!(paths, expect(&[("/p/data", "exec"), ("/p/script", "exec")]));
        let problem = problem.unwrap();
        assert!(
            problem.contains("/p/script") && problem.contains("/p/data is neither"),
            "{problem}"
        );
    }

    /// A program, interpreter or loader that the tracer cannot read or name
    /// makes a successful run incomplete.
    #[test]
    fn programs_the_tracer_cannot_read_make_a_successful_run_incomplete() {
        // A program that can be run but not read, and an interpreter that is not
        // found by the tracer although the kernel ran it.
        let mut fake = with_files("/p/hidden", &[]);
        fake.contents.insert("/p/hidden".to_owned(), None);
        let (paths, problem) = execve(&fake, Some(0));
        assert_eq!(paths, expect(&[("/p/hidden", "exec")]));
        assert!(problem.unwrap().contains("could not read /p/hidden"));
        assert_eq!(
            execve(&fake, failed(libc::EACCES)),
            (expect(&[("/p/hidden", "probe")]), None)
        );
        let fake = with_files("/p/script", &[("/p/script", text("#!/p/gone\n"))]);
        let (paths, problem) = execve(&fake, Some(0));
        assert_eq!(paths, expect(&[("/p/gone", "exec"), ("/p/script", "exec")]));
        assert!(problem.unwrap().contains("could not read /p/gone"));
        // Program headers that the tracer cannot read, and a loader path without
        // its NUL byte.
        let mut cut = dynamic("/lib64/ld.so");
        cut.truncate(ELF_HEADER_SIZE + PROGRAM_HEADER_SIZE);
        let mut unterminated = dynamic("/lib64/ld.so");
        unterminated.pop();
        unterminated.push(b'x');
        for content in [cut, unterminated] {
            let fake = with_files("/p/elf", &[("/p/elf", content)]);
            let (paths, problem) = execve(&fake, Some(0));
            assert_eq!(paths, expect(&[("/p/elf", "exec")]));
            assert!(problem.unwrap().contains("/p/elf"));
        }
        // A program behind a descriptor that names no path.
        let fake = with_files("/p/unused", &[]);
        let (paths, problem) =
            exec_in(&fake, "execveat", &[PIPE, EMPTY, 0, 0, EMPTY_PATH], Some(0));
        assert_eq!(paths, expect(&[]));
        assert!(problem.unwrap().contains("cannot tell"));
        // A relative interpreter of a thread whose working directory is unknown.
        let mut fake = with_files("/p/script", &[("/p/script", text("#!rel\n"))]);
        fake.links.remove(&libc::AT_FDCWD);
        let (paths, problem) = execve(&fake, Some(0));
        assert_eq!(paths, expect(&[("/p/script", "exec")]));
        assert!(problem.unwrap().contains("working directory"));
    }

    /// A call that is not an `exec` records only its own access, never the
    /// interpreter of the file it opens.
    #[test]
    fn calls_other_than_exec_load_nothing() {
        let fake = with_files("/p/outer", &[("/work/rel", text("#!/p/inner\n"))]);
        assert_eq!(
            exec_in(&fake, "open", &[REL, reg(libc::O_RDONLY)], Some(3)),
            (expect(&[("/work/rel", "read")]), None)
        );
    }

    /// The metadata of a regular file whose inode number is `inode`.
    fn regular(inode: u64) -> FileMetadata {
        FileMetadata {
            device: 1,
            inode,
            size: 4,
            mtime_nanos: 5,
            ctime_nanos: 6,
            mode: 0o100_644,
            uid: 7,
            gid: 8,
        }
    }

    /// The first reads of the call `name` with `arguments`, decoded at its entry in
    /// `before` and returning `result` in `after`, as paths with inode numbers.
    fn first_reads_across(
        before: &Fake,
        after: &Fake,
        name: &str,
        arguments: &[u64],
        result: Option<i64>,
    ) -> Vec<(String, u64)> {
        let mut call = decode(before, name, arguments);
        call.loaded(result, after);
        call.first_reads(result, after)
            .into_iter()
            .map(|(path, metadata)| (path.to_string(), metadata.inode))
            .collect()
    }

    /// The first reads of the call `name` with `arguments` in `fake` when it
    /// returns `result`, as paths with inode numbers.
    fn first_reads_in(
        fake: &Fake,
        name: &str,
        arguments: &[u64],
        result: Option<i64>,
    ) -> Vec<(String, u64)> {
        first_reads_across(fake, fake, name, arguments, result)
    }

    /// The expected first reads, from pairs of a path and its inode number.
    fn noted(pairs: &[(&str, u64)]) -> Vec<(String, u64)> {
        pairs
            .iter()
            .map(|(path, inode)| ((*path).to_owned(), *inode))
            .collect()
    }

    /// A successful call that opens a regular file for reading gives the metadata
    /// of the file behind the descriptor it returned, and a call that only writes,
    /// truncates, only looks the path up, or fails gives nothing.
    #[test]
    fn opening_for_reading_gives_the_file_behind_the_returned_descriptor() {
        let mut fake = Fake::new();
        fake.regular.insert("/dir/rel".to_owned(), regular(11));
        fake.regular.insert("/work/rel".to_owned(), regular(21));
        fake.links.insert(12, ("/work/rel", object(21, 1, false)));
        let opened = Some(returned(OPENED));
        let openat = |flags: c_int, result: Option<i64>| {
            first_reads_in(&fake, "openat", &[DIR, REL, reg(flags)], result)
        };
        let rel = noted(&[("/dir/rel", 11)]);
        for flags in [
            libc::O_RDONLY,
            libc::O_RDWR,
            libc::O_RDWR | libc::O_CREAT,
            libc::O_RDONLY | libc::O_NOFOLLOW,
        ] {
            assert_eq!(openat(flags, opened), rel, "{flags:#o}");
        }
        for flags in [
            libc::O_WRONLY,
            libc::O_RDWR | libc::O_TRUNC,
            libc::O_RDONLY | libc::O_TRUNC,
            libc::O_PATH,
            libc::O_RDONLY | libc::O_DIRECTORY,
            libc::O_TMPFILE | libc::O_RDWR,
        ] {
            assert_eq!(openat(flags, opened), noted(&[]), "{flags:#o}");
        }
        assert_eq!(openat(libc::O_RDONLY, failed(libc::ENOENT)), noted(&[]));
        assert_eq!(openat(libc::O_RDONLY, None), noted(&[]));
        // `open` and `openat2` too, and a descriptor that names no regular file
        // gives nothing.
        assert_eq!(
            first_reads_in(&fake, "open", &[REL, reg(libc::O_RDONLY)], Some(12)),
            noted(&[("/work/rel", 21)])
        );
        assert_eq!(
            first_reads_in(&fake, "openat2", &[DIR, REL, HOW_PLAIN, 24], opened),
            rel
        );
        assert_eq!(
            first_reads_in(
                &fake,
                "open",
                &[REL, reg(libc::O_RDONLY)],
                Some(returned(PIPE))
            ),
            noted(&[])
        );
        // Reading a link opens no file.
        assert_eq!(
            first_reads_in(&fake, "readlink", &[REL], Some(4)),
            noted(&[])
        );
    }

    /// An opening call with `O_CREAT` reads first only a file that was at the path
    /// at its entry; a file it created was written first.
    #[test]
    fn creating_a_file_gives_nothing() {
        let created = Fake::new();
        let mut existing = Fake::new();
        existing.regular.insert("/dir/rel".to_owned(), regular(11));
        let arguments = [DIR, REL, reg(libc::O_RDWR | libc::O_CREAT)];
        let opened = Some(returned(OPENED));
        assert_eq!(
            first_reads_across(&created, &existing, "openat", &arguments, opened),
            noted(&[])
        );
        assert_eq!(
            first_reads_across(&existing, &existing, "openat", &arguments, opened),
            noted(&[("/dir/rel", 11)])
        );
    }

    /// A successful `exec` gives the metadata of the program and of each
    /// interpreter and loader the kernel loaded, through their paths, in order; a
    /// failed one gives nothing.
    #[test]
    fn running_gives_the_program_and_what_the_kernel_loaded() {
        let mut fake = with_files("/p/outer", &[
            ("/p/outer", text("#!/p/inner\n")),
            ("/p/inner", dynamic("/lib64/ld.so")),
        ]);
        for (inode, path) in [(31, "/p/outer"), (32, "/p/inner"), (33, "/lib64/ld.so")] {
            fake.regular.insert(path.to_owned(), regular(inode));
        }
        assert_eq!(
            first_reads_in(&fake, "execve", &[PROGRAM, 0, 0], Some(0)),
            noted(&[("/p/outer", 31), ("/p/inner", 32), ("/lib64/ld.so", 33)])
        );
        assert_eq!(
            first_reads_in(&fake, "execve", &[PROGRAM, 0, 0], failed(libc::ENOENT)),
            noted(&[])
        );
        assert_eq!(
            first_reads_in(&fake, "execve", &[PROGRAM, 0, 0], None),
            noted(&[])
        );
    }

    /// `AT_SYMLINK_FOLLOW` as a register holds it.
    const SYMLINK_FOLLOW: u64 = 0x400;
    /// `AT_SYMLINK_NOFOLLOW` as a register holds it.
    const SYMLINK_NOFOLLOW: u64 = 0x100;
    /// A descriptor of an inotify instance or a fanotify group, which names no
    /// path that the call touches.
    const WATCHER: u64 = 12;
    /// `FAN_MARK_ADD`, from the kernel's `include/uapi/linux/fanotify.h`.
    const MARK_ADD: u64 = 0x1;
    /// `FAN_MARK_DONT_FOLLOW`, from the kernel's `include/uapi/linux/fanotify.h`.
    const MARK_DONT_FOLLOW: u64 = 0x4;
    /// `FAN_OPEN`, a mask of `fanotify_mark`.
    const FAN_OPEN: u64 = 0x20;
    /// `IN_DONT_FOLLOW` with `IN_OPEN`, a mask of `inotify_add_watch`.
    const IN_DONT_FOLLOW_OPEN: u64 = 0x0200_0020;

    /// `file_getattr` and `name_to_handle_at` probe the path relative to their
    /// directory descriptor, and the descriptor itself with `AT_EMPTY_PATH`, whether
    /// or not they follow a final link; `statfs` probes its path and `fstatfs` the
    /// path behind its descriptor; a failure is a probe too.
    #[test]
    fn file_attributes_handles_and_file_systems_are_probes() {
        for name in ["file_getattr", "name_to_handle_at"] {
            assert_eq!(
                succeed(name, &[DIR, REL, 0, 0, 0]),
                expect(&[("/dir/rel", "probe")]),
                "{name}"
            );
            assert_eq!(
                succeed(name, &[CWD, ABS, 0, 0, SYMLINK_NOFOLLOW]),
                expect(&[("/abs/x/y", "probe")]),
                "{name}"
            );
            assert_eq!(
                succeed(name, &[DIR, EMPTY, 0, 0, EMPTY_PATH]),
                expect(&[("/dir", "probe")]),
                "{name}"
            );
            assert_eq!(
                succeed(name, &[FILE, EMPTY, 0, 0, EMPTY_PATH]),
                expect(&[("/dir/file", "probe")]),
                "{name}"
            );
            assert_eq!(succeed(name, &[DIR, EMPTY, 0, 0, 0]), expect(&[]), "{name}");
            assert_eq!(
                finish(name, &[DIR, REL, 0, 0, 0], failed(libc::ENOENT)),
                (expect(&[("/dir/rel", "probe")]), None),
                "{name}"
            );
        }
        assert_eq!(
            succeed("name_to_handle_at", &[CWD, REL, 0, 0, SYMLINK_FOLLOW]),
            expect(&[("/work/rel", "probe")])
        );
        assert_eq!(succeed("statfs", &[REL]), expect(&[("/work/rel", "probe")]));
        assert_eq!(
            succeed("fstatfs", &[FILE]),
            expect(&[("/dir/file", "probe")])
        );
        assert_eq!(succeed("fstatfs", &[PIPE]), expect(&[]));
        assert_eq!(
            finish("statfs", &[REL], failed(libc::ENOENT)),
            (expect(&[("/work/rel", "probe")]), None)
        );
    }

    /// `inotify_add_watch` probes the path it watches, relative to the working
    /// directory, and `fanotify_mark` the path relative to its directory
    /// descriptor, or the descriptor itself for a null path; flushing marks names
    /// no path, and neither does an empty path.
    #[test]
    fn watches_probe_the_paths_they_watch() {
        assert_eq!(
            succeed("inotify_add_watch", &[WATCHER, REL, 0x20]),
            expect(&[("/work/rel", "probe")])
        );
        assert_eq!(
            succeed("inotify_add_watch", &[WATCHER, ABS, IN_DONT_FOLLOW_OPEN]),
            expect(&[("/abs/x/y", "probe")])
        );
        assert_eq!(
            finish(
                "inotify_add_watch",
                &[WATCHER, REL, 0x20],
                failed(libc::ENOENT)
            ),
            (expect(&[("/work/rel", "probe")]), None)
        );
        assert_eq!(
            succeed("fanotify_mark", &[WATCHER, MARK_ADD, FAN_OPEN, DIR, REL]),
            expect(&[("/dir/rel", "probe")])
        );
        assert_eq!(
            succeed("fanotify_mark", &[
                WATCHER,
                MARK_ADD | MARK_DONT_FOLLOW,
                FAN_OPEN,
                CWD,
                REL
            ]),
            expect(&[("/work/rel", "probe")])
        );
        assert_eq!(
            succeed("fanotify_mark", &[WATCHER, MARK_ADD, FAN_OPEN, FILE, 0]),
            expect(&[("/dir/file", "probe")])
        );
        assert_eq!(
            succeed("fanotify_mark", &[WATCHER, MARK_ADD, FAN_OPEN, DIR, EMPTY]),
            expect(&[])
        );
        assert_eq!(
            succeed("fanotify_mark", &[WATCHER, FAN_MARK_FLUSH, 0, DIR, REL]),
            expect(&[])
        );
        assert_eq!(
            finish(
                "fanotify_mark",
                &[WATCHER, MARK_ADD, FAN_OPEN, DIR, REL],
                failed(libc::ENOENT)
            ),
            (expect(&[("/dir/rel", "probe")]), None)
        );
    }

    /// Reading or listing extended attributes records the xattr kind for the path,
    /// the path behind a descriptor, or the descriptor of an `at` call with
    /// `AT_EMPTY_PATH`; a failure that answers from the attributes records the xattr
    /// kind too, any other failure a probe, and an unseen result both.
    #[test]
    fn reading_and_listing_extended_attributes_records_xattr() {
        for name in ["getxattr", "lgetxattr", "listxattr", "llistxattr"] {
            assert_eq!(
                succeed(name, &[REL, OTHER, 0, 0]),
                expect(&[("/work/rel", "xattr")]),
                "{name}"
            );
        }
        for name in ["fgetxattr", "flistxattr"] {
            assert_eq!(
                succeed(name, &[FILE, OTHER, 0, 0]),
                expect(&[("/dir/file", "xattr")]),
                "{name}"
            );
        }
        for name in ["getxattrat", "listxattrat"] {
            assert_eq!(
                succeed(name, &[DIR, REL, 0, OTHER, 0, 0]),
                expect(&[("/dir/rel", "xattr")]),
                "{name}"
            );
            assert_eq!(
                succeed(name, &[CWD, ABS, SYMLINK_NOFOLLOW, OTHER, 0, 0]),
                expect(&[("/abs/x/y", "xattr")]),
                "{name}"
            );
            assert_eq!(
                succeed(name, &[FILE, EMPTY, EMPTY_PATH, OTHER, 0, 0]),
                expect(&[("/dir/file", "xattr")]),
                "{name}"
            );
            assert_eq!(
                succeed(name, &[FILE, 0, EMPTY_PATH, OTHER, 0, 0]),
                expect(&[("/dir/file", "xattr")]),
                "{name}"
            );
            assert_eq!(
                succeed(name, &[DIR, EMPTY, 0, OTHER, 0, 0]),
                expect(&[]),
                "{name}"
            );
        }
        for errno in [libc::ENODATA, libc::ERANGE, libc::EOPNOTSUPP] {
            for (name, arguments) in [
                ("getxattr", [REL, OTHER, 0, 0, 0, 0]),
                ("listxattr", [REL, 0, 0, 0, 0, 0]),
                ("fgetxattr", [FILE, OTHER, 0, 0, 0, 0]),
                ("getxattrat", [DIR, REL, 0, OTHER, 0, 0]),
            ] {
                let (paths, problem) = finish(name, &arguments, failed(errno));
                assert_eq!(problem, None, "{name} {errno}");
                assert_eq!(paths.len(), 1, "{name} {errno}");
                assert_eq!(paths[0].1, "xattr", "{name} {errno}");
            }
        }
        for errno in [libc::ENOENT, libc::EACCES, libc::EINTR] {
            assert_eq!(
                finish("getxattr", &[REL, OTHER, 0, 0], failed(errno)),
                (expect(&[("/work/rel", "probe")]), None),
                "{errno}"
            );
        }
        assert_eq!(
            finish("lgetxattr", &[REL, OTHER, 0, 0], None),
            (expect(&[("/work/rel", "probe+xattr")]), None)
        );
    }

    /// Setting or removing an extended attribute is a write, of the path, the path
    /// behind a descriptor, or the descriptor of an `at` call with `AT_EMPTY_PATH`,
    /// and a failure is a probe, even one that says the attribute does not exist.
    #[test]
    fn setting_and_removing_extended_attributes_are_writes() {
        for name in ["setxattr", "lsetxattr", "removexattr", "lremovexattr"] {
            assert_eq!(
                succeed(name, &[REL, OTHER, 0, 0, 0]),
                expect(&[("/work/rel", "write")]),
                "{name}"
            );
        }
        for name in ["fsetxattr", "fremovexattr"] {
            assert_eq!(
                succeed(name, &[FILE, OTHER, 0, 0, 0]),
                expect(&[("/dir/file", "write")]),
                "{name}"
            );
        }
        for name in ["setxattrat", "removexattrat"] {
            assert_eq!(
                succeed(name, &[DIR, REL, 0, OTHER, 0, 0]),
                expect(&[("/dir/rel", "write")]),
                "{name}"
            );
            assert_eq!(
                succeed(name, &[FILE, EMPTY, EMPTY_PATH, OTHER, 0, 0]),
                expect(&[("/dir/file", "write")]),
                "{name}"
            );
        }
        for errno in [libc::ENODATA, libc::ENOENT, libc::EOPNOTSUPP] {
            assert_eq!(
                finish("removexattr", &[REL, OTHER], failed(errno)),
                (expect(&[("/work/rel", "probe")]), None),
                "{errno}"
            );
        }
    }

    /// `fchmodat2` and `file_setattr` write the path relative to their directory
    /// descriptor, or the descriptor itself with `AT_EMPTY_PATH`, whether or not
    /// they follow a final link, and `fallocate` writes the path behind its
    /// descriptor; a failure is a probe.
    #[test]
    fn fchmodat2_file_setattr_and_fallocate_are_writes() {
        assert_eq!(
            succeed("fchmodat2", &[DIR, REL, 0o644, 0]),
            expect(&[("/dir/rel", "write")])
        );
        assert_eq!(
            succeed("fchmodat2", &[CWD, REL, 0o644, SYMLINK_NOFOLLOW]),
            expect(&[("/work/rel", "write")])
        );
        assert_eq!(
            succeed("fchmodat2", &[FILE, EMPTY, 0o644, EMPTY_PATH]),
            expect(&[("/dir/file", "write")])
        );
        assert_eq!(
            finish("fchmodat2", &[DIR, REL, 0o644, 0], failed(libc::ENOENT)),
            (expect(&[("/dir/rel", "probe")]), None)
        );
        assert_eq!(
            succeed("file_setattr", &[DIR, REL, 0, 0, 0]),
            expect(&[("/dir/rel", "write")])
        );
        assert_eq!(
            succeed("file_setattr", &[FILE, EMPTY, 0, 0, EMPTY_PATH]),
            expect(&[("/dir/file", "write")])
        );
        assert_eq!(
            succeed("file_setattr", &[CWD, ABS, 0, 0, SYMLINK_NOFOLLOW]),
            expect(&[("/abs/x/y", "write")])
        );
        assert_eq!(
            succeed("fallocate", &[FILE, 0, 0, 10]),
            expect(&[("/dir/file", "write")])
        );
        assert_eq!(
            finish("fallocate", &[FILE, 0, 0, 10], failed(libc::ENOSPC)),
            (expect(&[("/dir/file", "probe")]), None)
        );
    }

    /// One decoder case of a call on extended attributes, file attributes, file
    /// handles, file systems or watches, or of `fchmodat2` or `fallocate`.
    struct Case {
        /// The name of the call.
        name: &'static str,
        /// Its arguments, which `decode` pads with zeros.
        arguments: &'static [u64],
        /// The path that the call names and the kinds that a success records, or
        /// `None` when the arguments name no path.
        named: Option<(&'static str, &'static str)>,
    }

    /// The case of the call `name` with `arguments`, which names `path` and
    /// records `kinds` when it succeeds.
    const fn names(
        name: &'static str,
        arguments: &'static [u64],
        path: &'static str,
        kinds: &'static str,
    ) -> Case {
        Case {
            name,
            arguments,
            named: Some((path, kinds)),
        }
    }

    /// The case of the call `name` with `arguments`, which name no path.
    const fn names_nothing(
        name: &'static str,
        arguments: &'static [u64],
    ) -> Case {
        Case {
            name,
            arguments,
            named: None,
        }
    }

    /// The 25 tracked calls on extended attributes, file attributes, file handles,
    /// file systems and watches, with `fchmodat2` and `fallocate`, in the order of
    /// `TRACKED_SYSTEM_CALLS`.
    const NEW_CALLS: [&str; 25] = [
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
        "fchmodat2",
        "file_setattr",
        "fallocate",
    ];

    /// The calls that read or list extended attributes, whose failures with
    /// [`ANSWERS`] record the xattr kind.
    const READS_ATTRIBUTES: [&str; 8] = [
        "getxattr",
        "lgetxattr",
        "fgetxattr",
        "getxattrat",
        "listxattr",
        "llistxattr",
        "flistxattr",
        "listxattrat",
    ];

    /// The failures that answer from the attributes: the attribute does not
    /// exist, the buffer is too small, or the file system has no attributes.
    const ANSWERS: [c_int; 3] = [libc::ENODATA, libc::ERANGE, libc::EOPNOTSUPP];

    /// Ordinary failures, which record a probe for every call.
    const ORDINARY: [c_int; 4] = [libc::ENOENT, libc::EACCES, libc::EPERM, libc::EINTR];

    /// Every case of every call in [`NEW_CALLS`]: its arguments, flags,
    /// descriptors, null and empty paths.
    const CASES: &[Case] = &[
        // File attributes and handles: a path relative to a directory descriptor,
        // with or without following a final link, or the descriptor itself with
        // `AT_EMPTY_PATH`.
        names("file_getattr", &[DIR, REL, 0, 0, 0], "/dir/rel", "probe"),
        names(
            "file_getattr",
            &[CWD, ABS, 0, 0, SYMLINK_NOFOLLOW],
            "/abs/x/y",
            "probe",
        ),
        names(
            "file_getattr",
            &[DIR, EMPTY, 0, 0, EMPTY_PATH],
            "/dir",
            "probe",
        ),
        names(
            "file_getattr",
            &[FILE, EMPTY, 0, 0, EMPTY_PATH],
            "/dir/file",
            "probe",
        ),
        names_nothing("file_getattr", &[DIR, EMPTY, 0, 0, 0]),
        names(
            "name_to_handle_at",
            &[DIR, REL, 0, 0, 0],
            "/dir/rel",
            "probe",
        ),
        names(
            "name_to_handle_at",
            &[CWD, REL, 0, 0, SYMLINK_FOLLOW],
            "/work/rel",
            "probe",
        ),
        names(
            "name_to_handle_at",
            &[CWD, ABS, 0, 0, SYMLINK_NOFOLLOW],
            "/abs/x/y",
            "probe",
        ),
        names(
            "name_to_handle_at",
            &[DIR, EMPTY, 0, 0, EMPTY_PATH],
            "/dir",
            "probe",
        ),
        names(
            "name_to_handle_at",
            &[FILE, EMPTY, 0, 0, EMPTY_PATH],
            "/dir/file",
            "probe",
        ),
        names_nothing("name_to_handle_at", &[DIR, EMPTY, 0, 0, 0]),
        // File systems: a path, or the path behind a descriptor.
        names("statfs", &[REL], "/work/rel", "probe"),
        names("statfs", &[ABS], "/abs/x/y", "probe"),
        names_nothing("statfs", &[EMPTY]),
        names("fstatfs", &[FILE], "/dir/file", "probe"),
        names_nothing("fstatfs", &[PIPE]),
        // Watches.
        names(
            "inotify_add_watch",
            &[WATCHER, REL, 0x20],
            "/work/rel",
            "probe",
        ),
        names(
            "inotify_add_watch",
            &[WATCHER, ABS, IN_DONT_FOLLOW_OPEN],
            "/abs/x/y",
            "probe",
        ),
        names_nothing("inotify_add_watch", &[WATCHER, EMPTY, 0x20]),
        names(
            "fanotify_mark",
            &[WATCHER, MARK_ADD, FAN_OPEN, DIR, REL],
            "/dir/rel",
            "probe",
        ),
        names(
            "fanotify_mark",
            &[WATCHER, MARK_ADD | MARK_DONT_FOLLOW, FAN_OPEN, CWD, REL],
            "/work/rel",
            "probe",
        ),
        names(
            "fanotify_mark",
            &[WATCHER, MARK_ADD, FAN_OPEN, FILE, 0],
            "/dir/file",
            "probe",
        ),
        names_nothing("fanotify_mark", &[WATCHER, MARK_ADD, FAN_OPEN, DIR, EMPTY]),
        names_nothing("fanotify_mark", &[WATCHER, FAN_MARK_FLUSH, 0, DIR, REL]),
        // Reading and listing extended attributes.
        names("getxattr", &[REL, OTHER, 0, 0], "/work/rel", "xattr"),
        names("getxattr", &[ABS, OTHER, 0, 0], "/abs/x/y", "xattr"),
        names_nothing("getxattr", &[EMPTY, OTHER, 0, 0]),
        names("lgetxattr", &[REL, OTHER, 0, 0], "/work/rel", "xattr"),
        names("lgetxattr", &[ABS, OTHER, 0, 0], "/abs/x/y", "xattr"),
        names_nothing("lgetxattr", &[EMPTY, OTHER, 0, 0]),
        names("fgetxattr", &[FILE, OTHER, 0, 0], "/dir/file", "xattr"),
        names_nothing("fgetxattr", &[PIPE, OTHER, 0, 0]),
        names(
            "getxattrat",
            &[DIR, REL, 0, OTHER, 0, 0],
            "/dir/rel",
            "xattr",
        ),
        names(
            "getxattrat",
            &[CWD, ABS, SYMLINK_NOFOLLOW, OTHER, 0, 0],
            "/abs/x/y",
            "xattr",
        ),
        names(
            "getxattrat",
            &[FILE, EMPTY, EMPTY_PATH, OTHER, 0, 0],
            "/dir/file",
            "xattr",
        ),
        names(
            "getxattrat",
            &[FILE, 0, EMPTY_PATH, OTHER, 0, 0],
            "/dir/file",
            "xattr",
        ),
        names_nothing("getxattrat", &[DIR, EMPTY, 0, OTHER, 0, 0]),
        names("listxattr", &[REL, 0, 0], "/work/rel", "xattr"),
        names("listxattr", &[ABS, 0, 0], "/abs/x/y", "xattr"),
        names_nothing("listxattr", &[EMPTY, 0, 0]),
        names("llistxattr", &[REL, 0, 0], "/work/rel", "xattr"),
        names("llistxattr", &[ABS, 0, 0], "/abs/x/y", "xattr"),
        names_nothing("llistxattr", &[EMPTY, 0, 0]),
        names("flistxattr", &[FILE, 0, 0], "/dir/file", "xattr"),
        names_nothing("flistxattr", &[PIPE, 0, 0]),
        names("listxattrat", &[DIR, REL, 0, 0, 0], "/dir/rel", "xattr"),
        names(
            "listxattrat",
            &[CWD, ABS, SYMLINK_NOFOLLOW, 0, 0],
            "/abs/x/y",
            "xattr",
        ),
        names(
            "listxattrat",
            &[FILE, EMPTY, EMPTY_PATH, 0, 0],
            "/dir/file",
            "xattr",
        ),
        names(
            "listxattrat",
            &[FILE, 0, EMPTY_PATH, 0, 0],
            "/dir/file",
            "xattr",
        ),
        names_nothing("listxattrat", &[DIR, EMPTY, 0, 0, 0]),
        // Setting and removing extended attributes.
        names("setxattr", &[REL, OTHER, 0, 0, 0], "/work/rel", "write"),
        names("setxattr", &[ABS, OTHER, 0, 0, 0], "/abs/x/y", "write"),
        names_nothing("setxattr", &[EMPTY, OTHER, 0, 0, 0]),
        names("lsetxattr", &[REL, OTHER, 0, 0, 0], "/work/rel", "write"),
        names("lsetxattr", &[ABS, OTHER, 0, 0, 0], "/abs/x/y", "write"),
        names_nothing("lsetxattr", &[EMPTY, OTHER, 0, 0, 0]),
        names("fsetxattr", &[FILE, OTHER, 0, 0, 0], "/dir/file", "write"),
        names_nothing("fsetxattr", &[PIPE, OTHER, 0, 0, 0]),
        names(
            "setxattrat",
            &[DIR, REL, 0, OTHER, 0, 0],
            "/dir/rel",
            "write",
        ),
        names(
            "setxattrat",
            &[CWD, ABS, SYMLINK_NOFOLLOW, OTHER, 0, 0],
            "/abs/x/y",
            "write",
        ),
        names(
            "setxattrat",
            &[FILE, EMPTY, EMPTY_PATH, OTHER, 0, 0],
            "/dir/file",
            "write",
        ),
        names(
            "setxattrat",
            &[FILE, 0, EMPTY_PATH, OTHER, 0, 0],
            "/dir/file",
            "write",
        ),
        names_nothing("setxattrat", &[DIR, EMPTY, 0, OTHER, 0, 0]),
        names("removexattr", &[REL, OTHER], "/work/rel", "write"),
        names("removexattr", &[ABS, OTHER], "/abs/x/y", "write"),
        names_nothing("removexattr", &[EMPTY, OTHER]),
        names("lremovexattr", &[REL, OTHER], "/work/rel", "write"),
        names("lremovexattr", &[ABS, OTHER], "/abs/x/y", "write"),
        names_nothing("lremovexattr", &[EMPTY, OTHER]),
        names("fremovexattr", &[FILE, OTHER], "/dir/file", "write"),
        names_nothing("fremovexattr", &[PIPE, OTHER]),
        names("removexattrat", &[DIR, REL, 0, OTHER], "/dir/rel", "write"),
        names(
            "removexattrat",
            &[CWD, ABS, SYMLINK_NOFOLLOW, OTHER],
            "/abs/x/y",
            "write",
        ),
        names(
            "removexattrat",
            &[FILE, EMPTY, EMPTY_PATH, OTHER],
            "/dir/file",
            "write",
        ),
        names(
            "removexattrat",
            &[FILE, 0, EMPTY_PATH, OTHER],
            "/dir/file",
            "write",
        ),
        names_nothing("removexattrat", &[DIR, EMPTY, 0, OTHER]),
        // Changes of modes, file attributes and allocation.
        names("fchmodat2", &[DIR, REL, 0o644, 0], "/dir/rel", "write"),
        names(
            "fchmodat2",
            &[CWD, REL, 0o644, SYMLINK_NOFOLLOW],
            "/work/rel",
            "write",
        ),
        names(
            "fchmodat2",
            &[FILE, EMPTY, 0o644, EMPTY_PATH],
            "/dir/file",
            "write",
        ),
        names_nothing("fchmodat2", &[DIR, EMPTY, 0o644, 0]),
        names("file_setattr", &[DIR, REL, 0, 0, 0], "/dir/rel", "write"),
        names(
            "file_setattr",
            &[CWD, ABS, 0, 0, SYMLINK_NOFOLLOW],
            "/abs/x/y",
            "write",
        ),
        names(
            "file_setattr",
            &[DIR, EMPTY, 0, 0, EMPTY_PATH],
            "/dir",
            "write",
        ),
        names(
            "file_setattr",
            &[FILE, EMPTY, 0, 0, EMPTY_PATH],
            "/dir/file",
            "write",
        ),
        names_nothing("file_setattr", &[DIR, EMPTY, 0, 0, 0]),
        names("fallocate", &[FILE, 0, 0, 10], "/dir/file", "write"),
        names_nothing("fallocate", &[PIPE, 0, 0, 10]),
    ];

    /// The table of cases covers each of the 25 calls, in the order in which they
    /// are tracked, with at least one case that names a path and one that names
    /// none.
    #[test]
    fn every_new_call_has_cases_that_name_a_path_and_cases_that_name_none() {
        let tracked: Vec<&str> = TRACKED_SYSTEM_CALLS
            .iter()
            .map(|call| call.name)
            .filter(|name| NEW_CALLS.contains(name))
            .collect();
        assert_eq!(tracked, NEW_CALLS);
        for name in NEW_CALLS {
            let cases: Vec<&Case> = CASES.iter().filter(|case| case.name == name).collect();
            assert!(cases.iter().any(|case| case.named.is_some()), "{name}");
            assert!(cases.iter().any(|case| case.named.is_none()), "{name}");
        }
        for case in CASES {
            assert!(NEW_CALLS.contains(&case.name), "{}", case.name);
        }
        for name in READS_ATTRIBUTES {
            assert!(NEW_CALLS.contains(&name), "{name}");
        }
    }

    /// Each case records its path with the kinds of its call when the call
    /// succeeds, and arguments that name no path record nothing.
    #[test]
    fn every_new_call_records_its_kinds_when_it_succeeds() {
        for case in CASES {
            let expected = case
                .named
                .map_or_else(Vec::new, |(path, kinds)| expect(&[(path, kinds)]));
            assert_eq!(
                succeed(case.name, case.arguments),
                expected,
                "{} {:?}",
                case.name,
                case.arguments
            );
        }
    }

    /// An ordinary failure of every call records exactly a probe of its path,
    /// and a failure of arguments that name no path records nothing.
    #[test]
    fn every_new_call_records_a_probe_when_it_fails() {
        for case in CASES {
            let expected = case
                .named
                .map_or_else(Vec::new, |(path, _)| expect(&[(path, "probe")]));
            for errno in ORDINARY {
                assert_eq!(
                    finish(case.name, case.arguments, failed(errno)),
                    (expected.clone(), None),
                    "{} {:?} {errno}",
                    case.name,
                    case.arguments
                );
            }
        }
    }

    /// Each of the eight calls that read or list extended attributes records
    /// exactly the xattr kind when it fails with `ENODATA`, `ERANGE` or
    /// `EOPNOTSUPP`, because that answer depends on the attributes; for every other
    /// call those failures are ordinary and record a probe.
    #[test]
    fn only_attribute_reads_record_xattr_for_the_answers_of_the_attributes() {
        for case in CASES {
            let kinds = if READS_ATTRIBUTES.contains(&case.name) {
                "xattr"
            } else {
                "probe"
            };
            let expected = case
                .named
                .map_or_else(Vec::new, |(path, _)| expect(&[(path, kinds)]));
            for errno in ANSWERS {
                assert_eq!(
                    finish(case.name, case.arguments, failed(errno)),
                    (expected.clone(), None),
                    "{} {:?} {errno}",
                    case.name,
                    case.arguments
                );
            }
        }
    }

    /// The address of a `struct clone_args` whose flags ask for a new mount
    /// namespace.
    const CLONE_ARGS_NEW_NAMESPACE: u64 = 0x2_0000;
    /// The address of a `struct clone_args` whose flags ask for a thread, without a
    /// new mount namespace.
    const CLONE_ARGS_THREAD: u64 = 0x2_1000;
    /// The address of the last four bytes of a region, too few for the flags of a
    /// `struct clone_args`.
    const CLONE_ARGS_SHORT: u64 = 0x2_2000;

    /// The fake thread with the `struct clone_args` regions of the `clone3` tests:
    /// the 64 bytes of `CLONE_ARGS_SIZE_VER0`, with `flags` first.
    fn clone3_thread() -> Fake {
        let clone_args = |flags: c_int| {
            let mut bytes = flag(flags).to_le_bytes().to_vec();
            bytes.resize(64, 0);
            bytes
        };
        let mut fake = Fake::new();
        fake.memory.insert(
            CLONE_ARGS_NEW_NAMESPACE,
            clone_args(libc::CLONE_NEWNS | libc::CLONE_NEWUTS),
        );
        fake.memory.insert(
            CLONE_ARGS_THREAD,
            clone_args(libc::CLONE_VM | libc::CLONE_FS | libc::CLONE_FILES | libc::CLONE_THREAD),
        );
        fake.memory.insert(CLONE_ARGS_SHORT, vec![0; 4]);
        fake
    }

    /// The arguments `values`, padded with zeros to six.
    fn padded(values: &[u64]) -> Arguments {
        let mut arguments = [0; 6];
        for (slot, value) in arguments.iter_mut().zip(values) {
            *slot = *value;
        }
        arguments
    }

    /// Whether `call` with `values`, in the clone3 thread, marks the run, and what
    /// it says the process made.
    fn judged(
        call: &str,
        values: &[u64],
    ) -> Option<String> {
        let number = crate::UNTRACEABLE_SYSTEM_CALLS
            .iter()
            .find(|untraceable| untraceable.name == call)
            .unwrap()
            .number;
        untraceable(number, padded(values), &clone3_thread())
    }

    /// Each call that can change what a path means, or that uses a path in a way the
    /// tracer does not record, marks the run with arguments that make it do so,
    /// whatever else they hold, and the reason names the call in backticks.
    #[test]
    fn each_untraceable_call_marks_the_run() {
        let new_namespace = flag(libc::CLONE_NEWNS);
        let child_signal = flag(libc::SIGCHLD);
        for call in crate::UNTRACEABLE_SYSTEM_CALLS {
            let cases: Vec<Vec<u64>> = match call.name {
                "clone" => {
                    vec![vec![new_namespace | child_signal], vec![
                        new_namespace,
                        0x10,
                    ]]
                },
                "unshare" => {
                    vec![vec![new_namespace], vec![
                        new_namespace | flag(libc::CLONE_FILES),
                    ]]
                },
                "setns" => vec![vec![3, 0], vec![3, new_namespace], vec![CWD, 0]],
                "clone3" => vec![vec![CLONE_ARGS_NEW_NAMESPACE, 64]],
                _ => vec![Vec::new(), vec![REL], vec![CLOSED, ABS, CWD, u64::MAX]],
            };
            for values in cases {
                let made = judged(call.name, &values);
                let named = format!("`{}`", call.name);
                assert!(
                    made.as_ref().is_some_and(|made| made.contains(&named)),
                    "{} {values:?}: {made:?}",
                    call.name
                );
            }
        }
        assert_eq!(judged("chroot", &[REL]), Some("made `chroot`".to_owned()));
        assert_eq!(
            judged("clone", &[new_namespace]),
            Some("made `clone` with `CLONE_NEWNS`".to_owned())
        );
        assert_eq!(
            judged("setns", &[3, 0]),
            Some("made `setns`, which can enter a mount namespace".to_owned())
        );
    }

    /// `clone`, `clone3` and `unshare` without `CLONE_NEWNS`, and `setns` with another
    /// namespace type, do not mark the run.
    #[test]
    fn calls_without_a_new_mount_namespace_do_not_mark_the_run() {
        let child_signal = flag(libc::SIGCHLD);
        for (call, values) in [
            ("clone", vec![child_signal]),
            ("clone", vec![flag(
                libc::CLONE_VM | libc::CLONE_THREAD | libc::CLONE_NEWUTS,
            )]),
            ("clone3", vec![CLONE_ARGS_THREAD, 64]),
            ("unshare", vec![flag(libc::CLONE_FILES)]),
            ("unshare", vec![0]),
            ("setns", vec![3, flag(libc::CLONE_NEWUTS)]),
            ("setns", vec![3, flag(libc::CLONE_NEWNET)]),
        ] {
            assert_eq!(judged(call, &values), None, "{call} {values:?}");
        }
    }

    /// `clone3` marks the run when the tracer cannot read the flags of its
    /// `struct clone_args`: a null or unmapped pointer, or memory that ends before
    /// the flags do.
    #[test]
    fn clone3_whose_flags_cannot_be_read_marks_the_run() {
        for address in [0, UNMAPPED, CLONE_ARGS_SHORT] {
            let made = judged("clone3", &[address, 64]).unwrap_or_default();
            assert!(
                made.starts_with("made `clone3`, whose flags the tracer could not read: "),
                "{address:#x}: {made}"
            );
        }
    }

    /// A number that is not in the list of calls that mark a run, such as a tracked
    /// call or another known call, is not judged.
    #[test]
    fn other_numbers_are_not_untraceable() {
        let fake = clone3_thread();
        for number in [libc::SYS_openat, libc::SYS_read, libc::SYS_getpid, 400] {
            assert_eq!(
                untraceable(number, padded(&[REL, 0]), &fake),
                None,
                "{number}"
            );
        }
    }

    /// The address of a `struct sockaddr_in` for 127.0.0.1 port 80, padded with
    /// zeros to 128 bytes.
    const SOCK_V4: u64 = 0x2_0000;
    /// The address of a `struct sockaddr_in6` for `fe80::1` port 443 with scope 2.
    const SOCK_V6: u64 = 0x2_1000;
    /// The address of a `struct sockaddr_un` for the relative path `sock`, with its
    /// NUL byte.
    const SOCK_UNIX: u64 = 0x2_2000;
    /// The address of a `struct sockaddr_un` for the absolute path `/abs/s`, with its
    /// NUL byte, padded with zeros to 120 bytes.
    const SOCK_ABSOLUTE: u64 = 0x2_3000;
    /// The address of a `struct sockaddr_un` for the abstract name `k`, a zero byte
    /// and `x`.
    const SOCK_ABSTRACT: u64 = 0x2_4000;
    /// The address of a `struct sockaddr_nl`, a netlink address.
    const SOCK_NETLINK: u64 = 0x2_5000;
    /// The address of a `struct msghdr` whose `msg_name` is [`SOCK_V4`], 16 bytes.
    const MSG_V4: u64 = 0x2_6000;
    /// The address of a `struct msghdr` with a null `msg_name`.
    const MSG_NULL: u64 = 0x2_7000;
    /// The address of a `struct msghdr` whose `msg_name` is [`SOCK_V4`] with a
    /// `msg_namelen` of 4,096, which the kernel cuts to 128.
    const MSG_LONG: u64 = 0x2_8000;
    /// The address of an array of two `struct mmsghdr`, whose names are [`SOCK_UNIX`]
    /// and [`SOCK_V6`].
    const MMSG: u64 = 0x2_9000;
    /// The address of an array of one `struct mmsghdr`, whose name is not mapped.
    const MMSG_UNMAPPED: u64 = 0x2_A000;

    /// The two bytes of the address family `family`, as a `sa_family_t`.
    fn family(family: c_int) -> Vec<u8> {
        u16::try_from(family).unwrap().to_ne_bytes().to_vec()
    }

    /// A `struct sockaddr_in` for `address` and `port`, padded with zeros to `size`.
    fn sockaddr_in(
        address: [u8; 4],
        port: u16,
        size: usize,
    ) -> Vec<u8> {
        let mut bytes = family(libc::AF_INET);
        bytes.extend(port.to_be_bytes());
        bytes.extend(address);
        bytes.resize(size, 0);
        bytes
    }

    /// A `struct sockaddr_in6` for `address`, `port` and `scope`, with a flow
    /// information that is not zero, which names no destination.
    fn sockaddr_in6(
        address: [u8; 16],
        port: u16,
        scope: u32,
    ) -> Vec<u8> {
        let mut bytes = family(libc::AF_INET6);
        bytes.extend(port.to_be_bytes());
        bytes.extend(7_u32.to_ne_bytes());
        bytes.extend(address);
        bytes.extend(scope.to_ne_bytes());
        bytes
    }

    /// A `struct sockaddr_un` with the bytes `name` after its family, padded with
    /// zeros to `size`.
    fn sockaddr_un(
        name: &[u8],
        size: usize,
    ) -> Vec<u8> {
        let mut bytes = family(libc::AF_UNIX);
        bytes.extend(name);
        bytes.resize(size.max(bytes.len()), 0);
        bytes
    }

    /// The first bytes of a `struct msghdr` whose `msg_name` is `name` with
    /// `length` bytes, padded with zeros to its 56 bytes.
    fn msghdr(
        name: u64,
        length: u32,
    ) -> Vec<u8> {
        let mut bytes = name.to_le_bytes().to_vec();
        bytes.extend(length.to_le_bytes());
        bytes.resize(56, 0);
        bytes
    }

    /// The IPv6 address `fe80::1`.
    const LINK_LOCAL: [u8; 16] = [0xFE, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

    /// The fake thread of every test, with the socket addresses and message headers
    /// of the socket tests in its memory.
    fn socket_thread() -> Fake {
        let mut fake = Fake::new();
        let mut messages = msghdr(SOCK_UNIX, 7);
        messages.resize(64, 0);
        messages.extend(msghdr(SOCK_V6, 28));
        messages.resize(128, 0);
        fake.memory.extend([
            (SOCK_V4, sockaddr_in([127, 0, 0, 1], 80, 128)),
            (SOCK_V6, sockaddr_in6(LINK_LOCAL, 443, 2)),
            (SOCK_UNIX, sockaddr_un(b"sock\0", 0)),
            (SOCK_ABSOLUTE, sockaddr_un(b"/abs/s\0", 120)),
            (SOCK_ABSTRACT, sockaddr_un(b"\0k\0x", 0)),
            (SOCK_NETLINK, {
                let mut bytes = family(libc::AF_NETLINK);
                bytes.resize(12, 0);
                bytes
            }),
            (MSG_V4, msghdr(SOCK_V4, 16)),
            (MSG_NULL, msghdr(0, 16)),
            (MSG_LONG, msghdr(SOCK_V4, 4096)),
            (MMSG, messages),
            (MMSG_UNMAPPED, msghdr(UNMAPPED, 16)),
        ]);
        fake
    }

    /// The accesses, the connections as text and the incomplete-trace report of the
    /// socket call `name` with `arguments` when it returns `result`.
    fn connected(
        name: &str,
        arguments: &[u64],
        result: Option<i64>,
    ) -> (Vec<(String, String)>, Vec<String>, Option<String>) {
        let call = decode(&socket_thread(), name, arguments);
        let connections = call
            .connections(result)
            .iter()
            .map(ToString::to_string)
            .collect();
        let mut accesses = Accesses::new();
        let problem = call.finish(result, &mut accesses);
        let paths = accesses
            .iter()
            .map(|(path, kinds)| (path.to_string(), kinds.to_string()))
            .collect();
        (paths, connections, problem)
    }

    /// The expected connections, as text.
    fn named(connections: &[&str]) -> Vec<String> {
        connections.iter().map(|text| (*text).to_owned()).collect()
    }

    /// One destination case of a socket call: the address, its length, the paths
    /// that a success records with their kinds, and the connection as text.
    type DestinationCase = (
        u64,
        u64,
        &'static [(&'static str, &'static str)],
        &'static str,
    );

    /// `connect` and `sendto` record an IPv4 address, an IPv6 address with its scope,
    /// a relative or absolute Unix socket path, which they probe, and an abstract
    /// name, which touches no path; an IPv6 address too short for its scope has
    /// none.
    #[test]
    fn connect_and_sendto_record_each_kind_of_destination() {
        let cases: [DestinationCase; 6] = [
            (SOCK_V4, 16, &[], "127.0.0.1:80"),
            (SOCK_V6, 28, &[], "[fe80::1%2]:443"),
            (SOCK_V6, 24, &[], "[fe80::1]:443"),
            (
                SOCK_UNIX,
                7,
                &[("/work/sock", "probe")],
                "Unix socket /work/sock",
            ),
            (
                SOCK_ABSOLUTE,
                9,
                &[("/abs/s", "probe")],
                "Unix socket /abs/s",
            ),
            (SOCK_ABSTRACT, 6, &[], "abstract Unix socket @k\\x00x"),
        ];
        for (address, length, paths, connection) in cases {
            for (name, arguments) in [
                ("connect", vec![3, address, length]),
                ("sendto", vec![3, REL, 1, 0, address, length]),
            ] {
                assert_eq!(
                    connected(name, &arguments, Some(0)),
                    (expect(paths), named(&[connection]), None),
                    "{name} {address:#x} {length}"
                );
            }
        }
    }

    /// A Unix socket path ends at its first NUL byte or at the length the call
    /// gives, whichever comes first.
    #[test]
    fn unix_socket_paths_end_at_a_nul_byte_or_the_length() {
        for length in [6, 8, 9, 20] {
            assert_eq!(
                connected("connect", &[3, SOCK_ABSOLUTE, length], Some(0)).1,
                named(&[if length == 6 {
                    "Unix socket /abs"
                } else {
                    "Unix socket /abs/s"
                }]),
                "{length}"
            );
        }
    }

    /// An address shorter or longer than the kernel accepts for its family, an
    /// unnamed Unix socket, a length of 0 or less, a null address, and a family
    /// other than IPv4, IPv6 and Unix, such as netlink, name nothing, and leave the
    /// trace complete; a long IPv4 length that the kernel accepts still names the
    /// address.
    #[test]
    fn short_long_null_and_other_addresses_name_nothing() {
        let nothing = (expect(&[]), Vec::new(), None);
        for (address, length) in [
            (SOCK_V4, 15),
            (SOCK_V4, 129),
            (SOCK_V6, 23),
            (SOCK_UNIX, 2),
            (SOCK_UNIX, 1),
            (SOCK_ABSOLUTE, 111),
            (SOCK_V4, 0),
            (SOCK_V4, reg(-1)),
            (0, 16),
            (SOCK_NETLINK, 12),
        ] {
            assert_eq!(
                connected("connect", &[3, address, length], Some(0)),
                nothing,
                "{address:#x} {length}"
            );
        }
        assert_eq!(
            connected("connect", &[3, SOCK_V4, 128], Some(0)).1,
            named(&["127.0.0.1:80"])
        );
        assert_eq!(
            connected("connect", &[3, SOCK_ABSOLUTE, 110], Some(0)).1,
            named(&["Unix socket /abs/s"])
        );
        // `sendto` without a destination, as `send` makes it.
        assert_eq!(connected("sendto", &[3, REL, 1, 0, 0, 0], Some(1)), nothing);
        assert_eq!(
            connected("sendto", &[3, REL, 1, 0, 0, 16], Some(1)),
            nothing
        );
    }

    /// An address that cannot be read makes the trace incomplete, unless the call
    /// failed with `EFAULT`, which the kernel gives for it before it acts.
    #[test]
    fn unreadable_addresses_are_problems_unless_the_call_rejected_them() {
        let (paths, connections, problem) = connected("connect", &[3, UNMAPPED, 16], Some(0));
        assert_eq!((paths, connections), (expect(&[]), Vec::new()));
        assert!(
            problem.is_some_and(|problem| problem.starts_with("could not decode connect: ")),
            "unmapped"
        );
        assert_eq!(
            connected("connect", &[3, UNMAPPED, 16], failed(libc::EFAULT)),
            (expect(&[]), Vec::new(), None)
        );
        // An address that ends before the length the call gives.
        assert!(connected("connect", &[3, SOCK_V6, 64], Some(0)).2.is_some());
        assert!(connected("sendmsg", &[3, UNMAPPED, 0], Some(0)).2.is_some());
        assert!(
            connected("sendmmsg", &[3, MMSG_UNMAPPED, 1, 0], Some(0))
                .2
                .is_some()
        );
        assert_eq!(
            connected("sendmmsg", &[3, MMSG_UNMAPPED, 1, 0], failed(libc::EFAULT)).2,
            None
        );
    }

    /// `sendmsg` reads the destination from the `msg_name` of its `struct msghdr`,
    /// whose `msg_namelen` the kernel cuts to 128 bytes, and a null `msg_name` names
    /// none; `sendmmsg` reads the `msg_name` of each of the messages its count
    /// gives.
    #[test]
    fn sendmsg_and_sendmmsg_read_the_names_of_their_messages() {
        assert_eq!(
            connected("sendmsg", &[3, MSG_V4, 0], Some(1)),
            (expect(&[]), named(&["127.0.0.1:80"]), None)
        );
        assert_eq!(
            connected("sendmsg", &[3, MSG_LONG, 0], Some(1)),
            (expect(&[]), named(&["127.0.0.1:80"]), None)
        );
        assert_eq!(
            connected("sendmsg", &[3, MSG_NULL, 0], Some(1)),
            (expect(&[]), Vec::new(), None)
        );
        assert_eq!(
            connected("sendmmsg", &[3, MMSG, 2, 0], Some(2)),
            (
                expect(&[("/work/sock", "probe")]),
                named(&["Unix socket /work/sock", "[fe80::1%2]:443"]),
                None
            )
        );
        assert_eq!(
            connected("sendmmsg", &[3, MMSG, 1, 0], Some(1)),
            (
                expect(&[("/work/sock", "probe")]),
                named(&["Unix socket /work/sock"]),
                None
            )
        );
        assert_eq!(
            connected("sendmmsg", &[3, MMSG, 0, 0], Some(0)),
            (expect(&[]), Vec::new(), None)
        );
    }

    /// `bind` writes a Unix socket path, and probes it when it fails, but records
    /// no connection, whatever its family.
    #[test]
    fn bind_writes_a_unix_socket_path_and_records_no_connection() {
        assert_eq!(
            connected("bind", &[3, SOCK_UNIX, 7], Some(0)),
            (expect(&[("/work/sock", "write")]), Vec::new(), None)
        );
        assert_eq!(
            connected("bind", &[3, SOCK_UNIX, 7], failed(libc::EADDRINUSE)),
            (expect(&[("/work/sock", "probe")]), Vec::new(), None)
        );
        for (address, length) in [(SOCK_V4, 16), (SOCK_ABSTRACT, 6)] {
            assert_eq!(
                connected("bind", &[3, address, length], Some(0)),
                (expect(&[]), Vec::new(), None),
                "{address:#x}"
            );
        }
    }

    /// A call that fails with an error that says that a Unix socket path was not
    /// found records no Unix connection, by path or abstract name, but still the IP
    /// destinations of the same call; any other failure, such as a refused
    /// connection, and a result that was never seen, keep every connection. Every
    /// failure probes the path.
    #[test]
    fn failures_that_find_no_path_record_no_unix_connection() {
        for errno in NOT_FOUND {
            assert_eq!(
                connected("connect", &[3, SOCK_UNIX, 7], failed(errno)),
                (expect(&[("/work/sock", "probe")]), Vec::new(), None),
                "{errno}"
            );
            assert_eq!(
                connected("connect", &[3, SOCK_ABSTRACT, 6], failed(errno)).1,
                Vec::<String>::new(),
                "{errno}"
            );
            assert_eq!(
                connected("connect", &[3, SOCK_V4, 16], failed(errno)).1,
                named(&["127.0.0.1:80"]),
                "{errno}"
            );
            assert_eq!(
                connected("sendmmsg", &[3, MMSG, 2, 0], failed(errno)).1,
                named(&["[fe80::1%2]:443"]),
                "{errno}"
            );
        }
        for result in [failed(libc::ECONNREFUSED), failed(libc::EAGAIN), None] {
            assert_eq!(
                connected("connect", &[3, SOCK_UNIX, 7], result).1,
                named(&["Unix socket /work/sock"]),
                "{result:?}"
            );
            assert_eq!(
                connected("connect", &[3, SOCK_V4, 16], result).1,
                named(&["127.0.0.1:80"]),
                "{result:?}"
            );
        }
        assert_eq!(
            connected("connect", &[3, SOCK_UNIX, 7], failed(libc::ECONNREFUSED)).0,
            expect(&[("/work/sock", "probe")])
        );
        // A call that is not a socket call has no connection.
        let call = decode(&socket_thread(), "stat", &[REL]);
        assert_eq!(call.connections(Some(0)), Vec::new());
    }

    /// The socket names that the kernel reads from addresses of each family and
    /// length.
    #[test]
    fn socket_names_are_read_as_the_kernel_reads_them() {
        let ip = |text: &str| Some(SocketName::Ip(text.parse().unwrap()));
        assert_eq!(
            socket_name(&sockaddr_in([10, 0, 0, 2], 8080, 16)),
            ip("10.0.0.2:8080")
        );
        assert_eq!(socket_name(&sockaddr_in([10, 0, 0, 2], 8080, 15)), None);
        assert_eq!(
            socket_name(&sockaddr_in6(LINK_LOCAL, 1, 9)),
            ip("[fe80::1%9]:1")
        );
        assert_eq!(
            sockaddr_in6(LINK_LOCAL, 1, 9)
                .first_chunk::<24>()
                .and_then(|short| socket_name(short.as_slice())),
            ip("[fe80::1]:1")
        );
        assert_eq!(
            socket_name(&sockaddr_un(b"p\0q", 0)),
            Some(SocketName::Path(b"p".to_vec()))
        );
        assert_eq!(
            socket_name(&sockaddr_un(b"\0", 0)),
            Some(SocketName::Abstract(Vec::new()))
        );
        assert_eq!(socket_name(&family(libc::AF_UNIX)), None);
        assert_eq!(socket_name(&family(libc::AF_INET6)), None);
        assert_eq!(socket_name(&[]), None);
        assert_eq!(socket_name(&[1]), None);
    }

    /// The observations of the call `name` with `arguments` in `fake` when it
    /// returns `result`, each path with each kind and its outcomes, joined with
    /// `,`; the observations must combine to exactly the accesses that the call
    /// records.
    fn observe_in(
        fake: &Fake,
        name: &str,
        arguments: &[u64],
        result: Option<i64>,
    ) -> Vec<(String, String, String)> {
        let mut accesses = Accesses::new();
        let mut observations = Observations::new();
        decode(fake, name, arguments).finish_observed(
            result,
            &mut accesses,
            Some(&mut observations),
        );
        assert_eq!(observations.accesses(), accesses, "{name}");
        let mut flat = Vec::new();
        for (path, kinds) in &observations {
            for (kind, outcomes) in kinds {
                let outcomes: Vec<String> = outcomes.iter().map(ToString::to_string).collect();
                flat.push((path.to_string(), kind.to_string(), outcomes.join(",")));
            }
        }
        flat
    }

    /// The expected observations, from triples of a path, a kind and outcomes.
    fn observed(triples: &[(&str, &str, &str)]) -> Vec<(String, String, String)> {
        triples
            .iter()
            .map(|(path, kind, outcomes)| {
                (
                    (*path).to_owned(),
                    (*kind).to_owned(),
                    (*outcomes).to_owned(),
                )
            })
            .collect()
    }

    /// A success carries `ok` on every kind it records, and a failure its error's
    /// name on its probe, `EINTR` and the kernel's restart errors included.
    #[test]
    fn calls_carry_their_outcome_on_every_kind() {
        let fake = Fake::new();
        let open = [CWD, REL, 0];
        assert_eq!(
            observe_in(&fake, "openat", &open, Some(3)),
            observed(&[("/work/rel", "read", "ok")])
        );
        for (errno, name) in [
            (libc::ENOENT, "ENOENT"),
            (libc::EACCES, "EACCES"),
            (libc::EINTR, "EINTR"),
            (512, "ERESTARTSYS"),
            (513, "ERESTARTNOINTR"),
            (514, "ERESTARTNOHAND"),
            (516, "ERESTART_RESTARTBLOCK"),
            (4000, "errno 4000"),
        ] {
            assert_eq!(
                observe_in(&fake, "openat", &open, failed(errno)),
                observed(&[("/work/rel", "probe", name)]),
                "{name}"
            );
        }
        assert_eq!(
            observe_in(&fake, "renameat", &[CWD, REL, CWD, OTHER], Some(0)),
            observed(&[("/work/other", "write", "ok"), ("/work/rel", "write", "ok")])
        );
    }

    /// A call that reads extended attributes and fails with an answer of the
    /// attributes carries the error's name on the xattr kind, and any other failure
    /// on a probe.
    #[test]
    fn attribute_answers_carry_their_error_on_the_xattr_kind() {
        let fake = Fake::new();
        for (errno, name) in [
            (libc::ENODATA, "ENODATA"),
            (libc::ERANGE, "ERANGE"),
            (libc::EOPNOTSUPP, "EOPNOTSUPP"),
        ] {
            assert_eq!(
                observe_in(&fake, "getxattr", &[REL], failed(errno)),
                observed(&[("/work/rel", "xattr", name)]),
                "{name}"
            );
        }
        assert_eq!(
            observe_in(&fake, "getxattr", &[REL], failed(libc::ENOENT)),
            observed(&[("/work/rel", "probe", "ENOENT")])
        );
        assert_eq!(
            observe_in(&fake, "getxattr", &[REL], Some(4)),
            observed(&[("/work/rel", "xattr", "ok")])
        );
    }

    /// A call whose result was never seen carries `unknown` on every kind it
    /// records: those of its rule and the probe.
    #[test]
    fn an_unseen_result_carries_unknown_on_every_kind() {
        let fake = Fake::new();
        assert_eq!(
            observe_in(&fake, "openat", &[CWD, REL, 0], None),
            observed(&[
                ("/work/rel", "read", "unknown"),
                ("/work/rel", "probe", "unknown")
            ])
        );
        assert_eq!(Call::outcome(None), Outcome::Unknown);
        assert_eq!(Call::outcome(Some(0)), Outcome::Ok);
        assert_eq!(Call::outcome(Some(-4096)), Outcome::Ok);
        assert_eq!(
            Call::outcome(Some(-4095)),
            Outcome::Error("errno 4095".to_owned())
        );
    }

    /// The number of outcomes that [`Call::outcome`] has built on this thread.
    fn outcomes_built() -> usize {
        OUTCOMES_BUILT.with(std::cell::Cell::get)
    }

    /// A call recorded without observations, as a build records it, builds no
    /// outcome, so no error name is looked up, and it records the same accesses
    /// and gives the same reason as with observations, which build one outcome
    /// per call that records its paths: for a success, failures, an attribute
    /// answer, a result never seen, a call that could not be decoded, and one whose
    /// error says the kernel refused that input, which records nothing either way.
    #[test]
    fn calls_without_observations_build_no_outcome() {
        /// A call's name, arguments and result, the outcomes that recording it with
        /// observations builds, and whether it makes the trace incomplete.
        type Case = (&'static str, &'static [u64], Option<i64>, usize, bool);
        let fake = Fake::new();
        let cases: [Case; 7] = [
            ("openat", &[CWD, REL, 0], Some(3), 1, false),
            ("openat", &[CWD, REL, 0], failed(libc::ENOENT), 1, false),
            ("openat", &[CWD, REL, 0], failed(libc::EINTR), 1, false),
            ("openat", &[CWD, REL, 0], None, 1, false),
            ("getxattr", &[REL], failed(libc::ENODATA), 1, false),
            ("rename", &[REL, UNMAPPED], Some(0), 1, true),
            ("rename", &[REL, UNMAPPED], failed(libc::EFAULT), 0, false),
        ];
        for (name, arguments, result, observed, incomplete) in cases {
            let before = outcomes_built();
            let mut built = Accesses::new();
            let built_reason = decode(&fake, name, arguments).finish(result, &mut built);
            assert_eq!(outcomes_built(), before, "{name} {result:?}");
            let mut accesses = Accesses::new();
            let mut observations = Observations::new();
            let reason = decode(&fake, name, arguments).finish_observed(
                result,
                &mut accesses,
                Some(&mut observations),
            );
            assert_eq!(
                outcomes_built(),
                before.saturating_add(observed),
                "{name} {result:?}"
            );
            assert_eq!(built, accesses, "{name} {result:?}");
            assert_eq!(built_reason, reason, "{name} {result:?}");
            assert_eq!(reason.is_some(), incomplete, "{name} {result:?}");
        }
    }

    /// The address of an argument vector in the fake thread's memory.
    const VECTOR: u64 = 0x20000;
    /// The address of an argument vector whose second pointer is unmapped.
    const BROKEN_VECTOR: u64 = 0x28000;

    /// The fake thread with an argument vector at [`VECTOR`]: a word, an empty
    /// string, a string with spaces, and bytes that are not UTF-8.
    fn with_vector() -> Fake {
        let mut fake = Fake::new();
        let strings: [(u64, &[u8]); 4] = [
            (0x21000, b"prog\0"),
            (0x22000, b"\0"),
            (0x23000, b"two  words\0"),
            (0x24000, b"\xFF\xFEraw\0"),
        ];
        let mut pointers = Vec::new();
        for (address, bytes) in strings {
            fake.memory.insert(address, bytes.to_vec());
            pointers.extend_from_slice(&address.to_ne_bytes());
        }
        pointers.extend_from_slice(&0_u64.to_ne_bytes());
        fake.memory.insert(VECTOR, pointers);
        let broken: Vec<u8> = [0x21000, UNMAPPED, 0]
            .iter()
            .flat_map(|pointer: &u64| pointer.to_ne_bytes())
            .collect();
        fake.memory.insert(BROKEN_VECTOR, broken);
        fake
    }

    /// The argument vector of `execve` and `execveat` is read exactly as the
    /// kernel reads it, and a vector that cannot be read is left empty, as is any
    /// other call's.
    #[test]
    fn exec_argument_vectors_are_read_exactly() {
        let fake = with_vector();
        let expected = vec![
            OsString::from("prog"),
            OsString::new(),
            OsString::from("two  words"),
            OsString::from_vec(b"\xFF\xFEraw".to_vec()),
        ];
        assert_eq!(
            argument_vector(libc::SYS_execve, [ABS, VECTOR, 0, 0, 0, 0], &fake),
            expected
        );
        assert_eq!(
            argument_vector(libc::SYS_execveat, [CWD, ABS, VECTOR, 0, 0, 0], &fake),
            expected
        );
        for vector in [0, UNMAPPED, BROKEN_VECTOR] {
            assert_eq!(
                argument_vector(libc::SYS_execve, [ABS, vector, 0, 0, 0, 0], &fake),
                Vec::<OsString>::new(),
                "{vector:#x}"
            );
        }
        assert_eq!(
            argument_vector(libc::SYS_openat, [CWD, ABS, VECTOR, 0, 0, 0], &fake),
            Vec::<OsString>::new()
        );
    }

    /// An `exec` names the program that it starts, the path of its first operand,
    /// and any other call names none.
    #[test]
    fn execs_name_their_program() {
        let fake = Fake::new();
        let execve = decode(&fake, "execve", &[ABS, 0, 0]);
        assert_eq!(
            execve.program().map(ToString::to_string),
            Some("/abs/x/y".to_owned())
        );
        let execveat = decode(&fake, "execveat", &[CWD, REL, 0, 0, 0]);
        assert_eq!(
            execveat.program().map(ToString::to_string),
            Some("/work/rel".to_owned())
        );
        assert_eq!(decode(&fake, "openat", &[CWD, REL, 0]).program(), None);
    }
}
