// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The event loop that follows a traced process tree until all of it has exited.
//!
//! The loop runs on a thread that each run creates only to start and trace its
//! tree. Linux makes that thread the tracer of every process and thread in the
//! tree, and its waits report only its own children and tracees. Its only child is
//! the tree's first process, so every state change it waits for belongs to the
//! tree: no other child of the process, whichever thread started it, is ever
//! collected, signaled or adopted, and tracers on other threads never see each
//! other's events.
//!
//! The tree has ended when the thread has nothing left to wait for. A process or
//! thread whose creation the loop has not seen yet is still a tracee of the thread,
//! so the loop keeps waiting for it, and kills it after the escalation, before it
//! ends.
//!
//! # Cancellation
//!
//! When the request is cancelled, the cancellation watchdog interrupts the tree: it
//! sends `SIGINT` once to every live process. For a grace period the loop then
//! follows the tree as it does without cancellation: it delivers every signal,
//! `SIGINT` included, records every access, and follows the processes and threads
//! the tree creates, for example from a handler. When the grace period ends with
//! some of the tree still running, the watchdog escalates: it sets the
//! [`Escalation`] flag and sends `SIGKILL` to every live process, and from then on
//! the loop kills each thread at its next stop and each new thread at once.
//!
//! # Signals
//!
//! A thread that is about to receive a signal stops first, and the loop resumes it
//! with the signal. The loop handles such a stop while it is still in place,
//! without collecting it: a collected stop no longer holds the signal, so a thread
//! killed before the loop resumed it would lose the signal, and its process would
//! go on without it. That happens to every other thread of a process when one of
//! them runs `exec`, which kills them, and the process lives on in the new program.
//! A thread killed in a stop that is still in place queues its signal again for its
//! process, so the interrupt that cancellation sends reaches the process even when
//! the thread that took it dies in another thread's `exec`. Event stops and stops
//! at the exit of a system call hold no signal and are collected before the loop
//! handles them: Linux refuses every request for a thread that stopped at an
//! `exec` under an ID it took over until that stop is collected.
//!
//! A stop signal, such as `SIGSTOP`, or `SIGTSTP` from a terminal's Ctrl-Z, that
//! the loop delivers puts the thread's process into a group stop, which reports
//! the stop once more, with no signal information. The loop resumes a thread from
//! a group stop at once, with no signal, so the process runs on. A tracer that
//! attaches with `PTRACE_TRACEME` cannot keep a group stop and still learn when
//! `SIGCONT` ends it; only `PTRACE_SEIZE` with `PTRACE_LISTEN` can, which would
//! change how every traced process starts. So the limit stands: a Ctrl-Z stops the
//! tracer's process, which keeps the default action of `SIGTSTP`, `SIGTTIN` and
//! `SIGTTOU`, and each traced process then waits at its next stop for the loop,
//! while a stop signal sent to one traced process alone does not keep it stopped.
//!
//! # Forwarded signals
//!
//! Until the request is cancelled, the watchdog also takes the signals that the
//! caller forwards, every few milliseconds, and delivers each once: to the first
//! process until the loop has collected its end, and afterwards to every live
//! process ([`signal_first`]). The live set knows the first process by its
//! identity, not by its ID: a later process of the tree that Linux gives the first
//! process's ID is a live process like any other. The loop then delivers such a
//! signal as any other. A request that the caller forwards nothing to receives no
//! signal from the tracer until it is cancelled.
//!
//! The command's termination is likewise the first process's own end, never the
//! end of a later process under its ID, as the loop's handling of exits says.
//!
//! # Which IDs are signaled
//!
//! Linux gives a released process or thread ID to the next process or thread it
//! creates, so the tracer signals an ID only while it can prove the ID still names
//! a thread of its own tree. Holding a lock cannot prove that for a thread ID: a
//! thread that is not its process's leader and runs `exec` takes over the leader's
//! ID, and the kernel releases its former ID at once, without an exit for the loop
//! to collect and before the loop sees the `exec` event.
//!
//! The loop therefore shares with the cancellation watchdog, in a mutex, the set of
//! live processes of the tree, by process ID, and never the ID of a thread that
//! does not lead its process. A process ID stays with the process while the loop
//! has not collected the leader's exit: Linux keeps an exited leader until every
//! other thread of its process has exited, and an `exec` by another thread hands
//! that thread the same ID. The loop collects a leader's exit only while it holds
//! the mutex, and removes the ID at the same time, so every ID in the set names a
//! process of the tree, and the watchdog's `SIGINT` and `SIGKILL`, which are sent
//! to a whole process, never reach another process.
//!
//! The loop itself signals a thread by its own ID only when the thread cannot run
//! `exec`: it is in a `ptrace` stop that the loop has seen and not resumed, or
//! it was just created and has not run yet.
//!
//! # System calls
//!
//! A tracked system call stops its thread twice. At the seccomp stop before the
//! call runs, the loop decodes the call's paths from the thread, and resumes it
//! with `PTRACE_SYSCALL`, so the thread stops again when the call returns. At that
//! stop the loop reads the result and records the paths in the outcome's accesses.
//! Before it records them, it notes the first-read metadata of each regular file
//! that the call opened for reading or ran, unless the run wrote that path earlier
//! or already noted it (see [`decode::Call::first_reads`]).
//! A call that a signal interrupts reaches that stop too, with `EINTR` or one of the
//! kernel's internal restart errors, and is recorded like a failed call, so a call
//! that returns `EINTR` without restarting keeps its lookup; a call that restarts
//! stops at its entry again and is decoded again.
//! A thread that runs `exec` from a thread that does not lead its process keeps its
//! call under the ID it takes over, and a thread that exits inside a call records
//! the call with its result unknown.
//!
//! A call that the filter stops for because it can make the trace incomplete, one
//! of the untraceable calls of [`filter`], stops its thread once, at its entry: the
//! loop asks [`decode::untraceable`] whether the call with its arguments marks the
//! run, marks it with the process and the call unless it is already marked, and
//! lets the call run. A native call that the tracer does not know stops once too:
//! the loop notes its number, as the filter saw it, and lets it run, so the trace
//! stays complete.
//!
//! A socket call that connects or sends to a destination adds, when it returns,
//! the connections that [`decode::Call::connections`] gives to the outcome.
//!
//! # The process tree
//!
//! When the request asks for the process tree, the loop also keeps the
//! bookkeeping of [`Processes`]: it tells it each creation event, each first stop
//! of a thread it did not follow yet, each `exec` event and each exit. At the entry
//! of a tracked call it notes the program that the thread's process runs, and for
//! `execve` and `execveat` it reads the argument vector; at the `exec` event the
//! call's program becomes the one that the call starts. When the call returns, its
//! paths go into the run's accesses exactly as without the tree, and into the
//! observations of that program, each kind with the call's outcome. Connections,
//! unknown system calls and every reason that the trace is incomplete go to the
//! program that the thread's process runs, while the run keeps the first reason
//! only. Without the tree, the loop does none of this.
//!
//! The live set that the loop shares with the cancellation watchdog is locked with
//! `kizz_threads::lock`, never with `Mutex::lock`, which may panic when the calling
//! thread holds the lock ("Kizz never panics"); a set that stays locked is an error
//! of the loop or of the watchdog.
//!
//! # The cleanup of last resort
//!
//! Every process and thread of the tree is a tracee of the tracing thread, with
//! `PTRACE_O_EXITKILL`, so when that thread ends, Linux sends `SIGKILL` to every
//! one of them: also to one in a stop that the loop already collected, which no
//! wait reports again, to one that left the first process's process group or
//! session, and to one whose creation the loop never saw. That needs no live set,
//! no `/proc`, and no process or thread ID, so no signal can reach a process that
//! took over a released ID. So when the loop cannot go on, because a wait, a
//! `ptrace` request or the live set fails, it returns its error, and the tracing
//! thread ends with it, which kills the tree; the tree's pipes then close, the
//! threads that read and feed them end, and the caller collects the first process
//! through its process descriptor. When the watchdog cannot lock the live set to
//! signal the tree, it asks the loop to end, with [`Escalation::end`], which wakes
//! the tracing thread from its wait.

use std::{
    collections::{BTreeMap, BTreeSet, btree_map, btree_set},
    ffi::OsString,
    io, mem,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use kizz_trace::{
    Accesses, Connection, FirstReads, IncompleteReason, ProcessTree, RunRequest, Termination,
};
use libc::{c_int, c_long, c_ulong, pid_t};

use crate::{
    decode, filter,
    processes::{Creation, Processes, ProgramId},
    sys,
};

/// The signal of a stop at the exit of a system call, with
/// `PTRACE_O_TRACESYSGOOD`: `SIGTRAP` with the high bit set.
const SYSTEM_CALL_STOP: c_int = libc::SIGTRAP | 0x80;

/// The live processes of a traced tree, by process ID, which the loop shares with
/// the cancellation watchdog in [`LiveProcesses`]. It never holds the ID of a
/// thread that does not lead its process.
///
/// It also knows the tree's first process by its identity, not by its number:
/// from the time the first process enters the set until the loop collects its end.
/// Linux may then give the first process's ID to a later process of the tree,
/// which enters the set under that ID but is never the first process.
#[derive(Debug, Default)]
pub(crate) struct Live {
    /// The process IDs of the live processes.
    processes: BTreeSet<pid_t>,
    /// The process ID of the first process while the loop has not collected its
    /// end, and `None` before it enters the set and once its end is collected.
    first: Option<pid_t>,
}

impl Live {
    /// Bring the tree's first process, `pid`, into the set, as its first process.
    fn enter_first(
        &mut self,
        pid: pid_t,
    ) {
        self.processes.insert(pid);
        self.first = Some(pid);
    }

    /// Bring the process `pid`, a later process of the tree, into the set; it is
    /// never the first process, even under the first process's former ID.
    fn enter(
        &mut self,
        pid: pid_t,
    ) {
        self.processes.insert(pid);
    }

    /// Note that the loop collected the end of the process `pid`: it leaves the
    /// set, and when it is the first process, the set knows no first process from
    /// now on, so a later process under the same ID is never taken for it.
    fn ended(
        &mut self,
        pid: pid_t,
    ) {
        self.processes.remove(&pid);
        if self.first == Some(pid) {
            self.first = None;
        }
    }

    /// The process ID of the first process, while the loop has not collected its
    /// end.
    const fn first(&self) -> Option<pid_t> {
        self.first
    }

    /// The process IDs of the live processes, in order.
    fn iter(&self) -> btree_set::Iter<'_, pid_t> {
        self.processes.iter()
    }

    /// Forget every process, the first process included, once the loop has
    /// collected every thread of the tree.
    fn clear(&mut self) {
        self.processes.clear();
        self.first = None;
    }

    /// Whether the process `pid` is live.
    #[cfg(test)]
    fn contains(
        &self,
        pid: pid_t,
    ) -> bool {
        self.processes.contains(&pid)
    }

    /// Whether no process is live.
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.processes.is_empty()
    }
}

/// The live processes of a traced tree, shared with the cancellation watchdog.
pub(crate) type LiveProcesses = Mutex<Live>;

/// What the live set holds, as a lock error names it.
const LIVE_SET: &str = "the live processes of the traced tree";

/// Lock the live processes, recovering the set if a thread panicked while holding
/// the lock, with `kizz_threads::lock`, never `Mutex::lock`, which may panic when
/// the calling thread holds the lock ("Kizz never panics").
///
/// # Errors
///
/// An error that names the set when it stays locked for
/// [`kizz_threads::LOCK_PATIENCE`].
pub(crate) fn lock(live: &LiveProcesses) -> io::Result<MutexGuard<'_, Live>> {
    lock_within(live, kizz_threads::LOCK_PATIENCE)
}

/// [`lock`], waiting at most `patience` while another holder keeps the set.
///
/// # Errors
///
/// An error that names the set when it stays locked for `patience`.
fn lock_within(
    live: &LiveProcesses,
    patience: Duration,
) -> io::Result<MutexGuard<'_, Live>> {
    kizz_threads::lock_within(live, LIVE_SET, patience).map_err(io::Error::other)
}

