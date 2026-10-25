// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Safe wrappers over the `libc` calls the tracer makes in its own process.
//!
//! Every `ptrace` request here must come from the thread that started the traced
//! process tree, because Linux makes that thread, not the whole process, the tracer.
//! The calls that run in the child between `fork` and `exec` live in the `spawn`
//! module, because they must avoid everything that is not async-signal-safe.

use std::{
    ffi::{CStr, CString},
    fs, io,
    mem::{self, MaybeUninit},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::{OsStrExt as _, OsStringExt as _},
            fs::{FileExt as _, MetadataExt as _, OpenOptionsExt as _},
        },
    },
    path::Path,
    ptr,
};

use kizz_trace::{
    Disk, ExtendedAttributes, FileMetadata, FileSystem, Kernel, MachineState, Memory, ResourceLimit,
};
use libc::{c_int, c_long, c_uint, c_ulong, c_void, pid_t};

use crate::decode;

/// The flags of every wait: report every traced thread, and only the children and
/// tracees of the calling thread, so tracers on other threads of the process never
/// see each other's processes.
const WAIT_FLAGS: c_int = libc::__WALL | libc::__WNOTHREAD;

/// Turn the `-1` failure value of a call into the last OS error.
fn check(ret: c_int) -> io::Result<c_int> {
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

/// Make one `ptrace` request whose address argument is unused and whose data
/// argument is a value.
fn ptrace_value(
    request: c_uint,
    pid: pid_t,
    data: c_long,
) -> io::Result<()> {
    // SAFETY: the requests made with a value never dereference the data argument,
    // and an address argument of null is ignored by every request made here.
    let ret = unsafe { libc::ptrace(request, pid, ptr::null_mut::<c_void>(), data) };
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Make one `ptrace` request whose address argument is unused and whose data
/// argument points to a buffer the kernel fills.
///
/// # Safety
///
/// `data` must point to writable memory of the size the request writes.
unsafe fn ptrace_pointer(
    request: c_uint,
    pid: pid_t,
    data: *mut c_void,
) -> io::Result<()> {
    // SAFETY: the caller guarantees that `data` is large enough for the request.
    let ret = unsafe { libc::ptrace(request, pid, ptr::null_mut::<c_void>(), data) };
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Set the tracing options of a tracee in a `ptrace` stop.
pub(crate) fn set_options(
    pid: pid_t,
    options: c_int,
) -> io::Result<()> {
    ptrace_value(libc::PTRACE_SETOPTIONS, pid, c_long::from(options))
}

/// Resume a tracee in a `ptrace` stop, delivering `signal` unless it is zero.
pub(crate) fn resume(
    pid: pid_t,
    signal: c_int,
) -> io::Result<()> {
    ptrace_value(libc::PTRACE_CONT, pid, c_long::from(signal))
}

/// Resume a tracee in a `ptrace` stop until its current system call returns,
/// delivering `signal` unless it is zero: the tracee stops again at the exit of
/// the call, where the tracer reads the result.
pub(crate) fn resume_to_exit(
    pid: pid_t,
    signal: c_int,
) -> io::Result<()> {
    ptrace_value(libc::PTRACE_SYSCALL, pid, c_long::from(signal))
}

/// A traced thread in a `ptrace` stop, whose memory, working directory and
/// descriptors the decoding of a system call reads. Nothing in it is changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Thread(pub pid_t);

impl Thread {
    /// The link in `/proc` of the thread's descriptor `fd`, or of its working
    /// directory for `AT_FDCWD`.
    fn link(
        self,
        fd: c_int,
    ) -> String {
        if fd == libc::AT_FDCWD {
            format!("/proc/{}/cwd", self.0)
        } else {
            format!("/proc/{}/fd/{fd}", self.0)
        }
    }
}

/// The target of a symbolic link, as bytes.
fn read_link(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    Ok(fs::read_link(path)?.into_os_string().into_vec())
}

/// The identity of the object that `metadata` describes.
fn object(metadata: &fs::Metadata) -> decode::Object {
    decode::Object {
        device: metadata.dev(),
        inode: metadata.ino(),
        links: metadata.nlink(),
        symbolic_link: metadata.file_type().is_symlink(),
        directory: metadata.is_dir(),
    }
}

impl decode::Inspect for Thread {
    fn memory(
        &self,
        address: u64,
        buffer: &mut [u8],
    ) -> io::Result<usize> {
        let address = usize::try_from(address)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let local = libc::iovec {
            iov_base: buffer.as_mut_ptr().cast::<c_void>(),
            iov_len: buffer.len(),
        };
        let remote = libc::iovec {
            iov_base: ptr::without_provenance_mut::<c_void>(address),
            iov_len: buffer.len(),
        };
        // SAFETY: `local` describes `buffer`, which is writable for its whole
        // length; `remote` is only an address in the traced thread, which the
        // kernel checks.
        let read =
            unsafe { libc::process_vm_readv(self.0, &raw const local, 1, &raw const remote, 1, 0) };
        usize::try_from(read).map_err(|_| io::Error::last_os_error())
    }

    fn working_directory(&self) -> io::Result<Vec<u8>> {
        read_link(self.link(libc::AT_FDCWD))
    }

    fn descriptor(
        &self,
        fd: c_int,
    ) -> io::Result<Vec<u8>> {
        read_link(self.link(fd))
    }

    fn link_object(
        &self,
        fd: c_int,
    ) -> io::Result<decode::Object> {
        // `stat` follows the link in `/proc` to the object itself, even when it
        // was removed.
        Ok(object(&fs::metadata(self.link(fd))?))
    }

    fn path_object(
        &self,
        path: &Path,
    ) -> io::Result<decode::Object> {
        Ok(object(&fs::symlink_metadata(path)?))
    }

    fn link_target(
        &self,
        path: &Path,
    ) -> io::Result<Vec<u8>> {
        read_link(path)
    }

    fn read_file(
        &self,
        path: &Path,
        offset: u64,
        buffer: &mut [u8],
    ) -> io::Result<usize> {
        // Only a regular file is opened, and without blocking or taking a
        // controlling terminal, because opening a FIFO or a device can wait or act.
        let not_regular = || io::Error::new(io::ErrorKind::InvalidInput, "not a regular file");
        if !fs::metadata(path)?.is_file() {
            return Err(not_regular());
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err(not_regular());
        }
        let mut filled = 0_usize;
        while let Some(rest) = buffer.get_mut(filled..)
            && !rest.is_empty()
        {
            let at = u64::try_from(filled)
                .ok()
                .and_then(|filled| offset.checked_add(filled))
                .ok_or_else(|| io::Error::from_raw_os_error(libc::EOVERFLOW))?;
            let read = match file.read_at(rest, at) {
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            if read == 0 {
                break;
            }
            filled = filled.saturating_add(read);
        }
        Ok(filled)
    }

    fn descriptor_file(
        &self,
        fd: c_int,
    ) -> Option<FileMetadata> {
        // `stat` follows the link in `/proc` to the file the descriptor holds open.
        FileMetadata::at(Path::new(&self.link(fd)))
    }

    fn path_file(
        &self,
        path: &Path,
    ) -> Option<FileMetadata> {
        FileMetadata::at(path)
    }
}

/// The message of the `ptrace` event stop a tracee is in: the new thread or process
/// for a fork, vfork or clone, the former thread ID for an `exec`, and the filter's
/// return data for a seccomp stop.
pub(crate) fn event_message(pid: pid_t) -> io::Result<c_ulong> {
    let mut message: c_ulong = 0;
    // SAFETY: `PTRACE_GETEVENTMSG` writes one `unsigned long`.
    unsafe {
        ptrace_pointer(
            libc::PTRACE_GETEVENTMSG,
            pid,
            (&raw mut message).cast::<c_void>(),
        )?;
    }
    Ok(message)
}

/// Whether a tracee stopped with a signal is in a signal-delivery stop, which has
/// signal information, rather than a group stop, which has none.
pub(crate) fn has_signal_info(pid: pid_t) -> io::Result<bool> {
    // SAFETY: `siginfo_t` is plain data, for which all zero bytes are a valid value.
    let mut info: libc::siginfo_t = unsafe { mem::zeroed() };
    // SAFETY: `PTRACE_GETSIGINFO` writes one `siginfo_t`.
    let result = unsafe {
        ptrace_pointer(
            libc::PTRACE_GETSIGINFO,
            pid,
            (&raw mut info).cast::<c_void>(),
        )
    };
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.raw_os_error() == Some(libc::EINVAL) => Ok(false),
        Err(error) => Err(error),
    }
}

/// The general-purpose registers of a tracee in a `ptrace` stop.
pub(crate) fn registers(pid: pid_t) -> io::Result<libc::user_regs_struct> {
    // SAFETY: `user_regs_struct` is plain integers, for which zero is a valid value.
    let mut registers: libc::user_regs_struct = unsafe { mem::zeroed() };
    // SAFETY: `PTRACE_GETREGS` writes one `user_regs_struct`.
    unsafe {
        ptrace_pointer(
            libc::PTRACE_GETREGS,
            pid,
            (&raw mut registers).cast::<c_void>(),
        )?;
    }
    Ok(registers)
}

/// Send `signal` to the process that contains the thread `pid`.
///
/// The caller must know that `pid` still names a thread of its own tree: Linux
/// gives a released ID to the next process or thread it creates.
pub(crate) fn kill(
    pid: pid_t,
    signal: c_int,
) -> io::Result<()> {
    // SAFETY: `kill` has no memory-safety requirements.
    check(unsafe { libc::kill(pid, signal) }).map(drop)
}

/// The process ID, which Linux calls the thread group ID, of the process that
/// contains the thread `tid`, read from `/proc/<tid>/status`.
pub(crate) fn thread_group(tid: pid_t) -> io::Result<pid_t> {
    let status = fs::read_to_string(format!("/proc/{tid}/status"))?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Tgid:"))
        .and_then(|value| value.trim().parse().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the status of thread {tid} has no thread group"),
            )
        })
}

/// A state change of a child or tracee that a wait reported without collecting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pending {
    /// The thread is in a stop, which this wait status describes. The stop stays
    /// in place until the tracer either collects it with [`collect`] or resumes
    /// or kills the thread.
    ///
    /// While a signal-delivery stop is in place, the thread keeps the signal it is
    /// about to receive; collecting the stop clears it, and the signal is then
    /// delivered only if the tracer resumes the thread with it. A thread that is
    /// killed in a stop that is still in place, for example when another thread of
    /// its process runs `exec`, queues its signal again for its process.
    Stopped(c_int),
    /// The thread has exited; [`collect`] collects its exit.
    Ended,
}

