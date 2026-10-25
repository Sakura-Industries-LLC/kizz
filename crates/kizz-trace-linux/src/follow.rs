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
//! A call that a signal interrupts reaches that stop too, with `EINTR` or one of the
//! kernel's internal restart errors, and is recorded like a failed call, so a call
//! that returns `EINTR` without restarting keeps its lookup; a call that restarts
//! stops at its entry again and is decoded again.
//! A thread that runs `exec` from a thread that does not lead its process keeps its
//! call under the ID it takes over, and a thread that exits inside a call records
//! the call with its result unknown.

use std::{
    collections::{BTreeMap, BTreeSet, btree_map},
    io, mem,
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};

use kizz_trace::{Accesses, IncompleteReason, Termination};
use libc::{c_int, c_long, c_ulong, pid_t};

use crate::{decode, filter, sys};

/// The signal of a stop at the exit of a system call, with
/// `PTRACE_O_TRACESYSGOOD`: `SIGTRAP` with the high bit set.
const SYSTEM_CALL_STOP: c_int = libc::SIGTRAP | 0x80;

/// The process IDs of the live processes of a traced tree, shared with the
/// cancellation watchdog. It never holds the ID of a thread that does not lead its
/// process.
pub(crate) type LiveProcesses = Mutex<BTreeSet<pid_t>>;

/// Lock the live processes, recovering the set if a thread panicked while holding
/// the lock.
pub(crate) fn lock(live: &LiveProcesses) -> MutexGuard<'_, BTreeSet<pid_t>> {
    live.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether the grace period after cancellation has ended with some of the tree
/// still running, so the tracer kills the tree. Shared by the event loop and the
/// cancellation watchdog, which sets it.
#[derive(Debug)]
pub(crate) struct Escalation(AtomicBool);