/// Whether the grace period after cancellation has ended with some of the tree
/// still running, so the tracer kills the tree, and whether the watchdog asked the
/// tracing thread to end the run. Shared by the event loop and the cancellation
/// watchdog, which sets it.
#[derive(Debug)]
pub(crate) struct Escalation {
    /// Whether the tree is being killed.
    escalated: AtomicBool,
    /// Whether the watchdog asked the tracing thread to end the run.
    ended: AtomicBool,
    /// The tracing thread, which [`Escalation::end`] wakes from its wait.
    tracer: libc::pthread_t,
}

impl Escalation {
    /// A flag that is not set, for the tree that the calling thread traces: the
    /// tree is not being killed. The calling thread becomes one that
    /// [`Escalation::end`] can wake from a wait.
    ///
    /// # Errors
    ///
    /// The error of preparing the calling thread to be woken; without it, the
    /// watchdog could not end a loop that waits.
    pub(crate) fn new() -> io::Result<Self> {
        Ok(Self {
            escalated: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            tracer: sys::prepare_wake()?,
        })
    }

    /// Note that the tree is being killed. Set it before the live processes are
    /// killed, so a process that the loop adds to the live set after they were
    /// killed is killed by the loop.
    pub(crate) fn escalate(&self) {
        self.escalated.store(true, Ordering::SeqCst);
    }

    /// Whether the tree is being killed.
    pub(crate) fn is_escalated(&self) -> bool {
        self.escalated.load(Ordering::SeqCst)
    }

    /// Ask the tracing thread to end the run, for a watchdog that cannot lock the
    /// live set to signal the tree: note it, with the escalation, and wake the
    /// tracing thread from the wait it may be in. The loop then returns at its next
    /// wait, or at once from the one it is in, and the tracing thread ends, which
    /// kills the tree (see the module's "The cleanup of last resort").
    ///
    /// A wake that comes just before the thread starts to wait is not lost: the
    /// wait asks [`Escalation::is_ended`] before it waits. The caller must not
    /// outlive the tracing thread, as the watchdog, which the tracing thread waits
    /// for, does not.
    ///
    /// # Errors
    ///
    /// The error of the wake; the request stays noted.
    pub(crate) fn end(&self) -> io::Result<()> {
        self.escalate();
        self.ended.store(true, Ordering::SeqCst);
        sys::wake(self.tracer)
    }

    /// Whether the watchdog asked the tracing thread to end the run.
    pub(crate) fn is_ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }
}

/// Send `signal` to every live process, waiting at most `patience` for the live
/// set.
///
/// The set stays locked while the signals are sent, so the loop cannot collect the
/// exit of a process in between, and every ID still names a process of the tree.
///
/// # Errors
///
/// The error of a set that stays locked for `patience`, when no signal is sent.
pub(crate) fn signal_all(
    live: &LiveProcesses,
    signal: c_int,
    patience: Duration,
) -> io::Result<()> {
    for &pid in lock_within(live, patience)?.iter() {
        // A process that already exited needs no signal.
        drop(sys::kill(pid, signal));
    }
    Ok(())
}

/// Deliver a forwarded `signal` once: to the tree's first process while the live
/// set knows it, and otherwise to every live process, waiting at most `patience`
/// for the live set.
///
/// The set knows the first process by its identity, from the time it enters the
/// set until the loop collects its end (see [`Live`]), never by an ID that a
/// later process of the tree may hold again: once the first process's end is
/// collected, the signal goes to every live process, also to a later process
/// under the first process's former ID, as to any other.
///
/// The set stays locked while the signal is sent, so, as with [`signal_all`], the
/// loop cannot collect the exit of a process in between, and every ID signaled
/// still names a process of the tree.
///
/// # Errors
///
/// The error of a set that stays locked for `patience`, when no signal is sent.
pub(crate) fn signal_first(
    live: &LiveProcesses,
    signal: c_int,
    patience: Duration,
) -> io::Result<()> {
    let set = lock_within(live, patience)?;
    if let Some(first) = set.first() {
        // A process that already exited needs no signal.
        drop(sys::kill(first, signal));
    } else {
        for &pid in set.iter() {
            drop(sys::kill(pid, signal));
        }
    }
    Ok(())
}

/// Interrupt the tree after cancellation: send `SIGINT` once to every live process,
/// under the rules of [`signal_all`], waiting at most
/// [`kizz_threads::LOCK_PATIENCE`]. The watchdog calls [`signal_all`] with its own
/// patience; the tests that hand an interrupt to the loop by hand call this.
///
/// # Errors
///
/// The error of a set that stays locked, when no signal is sent.
#[cfg(test)]
pub(crate) fn interrupt_all(live: &LiveProcesses) -> io::Result<()> {
    signal_all(live, libc::SIGINT, kizz_threads::LOCK_PATIENCE)
}

/// Send `SIGKILL` to every live process, which kills all of its threads, under the
/// rules of [`signal_all`].
///
/// # Errors
///
/// The error of a set that stays locked, when no signal is sent.
pub(crate) fn kill_all(live: &LiveProcesses) -> io::Result<()> {
    signal_all(live, libc::SIGKILL, kizz_threads::LOCK_PATIENCE)
}

/// Whether a traced thread leads its process, so its ID is the process ID.
///
/// The thread must be one the loop has not resumed since it was created, or one in
/// a `ptrace` stop, so its ID cannot have been released. When the answer cannot be
/// read, the thread is taken not to lead, so the watchdog never signals its ID; the
/// loop still kills it at its next stop after the escalation.
fn leads_its_process(tid: pid_t) -> bool {
    sys::thread_group(tid).is_ok_and(|process| process == tid)
}

/// What the loop observed about a tree that has exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Outcome {
    /// How the first process ended, if the loop saw it end.
    pub termination: Option<Termination>,
    /// Every path the tree touched, with its combined access kinds.
    pub accesses: Accesses,
    /// Why the trace is incomplete, if it is.
    pub incomplete: Option<IncompleteReason>,
    /// The first-read metadata of the regular files the tree read or ran before it
    /// wrote them.
    pub first_reads: FirstReads,
    /// The numbers of the native system calls outside the known table that the
    /// tree made, as the filter saw them.
    pub unknown_system_calls: BTreeSet<u32>,
    /// The connections that the tree made, sorted and without repeats.
    pub connections: BTreeSet<Connection>,
    /// The process tree, when the request asked for it.
    pub tree: Option<ProcessTree>,
}

/// A tracked system call that a thread is in, decoded at its entry, until the loop
/// reads its result.
#[derive(Debug)]
struct Pending {
    /// The decoded call.
    call: decode::Call,
    /// With the process tree, the program that the call belongs to: the one that
    /// the thread's process ran at the entry, or the one that a successful `exec`
    /// started.
    caller: Option<ProgramId>,
    /// With the process tree, the argument vector of an `execve` or `execveat`,
    /// read at the entry.
    arguments: Vec<OsString>,
}

/// What the loop knows about one traced thread.
#[derive(Debug)]
struct Tracee {
    /// Whether the thread still has to report the `SIGSTOP` with which every
    /// automatically traced thread starts.
    awaiting_initial_stop: bool,
    /// Whether the loop has seen the event of the thread that created this one.
    /// A new thread can report its first stop, and even its exit, before that
    /// event.
    announced: bool,
    /// The tracked system call the thread is in, until the loop reads its result.
    pending: Option<Pending>,
}

impl Tracee {
    /// A thread that is in no tracked system call.
    const fn new(
        awaiting_initial_stop: bool,
        announced: bool,
    ) -> Self {
        Self {
            awaiting_initial_stop,
            announced,
            pending: None,
        }
    }
}

/// The state of the loop.
struct Tree<'a> {
    /// The first process's ID, under which its end is the command's termination.
    root: pid_t,
    /// Whether the loop has handled the first process's own end. Linux may then
    /// give its ID to a later process of the tree, whose end under that ID is
    /// never the command's termination.
    root_ended: bool,
    /// Every traced thread that has not exited.
    tracees: BTreeMap<pid_t, Tracee>,
    /// The threads that exited before the loop saw the event that created them, so
    /// that event, when it arrives, does not follow a thread that is gone.
    exited_unannounced: BTreeSet<pid_t>,
    /// The processes of the threads in `tracees`, by the IDs of the threads that
    /// lead them, shared with the cancellation watchdog.
    live: &'a LiveProcesses,
    /// Whether the grace period after cancellation has ended, so the tree is
    /// being killed.
    escalation: &'a Escalation,
    /// Whether the loop has already killed every live process after the
    /// escalation.
    killed: bool,
    /// What the loop has observed so far.
    outcome: Outcome,
    /// The bookkeeping of the processes and programs, when the request asked for
    /// the process tree.
    processes: Option<Processes>,
}

/// Whether an error means the thread no longer exists, which happens when it was
/// killed while the loop handled it; its exit arrives later.
fn vanished(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ESRCH)
}

/// Ignore an error that only means the thread vanished.
fn unless_vanished(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if !vanished(&error) => Err(error),
        Ok(()) | Err(_) => Ok(()),
    }
}