/// Wait for a state change of a child or tracee of the calling thread, with the
/// `waitid` target `id_type` and `id` and the extra `flags`, without collecting it,
/// and return the thread and the change, or `None` when there is nothing to report.
fn peek(
    id_type: libc::idtype_t,
    id: libc::id_t,
    flags: c_int,
) -> io::Result<Option<(pid_t, Pending)>> {
    loop {
        // SAFETY: `siginfo_t` is plain data, for which all zero bytes are a valid
        // value.
        let mut info: libc::siginfo_t = unsafe { mem::zeroed() };
        // SAFETY: `info` is a valid `siginfo_t` for `waitid` to fill.
        let ret = unsafe {
            libc::waitid(
                id_type,
                id,
                &raw mut info,
                libc::WEXITED | libc::WSTOPPED | libc::WNOWAIT | WAIT_FLAGS | flags,
            )
        };
        match check(ret) {
            Ok(_) => {
                // SAFETY: a successful `waitid` filled in the child fields of
                // `info`, or left them zero when `WNOHANG` found no change.
                let pid = unsafe { info.si_pid() };
                if pid == 0 {
                    return Ok(None);
                }
                let pending = if matches!(info.si_code, libc::CLD_TRAPPED | libc::CLD_STOPPED) {
                    // SAFETY: as above. For a stop, the status holds the signal and,
                    // for a `ptrace` event stop, the event in the bits above it, as
                    // a wait status does from its second byte on.
                    let code = unsafe { info.si_status() };
                    Pending::Stopped(code.checked_shl(8).unwrap_or(0) | 0x7F)
                } else {
                    Pending::Ended
                };
                return Ok(Some((pid, pending)));
            },
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {},
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => return Ok(None),
            Err(error) => return Err(error),
        }
    }
}

