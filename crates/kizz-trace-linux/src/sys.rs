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
    fs, io, mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStringExt as _,
            fs::{FileExt as _, MetadataExt as _, OpenOptionsExt as _},
        },
    },
    path::Path,
    ptr,
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

/// Unit tests for the wrappers that need no traced process.
#[cfg(test)]
mod tests {
    use std::{os::fd::AsRawFd, sync::mpsc, thread};

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
}