/// A thread ID from a `ptrace` event message.
fn message_pid(message: c_ulong) -> io::Result<pid_t> {
    pid_t::try_from(message).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Whether a wait status reports the end of a thread.
fn ended(status: c_int) -> bool {
    libc::WIFEXITED(status) || libc::WIFSIGNALED(status)
}

/// A state change that the loop takes: the thread, and its wait status, or `None`
/// when the change was gone by the time it was collected.
type Change = (pid_t, Option<c_int>);

/// Whether a stop stays in place while the loop handles it: a stop for a signal,
/// either a signal-delivery stop or a group stop, which holds the signal until the
/// loop resumes the thread. A `ptrace` event stop and a stop at the exit of a system
/// call hold none and are collected.
fn stays_in_place(status: c_int) -> bool {
    sys::stop_event(status) == 0 && libc::WSTOPSIG(status) != SYSTEM_CALL_STOP
}

/// Take a change of the thread `pid` that a wait reported: leave a stop for a
/// signal in place, and collect any other change, and return the wait status.
///
/// An exit is collected while the live set is locked. When the thread leads its
/// process, the process has ended and leaves the set at the same time; any other
/// thread's ID was never in the set. When that process is the first process, the
/// set stops knowing it as the first process at the same time, so the watchdog
/// never takes a later process under its ID for it.
fn take(
    live: &LiveProcesses,
    pid: pid_t,
    pending: sys::Pending,
) -> io::Result<Option<c_int>> {
    if let sys::Pending::Stopped(status) = pending
        && stays_in_place(status)
    {
        return Ok(Some(status));
    }
    let mut live = lock(live)?;
    let status = sys::collect(pid)?;
    if let Some(status) = status
        && ended(status)
    {
        live.ended(pid);
    }
    Ok(status)
}

/// Take the next state change of a thread of the tree, or return `None` when the
/// calling thread has nothing left to wait for, which means that every process and
/// thread of the tree has exited and been collected.
///
/// # Errors
///
/// The error of the wait or of the set, or [`sys::stopped_waiting`] once the
/// watchdog asked the tracing thread to end the run, before the wait or during it.
fn next_change(
    live: &LiveProcesses,
    escalation: &Escalation,
) -> io::Result<Option<Change>> {
    let Some((pid, pending)) = sys::wait_pending(&|| escalation.is_ended())? else {
        return Ok(None);
    };
    Ok(Some((pid, take(live, pid, pending)?)))
}

impl<'a> Tree<'a> {
    /// The state of a tree whose first process is `root`, already resumed, which
    /// enters the live set as its first process.
    fn new(
        root: pid_t,
        live: &'a LiveProcesses,
        escalation: &'a Escalation,
    ) -> io::Result<Self> {
        lock(live)?.enter_first(root);
        Ok(Self {
            root,
            root_ended: false,
            tracees: BTreeMap::from([(root, Tracee::new(false, true))]),
            exited_unannounced: BTreeSet::new(),
            live,
            escalation,
            killed: false,
            outcome: Outcome {
                termination: None,
                accesses: Accesses::new(),
                incomplete: None,
                first_reads: FirstReads::new(),
                unknown_system_calls: BTreeSet::new(),
                connections: BTreeSet::new(),
                tree: None,
            },
            processes: None,
        })
    }

    /// Keep the bookkeeping of the processes and programs of the tree, whose first
    /// process runs `request`, for the process tree.
    fn keep_processes(
        &mut self,
        request: &RunRequest,
    ) {
        self.processes = Some(Processes::new(
            self.root,
            request.program.clone(),
            request.args.clone(),
        ));
    }

    /// The program that the process of the thread `pid` runs now, with the process
    /// tree.
    fn current(
        &self,
        pid: pid_t,
    ) -> Option<ProgramId> {
        self.processes
            .as_ref()
            .and_then(|processes| processes.current(pid))
    }

    /// Start following a thread, unless it is already followed. A thread that
    /// leads its process brings the process into the live set, never as the first
    /// process, even under the first process's former ID.
    fn adopt(
        &mut self,
        pid: pid_t,
        tracee: Tracee,
        leader: bool,
    ) -> io::Result<()> {
        if let btree_map::Entry::Vacant(entry) = self.tracees.entry(pid) {
            entry.insert(tracee);
            if leader {
                lock(self.live)?.enter(pid);
            }
        }
        Ok(())
    }

    /// Stop following a thread that no longer exists, and return the tracked call
    /// it was in.
    ///
    /// Only the former ID of a thread that ran `exec` disappears this way, and
    /// that thread did not lead its process, so its ID was never in the live set.
    fn forget(
        &mut self,
        pid: pid_t,
    ) -> Option<Pending> {
        self.tracees.remove(&pid).and_then(|tracee| tracee.pending)
    }

    /// Note why the trace is incomplete, unless an earlier reason was noted, and
    /// with the process tree, attribute the reason to `program`, the program that
    /// caused it.
    fn incomplete(
        &mut self,
        program: Option<ProgramId>,
        reason: IncompleteReason,
    ) {
        if let (Some(processes), Some(program)) = (self.processes.as_mut(), program) {
            processes.incomplete(program, reason.clone());
        }
        if self.outcome.incomplete.is_none() {
            self.outcome.incomplete = Some(reason);
        }
    }

    /// Record the paths of a tracked call of the thread `pid` that returned
    /// `result`, or whose result was never seen, with what the kernel loaded to run
    /// the program of an `exec`, and the connections of a socket call; with the
    /// process tree, also attribute them, each kind with the call's outcome, and a
    /// reason that the call makes the trace incomplete, to the program of the call.
    ///
    /// The first-read metadata of the files the call opened for reading or ran is
    /// noted first, while the accesses hold only the calls that returned earlier,
    /// so a write by the call itself does not count as an earlier write.
    fn finish(
        &mut self,
        pid: pid_t,
        pending: Pending,
        result: Option<i64>,
    ) {
        let Pending {
            mut call, caller, ..
        } = pending;
        let thread = sys::Thread(pid);
        call.loaded(result, &thread);
        for (path, metadata) in call.first_reads(result, &thread) {
            self.outcome
                .first_reads
                .note(&path, metadata, &self.outcome.accesses);
        }
        let connections = call.connections(result);
        if let (Some(processes), Some(caller)) = (self.processes.as_mut(), caller) {
            processes.connect(caller, connections.iter().cloned());
        }
        self.outcome.connections.extend(connections);
        let observations = match (self.processes.as_mut(), caller) {
            (Some(processes), Some(caller)) => processes.observations(caller),
            _ => None,
        };
        if let Some(detail) = call.finish_observed(result, &mut self.outcome.accesses, observations)
        {
            self.incomplete(caller, IncompleteReason::Other { detail });
        }
    }

    /// Resume a stopped thread, delivering `signal` unless it is zero. A thread in
    /// a tracked call is resumed to the exit of the call.
    fn resume(
        &self,
        pid: pid_t,
        signal: c_int,
    ) -> io::Result<()> {
        let in_call = self
            .tracees
            .get(&pid)
            .is_some_and(|tracee| tracee.pending.is_some());
        unless_vanished(if in_call {
            sys::resume_to_exit(pid, signal)
        } else {
            sys::resume(pid, signal)
        })
    }

    /// Follow the tree until every thread in it has exited and been collected.
    ///
    /// The loop ends only when the calling thread has nothing left to wait for, so
    /// it also waits for threads whose creation it has not seen.
    fn run(&mut self) -> io::Result<()> {
        while let Some((pid, status)) = next_change(self.live, self.escalation)? {
            if let Some(status) = status {
                self.handle(pid, status)?;
            }
        }
        // Every thread has been collected; an ID left behind is one whose
        // disappearance the loop could not read, and must never be signaled.
        self.tracees.clear();
        lock(self.live)?.clear();
        Ok(())
    }

    /// Handle one wait status of a thread that the loop took.
    fn handle(
        &mut self,
        pid: pid_t,
        status: c_int,
    ) -> io::Result<()> {
        if ended(status) {
            self.exited(pid, status);
            Ok(())
        } else if libc::WIFSTOPPED(status) {
            self.stopped(pid, status)
        } else {
            Ok(())
        }
    }

    /// Handle the exit of a thread.
    ///
    /// The command's termination is the first process's own end, by its identity:
    /// the first exit under the first process's ID that the loop handles, which
    /// Linux reports only once every other thread of that process has exited.
    /// Linux may then give the ID to a later process of the tree, whose end never
    /// sets or replaces the termination, with or without the process tree; with
    /// it, that process keeps its own status, as every process does.
    fn exited(
        &mut self,
        pid: pid_t,
        status: c_int,
    ) {
        let tracee = self.tracees.remove(&pid);
        let announced = tracee.as_ref().is_some_and(|tracee| tracee.announced);
        if !announced {
            self.exited_unannounced.insert(pid);
        }
        if let Some(pending) = tracee.and_then(|tracee| tracee.pending) {
            // The thread ended inside the call, which may have taken effect.
            self.finish(pid, pending, None);
        }
        let first = pid == self.root && !self.root_ended;
        if first {
            self.root_ended = true;
        }
        // Without the process tree, only the first process's status is kept.
        if self.processes.is_none() && !first {
            return;
        }
        let termination = if libc::WIFEXITED(status) {
            Termination::Exited(libc::WEXITSTATUS(status))
        } else {
            Termination::Signaled(libc::WTERMSIG(status))
        };
        if let Some(processes) = self.processes.as_mut() {
            processes.exited(pid, termination);
        }
        if first {
            self.outcome.termination = Some(termination);
        }
    }

    /// Kill the process of a stopped thread after the escalation; the first time,
    /// kill every live process as well.
    ///
    /// The thread is in a stop that the loop has seen and not resumed, so it
    /// cannot have run `exec`, and its ID still names it.
    fn kill(
        &mut self,
        pid: pid_t,
    ) -> io::Result<()> {
        if !self.killed {
            self.killed = true;
            kill_all(self.live)?;
        }
        unless_vanished(sys::kill(pid, libc::SIGKILL))
    }

    /// Handle a `ptrace` stop of a thread.
    fn stopped(
        &mut self,
        pid: pid_t,
        status: c_int,
    ) -> io::Result<()> {
        let signal = libc::WSTOPSIG(status);
        let event = sys::stop_event(status);
        // A new thread or process can report its first stop before the event of
        // the thread that created it. It is stopped, so its ID still names it.
        if !self.tracees.contains_key(&pid) {
            let process = sys::thread_group(pid).ok();
            if let Some(processes) = self.processes.as_mut() {
                processes.first_stop(pid, process);
            }
            self.adopt(pid, Tracee::new(true, false), process == Some(pid))?;
        }
        if signal == libc::SIGTRAP && event != 0 {
            return self.event_stop(pid, event);
        }
        if self.escalation.is_escalated() {
            return self.kill(pid);
        }
        if signal == SYSTEM_CALL_STOP && event == 0 {
            return self.system_call_exit(pid);
        }
        if let Some(tracee) = self.tracees.get_mut(&pid)
            && tracee.awaiting_initial_stop
            && signal == libc::SIGSTOP
            && event == 0
        {
            tracee.awaiting_initial_stop = false;
            return unless_vanished(sys::resume(pid, 0));
        }
        // The stop is still in place, so a thread that vanished since it stopped,
        // killed by another thread's `exec` for example, queued the signal again
        // for its process, and the request fails without losing it. A thread that
        // took over the ID in its `exec` refuses the request too, until the loop
        // collects its `exec` stop.
        match sys::has_signal_info(pid) {
            // A signal-delivery stop: deliver the signal.
            Ok(true) => self.resume(pid, signal),
            // A group stop: let the thread continue, since with `PTRACE_TRACEME`
            // the loop could not learn when `SIGCONT` ends the stop (see the
            // module's "Signals").
            Ok(false) => self.resume(pid, 0),
            Err(error) => unless_vanished(Err(error)),
        }
    }

    /// Handle the stop at the exit of a tracked system call: read the result,
    /// record the call's paths, and let the thread continue.
    fn system_call_exit(
        &mut self,
        pid: pid_t,
    ) -> io::Result<()> {
        let pending = self
            .tracees
            .get_mut(&pid)
            .and_then(|tracee| tracee.pending.take());
        if let Some(mut pending) = pending {
            // Without the registers the result is unknown, so the call is recorded
            // as one that may or may not have taken effect.
            let result = sys::registers(pid)
                .ok()
                .map(|registers| i64::from_ne_bytes(registers.rax.to_ne_bytes()));
            // A descriptor the call returned names the object it actually opened.
            pending.call.opened(result, &sys::Thread(pid));
            self.finish(pid, pending, result);
        }
        unless_vanished(sys::resume(pid, 0))
    }

    /// Decode the tracked system call a thread is entering at a seccomp stop, and
    /// keep it until the call returns; with the process tree, also note the program
    /// that the thread's process runs, and the argument vector of an `exec`.
    fn system_call_entry(
        &mut self,
        pid: pid_t,
    ) {
        let registers = match sys::registers(pid) {
            Ok(registers) => registers,
            // A thread that vanished makes no call.
            Err(error) if vanished(&error) => return,
            Err(error) => {
                self.unreadable_registers(pid, &error);
                return;
            },
        };
        let thread = sys::Thread(pid);
        let Some((number, call)) = c_long::try_from(registers.orig_rax)
            .ok()
            .and_then(|number| {
                decode::enter(number, arguments(&registers), &thread).map(|call| (number, call))
            })
        else {
            return;
        };
        let (caller, vector) = if self.processes.is_some() {
            (
                self.current(pid),
                decode::argument_vector(number, arguments(&registers), &thread),
            )
        } else {
            (None, Vec::new())
        };
        let pending = Pending {
            call,
            caller,
            arguments: vector,
        };
        let earlier = self
            .tracees
            .get_mut(&pid)
            .and_then(|tracee| tracee.pending.replace(pending));
        if let Some(earlier) = earlier {
            // A call whose exit was never seen; it cannot happen when every stop
            // is handled, and is recorded as one whose result is unknown.
            self.finish(pid, earlier, None);
        }
    }

    /// Follow a thread or process that the thread `creator` just created at an
    /// `event`, and with the process tree, note the creation.
    ///
    /// After the escalation a new thread is killed at once: its first stop may
    /// still be pending, and nothing else would signal it before then. During the
    /// grace period after cancellation it runs, as it does without cancellation.
    fn created(
        &mut self,
        creator: pid_t,
        child: pid_t,
        event: c_int,
    ) -> io::Result<()> {
        if let Some(processes) = self.processes.as_mut() {
            // A thread that exited before both its first stop and this event left
            // nothing to attribute, and is not noted, so its ID names nothing.
            let early = processes.awaits_creation(child);
            if early || !self.exited_unannounced.contains(&child) {
                let how = match event {
                    libc::PTRACE_EVENT_FORK => Creation::Fork,
                    libc::PTRACE_EVENT_VFORK => Creation::Vfork,
                    _ => {
                        Creation::Clone {
                            thread: !early && !leads_its_process(child),
                        }
                    },
                };
                processes.created(creator, child, how);
            }
        }
        if self.exited_unannounced.remove(&child) {
            // It already exited, and its exit has been collected.
            return Ok(());
        }
        if let Some(tracee) = self.tracees.get_mut(&child) {
            // Its first stop came first and was handled: after the escalation it
            // was killed then; otherwise it may be running now, and only the live
            // set and its next stop may signal it.
            tracee.announced = true;
            return Ok(());
        }
        // A fork or vfork always starts a process; a clone starts a thread unless
        // it was asked for a process.
        let leader = event != libc::PTRACE_EVENT_CLONE || leads_its_process(child);
        self.adopt(child, Tracee::new(true, true), leader)?;
        if self.escalation.is_escalated() {
            // The new thread has not run since it was created, so it cannot have
            // run `exec`, and its ID still names it.
            return unless_vanished(sys::kill(child, libc::SIGKILL));
        }
        Ok(())
    }

    /// With the process tree, note the successful `exec` that the thread `former`
    /// made and that now runs under `pid`: the process runs the program that the
    /// call names, with the arguments that it passed, and the call belongs to it.
    fn exec_started(
        &mut self,
        pid: pid_t,
        former: pid_t,
    ) {
        let Some(processes) = self.processes.as_mut() else {
            return;
        };
        let pending = self
            .tracees
            .get_mut(&pid)
            .and_then(|tracee| tracee.pending.as_mut());
        let (path, vector) = pending.as_ref().map_or((None, Vec::new()), |pending| {
            (pending.call.program().cloned(), pending.arguments.clone())
        });
        let started = processes.exec(pid, former, path, vector);
        if let Some(pending) = pending {
            pending.caller = started;
        }
    }

    /// Handle a `ptrace` event stop, then resume the thread, or kill it after the
    /// escalation.
    ///
    /// The event is handled even after the escalation, so a thread or process
    /// created at this stop is followed and killed before the loop can end.
    fn event_stop(
        &mut self,
        pid: pid_t,
        event: c_int,
    ) -> io::Result<()> {
        let message = match sys::event_message(pid) {
            Ok(message) => message,
            // A thread that vanished cannot say what it created; a created thread
            // still reports its first stop, and the loop waits for it.
            Err(error) => return unless_vanished(Err(error)),
        };
        let escalated = self.escalation.is_escalated();
        match event {
            libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK | libc::PTRACE_EVENT_CLONE => {
                self.created(pid, message_pid(message)?, event)?;
            },
            libc::PTRACE_EVENT_EXEC => {
                // A thread other than the leader that runs `exec` takes over the
                // leader's ID, and its former ID disappears without an exit. The
                // live set never held that ID, so nothing has signaled it. The
                // `exec` call it is in continues under the leader's ID, and a call
                // the former leader was in ended with it.
                let former = message_pid(message)?;
                if former != pid {
                    let call = self.forget(former);
                    let ended = match self.tracees.get_mut(&pid) {
                        Some(tracee) => mem::replace(&mut tracee.pending, call),
                        None => call,
                    };
                    if let Some(ended) = ended {
                        self.finish(pid, ended, None);
                    }
                }
                self.exec_started(pid, former);
            },
            libc::PTRACE_EVENT_SECCOMP if !escalated => self.seccomp_stop(pid, message),
            _ => {},
        }
        if escalated {
            return self.kill(pid);
        }
        self.resume(pid, 0)
    }

    /// Mark the run as incompletely traced because the registers of thread `pid`
    /// could not be read at a system call, which the loop then cannot judge.
    fn unreadable_registers(
        &mut self,
        pid: pid_t,
        error: &io::Error,
    ) {
        self.incomplete(self.current(pid), IncompleteReason::Other {
            detail: format!(
                "could not read the registers of thread {pid} at a system call: {error}"
            ),
        });
    }

    /// Handle the entry of a call that can make the trace incomplete: mark the run
    /// with the process and the call when the call, with its arguments, marks it,
    /// unless the run is already marked and the request did not ask for the process
    /// tree, which lists every reason. The call then runs unchanged.
    fn untraceable_entry(
        &mut self,
        pid: pid_t,
    ) {
        if self.outcome.incomplete.is_some() && self.processes.is_none() {
            return;
        }
        let registers = match sys::registers(pid) {
            Ok(registers) => registers,
            // A thread that vanished makes no call.
            Err(error) if vanished(&error) => return,
            Err(error) => {
                self.unreadable_registers(pid, &error);
                return;
            },
        };
        let made = c_long::try_from(registers.orig_rax)
            .ok()
            .and_then(|number| {
                decode::untraceable(number, arguments(&registers), &sys::Thread(pid))
            });
        if let Some(made) = made {
            self.incomplete(self.current(pid), IncompleteReason::UntraceableSystemCall {
                detail: format!("process {pid} {made}"),
            });
        }
    }

    /// Handle the entry of a native call that the tracer does not know: note its
    /// number, the low 32 bits of the number register, as the filter saw it, and
    /// with the process tree, attribute it to the program of the thread's process.
    /// The call then runs unchanged, and the trace stays complete.
    fn unknown_entry(
        &mut self,
        pid: pid_t,
    ) {
        match sys::registers(pid) {
            Ok(registers) => {
                let low = registers.orig_rax & u64::from(u32::MAX);
                if let Ok(number) = u32::try_from(low) {
                    self.outcome.unknown_system_calls.insert(number);
                    if let (Some(program), Some(processes)) =
                        (self.current(pid), self.processes.as_mut())
                    {
                        processes.unknown(program, number);
                    }
                }
            },
            // A thread that vanished makes no call.
            Err(error) if vanished(&error) => {},
            Err(error) => self.unreadable_registers(pid, &error),
        }
    }

    /// Handle a seccomp stop with the filter's return data. A tracked call is
    /// decoded, and the thread is resumed to its exit. A call that can make the
    /// trace incomplete is judged, and a native call that the tracer does not know
    /// is noted, at its entry only. A non-native call marks the run as incompletely
    /// traced.
    fn seccomp_stop(
        &mut self,
        pid: pid_t,
        data: c_ulong,
    ) {
        let interface = match u16::try_from(data) {
            Ok(filter::DATA_TRACKED) => {
                self.system_call_entry(pid);
                return;
            },
            Ok(filter::DATA_UNTRACEABLE) => {
                self.untraceable_entry(pid);
                return;
            },
            Ok(filter::DATA_UNKNOWN) => {
                self.unknown_entry(pid);
                return;
            },
            Ok(filter::DATA_FOREIGN_ARCH) => "the 32-bit `int 0x80` interface",
            Ok(filter::DATA_X32) => "the x32 interface",
            Ok(_) | Err(_) => return,
        };
        if self.outcome.incomplete.is_some() && self.processes.is_none() {
            return;
        }
        let call = match sys::registers(pid) {
            Ok(registers) => {
                format!(
                    "system call {}",
                    registers.orig_rax & !c_ulong::from(filter::X32_SYSCALL_BIT)
                )
            },
            Err(_) => "a system call".to_owned(),
        };
        self.incomplete(self.current(pid), IncompleteReason::NonNativeSystemCall {
            detail: format!("process {pid} made {call} through {interface}"),
        });
    }
}

/// The six argument registers of a system call, in order, from the registers of a
/// thread stopped at the call.
const fn arguments(registers: &libc::user_regs_struct) -> decode::Arguments {
    [
        registers.rdi,
        registers.rsi,
        registers.rdx,
        registers.r10,
        registers.r8,
        registers.r9,
    ]
}

/// Follow the tree whose first process is `root` until all of it has exited, and
/// with `tree`, the request that the first process runs, keep its process tree.
///
/// `root` must be a resumed tracee, and the only child, of the calling thread.
/// Until `escalation` is set, the loop follows the tree in full, also during the
/// grace period after cancellation; once it is set, every thread the loop sees is
/// killed.
///
/// # Errors
///
/// Returns the error of a wait, a `ptrace` request or the live set, or
/// [`sys::stopped_waiting`] when the watchdog asked the tracing thread to end the
/// run. The tree may then still be running: the caller returns the error from the
/// tracing thread, whose end kills the tree (see "The cleanup of last resort").
pub(crate) fn follow(
    root: pid_t,
    live: &LiveProcesses,
    escalation: &Escalation,
    tree: Option<&RunRequest>,
) -> io::Result<Outcome> {
    let mut followed = Tree::new(root, live, escalation)?;
    if let Some(request) = tree {
        followed.keep_processes(request);
    }
    followed.run()?;
    followed.outcome.tree = followed.processes.take().map(Processes::into_tree);
    Ok(followed.outcome)
}

/// Put `root`, the first process of a started tree that the loop does not follow
/// yet, in the live set as its first process, so an interrupt for a request that
/// is already cancelled, and a forwarded signal, reach it.
///
/// # Errors
///
/// The error of a set that stays locked for [`kizz_threads::LOCK_PATIENCE`]. The
/// caller then returns it from the tracing thread at once, instead of going on
/// into a loop that could not lock the set either, and the thread's end kills the
/// tree (see "The cleanup of last resort").
pub(crate) fn track_root(
    live: &LiveProcesses,
    root: pid_t,
) -> io::Result<()> {
    track_root_within(live, root, kizz_threads::LOCK_PATIENCE)
}

/// [`track_root`], waiting at most `patience` for the live set, so a test can
/// hold the set and see the error at once.
///
/// # Errors
///
/// The error of a set that stays locked for `patience`.
fn track_root_within(
    live: &LiveProcesses,
    root: pid_t,
    patience: Duration,
) -> io::Result<()> {
    lock_within(live, patience)?.enter_first(root);
    Ok(())
}

/// Tests that drive the loop by hand with real traced processes, to stop it at
/// exactly the moment a test needs.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: helper processes and tests start plain std threads, and the tests \
              assert and lock"
)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        ffi::OsString,
        fs,
        io::{BufRead as _, BufReader, Read as _, Write as _},
        os::fd::OwnedFd,
        panic,
        path::Path,
        process::{Child, Command, Stdio},
        ptr,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    use kizz_trace::{AbsolutePath, Cancellation, CaptureMode, EchoSink, RunRequest};
    use libc::c_long;

    use super::*;
    use crate::{
        Launch, capture,
        spawn::{self, Program, Started},
        supervise, watch,
    };

    /// The variable that selects the helper mode of this test executable.
    const HELPER_VARIABLE: &str = "KIZZ_TRACE_UNIT_TEST_HELPER";
    /// The variable that names the file in which the helper mode `escape-exit`
    /// writes its first process's ID and its child's.
    const MARKER_VARIABLE: &str = "KIZZ_TRACE_UNIT_TEST_MARKER";
    /// How long a test waits for something another process does.
    const PATIENCE: Duration = Duration::from_secs(30);
    /// How long a test lets the live set stay locked before a lock of it fails, in
    /// place of [`kizz_threads::LOCK_PATIENCE`].
    const LIVE_SET_PATIENCE: Duration = Duration::from_millis(50);
    /// How long a test waits for a second delivery of a signal that must arrive
    /// once, or for a delivery that must not arrive.
    const SETTLE: Duration = Duration::from_millis(300);

    /// Suspend the calling thread for good.
    fn pause_forever() -> ! {
        loop {
            // SAFETY: `pause` has no requirements and is async-signal-safe.
            unsafe { libc::pause() };
        }
    }

    /// Park the calling thread for good.
    fn park_forever() {
        loop {
            thread::park();
        }
    }

    /// The work of a helper mode, run by this test executable when a test traces it
    /// with [`HELPER_VARIABLE`] set. In a normal test run it does nothing.
    #[test]
    fn helper_process() {
        let Some(mode) = std::env::var_os(HELPER_VARIABLE) else {
            return;
        };
        match mode.to_str() {
            Some("fork") => {
                // SAFETY: both sides only call `pause`, which is async-signal-safe.
                unsafe { libc::fork() };
                pause_forever();
            },
            Some("vfork") => {
                let unused: c_long = 0;
                // SAFETY: without `CLONE_VM` the child gets its own copy of memory,
                // as with `fork`, and both sides only call `pause`.
                unsafe {
                    libc::syscall(
                        libc::SYS_clone,
                        c_long::from(libc::CLONE_VFORK | libc::SIGCHLD),
                        unused,
                        unused,
                        unused,
                        unused,
                    )
                };
                pause_forever();
            },
            Some("clone") => {
                drop(thread::spawn(park_forever));
                pause_forever();
            },
            Some("thread-exec") => {
                // A thread that does not lead the process runs `exec`.
                drop(thread::spawn(|| {
                    let argv = [c"sleep".as_ptr(), c"1000".as_ptr(), ptr::null()];
                    // SAFETY: the program and the arguments are NUL-terminated
                    // strings, and `argv` is a null-terminated array.
                    unsafe { libc::execv(c"/bin/sleep".as_ptr(), argv.as_ptr()) };
                    panic!("could not run sleep: {}", io::Error::last_os_error());
                }));
                pause_forever();
            },
            Some(mode @ ("interrupt-leader-exec" | "interrupt-thread-exec")) => {
                // A `SIGINT` sent to the process waits in its shared pending set
                // until a thread that does not block it takes it, whichever thread
                // Linux wakes for it, so only a thread that does not block it can
                // take the interrupt. libtest runs this test on a thread of its
                // own while the leader waits for it. This thread blocks `SIGINT`
                // for good, and the thread that runs `exec` inherits the block
                // until the test releases it, after the interrupt was taken.
                set_interrupt_mask(libc::SIG_BLOCK);
                drop(thread::spawn(exec_when_released));
                if mode == "interrupt-thread-exec" {
                    // The test blocks `SIGINT` in the leader from its first stop on,
                    // before the process has any other thread, and every thread
                    // inherits that, so this new thread, which does not lead the
                    // process and unblocks it, is the only one that can take the
                    // interrupt.
                    drop(thread::spawn(|| {
                        set_interrupt_mask(libc::SIG_UNBLOCK);
                        mark_ready();
                        pause_forever();
                    }));
                } else {
                    // The leader, which libtest leaves with `SIGINT` unblocked once
                    // it has started this thread, is the only thread that does not
                    // block it, so it is the only one that can take the interrupt.
                    mark_ready();
                }
                pause_forever();
            },
            Some("escape") => {
                // The child leaves the session and the process group of the first
                // process, makes the tracked call that tells the test that it is
                // ready, and waits for good; the first process waits for good too.
                // SAFETY: the child only calls `setsid`, `chdir` and `pause`, which
                // are async-signal-safe.
                if unsafe { libc::fork() } == 0 {
                    // SAFETY: as above; the path is a NUL-terminated string.
                    unsafe {
                        libc::setsid();
                        libc::chdir(c"/".as_ptr());
                    }
                    pause_forever();
                }
                pause_forever();
            },
            Some("escape-exit") => {
                // The child leaves the session and the process group of the first
                // process and waits for good, blocked, with no stop for the tracer;
                // the first process names both processes in the marker file and
                // exits.
                let marker = std::env::var_os(MARKER_VARIABLE).unwrap();
                // SAFETY: the child only calls `setsid` and `pause`, which are
                // async-signal-safe.
                let child = unsafe { libc::fork() };
                if child == 0 {
                    // SAFETY: as above.
                    unsafe { libc::setsid() };
                    pause_forever();
                }
                fs::write(marker, format!("{} {child}", std::process::id())).unwrap();
                // SAFETY: `_exit` ends the process at once, which is all it does.
                unsafe { libc::_exit(0) };
            },
            other => panic!("unknown helper mode {other:?}"),
        }
    }

    /// A request that runs this test executable in a helper mode.
    fn helper(mode: &str) -> RunRequest {
        let executable = std::env::current_exe().unwrap();
        let args = [
            "kizz_trace_linux",
            "--exact",
            "follow::tests::helper_process",
            "--nocapture",
            "--test-threads=1",
            "--quiet",
        ];
        RunRequest {
            program: AbsolutePath::new(executable).unwrap(),
            args: args.iter().map(OsString::from).collect(),
            env: BTreeMap::from([(OsString::from(HELPER_VARIABLE), OsString::from(mode))]),
            cwd: AbsolutePath::new("/").unwrap(),
            stdin: None,
            capture: CaptureMode::Split,
            echo: false,
            tree: false,
            terminal: false,
        }
    }

    /// The signal set that holds only `SIGINT`.
    fn interrupt_set() -> libc::sigset_t {
        // SAFETY: `sigset_t` is plain data, for which all zero bytes are a valid
        // value.
        let mut set: libc::sigset_t = unsafe { mem::zeroed() };
        // SAFETY: `set` is a valid signal set, and `SIGINT` a valid signal.
        unsafe {
            libc::sigemptyset(&raw mut set);
            libc::sigaddset(&raw mut set, libc::SIGINT);
        }
        set
    }

    /// Block or unblock `SIGINT` in the calling thread, as `how` says.
    fn set_interrupt_mask(how: c_int) {
        let set = interrupt_set();
        // SAFETY: `set` is a valid signal set, and the old mask is not asked for.
        let ret = unsafe { libc::pthread_sigmask(how, &raw const set, ptr::null_mut()) };
        assert_eq!(ret, 0, "could not change the signal mask");
    }

    /// Tell the test that the helper is ready, with a tracked system call that
    /// nothing else in the helper makes and that changes nothing: `chdir` to `/`,
    /// the working directory of every helper.
    fn mark_ready() {
        // SAFETY: the path is a NUL-terminated string.
        let ret = unsafe { libc::chdir(c"/".as_ptr()) };
        assert_eq!(
            ret,
            0,
            "could not change to /: {}",
            io::Error::last_os_error()
        );
    }

    /// Wait until the test writes to standard input, then run `sleep` from this
    /// thread, which does not lead its process; the `exec` kills every other thread
    /// of the process. The program sleeps for as long as the test's patience, so a
    /// run that lost the interrupt would end late, with an exit code.
    fn exec_when_released() {
        let mut byte = [0_u8];
        io::stdin().read_exact(&mut byte).unwrap();
        // Another thread has already taken the interrupt, so none is pending for
        // this one, and the program must not start with `SIGINT` blocked.
        set_interrupt_mask(libc::SIG_UNBLOCK);
        let argv = [c"sleep".as_ptr(), c"30".as_ptr(), ptr::null()];
        // SAFETY: the program and the arguments are NUL-terminated strings, and
        // `argv` is a null-terminated array.
        unsafe { libc::execv(c"/bin/sleep".as_ptr(), argv.as_ptr()) };
        panic!("could not run sleep: {}", io::Error::last_os_error());
    }

    /// Whether a process or thread has exited: it no longer exists, or it is a
    /// zombie that only waits to be collected.
    fn has_exited(pid: pid_t) -> bool {
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return true;
        };
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.trim_start().chars().next());
        matches!(state, Some('Z' | 'X'))
    }

    /// Take the waiting state change of a known thread, if it has one, as
    /// [`next_change`] does. A thread whose ID was released by an `exec` that the
    /// loop has not handled yet has nothing to take.
    fn pending_known(
        live: &LiveProcesses,
        pid: pid_t,
    ) -> Option<c_int> {
        let pending = sys::pending(pid)
            .unwrap_or_else(|error| panic!("could not wait for thread {pid}: {error}"))?;
        take(live, pid, pending)
            .unwrap_or_else(|error| panic!("could not collect thread {pid}: {error}"))
    }

    /// How the test hands an event to the loop after cancellation.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Handover {
        /// The grace period has ended, and the loop handles the event after the
        /// escalation.
        Event,
        /// The grace period has ended, and the watchdog kills the live processes
        /// before the loop handles the event, as when it escalates while the event
        /// happens. For a creation event, the creator is killed, so the loop never
        /// learns of the new thread from it.
        Watchdog,
        /// The watchdog interrupts the live processes before the loop handles the
        /// event, as when cancellation is requested while the event happens, and
        /// the loop then handles it during the grace period.
        Interrupt,
    }

    /// A helper that a test traces and follows by hand.
    struct Traced {
        /// The helper's first process.
        root: pid_t,
        /// The process descriptor of the first process, which collects it.
        handle: OwnedFd,
        /// One report per output pipe: whether its reader reached end of file.
        finished: mpsc::Receiver<bool>,
        /// The write end of the helper's standard input, when the test holds it.
        gate: Option<fs::File>,
        /// The read end of the start-failure pipe, kept open while the test runs.
        _report: OwnedFd,
    }

    /// Start the helper `mode` under `ptrace`, with a reader on each output pipe.
    fn trace_helper(mode: &str) -> Traced {
        trace(&helper(mode))
    }

    /// Start the helper `mode` like [`trace_helper`], with its standard input a pipe
    /// whose write end the test holds as [`Traced::gate`].
    fn trace_gated_helper(mode: &str) -> Traced {
        let mut request = helper(mode);
        request.stdin = Some(Vec::new());
        trace(&request)
    }

    /// Start `request` under `ptrace`, with a reader on each output pipe.
    fn trace(request: &RunRequest) -> Traced {
        let program = Program::prepare(request).unwrap();
        let filter = filter::program().unwrap();
        let (child_stdio, tracer_stdio) = capture::pipes(request).unwrap();
        let (report, report_write) = sys::pipe().unwrap();
        let started = spawn::start(
            request,
            &program,
            &filter,
            child_stdio.as_ref(),
            report_write,
        );
        drop(child_stdio);
        let (root, handle) = match started {
            Ok(Started::Running(root, handle)) => (root, handle),
            other => panic!("the helper did not start: {other:?}"),
        };
        let (done, finished) = mpsc::channel();
        for (stream, pipe) in tracer_stdio.outputs {
            let done = done.clone();
            thread::spawn(move || {
                let echo: Option<&dyn EchoSink> = None;
                done.send(capture::collect(pipe, stream, echo).is_ok()).ok();
            });
        }
        Traced {
            root,
            handle,
            finished,
            gate: tracer_stdio.stdin,
            _report: report,
        }
    }

    /// Assert the invariant that keeps the watchdog's signals inside the tree:
    /// every ID in the live set names a followed thread that leads its process.
    fn assert_only_processes_are_live(
        tree: &Tree<'_>,
        live: &LiveProcesses,
    ) {
        for &pid in lock(live).unwrap().iter() {
            assert!(
                tree.tracees.contains_key(&pid),
                "the live ID {pid} is not followed"
            );
            assert_eq!(
                sys::thread_group(pid).unwrap(),
                pid,
                "the live ID {pid} names a thread that does not lead its process"
            );
        }
    }

    /// Follow the tree by hand, waiting only for threads the loop already knows, so
    /// a new thread's first stop stays pending, until `wanted` picks a stop, and
    /// return that thread and its wait status, taken as the loop takes it and not
    /// yet handled. The live-set invariant is checked before every round of waits.
    fn drive_until(
        tree: &mut Tree<'_>,
        live: &LiveProcesses,
        what: &str,
        wanted: impl Fn(pid_t, c_int) -> bool,
    ) -> (pid_t, c_int) {
        let deadline = Instant::now().checked_add(PATIENCE).unwrap();
        loop {
            assert!(Instant::now() < deadline, "the helper made no {what}");
            assert_only_processes_are_live(tree, live);
            let mut idle = true;
            let known: Vec<pid_t> = tree.tracees.keys().copied().collect();
            for pid in known {
                let Some(status) = pending_known(live, pid) else {
                    continue;
                };
                idle = false;
                if wanted(pid, status) {
                    return (pid, status);
                }
                tree.handle(pid, status).unwrap();
            }
            if idle {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Follow the tree by hand like [`drive_until`] while the loop holds the stop
    /// `held`, which it has taken and not handled yet: no change of that thread is
    /// taken or handled. Return the first change, other than the held stop, that
    /// `wanted` picks, as a wait reports it, not yet taken.
    fn drive_holding(
        tree: &mut Tree<'_>,
        live: &LiveProcesses,
        held: (pid_t, c_int),
        what: &str,
        wanted: impl Fn(pid_t, sys::Pending) -> bool,
    ) -> (pid_t, sys::Pending) {
        let deadline = Instant::now().checked_add(PATIENCE).unwrap();
        loop {
            assert!(Instant::now() < deadline, "the helper made no {what}");
            let mut idle = true;
            let known: Vec<pid_t> = tree.tracees.keys().copied().collect();
            for pid in known {
                let Some(pending) = sys::pending(pid)
                    .unwrap_or_else(|error| panic!("could not wait for thread {pid}: {error}"))
                else {
                    continue;
                };
                if (pid, pending) != (held.0, sys::Pending::Stopped(held.1)) && wanted(pid, pending)
                {
                    return (pid, pending);
                }
                if pid == held.0 {
                    continue;
                }
                idle = false;
                if let Some(status) = take(live, pid, pending).unwrap() {
                    tree.handle(pid, status).unwrap();
                }
            }
            if idle {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Block `SIGINT` in the traced thread `pid`, which is in a `ptrace` stop.
    fn block_interrupt(pid: pid_t) {
        let set = interrupt_set();
        // SAFETY: `PTRACE_SETSIGMASK` reads one kernel signal set, whose size is
        // the address argument, from the start of `set`, which is larger.
        let ret = unsafe {
            libc::ptrace(
                libc::PTRACE_SETSIGMASK,
                pid,
                ptr::without_provenance_mut::<libc::c_void>(mem::size_of::<u64>()),
                (&raw const set).cast::<libc::c_void>(),
            )
        };
        assert_eq!(
            ret,
            0,
            "could not block SIGINT in {pid}: {}",
            io::Error::last_os_error()
        );
    }

    /// The signal set `field`, such as `ShdPnd` or `SigBlk`, of the `/proc` status
    /// file `status`, as a bit set in which signal N is the bit N - 1.
    fn signal_set(
        status: &str,
        field: &str,
    ) -> u64 {
        let status = fs::read_to_string(status).unwrap();
        let set = status
            .lines()
            .find_map(|line| line.strip_prefix(field)?.strip_prefix(':'))
            .unwrap();
        u64::from_str_radix(set.trim(), 16).unwrap()
    }

    /// The signals pending for the whole process `pid`, as a bit set in which
    /// signal N is the bit N - 1.
    fn shared_pending(pid: pid_t) -> u64 {
        signal_set(&format!("/proc/{pid}/status"), "ShdPnd")
    }

    /// Whether the thread `tid` of the process `pid` blocks `SIGINT`.
    fn blocks_interrupt(
        pid: pid_t,
        tid: pid_t,
    ) -> bool {
        (signal_set(&format!("/proc/{pid}/task/{tid}/status"), "SigBlk") & INTERRUPT_BIT) != 0
    }

    /// The threads of the process `pid` that do not block `SIGINT`: the only
    /// threads that can take a `SIGINT` sent to the process, which waits in the
    /// process's shared pending set until one of them takes it.
    fn interrupt_takers(pid: pid_t) -> BTreeSet<pid_t> {
        fs::read_dir(format!("/proc/{pid}/task"))
            .unwrap()
            .map(|entry| {
                let name = entry.unwrap().file_name();
                name.to_str().unwrap().parse::<pid_t>().unwrap()
            })
            .filter(|&tid| !blocks_interrupt(pid, tid))
            .collect()
    }

    /// Wait until the thread `tid` of the process `pid`, which the helper leaves
    /// with `SIGINT` unblocked, no longer blocks it.
    ///
    /// glibc's `pthread_create` blocks every signal in the calling thread until it
    /// has started the new thread, so a leader that started a thread may still
    /// block `SIGINT` for a while after that thread has begun to run, and never
    /// blocks it again. The wait only lets the test observe the leader's final
    /// mask: a `SIGINT` sent while the leader still blocks it waits in the shared
    /// pending set, so the thread that takes it is the same either way.
    fn await_unblocked(
        pid: pid_t,
        tid: pid_t,
    ) {
        let deadline = Instant::now().checked_add(PATIENCE).unwrap();
        while blocks_interrupt(pid, tid) {
            assert!(
                Instant::now() < deadline,
                "thread {tid} kept SIGINT blocked"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Whether a wait status is a `ptrace` stop at `event`.
    fn is_event_stop(
        status: c_int,
        event: c_int,
    ) -> bool {
        libc::WIFSTOPPED(status)
            && libc::WSTOPSIG(status) == libc::SIGTRAP
            && sys::stop_event(status) == event
    }

    /// Let the loop finish after cancellation, and assert that it ended with every
    /// thread collected, the first process ended by `signal`, each of `others`
    /// gone, and no process left that holds an output pipe.
    fn finish_cancelled(
        mut tree: Tree<'_>,
        live: &LiveProcesses,
        traced: &Traced,
        others: &[pid_t],
        signal: c_int,
    ) {
        tree.run().unwrap();
        assert!(tree.tracees.is_empty());
        assert!(lock(live).unwrap().is_empty());
        assert_eq!(
            tree.outcome.termination,
            Some(Termination::Signaled(signal))
        );
        for &pid in others.iter().chain([&traced.root]) {
            assert!(has_exited(pid), "process {pid} survived cancellation");
        }
        for _ in 0..2 {
            assert_eq!(
                traced.finished.recv_timeout(PATIENCE),
                Ok(true),
                "an output pipe stayed open after cancellation"
            );
        }
    }

    /// Trace the helper `mode` and follow it by hand until a thread stops at a
    /// creation event `event`, with the new thread's first stop still pending.
    /// Then let the grace period end, hand the event over as `handover` says, and
    /// let the loop finish: the new thread must be gone too.
    fn cancel_at_creation(
        mode: &str,
        event: c_int,
        handover: Handover,
    ) {
        let traced = trace_helper(mode);
        let live = LiveProcesses::default();
        let escalation = Escalation::new().unwrap();
        let mut tree = Tree::new(traced.root, &live, &escalation).unwrap();
        let (creator, status) = drive_until(&mut tree, &live, "creation event", |_, status| {
            is_event_stop(status, event)
        });
        let created = message_pid(sys::event_message(creator).unwrap()).unwrap();
        assert!(
            !tree.tracees.contains_key(&created),
            "the new thread {created} was already followed"
        );
        escalation.escalate();
        match handover {
            Handover::Event => tree.handle(creator, status).unwrap(),
            Handover::Watchdog => kill_all(&live).unwrap(),
            Handover::Interrupt => panic!("a creation event is handed over after the escalation"),
        }
        assert_only_processes_are_live(&tree, &live);
        finish_cancelled(tree, &live, &traced, &[created], libc::SIGKILL);
    }

    /// Trace a helper whose thread that does not lead the process runs `exec`, and
    /// follow it by hand until the `exec` event, which the loop has not handled
    /// yet while the kernel has already released the thread's former ID. Then
    /// cancel, hand the event over as `handover` says, and let the loop finish.
    ///
    /// The former ID must never have been in the live set: the watchdog may signal
    /// only the process, whose ID the thread took over, with `SIGKILL` after the
    /// escalation as with the `SIGINT` that interrupts the tree.
    fn cancel_at_non_leader_exec(handover: Handover) {
        let traced = trace_helper("thread-exec");
        let root = traced.root;
        let live = LiveProcesses::default();
        let escalation = Escalation::new().unwrap();
        let mut tree = Tree::new(root, &live, &escalation).unwrap();
        let (runner, status) = drive_until(
            &mut tree,
            &live,
            "exec from a thread that does not lead its process",
            |pid, status| {
                is_event_stop(status, libc::PTRACE_EVENT_EXEC)
                    && sys::event_message(pid)
                        .and_then(message_pid)
                        .is_ok_and(|former| former != pid)
            },
        );
        let former = message_pid(sys::event_message(runner).unwrap()).unwrap();
        assert_eq!(runner, root, "the thread did not take over the leader's ID");
        assert!(
            tree.tracees.contains_key(&former),
            "the thread {former} was not followed before its exec"
        );
        assert!(
            !Path::new(&format!("/proc/{root}/task/{former}")).exists(),
            "the former ID {former} still names a thread of the process"
        );
        // The ID is released, and the loop has not seen that yet; the watchdog
        // would signal exactly the process.
        assert_eq!(lock(&live).unwrap().processes, BTreeSet::from([root]));
        let signal = match handover {
            Handover::Event => {
                escalation.escalate();
                tree.handle(runner, status).unwrap();
                assert!(!tree.tracees.contains_key(&former));
                libc::SIGKILL
            },
            Handover::Watchdog => {
                escalation.escalate();
                kill_all(&live).unwrap();
                libc::SIGKILL
            },
            Handover::Interrupt => {
                // The program the thread ran, `sleep`, ends at the interrupt, which
                // the loop delivers during the grace period.
                interrupt_all(&live).unwrap();
                assert_only_processes_are_live(&tree, &live);
                tree.handle(runner, status).unwrap();
                assert!(!tree.tracees.contains_key(&former));
                libc::SIGINT
            },
        };
        assert_only_processes_are_live(&tree, &live);
        finish_cancelled(tree, &live, &traced, &[], signal);
    }

    #[test]
    fn cancellation_at_a_fork_event_kills_the_new_process() {
        cancel_at_creation("fork", libc::PTRACE_EVENT_FORK, Handover::Event);
    }

    #[test]
    fn cancellation_at_a_vfork_event_kills_the_new_process() {
        cancel_at_creation("vfork", libc::PTRACE_EVENT_VFORK, Handover::Event);
    }

    #[test]
    fn cancellation_at_a_clone_event_kills_the_new_thread() {
        cancel_at_creation("clone", libc::PTRACE_EVENT_CLONE, Handover::Event);
    }

    #[test]
    fn a_process_created_while_its_creator_is_killed_is_still_killed() {
        cancel_at_creation("fork", libc::PTRACE_EVENT_FORK, Handover::Watchdog);
    }

    #[test]
    fn a_vforked_process_created_while_its_creator_is_killed_is_still_killed() {
        cancel_at_creation("vfork", libc::PTRACE_EVENT_VFORK, Handover::Watchdog);
    }

    #[test]
    fn a_thread_created_while_its_creator_is_killed_is_still_collected() {
        cancel_at_creation("clone", libc::PTRACE_EVENT_CLONE, Handover::Watchdog);
    }

    #[test]
    fn cancellation_at_a_non_leader_exec_signals_only_the_process() {
        cancel_at_non_leader_exec(Handover::Event);
    }

    #[test]
    fn the_watchdog_never_signals_the_former_id_of_a_non_leader_exec() {
        cancel_at_non_leader_exec(Handover::Watchdog);
    }

    #[test]
    fn the_interrupt_never_signals_the_former_id_of_a_non_leader_exec() {
        cancel_at_non_leader_exec(Handover::Interrupt);
    }

    /// The bit of `SIGINT`, signal 2, in a set of signals from `/proc`.
    const INTERRUPT_BIT: u64 = 0b10;

    /// Which thread of the helper takes the interrupt.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Taker {
        /// The thread that leads the process; the thread that runs `exec` takes
        /// over its ID.
        Leader,
        /// A thread that does not lead the process.
        Thread,
    }

    /// Trace a helper with a thread that does not lead its process and waits to run
    /// `exec`, interrupt it, and follow it by hand until `taker` stops to receive
    /// the interrupt. The loop takes that stop and, before it handles it, the
    /// waiting thread runs `exec`, which kills every other thread of the process,
    /// the taker included; only then does the loop handle the stop.
    ///
    /// The interrupt must not be lost with the taker: the stop was still in place,
    /// so the killed taker queued the signal again for its process, and the
    /// program that the `exec` started ends at it.
    ///
    /// A taker that does not lead the process is handed over once it has exited,
    /// before the loop collects its exit, which the `exec` waits for. A taker that
    /// leads it is handed over once the thread that ran `exec` has stopped at it
    /// under the leader's ID, and handling the taker's stop then must leave that
    /// `exec` stop alone for the loop to handle next.
    fn interrupt_meets_non_leader_exec(taker: Taker) {
        let mode = match taker {
            Taker::Leader => "interrupt-leader-exec",
            Taker::Thread => "interrupt-thread-exec",
        };
        let traced = trace_gated_helper(mode);
        let root = traced.root;
        let live = LiveProcesses::default();
        let escalation = Escalation::new().unwrap();
        let mut tree = Tree::new(root, &live, &escalation).unwrap();
        if taker == Taker::Thread {
            // The leader blocks `SIGINT` from its first stop on, before the
            // process has any other thread, and every thread inherits that.
            let (pid, status) = drive_until(&mut tree, &live, "first stop", |pid, status| {
                pid == root && libc::WIFSTOPPED(status)
            });
            block_interrupt(pid);
            tree.handle(pid, status).unwrap();
        }
        let (ready, status) =
            drive_until(&mut tree, &live, "sign that it is ready", |pid, status| {
                is_event_stop(status, libc::PTRACE_EVENT_SECCOMP)
                    && sys::registers(pid).is_ok_and(|registers| {
                        c_long::try_from(registers.orig_rax) == Ok(libc::SYS_chdir)
                    })
            });
        tree.handle(ready, status).unwrap();
        // Only the thread that the variant names can take the interrupt: it is the
        // one thread of the helper that does not block `SIGINT`, and none of the
        // others unblocks it before the test releases the thread that runs `exec`.
        let named = match taker {
            Taker::Leader => root,
            Taker::Thread => ready,
        };
        await_unblocked(root, named);
        assert_eq!(
            interrupt_takers(root),
            BTreeSet::from([named]),
            "another thread of the helper does not block SIGINT"
        );
        interrupt_all(&live).unwrap();
        let held = drive_until(&mut tree, &live, "stop for the interrupt", |_, status| {
            libc::WIFSTOPPED(status)
                && libc::WSTOPSIG(status) == libc::SIGINT
                && sys::stop_event(status) == 0
        });
        match taker {
            Taker::Leader => assert_eq!(held.0, root, "the leader did not take the interrupt"),
            Taker::Thread => {
                assert_ne!(held.0, root, "the leader took the interrupt");
                assert_eq!(held.0, ready, "another thread took the interrupt");
            },
        }
        // The loop has taken the stop. Now the waiting thread runs `exec`.
        traced.gate.as_ref().unwrap().write_all(b"x").unwrap();
        let (_, change) = drive_holding(
            &mut tree,
            &live,
            held,
            "exec from a thread that does not lead its process",
            |pid, pending| {
                match (taker, pending) {
                    (Taker::Leader, sys::Pending::Stopped(status)) => {
                        pid == root && is_event_stop(status, libc::PTRACE_EVENT_EXEC)
                    },
                    (Taker::Thread, sys::Pending::Ended) => pid == held.0,
                    _ => false,
                }
            },
        );
        assert_ne!(
            shared_pending(root) & INTERRUPT_BIT,
            0,
            "the interrupt was lost with the thread that took it"
        );
        tree.handle(held.0, held.1).unwrap();
        let exec = match change {
            sys::Pending::Stopped(exec) => {
                assert_eq!(
                    pending_known(&live, root),
                    Some(exec),
                    "handling the stop of the killed leader resumed the exec stop"
                );
                exec
            },
            sys::Pending::Ended => {
                drive_until(&mut tree, &live, "exec", |pid, status| {
                    pid == root && is_event_stop(status, libc::PTRACE_EVENT_EXEC)
                })
                .1
            },
        };
        assert_ne!(
            message_pid(sys::event_message(root).unwrap()).unwrap(),
            root,
            "the exec did not come from a thread that does not lead the process"
        );
        tree.handle(root, exec).unwrap();
        finish_cancelled(tree, &live, &traced, &[held.0], libc::SIGINT);
    }

    #[test]
    fn an_interrupt_taken_by_the_leader_survives_an_exec_by_another_thread() {
        interrupt_meets_non_leader_exec(Taker::Leader);
    }

    #[test]
    fn an_interrupt_taken_by_a_thread_survives_an_exec_by_another_thread() {
        interrupt_meets_non_leader_exec(Taker::Thread);
    }

    #[test]
    fn a_cloned_thread_never_joins_the_live_set() {
        let traced = trace_helper("clone");
        let live = LiveProcesses::default();
        let escalation = Escalation::new().unwrap();
        let mut tree = Tree::new(traced.root, &live, &escalation).unwrap();
        let (creator, status) = drive_until(&mut tree, &live, "clone event", |_, status| {
            is_event_stop(status, libc::PTRACE_EVENT_CLONE)
        });
        let created = message_pid(sys::event_message(creator).unwrap()).unwrap();
        tree.handle(creator, status).unwrap();
        assert!(tree.tracees.contains_key(&created));
        assert!(!lock(&live).unwrap().contains(created));
        assert_eq!(
            lock(&live).unwrap().processes,
            BTreeSet::from([traced.root])
        );
        assert_only_processes_are_live(&tree, &live);
        escalation.escalate();
        kill_all(&live).unwrap();
        finish_cancelled(tree, &live, &traced, &[created], libc::SIGKILL);
    }

    /// The session of the process `pid`, from its `/proc` stat file, or `None` once
    /// it is gone.
    fn session(pid: pid_t) -> Option<pid_t> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, rest) = stat.rsplit_once(')')?;
        rest.split_whitespace().nth(3)?.parse().ok()
    }

    /// Whether the process `pid` has exited within [`PATIENCE`], as
    /// [`has_exited`] says.
    fn exits(pid: pid_t) -> bool {
        let deadline = Instant::now().checked_add(PATIENCE).unwrap();
        while !has_exited(pid) {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(1));
        }
        true
    }

    /// How the thread that traces a helper ends in a test.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Ending {
        /// The thread returns, as the tracing thread does with an error.
        Returns,
        /// The thread panics.
        Panics,
    }

    /// What a test saw once the thread that traced a helper had ended, without any
    /// cleanup of its own: whether the first process is gone, whether its process
    /// descriptor collected it, and whether each output pipe reached its end.
    #[derive(Debug, PartialEq, Eq)]
    struct AfterTracer {
        /// Whether the first process is gone.
        exited: bool,
        /// Whether [`sys::reap`] collected the first process.
        reaped: bool,
        /// For each output pipe, whether its reader reached end of file.
        pipes: Vec<bool>,
    }

    /// What [`end_tracing_thread`] sees when the end of the tracing thread killed
    /// the whole tree.
    fn all_ended() -> AfterTracer {
        AfterTracer {
            exited: true,
            reaped: true,
            pipes: vec![true, true],
        }
    }

    /// Run `on_tracer` on a new thread that traces the helper `mode`, with the
    /// helper's first process, then let the thread end as `ending` says, with
    /// nothing killed, signaled or collected by the test or by kizz: the end of the
    /// tracing thread is the cleanup of last resort. Return what `on_tracer`
    /// returned and what [`AfterTracer`] lists, each wait bounded by [`PATIENCE`],
    /// so a tree left behind fails the test instead of hanging it.
    ///
    /// An unrelated child of the test process, started before the helper, must
    /// still run afterwards: the cleanup signals no process ID.
    fn end_tracing_thread<R: Send + 'static>(
        mode: &'static str,
        ending: Ending,
        on_tracer: impl FnOnce(pid_t) -> R + Send + 'static,
    ) -> (R, AfterTracer) {
        let mut unrelated = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let (sender, receiver) = mpsc::channel();
        let tracer = thread::spawn(move || {
            // This thread starts the helper, so it is the helper's tracer.
            let Traced {
                root,
                handle,
                finished,
                _report: report,
                ..
            } = trace_helper(mode);
            let result = on_tracer(root);
            sender.send((result, root, handle, finished, report)).ok();
            if ending == Ending::Panics {
                panic::resume_unwind(Box::new("the tracing thread failed"));
            }
        });
        let (result, root, handle, finished, report) = receiver
            .recv_timeout(PATIENCE)
            .expect("the tracing thread did not end");
        assert_eq!(tracer.join().is_err(), ending == Ending::Panics);
        // Collected on a thread of its own, so a first process that the end of its
        // tracing thread left alive fails the test instead of hanging it.
        let (reaped_sender, reaped) = mpsc::channel();
        thread::spawn(move || reaped_sender.send(sys::reap(&handle).is_ok()).ok());
        let reaped = reaped.recv_timeout(PATIENCE).unwrap_or(false);
        let exited = has_exited(root);
        let pipes = (0..2)
            .map(|_| finished.recv_timeout(PATIENCE).unwrap_or(false))
            .collect();
        drop(report);
        assert!(
            unrelated.try_wait().unwrap().is_none(),
            "the cleanup reached an unrelated child of the process"
        );
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
        (result, AfterTracer {
            exited,
            reaped,
            pipes,
        })
    }

    /// The end of the tracing thread kills a process that left the first process's
    /// session and process group and waits in a `ptrace` stop that its tracer
    /// already collected, which no wait reports again, and the first process,
    /// which its descriptor then collects; both output pipes end. The cleanup reads
    /// no `/proc`, needs no live set, and signals no ID, so neither an unreadable
    /// `/proc` nor a missing task status can keep it from ending.
    #[test]
    fn the_end_of_the_tracing_thread_kills_an_escaped_process_in_a_collected_stop() {
        let ((child, escaped, reported_again), after) =
            end_tracing_thread("escape", Ending::Returns, |root| {
                let live = LiveProcesses::default();
                let escalation = Escalation::new().unwrap();
                let mut tree = Tree::new(root, &live, &escalation).unwrap();
                let (child, status) = drive_until(
                    &mut tree,
                    &live,
                    "call of the escaped process",
                    |pid, status| {
                        pid != root
                            && is_event_stop(status, libc::PTRACE_EVENT_SECCOMP)
                            && sys::registers(pid).is_ok_and(|registers| {
                                c_long::try_from(registers.orig_rax) == Ok(libc::SYS_chdir)
                            })
                    },
                );
                assert!(libc::WIFSTOPPED(status), "{status}");
                // The loop took the stop, which collected it, and never resumes it.
                let reported_again = sys::pending(child).unwrap();
                (child, session(child) == Some(child), reported_again)
            });
        assert!(
            escaped,
            "the child did not leave the session of the first process"
        );
        assert_eq!(
            reported_again, None,
            "the collected stop was reported again"
        );
        assert!(
            exits(child),
            "the escaped process in a collected stop survived"
        );
        assert_eq!(after, all_ended());
    }

    /// A panic on the tracing thread, while the first process waits in a stop that
    /// the thread collected, kills the tree in the same way, and the panic reaches
    /// the thread's caller.
    #[test]
    fn a_panic_on_the_tracing_thread_kills_the_tree_that_waits_for_it() {
        let (stopped, after) = end_tracing_thread("clone", Ending::Panics, |root| {
            libc::WIFSTOPPED(sys::wait_for(root).unwrap())
        });
        assert!(stopped, "the first process did not stop");
        assert_eq!(after, all_ended());
    }

    /// The end of the tracing thread kills a process in which a thread that did not
    /// lead it ran `exec`, which released that thread's former ID, without
    /// signaling either ID: the process, now `sleep`, is gone.
    #[test]
    fn the_end_of_the_tracing_thread_kills_a_process_after_a_non_leader_exec() {
        let (released, after) = end_tracing_thread("thread-exec", Ending::Returns, |root| {
            let live = LiveProcesses::default();
            let escalation = Escalation::new().unwrap();
            let mut tree = Tree::new(root, &live, &escalation).unwrap();
            let (runner, _) = drive_until(
                &mut tree,
                &live,
                "exec from a thread that does not lead its process",
                |pid, status| {
                    is_event_stop(status, libc::PTRACE_EVENT_EXEC)
                        && sys::event_message(pid)
                            .and_then(message_pid)
                            .is_ok_and(|former| former != pid)
                },
            );
            let former = message_pid(sys::event_message(runner).unwrap()).unwrap();
            runner == root && !Path::new(&format!("/proc/{root}/task/{former}")).exists()
        });
        assert!(released, "the exec did not release the former ID");
        assert_eq!(after, all_ended());
    }

    /// A first process that cannot enter the live set, because the set stays
    /// locked, is an error at once, and the tracing thread's end, which the error
    /// brings, kills the tree. The test holds the set itself, on the tracing
    /// thread, where `Mutex::lock` would panic or wait for good.
    #[test]
    fn a_first_process_that_cannot_enter_the_live_set_is_an_error_and_the_end_kills_it() {
        let (message, after) = end_tracing_thread("fork", Ending::Returns, |root| {
            let live = LiveProcesses::default();
            let held = lock(&live).unwrap();
            let tracked = track_root_within(&live, root, LIVE_SET_PATIENCE);
            drop(held);
            assert!(lock(&live).unwrap().is_empty());
            tracked.map_or_else(|error| error.to_string(), |()| String::new())
        });
        assert!(
            message.starts_with("could not lock the live processes of the traced tree"),
            "{message}"
        );
        assert_eq!(after, all_ended());
    }

    /// A first process that enters the live set is in it, as the set's first
    /// process, and stays alive until the tracing thread ends.
    #[test]
    fn a_first_process_that_enters_the_live_set_lives_until_the_tracing_thread_ends() {
        let (alive, after) = end_tracing_thread("fork", Ending::Returns, |root| {
            let live = LiveProcesses::default();
            track_root_within(&live, root, LIVE_SET_PATIENCE).unwrap();
            assert_eq!(lock(&live).unwrap().processes, BTreeSet::from([root]));
            assert_eq!(lock(&live).unwrap().first(), Some(root));
            !has_exited(root)
        });
        assert!(alive, "the first process did not live");
        assert_eq!(after, all_ended());
    }

    /// A watchdog that cannot lock the live set asks the tracing thread to end the
    /// run, and asks again until the loop has ended, so a wake that arrives while
    /// the thread does not wait yet is not lost: the wait that starts after it ends
    /// at once. The watchdog then returns the set's error, and the escalation is
    /// set.
    #[test]
    fn a_watchdog_that_cannot_lock_the_live_set_ends_a_wait_that_starts_after_its_wake() {
        let ((waited, watched, escalated), after) =
            end_tracing_thread("fork", Ending::Returns, |_| {
                let live = LiveProcesses::default();
                let escalation = Escalation::new().unwrap();
                let cancel = Cancellation::new();
                cancel.cancel();
                let held = lock(&live).unwrap();
                let (done_sender, done) = mpsc::channel::<()>();
                let (live, escalation, cancel) = (&live, &escalation, &cancel);
                let (waited, watched) = thread::scope(|scope| {
                    let watchdog = scope
                        .spawn(move || watch(live, cancel, escalation, &done, LIVE_SET_PATIENCE));
                    // The wakes arrive while this thread sleeps, which goes on
                    // after each of them, and does not wait for the tree.
                    let deadline = Instant::now().checked_add(PATIENCE).unwrap();
                    while !escalation.is_ended() {
                        assert!(
                            Instant::now() < deadline,
                            "the watchdog never asked the run to end"
                        );
                        thread::sleep(Duration::from_millis(1));
                    }
                    thread::sleep(Duration::from_millis(20));
                    let waited = sys::wait_pending(&|| escalation.is_ended())
                        .map(|change| change.map(|(pid, _)| pid))
                        .map_err(|error| error.to_string());
                    drop(done_sender);
                    let watched = watchdog.join().unwrap().map_err(|error| error.to_string());
                    (waited, watched)
                });
                drop(held);
                (waited, watched, escalation.is_escalated())
            });
        assert_eq!(
            waited,
            Err(sys::stopped_waiting().to_string()),
            "the wait did not end"
        );
        let watched = watched.unwrap_err();
        assert!(
            watched.starts_with("could not lock the live processes of the traced tree"),
            "{watched}"
        );
        assert!(escalated, "the escalation was not set");
        assert_eq!(after, all_ended());
    }

    /// The first process's ID and its child's, which the helper mode `escape-exit`
    /// writes to `marker`, once it has.
    fn read_marker(marker: &Path) -> (pid_t, pid_t) {
        let deadline = Instant::now().checked_add(PATIENCE).unwrap();
        loop {
            let ids = fs::read_to_string(marker).ok().and_then(|text| {
                let (root, child) = text.split_once(' ')?;
                Some((root.parse().ok()?, child.parse().ok()?))
            });
            if let Some(ids) = ids {
                return ids;
            }
            assert!(Instant::now() < deadline, "the helper wrote no marker");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// A whole run whose first process has exited and been collected, while a
    /// process that left its session waits, blocked, with no stop for the tracer:
    /// the test then holds the live set and cancels. The watchdog cannot lock the
    /// set, so it asks the tracing thread to end, which wakes the loop from its
    /// wait; the run returns the set's error at once, the end of the tracing thread
    /// has killed the escaped process, and both output pipes have ended, since the
    /// run waited for its readers. An unrelated child of the test process still
    /// runs: nothing signaled a process group or an ID.
    #[test]
    fn a_watchdog_that_cannot_lock_the_live_set_ends_the_run_and_its_escaped_process() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let mut request = helper("escape-exit");
        request.env.insert(
            OsString::from(MARKER_VARIABLE),
            marker.clone().into_os_string(),
        );
        let mut unrelated = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let program = Program::prepare(&request).unwrap();
            let filter = filter::program().unwrap();
            let (child_stdio, tracer_stdio) = capture::pipes(&request).unwrap();
            let (report, report_write) = sys::pipe().unwrap();
            let live = LiveProcesses::default();
            let cancel = Cancellation::new();
            let launch = Launch {
                request: &request,
                program: &program,
                filter: &filter,
                stdio: child_stdio,
                report: report_write,
            };
            let (release, released) = mpsc::channel::<()>();
            let (live, cancel, marker) = (&live, &cancel, &marker);
            let (supervised, child) = thread::scope(|scope| {
                let holder = scope.spawn(move || {
                    let (root, child) = read_marker(marker);
                    // The loop has collected the first process, and the child has
                    // left its session.
                    let deadline = Instant::now().checked_add(PATIENCE).unwrap();
                    while Path::new(&format!("/proc/{root}")).exists()
                        || session(child) != Some(child)
                    {
                        assert!(Instant::now() < deadline, "the helper did not settle");
                        thread::sleep(Duration::from_millis(1));
                    }
                    let held = lock(live).unwrap();
                    cancel.cancel();
                    released.recv().ok();
                    drop(held);
                    child
                });
                let supervised =
                    supervise(launch, tracer_stdio, None, cancel, live, LIVE_SET_PATIENCE);
                release.send(()).ok();
                (supervised, holder.join().ok())
            });
            drop(report);
            let supervised = supervised.map(|supervised| {
                (
                    supervised
                        .followed
                        .map(|_| ())
                        .map_err(|error| error.to_string()),
                    supervised.streams.is_ok(),
                )
            });
            sender.send((supervised, child)).ok();
        });
        let (supervised, child) = receiver
            .recv_timeout(PATIENCE.saturating_mul(2))
            .expect("the run did not end");
        let (followed, streams) = supervised.unwrap();
        let message = followed.unwrap_err();
        assert!(
            message.starts_with("could not lock the live processes of the traced tree"),
            "{message}"
        );
        assert!(streams, "an output pipe did not end");
        let child = child.expect("the test did not hold the live set");
        assert!(exits(child), "the escaped process survived");
        assert!(
            unrelated.try_wait().unwrap().is_none(),
            "the cleanup reached an unrelated child of the process"
        );
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
    }

    /// A child of the test that counts the `SIGTERM`s it receives: a shell that
    /// writes `TERM` to its standard output for each one, after it wrote `ready`
    /// once its trap was set. Dropping it kills and collects the child.
    struct CountingChild {
        /// The shell, a child of the test that the test alone collects.
        child: Child,
        /// Its process ID.
        pid: pid_t,
        /// The lines that it writes, after `ready`.
        lines: mpsc::Receiver<String>,
    }

    impl CountingChild {
        /// Start the child, with an explicit environment, and wait until its trap
        /// is set.
        fn start() -> Self {
            let mut child = Command::new("/bin/sh")
                .args([
                    "-c",
                    "trap 'echo TERM' TERM; echo ready; while :; do sleep 0.01; done",
                ])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let pid = pid_t::try_from(child.id()).unwrap();
            let stdout = child.stdout.take().unwrap();
            let (sender, lines) = mpsc::channel();
            thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if sender.send(line).is_err() {
                        break;
                    }
                }
            });
            let counting = Self { child, pid, lines };
            assert_eq!(
                counting.lines.recv_timeout(PATIENCE).as_deref(),
                Ok("ready"),
                "the child did not set its trap"
            );
            counting
        }

        /// Assert that the child receives exactly `count` more `SIGTERM`s: each
        /// one within the test's patience, and no other within [`SETTLE`].
        fn assert_received(
            &self,
            count: usize,
            who: &str,
        ) {
            for _ in 0..count {
                assert_eq!(
                    self.lines.recv_timeout(PATIENCE).as_deref(),
                    Ok("TERM"),
                    "{who} did not receive the signal"
                );
            }
            assert_eq!(
                self.lines.recv_timeout(SETTLE).ok(),
                None,
                "{who} received one signal too many"
            );
        }
    }

    impl Drop for CountingChild {
        /// Kill and collect the child, which the test has not collected before.
        fn drop(&mut self) {
            drop(self.child.kill());
            drop(self.child.wait());
        }
    }

    /// A forwarded signal goes to the first process while the live set knows it,
    /// and no other process; once the loop has collected the first process's end,
    /// it goes once to every live process, also to a later process of the tree
    /// under the first process's former ID, which the live set never takes for the
    /// first process (Audit 1's P1).
    ///
    /// The test replays the live set's notes as the loop makes them, at the start,
    /// when it adopts a process, and when it collects an end, with two children of
    /// its own, one under the ID that the first process had. So it needs no real
    /// reuse of an ID, and it signals only its own live children, which it has not
    /// collected.
    #[test]
    fn a_forwarded_signal_never_takes_a_later_process_under_the_first_id_for_the_first() {
        let first = CountingChild::start();
        let other = CountingChild::start();
        let live = LiveProcesses::default();
        track_root_within(&live, first.pid, LIVE_SET_PATIENCE).unwrap();
        lock(&live).unwrap().enter(other.pid);
        signal_first(&live, libc::SIGTERM, LIVE_SET_PATIENCE).unwrap();
        first.assert_received(1, "the first process");
        other.assert_received(0, "another process, while the first lives,");
        // The loop collects the first process's end, and Linux gives its ID to a
        // later process of the tree, which the loop adopts.
        lock(&live).unwrap().ended(first.pid);
        lock(&live).unwrap().enter(first.pid);
        assert_eq!(lock(&live).unwrap().first(), None);
        assert_eq!(
            lock(&live).unwrap().processes,
            BTreeSet::from([first.pid, other.pid])
        );
        signal_first(&live, libc::SIGTERM, LIVE_SET_PATIENCE).unwrap();
        first.assert_received(1, "the later process under the first process's ID");
        other.assert_received(1, "another process, after the first ended,");
    }

    /// The run's exit status is the first process's own end, and the end of a later
    /// process of the tree under the same ID never sets or replaces it, with or
    /// without the process tree; with it, each of the two processes keeps its own
    /// status (Audit 1's P3).
    ///
    /// The test replays, through the loop's handling of creation events and exits,
    /// and the live set's notes as the loop collects each end, a first process that
    /// forks a child and exits with code 3, and a later process that the child
    /// forks under the first process's ID, which `SIGKILL` ends. So it needs no
    /// real reuse of an ID.
    #[test]
    fn a_later_process_under_the_first_id_never_sets_the_exit_status_of_the_run() {
        // IDs above Linux's largest process ID, 2^22, so they name no process; the
        // loop's handling of a fork event and of an exit makes no system call.
        const FIRST: pid_t = 5_000_001;
        const CHILD: pid_t = 5_000_002;
        // The wait status of an exit with code 3, which the second byte holds.
        const EXITED_3: c_int = 0x0300;
        // The wait status of an end by `SIGKILL`, which the low bits hold.
        const KILLED: c_int = libc::SIGKILL;
        // The wait status of an exit with code 0.
        const EXITED_0: c_int = 0;
        for keep_tree in [false, true] {
            let live = LiveProcesses::default();
            let escalation = Escalation::new().unwrap();
            let mut tree = Tree::new(FIRST, &live, &escalation).unwrap();
            if keep_tree {
                tree.keep_processes(&helper("none"));
            }
            tree.created(FIRST, CHILD, libc::PTRACE_EVENT_FORK).unwrap();
            lock(&live).unwrap().ended(FIRST);
            tree.handle(FIRST, EXITED_3).unwrap();
            tree.created(CHILD, FIRST, libc::PTRACE_EVENT_FORK).unwrap();
            assert_eq!(lock(&live).unwrap().first(), None);
            lock(&live).unwrap().ended(FIRST);
            tree.handle(FIRST, KILLED).unwrap();
            lock(&live).unwrap().ended(CHILD);
            tree.handle(CHILD, EXITED_0).unwrap();
            assert!(lock(&live).unwrap().is_empty());
            assert_eq!(
                tree.outcome.termination,
                Some(Termination::Exited(3)),
                "with the process tree: {keep_tree}"
            );
            let Some(processes) = tree.processes.take() else {
                assert!(!keep_tree, "the loop kept no process tree");
                continue;
            };
            assert!(
                keep_tree,
                "the loop kept a process tree it was not asked for"
            );
            let shape: Vec<(u32, Option<u32>, Option<Termination>)> = processes
                .into_tree()
                .processes
                .iter()
                .map(|process| (process.number, process.pid, process.status))
                .collect();
            assert_eq!(shape, [
                (1, Some(5_000_001), Some(Termination::Exited(3))),
                (2, Some(5_000_002), Some(Termination::Exited(0))),
                (
                    3,
                    Some(5_000_001),
                    Some(Termination::Signaled(libc::SIGKILL))
                ),
            ]);
        }
    }
}