/// Wait until a child or tracee of the calling thread changes state, without
/// collecting the change, and return its thread ID and the change, or `None` when
/// the calling thread has no child or tracee left to wait for.
///
/// The wait covers every child of the calling thread, so the caller must be a
/// thread whose only children are the traced program's first process.
pub(crate) fn wait_pending() -> io::Result<Option<(pid_t, Pending)>> {
    peek(libc::P_ALL, 0, 0)
}

/// The state change of the thread `pid` that a wait would report now, without
/// collecting it, or `None` when there is none or `pid` is not a child or tracee
/// of the calling thread.
#[cfg(test)]
pub(crate) fn pending(pid: pid_t) -> io::Result<Option<Pending>> {
    let id = libc::id_t::try_from(pid)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    Ok(peek(libc::P_PID, id, libc::WNOHANG)?.map(|(_, pending)| pending))
}

/// Collect the waiting state change of `pid`, if it has one, and return its wait
/// status.
pub(crate) fn collect(pid: pid_t) -> io::Result<Option<c_int>> {
    loop {
        let mut status: c_int = 0;
        // SAFETY: `status` is a valid `int` for `waitpid` to fill.
        let ret = unsafe { libc::waitpid(pid, &raw mut status, WAIT_FLAGS | libc::WNOHANG) };
        match check(ret) {
            Ok(0) => return Ok(None),
            Ok(_) => return Ok(Some(status)),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {},
            Err(error) => return Err(error),
        }
    }
}