impl Escalation {
    /// A flag that is not set: the tree is not being killed.
    pub(crate) const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    /// Note that the tree is being killed. Set it before the live processes are
    /// killed, so a process that the loop adds to the live set after they were
    /// killed is killed by the loop.
    pub(crate) fn escalate(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether the tree is being killed.
    pub(crate) fn is_escalated(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Send `signal` to every live process.
///
/// The set stays locked while the signals are sent, so the loop cannot collect the
/// exit of a process in between, and every ID still names a process of the tree.
fn signal_all(
    live: &LiveProcesses,
    signal: c_int,
) {
    for &pid in lock(live).iter() {
        // A process that already exited needs no signal.
        drop(sys::kill(pid, signal));
    }
}

/// Interrupt the tree after cancellation: send `SIGINT` once to every live process,
/// under the rules of [`signal_all`].
pub(crate) fn interrupt_all(live: &LiveProcesses) {
    signal_all(live, libc::SIGINT);
}

/// Send `SIGKILL` to every live process, which kills all of its threads, under the
/// rules of [`signal_all`].
pub(crate) fn kill_all(live: &LiveProcesses) {
    signal_all(live, libc::SIGKILL);
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
    /// The tracked system call the thread is in, decoded at its entry, until the
    /// loop reads its result.
    pending: Option<decode::Call>,
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
    /// The first process, whose end is the command's termination.
    root: pid_t,
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
/// thread's ID was never in the set.
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
    let mut live = lock(live);
    let status = sys::collect(pid)?;
    if let Some(status) = status
        && ended(status)
    {
        live.remove(&pid);
    }
    Ok(status)
}

/// Take the next state change of a thread of the tree, or return `None` when the
/// calling thread has nothing left to wait for, which means that every process and
/// thread of the tree has exited and been collected.
fn next_change(live: &LiveProcesses) -> io::Result<Option<Change>> {
    let Some((pid, pending)) = sys::wait_pending()? else {
        return Ok(None);
    };
    Ok(Some((pid, take(live, pid, pending)?)))
}

impl<'a> Tree<'a> {
    /// The state of a tree whose first process is `root`, already resumed.
    fn new(
        root: pid_t,
        live: &'a LiveProcesses,
        escalation: &'a Escalation,
    ) -> Self {
        lock(live).insert(root);
        Self {
            root,
            tracees: BTreeMap::from([(root, Tracee::new(false, true))]),
            exited_unannounced: BTreeSet::new(),
            live,
            escalation,
            killed: false,
            outcome: Outcome {
                termination: None,
                accesses: Accesses::new(),
                incomplete: None,
            },
        }
    }

    /// Start following a thread, unless it is already followed. A thread that
    /// leads its process brings the process into the live set.
    fn adopt(
        &mut self,
        pid: pid_t,
        tracee: Tracee,
        leader: bool,
    ) {
        if let btree_map::Entry::Vacant(entry) = self.tracees.entry(pid) {
            entry.insert(tracee);
            if leader {
                lock(self.live).insert(pid);
            }
        }
    }

    /// Stop following a thread that no longer exists, and return the tracked call
    /// it was in.
    ///
    /// Only the former ID of a thread that ran `exec` disappears this way, and
    /// that thread did not lead its process, so its ID was never in the live set.
    fn forget(
        &mut self,
        pid: pid_t,
    ) -> Option<decode::Call> {
        self.tracees.remove(&pid).and_then(|tracee| tracee.pending)
    }

    /// Note why the trace is incomplete, unless an earlier reason was noted.
    fn incomplete(
        &mut self,
        reason: IncompleteReason,
    ) {
        if self.outcome.incomplete.is_none() {
            self.outcome.incomplete = Some(reason);
        }
    }

    /// Record the paths of a tracked call that returned `result`, or whose result
    /// was never seen.
    fn finish(
        &mut self,
        call: decode::Call,
        result: Option<i64>,
    ) {
        if let Some(detail) = call.finish(result, &mut self.outcome.accesses) {
            self.incomplete(IncompleteReason::Other { detail });
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
        while let Some((pid, status)) = next_change(self.live)? {
            if let Some(status) = status {
                self.handle(pid, status)?;
            }
        }
        // Every thread has been collected; an ID left behind is one whose
        // disappearance the loop could not read, and must never be signaled.
        self.tracees.clear();
        lock(self.live).clear();
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
        if let Some(call) = tracee.and_then(|tracee| tracee.pending) {
            // The thread ended inside the call, which may have taken effect.
            self.finish(call, None);
        }
        if pid == self.root {
            self.outcome.termination = Some(if libc::WIFEXITED(status) {
                Termination::Exited(libc::WEXITSTATUS(status))
            } else {
                Termination::Signaled(libc::WTERMSIG(status))
            });
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
            kill_all(self.live);
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
            let leader = leads_its_process(pid);
            self.adopt(pid, Tracee::new(true, false), leader);
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
            // A group stop: let the thread continue.
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
        let call = self
            .tracees
            .get_mut(&pid)
            .and_then(|tracee| tracee.pending.take());
        if let Some(mut call) = call {
            // Without the registers the result is unknown, so the call is recorded
            // as one that may or may not have taken effect.
            let result = sys::registers(pid)
                .ok()
                .map(|registers| i64::from_ne_bytes(registers.rax.to_ne_bytes()));
            // A descriptor the call returned names the object it actually opened.
            call.opened(result, &sys::Thread(pid));
            self.finish(call, result);
        }
        unless_vanished(sys::resume(pid, 0))
    }

    /// Decode the tracked system call a thread is entering at a seccomp stop, and
    /// keep it until the call returns.
    fn system_call_entry(
        &mut self,
        pid: pid_t,
    ) {
        let registers = match sys::registers(pid) {
            Ok(registers) => registers,
            // A thread that vanished makes no call.
            Err(error) if vanished(&error) => return,
            Err(error) => {
                self.incomplete(IncompleteReason::Other {
                    detail: format!(
                        "could not read the registers of thread {pid} at a system call: {error}"
                    ),
                });
                return;
            },
        };
        let arguments = [
            registers.rdi,
            registers.rsi,
            registers.rdx,
            registers.r10,
            registers.r8,
            registers.r9,
        ];
        let Some(call) = c_long::try_from(registers.orig_rax)
            .ok()
            .and_then(|number| decode::enter(number, arguments, &sys::Thread(pid)))
        else {
            return;
        };
        let earlier = self
            .tracees
            .get_mut(&pid)
            .and_then(|tracee| tracee.pending.replace(call));
        if let Some(earlier) = earlier {
            // A call whose exit was never seen; it cannot happen when every stop
            // is handled, and is recorded as one whose result is unknown.
            self.finish(earlier, None);
        }
    }

    /// Follow a thread or process that a thread just created at an `event`.
    ///
    /// After the escalation a new thread is killed at once: its first stop may
    /// still be pending, and nothing else would signal it before then. During the
    /// grace period after cancellation it runs, as it does without cancellation.
    fn created(
        &mut self,
        child: pid_t,
        event: c_int,
    ) -> io::Result<()> {
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
        self.adopt(child, Tracee::new(true, true), leader);
        if self.escalation.is_escalated() {
            // The new thread has not run since it was created, so it cannot have
            // run `exec`, and its ID still names it.
            return unless_vanished(sys::kill(child, libc::SIGKILL));
        }
        Ok(())
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
                self.created(message_pid(message)?, event)?;
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
                        self.finish(ended, None);
                    }
                }
            },
            libc::PTRACE_EVENT_SECCOMP if !escalated => self.seccomp_stop(pid, message),
            _ => {},
        }
        if escalated {
            return self.kill(pid);
        }
        self.resume(pid, 0)
    }

    /// Handle a seccomp stop with the filter's return data. A tracked call is
    /// decoded, and the thread is resumed to its exit. A non-native call marks the
    /// run as incompletely traced.
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
            Ok(filter::DATA_FOREIGN_ARCH) => "the 32-bit `int 0x80` interface",
            Ok(filter::DATA_X32) => "the x32 interface",
            Ok(_) | Err(_) => return,
        };
        if self.outcome.incomplete.is_some() {
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
        self.outcome.incomplete = Some(IncompleteReason::NonNativeSystemCall {
            detail: format!("process {pid} made {call} through {interface}"),
        });
    }
}

