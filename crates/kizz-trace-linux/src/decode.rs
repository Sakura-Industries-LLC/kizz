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
//! the call returns, [`Call::finish`] records each path with its access kinds:
//!
//! * a call that succeeded records the kinds of its rule in [`SYSTEM_CALLS`];
//! * a call that failed records a probe for each path: it checked the path's existence or
//!   metadata, including a lookup that found nothing (`ENOENT`), and it read, wrote and
//!   ran nothing;
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

use std::{
    ffi::OsStr,
    io,
    os::unix::ffi::OsStrExt as _,
    path::{Path, PathBuf},
};

use kizz_trace::{AbsolutePath, AccessKind, AccessKinds, Accesses};
use libc::{c_int, c_long};

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
    // Changes through a descriptor, to the path behind it.
    call(libc::SYS_ftruncate, "ftruncate", &[descriptor(0, WRITE)]),
    call(libc::SYS_fchmod, "fchmod", &[descriptor(0, WRITE)]),
    call(libc::SYS_fchown, "fchown", &[descriptor(0, WRITE)]),
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
    /// kinds of the program's own path, so [`Call::finish`] records them as it
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

    /// Record the paths of the call, now that it returned `result`, or `None` when
    /// its result was never seen, and return a description of what could not be
    /// decoded, or of a program that ran but that [`Call::loaded`] could not
    /// follow, when that makes the trace incomplete.
    ///
    /// A call that failed records a probe for each path. That includes a call that a
    /// signal interrupted, which returns `EINTR`, or one of the kernel's internal
    /// restart errors when it restarts: either way its lookup happened, and a call
    /// that restarts is decoded again at its next entry.
    ///
    /// A part of the call that could not be decoded makes the trace incomplete,
    /// unless the call failed with the error the kernel gives that same input
    /// before it looks anything up: a bad address, a bad descriptor, or a path that
    /// is too long.
    pub(crate) fn finish(
        self,
        result: Option<i64>,
        accesses: &mut Accesses,
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
        for (path, kinds) in self.operands {
            let recorded = match (result, errno) {
                (Some(_), Some(_)) => AccessKinds::only(AccessKind::Probe),
                (Some(_), None) => kinds,
                (None, _) => kinds.with(AccessKind::Probe),
            };
            accesses.record_kinds(path, recorded);
        }
        if let Some(problem) = self.problem {
            return Some(format!("could not decode {}: {problem}", self.name));
        }
        self.unfollowed
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
    let mut bytes = vec![0_u8; length];
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
    let headers = read_exactly(inspect, file, elf.offset, size)?;
    for header in headers.as_chunks::<PROGRAM_HEADER_SIZE>().0 {
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
            Kinds::OpenFlags(index) => open_kinds(self.argument(index)),
            Kinds::OpenHow => AccessKinds::EMPTY,
        };
        match rule.operand {
            Operand::Path { dirfd, path, empty } => {
                let dirfd = dirfd.map_or(libc::AT_FDCWD, |index| int(self.argument(index)));
                self.path(dirfd, self.argument(path), empty, Lookup::Ordinary, kinds);
            },
            Operand::Descriptor(index) => self.descriptor(int(self.argument(index)), kinds),
            Operand::OpenHow => self.open_how(),
        }
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
        },
    };
    for rule in system_call.rules {
        decoder.rule(*rule);
    }
    if matches!(number, libc::SYS_execve | libc::SYS_execveat) {
        decoder.call.directory = Some(match decoder.base(libc::AT_FDCWD) {
            Ok(Some(path)) => Directory::Path(path),
            Ok(None) => Directory::Removed,
            Err(problem) => Directory::Unknown(problem),
        });
    }
    Some(decoder.call)
}

/// Unit tests of the decoding of every tracked call, with a fake thread.
#[cfg(test)]
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
}