/// Wait for the next state change of `pid` and return its wait status.
pub(crate) fn wait_for(pid: pid_t) -> io::Result<c_int> {
    loop {
        let mut status: c_int = 0;
        // SAFETY: `status` is a valid `int` for `waitpid` to fill.
        let ret = unsafe { libc::waitpid(pid, &raw mut status, WAIT_FLAGS) };
        match check(ret) {
            Ok(_) => return Ok(status),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {},
            Err(error) => return Err(error),
        }
    }
}

/// The `ptrace` event of a wait status, or zero when the stop is not an event stop.
pub(crate) fn stop_event(status: c_int) -> c_int {
    status.checked_shr(16).unwrap_or(0) & 0xFF
}

/// Create a pipe whose two ends are closed on `exec` and are never standard input,
/// output or error, so the child can move them onto those descriptors safely.
pub(crate) fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds: [c_int; 2] = [-1, -1];
    // SAFETY: `fds` has room for the two descriptors `pipe2` writes.
    check(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) })?;
    let [read, write] = fds;
    // SAFETY: `pipe2` returned two new descriptors that nothing else owns.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(read), OwnedFd::from_raw_fd(write)) };
    Ok((above_stdio(read)?, above_stdio(write)?))
}

/// Move a descriptor above standard error, keeping it closed on `exec`.
pub(crate) fn above_stdio(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() > libc::STDERR_FILENO {
        return Ok(fd);
    }
    // SAFETY: `fd` is an open descriptor, and `F_DUPFD_CLOEXEC` only duplicates it.
    let duplicate = check(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) })?;
    // SAFETY: `fcntl` returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicate) })
}

/// How many times a read of extended attributes is tried when they grow between
/// the call that sizes the buffer and the call that fills it.
const ATTRIBUTE_ATTEMPTS: usize = 8;

/// Read a list or value whose size the kernel tells first: `call` with a null
/// buffer and a size of zero returns the size, and with a buffer fills it. A
/// buffer that became too small (`ERANGE`) is sized again.
///
/// # Errors
///
/// Returns the error of the call, or `ERANGE` when the size kept changing.
fn sized_read(mut call: impl FnMut(*mut c_void, usize) -> isize) -> io::Result<Vec<u8>> {
    for _ in 0..ATTRIBUTE_ATTEMPTS {
        let size =
            usize::try_from(call(ptr::null_mut(), 0)).map_err(|_| io::Error::last_os_error())?;
        let mut buffer = vec![0_u8; size];
        let filled = call(buffer.as_mut_ptr().cast(), buffer.len());
        if let Ok(filled) = usize::try_from(filled) {
            buffer.truncate(filled);
            return Ok(buffer);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ERANGE) {
            return Err(error);
        }
    }
    Err(io::Error::from_raw_os_error(libc::ERANGE))
}

/// The names of the extended attributes of `path`, as `listxattr` lists them, or
/// `llistxattr` without following a final symbolic link.
fn attribute_names(
    path: &CStr,
    follow: bool,
) -> io::Result<Vec<u8>> {
    sized_read(|buffer, size| {
        // SAFETY: `path` is a NUL-terminated string, and `buffer` is null with a
        // size of zero or has room for `size` bytes, which is all the kernel writes.
        unsafe {
            if follow {
                libc::listxattr(path.as_ptr(), buffer.cast(), size)
            } else {
                libc::llistxattr(path.as_ptr(), buffer.cast(), size)
            }
        }
    })
}

/// The value of the extended attribute `name` of `path`, as `getxattr` reads it,
/// or `lgetxattr` without following a final symbolic link.
fn attribute_value(
    path: &CStr,
    name: &CStr,
    follow: bool,
) -> io::Result<Vec<u8>> {
    sized_read(|buffer, size| {
        // SAFETY: `path` and `name` are NUL-terminated strings, and `buffer` is null
        // with a size of zero or has room for `size` bytes, which is all the kernel
        // writes.
        unsafe {
            if follow {
                libc::getxattr(path.as_ptr(), name.as_ptr(), buffer, size)
            } else {
                libc::lgetxattr(path.as_ptr(), name.as_ptr(), buffer, size)
            }
        }
    })
}