/// Follow the tree whose first process is `root` until all of it has exited.
///
/// `root` must be a resumed tracee, and the only child, of the calling thread.
/// Until `escalation` is set, the loop follows the tree in full, also during the
/// grace period after cancellation; once it is set, every thread the loop sees is
/// killed.
///
/// # Errors
///
/// Returns the error of a wait or a `ptrace` request. The tree may then still be
/// running; the caller kills and collects it with [`abandon`].
pub(crate) fn follow(
    root: pid_t,
    live: &LiveProcesses,
    escalation: &Escalation,
) -> io::Result<Outcome> {
    let mut tree = Tree::new(root, live, escalation);
    tree.run()?;
    Ok(tree.outcome)
}

/// Kill every thread of a tree the loop could not follow to its end, including
/// threads whose creation it never saw, and collect each one.
pub(crate) fn abandon(live: &LiveProcesses) {
    kill_all(live);
    // Every thread still to report belongs to the tree; a stopped one is killed,
    // and the waits go on until nothing is left or waiting fails.
    while let Ok(Some((pid, status))) = next_change(live) {
        if status.is_some_and(|status| libc::WIFSTOPPED(status)) {
            // The thread is in a stop that was just reported and not resumed, so
            // it cannot have run `exec`, and its ID still names it.
            drop(sys::kill(pid, libc::SIGKILL));
        }
    }
    lock(live).clear();
}

/// Tests that drive the loop by hand with real traced processes, to stop it at
/// exactly the moment a test needs.
#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        ffi::OsString,
        fs,
        io::{Read as _, Write as _},
        os::fd::OwnedFd,
        path::Path,
        ptr,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    use kizz_trace::{AbsolutePath, CaptureMode, EchoSink, RunRequest};
    use libc::c_long;

    use super::*;
    use crate::{
        capture,
        spawn::{self, Program, Started},
    };

    /// The variable that selects the helper mode of this test executable.
    const HELPER_VARIABLE: &str = "KIZZ_TRACE_UNIT_TEST_HELPER";
    /// How long a test waits for something another process does.
    const PATIENCE: Duration = Duration::from_secs(30);

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
        let started = spawn::start(request, &program, &filter, &child_stdio, report_write);
        drop(child_stdio);
        let Ok(Started::Running(root)) = started else {
            panic!("the helper did not start: {started:?}");
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
        for &pid in lock(live).iter() {
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
        assert!(lock(live).is_empty());
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
        let escalation = Escalation::new();
        let mut tree = Tree::new(traced.root, &live, &escalation);
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
            Handover::Watchdog => kill_all(&live),
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
        let escalation = Escalation::new();
        let mut tree = Tree::new(root, &live, &escalation);
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
        assert_eq!(*lock(&live), BTreeSet::from([root]));
        let signal = match handover {
            Handover::Event => {
                escalation.escalate();
                tree.handle(runner, status).unwrap();
                assert!(!tree.tracees.contains_key(&former));
                libc::SIGKILL
            },
            Handover::Watchdog => {
                escalation.escalate();
                kill_all(&live);
                libc::SIGKILL
            },
            Handover::Interrupt => {
                // The program the thread ran, `sleep`, ends at the interrupt, which
                // the loop delivers during the grace period.
                interrupt_all(&live);
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
        let escalation = Escalation::new();
        let mut tree = Tree::new(root, &live, &escalation);
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
        interrupt_all(&live);
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
        let escalation = Escalation::new();
        let mut tree = Tree::new(traced.root, &live, &escalation);
        let (creator, status) = drive_until(&mut tree, &live, "clone event", |_, status| {
            is_event_stop(status, libc::PTRACE_EVENT_CLONE)
        });
        let created = message_pid(sys::event_message(creator).unwrap()).unwrap();
        tree.handle(creator, status).unwrap();
        assert!(tree.tracees.contains_key(&created));
        assert!(!lock(&live).contains(&created));
        assert_eq!(*lock(&live), BTreeSet::from([traced.root]));
        assert_only_processes_are_live(&tree, &live);
        escalation.escalate();
        kill_all(&live);
        finish_cancelled(tree, &live, &traced, &[created], libc::SIGKILL);
    }
}