/// The extended attributes of `path`, each name with its value, following a final
/// symbolic link when `follow` is set. A file system that keeps no extended
/// attributes (`EOPNOTSUPP`) has none, and an attribute removed between listing
/// the names and reading its value (`ENODATA`) is left out.
///
/// # Errors
///
/// Returns the error of listing the names or reading a value, such as `ENOENT` for
/// a path that names nothing, and an error for a path with a NUL byte.
pub(crate) fn extended_attributes(
    path: &Path,
    follow: bool,
) -> io::Result<ExtendedAttributes> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let names = match attribute_names(&path, follow) {
        Ok(names) => names,
        Err(error) if error.raw_os_error() == Some(libc::EOPNOTSUPP) => {
            return Ok(ExtendedAttributes::new());
        },
        Err(error) => return Err(error),
    };
    let mut attributes = ExtendedAttributes::new();
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let c_name = CString::new(name)?;
        match attribute_value(&path, &c_name, follow) {
            Ok(value) => {
                attributes.insert(name.to_vec(), value);
            },
            Err(error) if error.raw_os_error() == Some(libc::ENODATA) => {},
            Err(error) => return Err(error),
        }
    }
    Ok(attributes)
}

/// The file that reports the memory of the machine.
const MEMINFO_FILE: &str = "/proc/meminfo";

/// The resources whose limits a process inherits, each with the name that
/// `prlimit` gives it, in the kernel's order.
pub(crate) const RESOURCES: [(&str, libc::__rlimit_resource_t); 16] = [
    ("cpu", libc::RLIMIT_CPU),
    ("fsize", libc::RLIMIT_FSIZE),
    ("data", libc::RLIMIT_DATA),
    ("stack", libc::RLIMIT_STACK),
    ("core", libc::RLIMIT_CORE),
    ("rss", libc::RLIMIT_RSS),
    ("nproc", libc::RLIMIT_NPROC),
    ("nofile", libc::RLIMIT_NOFILE),
    ("memlock", libc::RLIMIT_MEMLOCK),
    ("as", libc::RLIMIT_AS),
    ("locks", libc::RLIMIT_LOCKS),
    ("sigpending", libc::RLIMIT_SIGPENDING),
    ("msgqueue", libc::RLIMIT_MSGQUEUE),
    ("nice", libc::RLIMIT_NICE),
    ("rtprio", libc::RLIMIT_RTPRIO),
    ("rttime", libc::RLIMIT_RTTIME),
];

/// The number of 64-bit words in the CPU mask that `sched_getaffinity` fills:
/// room for 1,024 CPUs, as in `cpu_set_t`.
const AFFINITY_WORDS: usize = 16;

/// The text of a field of `uname`, which ends with a zero byte within the field.
fn uname_field(field: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = field
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| {
            let [byte] = byte.to_ne_bytes();
            byte
        })
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The release and version of the kernel, from `uname`.
fn kernel() -> io::Result<Kernel> {
    let mut names = MaybeUninit::<libc::utsname>::uninit();
    // SAFETY: the pointer is valid for writing a `utsname`.
    check(unsafe { libc::uname(names.as_mut_ptr()) })?;
    // SAFETY: `uname` succeeded, so it initialized `names`.
    let names = unsafe { names.assume_init() };
    Ok(Kernel {
        release: uname_field(&names.release),
        version: uname_field(&names.version),
    })
}

/// A limit as `getrlimit` reports it, or `None` for no limit (`RLIM_INFINITY`).
fn finite(value: libc::rlim_t) -> Option<u64> {
    (value != libc::RLIM_INFINITY).then_some(value)
}

/// Every resource limit of the calling process, soft and hard, from `getrlimit`,
/// in the order of [`RESOURCES`].
fn limits() -> io::Result<Vec<ResourceLimit>> {
    RESOURCES
        .iter()
        .map(|&(name, resource)| {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: `limit` is a valid, writable `rlimit`.
            check(unsafe { libc::getrlimit(resource, &raw mut limit) })?;
            Ok(ResourceLimit {
                resource: name.to_owned(),
                soft: finite(limit.rlim_cur),
                hard: finite(limit.rlim_max),
            })
        })
        .collect()
}

/// The CPUs that the calling thread, and so a process it starts, may run on, by
/// number, sorted, from `sched_getaffinity`.
fn affinity() -> io::Result<Vec<u32>> {
    let mut mask = [0_u64; AFFINITY_WORDS];
    // SAFETY: `mask` has room for the size passed, which is all the kernel writes.
    let written = unsafe {
        libc::syscall(
            libc::SYS_sched_getaffinity,
            0,
            mem::size_of_val(&mask),
            mask.as_mut_ptr(),
        )
    };
    if written < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut cpus = Vec::new();
    for (first, word) in (0_u32..).step_by(64).zip(mask) {
        for bit in 0..u64::BITS {
            if word.checked_shr(bit).unwrap_or(0) & 1 == 1 {
                cpus.push(first.saturating_add(bit));
            }
        }
    }
    Ok(cpus)
}

/// The value of the line `name` of `/proc/meminfo`, in bytes.
fn meminfo_field(
    text: &str,
    name: &str,
) -> io::Result<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
        .and_then(|rest| rest.trim().strip_suffix("kB")?.trim().parse::<u64>().ok())
        .and_then(|kibibytes| kibibytes.checked_mul(1_024))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{MEMINFO_FILE} has no `{name}` line in kB"),
            )
        })
}

/// The total and the available memory of the machine, from the `MemTotal` and
/// `MemAvailable` lines of `/proc/meminfo`.
fn memory() -> io::Result<Memory> {
    let text = fs::read_to_string(MEMINFO_FILE)?;
    Ok(Memory {
        total: meminfo_field(&text, "MemTotal")?,
        available: meminfo_field(&text, "MemAvailable")?,
    })
}

/// The statistics of the file system that holds `path`: its type and sizes from
/// `statfs`, and its mount flags from `statvfs`, which reads them with the same
/// `statfs` call, because the `statfs` structure of the `libc` crate leaves them
/// out.
fn file_system(path: &Path) -> io::Result<(libc::statfs, libc::statvfs)> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut stats = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `path` is a NUL-terminated string, and the pointer is valid for
    // writing a `statfs`.
    check(unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) })?;
    // SAFETY: `statfs` succeeded, so it initialized `stats`.
    let stats = unsafe { stats.assume_init() };
    let mut mount = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a NUL-terminated string, and the pointer is valid for
    // writing a `statvfs`.
    check(unsafe { libc::statvfs(path.as_ptr(), mount.as_mut_ptr()) })?;
    // SAFETY: `statvfs` succeeded, so it initialized `mount`.
    Ok((stats, unsafe { mount.assume_init() }))
}

/// The type of the file system that `stats` describes and its mount flags, from
/// `mount`.
const fn file_system_of(
    stats: &libc::statfs,
    mount: &libc::statvfs,
) -> FileSystem {
    FileSystem {
        kind: u64::from_ne_bytes(stats.f_type.to_ne_bytes()),
        flags: mount.f_flag,
    }
}

/// The size and the free space, for a process without privileges, of the file
/// system that `stats` describes, in bytes.
fn disk_of(stats: &libc::statfs) -> Disk {
    let fragment = if stats.f_frsize > 0 {
        stats.f_frsize
    } else {
        stats.f_bsize
    };
    let fragment = u64::try_from(fragment).unwrap_or(0);
    Disk {
        size: stats.f_blocks.saturating_mul(fragment),
        free: stats.f_bavail.saturating_mul(fragment),
    }
}

/// The machine and process state that a command started in `cwd` by the calling
/// thread inherits: the kernel from `uname`, every resource limit from
/// `getrlimit`, the CPUs from `sched_getaffinity`, the memory from
/// `/proc/meminfo`, and the file system that holds `cwd` from `statfs`. A part
/// that cannot be read is `None`, such as the file system of a missing `cwd`.
pub(crate) fn machine_state(cwd: &Path) -> MachineState {
    let stats = file_system(cwd).ok();
    MachineState {
        kernel: kernel().ok(),
        limits: limits().ok(),
        affinity: affinity().ok(),
        memory: memory().ok(),
        filesystem: stats
            .as_ref()
            .map(|(stats, mount)| file_system_of(stats, mount)),
        disk: stats.as_ref().map(|(stats, _)| disk_of(stats)),
    }
}

/// Unit tests for the wrappers that need no traced process.
#[cfg(test)]
mod tests {
    use std::{
        os::{fd::AsRawFd, unix::fs::symlink},
        sync::mpsc,
        thread,
    };

    use super::*;

    #[test]
    fn pipe_ends_are_above_standard_error() {
        let (read, write) = pipe().unwrap();
        assert!(read.as_raw_fd() > libc::STDERR_FILENO);
        assert!(write.as_raw_fd() > libc::STDERR_FILENO);
    }

    #[test]
    fn stop_event_reads_the_event_byte() {
        // A seccomp event stop: event 7, signal 5 (`SIGTRAP`), stopped.
        assert_eq!(stop_event(0x0007_057F), libc::PTRACE_EVENT_SECCOMP);
        // A signal-delivery stop for signal 19 (`SIGSTOP`).
        assert_eq!(stop_event(0x137F), 0);
    }

    #[test]
    fn thread_group_is_the_process_of_every_thread() {
        let process = pid_t::try_from(std::process::id()).unwrap();
        assert_eq!(thread_group(process).unwrap(), process);
        let (sender, receiver) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let other = thread::spawn(move || {
            // SAFETY: `gettid` has no requirements.
            sender.send(unsafe { libc::gettid() }).unwrap();
            // Stay alive until the test has read this thread's status.
            released.recv().ok();
        });
        let tid = receiver.recv().unwrap();
        assert_ne!(tid, process);
        assert_eq!(thread_group(tid).unwrap(), process);
        drop(release);
        other.join().unwrap();
    }

    /// Set the extended attribute `name` of the file at `path` to `value`.
    fn set_attribute(
        path: &Path,
        name: &str,
        value: &[u8],
    ) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let name = CString::new(name).unwrap();
        // SAFETY: `path` and `name` are NUL-terminated strings, and `value` holds
        // `value.len()` bytes, which the kernel only reads.
        let ret = unsafe {
            libc::setxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        assert_eq!(ret, 0, "{}", io::Error::last_os_error());
    }

    /// The query reads every attribute of a file with its value, sorted by name,
    /// an empty value and a large one included; through a symbolic link it reads
    /// those of the file only when it follows the link; and a missing path fails
    /// with `ENOENT`.
    #[test]
    fn extended_attributes_are_read_with_their_values() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        fs::write(&file, "content").unwrap();
        let large = vec![b'x'; 3_000];
        set_attribute(&file, "user.b", b"2");
        set_attribute(&file, "user.a", b"");
        set_attribute(&file, "user.large", &large);
        let link = dir.path().join("link");
        symlink("file", &link).unwrap();

        let read = extended_attributes(&file, false).unwrap();
        let user: Vec<(&[u8], &[u8])> = read
            .iter()
            .filter(|(name, _)| name.starts_with(b"user."))
            .map(|(name, value)| (name.as_slice(), value.as_slice()))
            .collect();
        assert_eq!(user, [
            (b"user.a".as_slice(), b"".as_slice()),
            (b"user.b".as_slice(), b"2".as_slice()),
            (b"user.large".as_slice(), large.as_slice()),
        ]);
        assert_eq!(extended_attributes(&link, true).unwrap(), read);
        let own = extended_attributes(&link, false).unwrap();
        assert!(
            own.keys().all(|name| !name.starts_with(b"user.")),
            "{own:?}"
        );
        let missing = extended_attributes(&dir.path().join("missing"), true).unwrap_err();
        assert_eq!(missing.raw_os_error(), Some(libc::ENOENT));
    }

    /// The text of a field of `uname`, read by the test itself.
    fn uname_text(field: &[libc::c_char]) -> String {
        // SAFETY: `uname` ends every field with a zero byte within the field.
        unsafe { CStr::from_ptr(field.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }

    /// The CPUs in the `Cpus_allowed_list` line of the calling thread's status, as
    /// the kernel lists them, such as `0-3,6`.
    fn allowed_cpus() -> Vec<u32> {
        let status = fs::read_to_string("/proc/thread-self/status").unwrap();
        let allowed = status
            .lines()
            .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
            .unwrap()
            .trim();
        let mut cpus = Vec::new();
        for range in allowed.split(',') {
            let (first, last) = range.split_once('-').unwrap_or((range, range));
            cpus.extend(first.parse::<u32>().unwrap()..=last.parse::<u32>().unwrap());
        }
        cpus
    }

    /// The type, mount flags and size of the file system of `dir`, which the test
    /// reads itself through `libc` with `fstatfs` and `fstatvfs`.
    fn own_file_system(dir: &Path) -> (FileSystem, u64) {
        let opened = fs::File::open(dir).unwrap();
        let mut system = MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: the descriptor is open, and the pointer is valid for writing a
        // `statfs`.
        assert_eq!(
            unsafe { libc::fstatfs(opened.as_raw_fd(), system.as_mut_ptr()) },
            0
        );
        // SAFETY: `fstatfs` succeeded, so it initialized `system`.
        let system = unsafe { system.assume_init() };
        let mut mount = MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: the descriptor is open, and the pointer is valid for writing a
        // `statvfs`.
        assert_eq!(
            unsafe { libc::fstatvfs(opened.as_raw_fd(), mount.as_mut_ptr()) },
            0
        );
        // SAFETY: `fstatvfs` succeeded, so it initialized `mount`.
        let mount = unsafe { mount.assume_init() };
        let fragment = u64::try_from(system.f_frsize).unwrap();
        (
            FileSystem {
                kind: u64::from_ne_bytes(system.f_type.to_ne_bytes()),
                flags: mount.f_flag,
            },
            system.f_blocks.checked_mul(fragment).unwrap(),
        )
    }

    /// The machine query of the tracer reports what the test reads itself through
    /// `libc`: the release and version from `uname`, each resource limit from
    /// `prlimit`, as many CPUs as `sched_getaffinity` sets, the very CPUs the kernel
    /// lists for the thread, the total memory from `sysinfo`, and the type, flags
    /// and size of the file system of the working directory from `fstatfs` and
    /// `fstatvfs`.
    #[test]
    fn the_machine_state_is_what_libc_reports() {
        use kizz_trace::Tracer as _;

        let dir = tempfile::tempdir().unwrap();
        let queried = crate::LinuxTracer.machine_state(dir.path());

        let mut names = MaybeUninit::<libc::utsname>::uninit();
        // SAFETY: the pointer is valid for writing a `utsname`.
        assert_eq!(unsafe { libc::uname(names.as_mut_ptr()) }, 0);
        // SAFETY: `uname` succeeded, so it initialized `names`.
        let names = unsafe { names.assume_init() };
        assert_eq!(
            queried.kernel,
            Some(Kernel {
                release: uname_text(&names.release),
                version: uname_text(&names.version),
            })
        );

        let limits = queried.limits.unwrap();
        assert_eq!(limits.len(), RESOURCES.len());
        for (limit, &(name, resource)) in limits.iter().zip(&RESOURCES) {
            let mut read = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            // SAFETY: a null new limit changes nothing, and `read` is a valid,
            // writable `rlimit`.
            let ret = unsafe { libc::prlimit(0, resource, ptr::null(), &raw mut read) };
            assert_eq!(ret, 0, "{name}: {}", io::Error::last_os_error());
            assert_eq!(limit, &ResourceLimit {
                resource: name.to_owned(),
                soft: (read.rlim_cur != libc::RLIM_INFINITY).then_some(read.rlim_cur),
                hard: (read.rlim_max != libc::RLIM_INFINITY).then_some(read.rlim_max),
            });
        }

        let mut mask = [0_u64; AFFINITY_WORDS];
        // SAFETY: `mask` is as large and as aligned as a `cpu_set_t`, and the call
        // writes no more than the size passed.
        let ret = unsafe {
            libc::sched_getaffinity(0, mem::size_of_val(&mask), mask.as_mut_ptr().cast())
        };
        assert_eq!(ret, 0, "{}", io::Error::last_os_error());
        let set: u32 = mask.iter().map(|word| word.count_ones()).sum();
        let affinity = queried.affinity.unwrap();
        assert_ne!(set, 0);
        assert_eq!(u32::try_from(affinity.len()).unwrap(), set);
        assert_eq!(affinity, allowed_cpus());

        let mut info = MaybeUninit::<libc::sysinfo>::uninit();
        // SAFETY: the pointer is valid for writing a `sysinfo`.
        assert_eq!(unsafe { libc::sysinfo(info.as_mut_ptr()) }, 0);
        // SAFETY: `sysinfo` succeeded, so it initialized `info`.
        let info = unsafe { info.assume_init() };
        let memory = queried.memory.unwrap();
        assert_eq!(
            Some(memory.total),
            info.totalram.checked_mul(u64::from(info.mem_unit))
        );
        assert!(memory.available > 0 && memory.available <= memory.total);

        let (file_system, size) = own_file_system(dir.path());
        assert_eq!(queried.filesystem, Some(file_system));
        let disk = queried.disk.unwrap();
        assert_eq!(disk.size, size);
        assert!(disk.size > 0 && disk.free <= disk.size);
    }

    /// A working directory that does not exist has no file system, and every other
    /// part is still read.
    #[test]
    fn a_missing_working_directory_has_no_file_system() {
        let dir = tempfile::tempdir().unwrap();
        let state = machine_state(&dir.path().join("missing"));
        assert_eq!((state.filesystem, state.disk), (None, None));
        assert!(state.kernel.is_some() && state.limits.is_some());
        assert!(state.affinity.is_some() && state.memory.is_some());
    }

    /// `/proc/meminfo` lines are read in kibibytes and reported in bytes, and a
    /// missing line is an error.
    #[test]
    fn meminfo_lines_are_read_in_bytes() {
        let text = "MemTotal:       16 kB\nMemFree: 1 kB\nMemAvailable:    8 kB\n";
        assert_eq!(meminfo_field(text, "MemTotal").unwrap(), 16_384);
        assert_eq!(meminfo_field(text, "MemAvailable").unwrap(), 8_192);
        let missing = meminfo_field("MemTotal: 1 kB\n", "MemAvailable").unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::InvalidData);
    }
}
