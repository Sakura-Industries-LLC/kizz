// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The tracer's bookkeeping of the processes and programs of a traced tree, which
//! the event loop keeps only when the request asks for the process tree.
//!
//! The loop tells it what it sees, by thread ID: each creation event, each first
//! stop of a thread that it did not follow yet, each successful `exec`, by the
//! leader or by another thread that takes over the leader's ID, and each exit. It
//! asks it which program a thread's process runs when a call starts, and attributes
//! to that program what the call touched, its connections, the unknown system calls
//! and the reasons that the trace is incomplete. Nothing here makes a system call,
//! so unit tests replay events without real processes.
//!
//! The rules:
//!
//! * Each process is a thread group: a clone with `CLONE_THREAD` makes a thread of its
//!   creator's process, and a fork, a vfork, or a clone without it a new process, whose
//!   parent is the creator's process.
//! * Processes are numbered from 1 in the order they are first seen, by their creation
//!   event or their first stop, and the command is process 1.
//! * A new process first runs its creator's program, with the same path and arguments,
//!   which it lists as inherited only when something is attributed to it.
//! * A process whose first stop comes before its creation event has no parent and no
//!   known program until the event arrives, which gives it both; what it did in between
//!   belongs to the program that the event names. If the event never arrives, the process
//!   keeps no parent, and its program is taken to be the command's.
//! * A successful `exec` adds a program to the thread's process, which a thread that
//!   takes over the leader's ID keeps; the former ID of that thread names nothing
//!   afterwards.
//! * A process's status is how its leader's exit ended it. Once the leader has exited,
//!   its ID names nothing, so a new process that the system gives the same ID is a
//!   process of its own, with a new number.
//! * The command's process runs the program of the request until its first `exec`, the
//!   start of the program, which is listed only when something is attributed to it, as
//!   when the start fails.

use std::{collections::BTreeMap, ffi::OsString};

use kizz_trace::{
    AbsolutePath, Connection, IncompleteReason, Observations, Process, ProcessTree, Program,
    RunRequest, Termination,
};
use libc::pid_t;

/// One program of the tree, by the index of its process and its own index in that
/// process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProgramId {
    /// The index of the process.
    process: usize,
    /// The index of the program in the process.
    program: usize,
}

/// How a thread created a thread or process, as its `ptrace` event says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Creation {
    /// A fork, which makes a process.
    Fork,
    /// A vfork, which makes a process.
    Vfork,
    /// A clone, which makes a thread of the creator's process with `CLONE_THREAD`,
    /// and a process without it.
    Clone {
        /// Whether the clone made a thread, with `CLONE_THREAD`.
        thread: bool,
    },
}

/// What the bookkeeping knows of one program.
#[derive(Debug)]
struct Entry {
    /// The program, with what is attributed to it.
    program: Program,
    /// Whether the program's path and arguments are known: not for the inherited
    /// program of a process whose creation event has not arrived yet.
    known: bool,
    /// Whether the program is listed even when nothing is attributed to it: one
    /// that an `exec` started.
    started: bool,
}

/// What the bookkeeping knows of one process.
#[derive(Debug)]
struct Record {
    /// The process ID.
    pid: pid_t,
    /// The index of the process that created it, when known.
    parent: Option<usize>,
    /// The programs it ran, in order; it always has one.
    programs: Vec<Entry>,
    /// How its leader's exit ended it, once the loop saw that.
    status: Option<Termination>,
}

/// The processes and programs of one traced tree.
#[derive(Debug)]
pub(crate) struct Processes {
    /// Every process seen, in the order it was first seen.
    records: Vec<Record>,
    /// The process of each live thread, by thread ID.
    threads: BTreeMap<pid_t, usize>,
    /// The processes whose first stop came before their creation event, by the ID
    /// of their leader, until the event arrives.
    unannounced: BTreeMap<pid_t, usize>,
    /// The path of the request's program.
    command: AbsolutePath,
    /// The arguments of the request.
    command_args: Vec<OsString>,
}

impl Processes {
    /// The bookkeeping of a tree whose first process, `root`, is about to run
    /// `program` with `args`, the request's.
    pub(crate) fn new(
        root: pid_t,
        program: AbsolutePath,
        args: Vec<OsString>,
    ) -> Self {
        let mut processes = Self {
            records: Vec::new(),
            threads: BTreeMap::new(),
            unannounced: BTreeMap::new(),
            command: program.clone(),
            command_args: args.clone(),
        };
        let index = processes.add(root, None, Entry {
            program: Program::new(program, args, false),
            known: true,
            started: false,
        });
        processes.threads.insert(root, index);
        processes
    }

    /// Add a process with the ID `pid`, created by the process `parent`, which runs
    /// `entry`, and return its index.
    fn add(
        &mut self,
        pid: pid_t,
        parent: Option<usize>,
        entry: Entry,
    ) -> usize {
        self.records.push(Record {
            pid,
            parent,
            programs: vec![entry],
            status: None,
        });
        self.records.len().saturating_sub(1)
    }

    /// The path and arguments of the program that the process `index` runs now,
    /// or the request's when it has none.
    fn identity(
        &self,
        index: usize,
    ) -> (AbsolutePath, Vec<OsString>) {
        self.records
            .get(index)
            .and_then(|record| record.programs.last())
            .map_or_else(
                || (self.command.clone(), self.command_args.clone()),
                |entry| (entry.program.path.clone(), entry.program.args.clone()),
            )
    }

    /// Whether `child` is a process whose first stop came before its creation event,
    /// which has not arrived yet.
    pub(crate) fn awaits_creation(
        &self,
        child: pid_t,
    ) -> bool {
        self.unannounced.contains_key(&child)
    }

    /// Note that the thread `creator` created the thread or process `child` as
    /// `how` says.
    pub(crate) fn created(
        &mut self,
        creator: pid_t,
        child: pid_t,
        how: Creation,
    ) {
        let parent = self.threads.get(&creator).copied();
        if let Some(index) = self.unannounced.remove(&child) {
            // Its first stop came first: the event gives it its parent and the
            // program it inherited.
            let identity = parent.map(|parent| self.identity(parent));
            if let Some(record) = self.records.get_mut(index) {
                record.parent = parent;
                if let (Some((path, args)), Some(entry)) = (identity, record.programs.first_mut())
                    && !entry.known
                {
                    entry.program.path = path;
                    entry.program.args = args;
                    entry.known = true;
                }
            }
            return;
        }
        if how == (Creation::Clone { thread: true }) {
            if let Some(parent) = parent {
                self.threads.insert(child, parent);
            }
            return;
        }
        if self.threads.contains_key(&child) {
            return;
        }
        let (path, args) = parent.map_or_else(
            || (self.command.clone(), self.command_args.clone()),
            |parent| self.identity(parent),
        );
        let index = self.add(child, parent, Entry {
            program: Program::new(path, args, true),
            known: true,
            started: false,
        });
        self.threads.insert(child, index);
    }

    /// Note the first stop of the thread `tid`, which the loop did not follow yet,
    /// and whose process is `process`, its thread group ID, or `None` when that
    /// could not be read. A thread of a known process joins it; any other thread
    /// leads a new process, whose creation event has not arrived yet.
    pub(crate) fn first_stop(
        &mut self,
        tid: pid_t,
        process: Option<pid_t>,
    ) {
        if self.threads.contains_key(&tid) {
            return;
        }
        let known = process
            .filter(|process| *process != tid)
            .and_then(|process| self.threads.get(&process).copied());
        if let Some(index) = known {
            self.threads.insert(tid, index);
            return;
        }
        let index = self.add(tid, None, Entry {
            program: Program::new(self.command.clone(), self.command_args.clone(), true),
            known: false,
            started: false,
        });
        self.threads.insert(tid, index);
        self.unannounced.insert(tid, index);
    }

    /// Note a successful `exec` that the thread `former` made and that now runs
    /// under the ID `tid`, the same ID unless `former` took over its leader's, and
    /// that started the program at `path` with `args`. A path that the tracer could
    /// not tell is taken to be that of the program that made the call. Return the
    /// new program, to which the `exec` itself belongs.
    pub(crate) fn exec(
        &mut self,
        tid: pid_t,
        former: pid_t,
        path: Option<AbsolutePath>,
        args: Vec<OsString>,
    ) -> Option<ProgramId> {
        let index = self
            .threads
            .get(&tid)
            .or_else(|| self.threads.get(&former))
            .copied()?;
        if former != tid {
            self.threads.remove(&former);
            self.threads.insert(tid, index);
        }
        let path = path.unwrap_or_else(|| self.identity(index).0);
        let record = self.records.get_mut(index)?;
        record.programs.push(Entry {
            program: Program::new(path, args, false),
            known: true,
            started: true,
        });
        Some(ProgramId {
            process: index,
            program: record.programs.len().saturating_sub(1),
        })
    }

    /// The program that the process of the thread `tid` runs now.
    pub(crate) fn current(
        &self,
        tid: pid_t,
    ) -> Option<ProgramId> {
        let process = self.threads.get(&tid).copied()?;
        let programs = self.records.get(process)?.programs.len();
        Some(ProgramId {
            process,
            program: programs.checked_sub(1)?,
        })
    }

    /// The program `id`.
    fn program(
        &mut self,
        id: ProgramId,
    ) -> Option<&mut Program> {
        self.records
            .get_mut(id.process)?
            .programs
            .get_mut(id.program)
            .map(|entry| &mut entry.program)
    }

    /// The observations of the program `id`, to record what one of its calls
    /// touched.
    pub(crate) fn observations(
        &mut self,
        id: ProgramId,
    ) -> Option<&mut Observations> {
        self.program(id).map(|program| &mut program.observations)
    }

    /// Attribute `connections` to the program `id`.
    pub(crate) fn connect(
        &mut self,
        id: ProgramId,
        connections: impl IntoIterator<Item = Connection>,
    ) {
        if let Some(program) = self.program(id) {
            program.connections.extend(connections);
        }
    }

    /// Attribute the unknown system call `number` to the program `id`.
    pub(crate) fn unknown(
        &mut self,
        id: ProgramId,
        number: u32,
    ) {
        if let Some(program) = self.program(id) {
            program.unknown_system_calls.insert(number);
        }
    }

    /// Attribute a reason that the trace is incomplete to the program `id`.
    pub(crate) fn incomplete(
        &mut self,
        id: ProgramId,
        reason: IncompleteReason,
    ) {
        if let Some(program) = self.program(id) {
            program.incomplete.push(reason);
        }
    }

    /// Note that the thread `tid` exited as `termination` says. When it leads its
    /// process, the process has ended with that status, and its ID names nothing
    /// from now on.
    pub(crate) fn exited(
        &mut self,
        tid: pid_t,
        termination: Termination,
    ) {
        let Some(index) = self.threads.remove(&tid) else {
            return;
        };
        if let Some(record) = self.records.get_mut(index)
            && record.pid == tid
        {
            record.status = Some(termination);
            self.threads.retain(|_, process| *process != index);
        }
    }

    /// The process tree: every process in the order it was first seen, numbered
    /// from 1, with each program that an `exec` started and each other program to
    /// which something is attributed.
    pub(crate) fn into_tree(self) -> ProcessTree {
        let number = |index: usize| u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX);
        let processes = self
            .records
            .into_iter()
            .enumerate()
            .map(|(index, record)| {
                Process {
                    number: number(index),
                    pid: u32::try_from(record.pid).ok(),
                    parent: record.parent.map(number),
                    programs: record
                        .programs
                        .into_iter()
                        .filter(|entry| entry.started || !entry.program.is_empty())
                        .map(|entry| entry.program)
                        .collect(),
                    status: record.status,
                }
            })
            .collect();
        ProcessTree { processes }
    }
}

/// The tree of a request whose command could not start: `tree`, the tree that the
/// loop followed, if there is one, with the command's process, the child that kizz
/// forked with the ID `pid` if there was one, as process 1, without a status, and
/// listing at least the request's program, with whatever the attempt touched.
pub(crate) fn failed_start(
    tree: Option<ProcessTree>,
    request: &RunRequest,
    pid: Option<pid_t>,
) -> ProcessTree {
    let mut tree = tree.unwrap_or_default();
    if tree.processes.is_empty() {
        tree.processes.push(Process {
            number: 1,
            pid: pid.and_then(|pid| u32::try_from(pid).ok()),
            parent: None,
            programs: Vec::new(),
            status: None,
        });
    }
    if let Some(command) = tree.processes.first_mut() {
        command.status = None;
        if command.programs.is_empty() {
            command.programs.push(Program::new(
                request.program.clone(),
                request.args.clone(),
                false,
            ));
        }
    }
    tree
}

/// Unit tests that replay the events of traced trees without real processes.
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use kizz_trace::{AccessKind, AccessKinds, CaptureMode, Outcome};

    use super::*;

    /// An absolute path for a test.
    fn path(text: &str) -> AbsolutePath {
        AbsolutePath::new(text).unwrap()
    }

    /// An argument vector for a test.
    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    /// The bookkeeping of a tree whose command, with the ID 100, runs `/bin/sh -c`,
    /// after the `exec` that starts it.
    fn started() -> Processes {
        let mut processes = Processes::new(100, path("/bin/sh"), args(&["sh", "-c"]));
        processes.exec(100, 100, Some(path("/bin/sh")), args(&["sh", "-c"]));
        processes
    }

    /// Record a read of `file` by the program `id`.
    fn read(
        processes: &mut Processes,
        id: ProgramId,
        file: &str,
    ) {
        processes
            .observations(id)
            .unwrap()
            .record(path(file), AccessKind::Read, Outcome::Ok);
    }

    /// The paths that a program observed, in order.
    fn observed(program: &Program) -> Vec<String> {
        program
            .observations
            .iter()
            .map(|(path, _)| path.to_string())
            .collect()
    }

    /// Each program of a process, as its path and whether it is inherited.
    fn programs(process: &Process) -> Vec<(String, bool)> {
        process
            .programs
            .iter()
            .map(|program| (program.path.to_string(), program.inherited))
            .collect()
    }

    /// A fork, a vfork and a clone without `CLONE_THREAD` each make a new process
    /// whose parent is the creator's process; a clone with `CLONE_THREAD` makes a
    /// thread of the creator's process, whose observations fold into it.
    #[test]
    fn creations_make_processes_and_threads() {
        let mut processes = started();
        processes.created(100, 101, Creation::Fork);
        processes.created(100, 102, Creation::Vfork);
        processes.created(100, 103, Creation::Clone { thread: false });
        processes.created(100, 104, Creation::Clone { thread: true });
        // A thread of process 2 creates a process too.
        processes.created(101, 105, Creation::Clone { thread: true });
        processes.created(105, 106, Creation::Fork);
        for (tid, file) in [
            (101, "/fork"),
            (102, "/vfork"),
            (103, "/clone"),
            (104, "/thread"),
            (106, "/grandchild"),
        ] {
            let id = processes.current(tid).unwrap();
            read(&mut processes, id, file);
        }
        let tree = processes.into_tree();
        let shape: Vec<(u32, Option<u32>, Option<u32>)> = tree
            .processes
            .iter()
            .map(|process| (process.number, process.pid, process.parent))
            .collect();
        assert_eq!(shape, [
            (1, Some(100), None),
            (2, Some(101), Some(1)),
            (3, Some(102), Some(1)),
            (4, Some(103), Some(1)),
            (5, Some(106), Some(2)),
        ]);
        assert_eq!(observed(&tree.processes[0].programs[0]), ["/thread"]);
        for (number, file) in [
            (2, "/fork"),
            (3, "/vfork"),
            (4, "/clone"),
            (5, "/grandchild"),
        ] {
            let process = tree.process(number).unwrap();
            assert_eq!(programs(process), [("/bin/sh".to_owned(), true)]);
            assert_eq!(process.programs[0].args, args(&["sh", "-c"]));
            assert_eq!(observed(&process.programs[0]), [file]);
        }
    }

    /// A successful `exec` by the leader, and one by another thread that takes over
    /// the leader's ID, each add a program to the same process; the former ID of
    /// that thread names nothing afterwards.
    #[test]
    fn execs_add_programs_to_their_process() {
        let mut processes = started();
        processes.created(100, 101, Creation::Fork);
        processes.exec(101, 101, Some(path("/bin/make")), args(&["make"]));
        processes.created(101, 102, Creation::Clone { thread: true });
        let cc = processes.exec(101, 102, Some(path("/bin/cc")), args(&["cc", "-c"]));
        assert_eq!(processes.current(101), cc);
        assert_eq!(processes.current(102), None);
        let tree = processes.into_tree();
        assert_eq!(tree.processes.len(), 2);
        let child = tree.process(2).unwrap();
        // The inherited shell is left out: nothing is attributed to it.
        assert_eq!(programs(child), [
            ("/bin/make".to_owned(), false),
            ("/bin/cc".to_owned(), false)
        ]);
        assert_eq!(child.programs[1].args, args(&["cc", "-c"]));
        assert_eq!(programs(&tree.processes[0]), [(
            "/bin/sh".to_owned(),
            false
        )]);
    }

    /// An observation goes to the program that ran when the call was made, a
    /// successful `exec`'s paths to the program that it starts, and a failed
    /// `exec`'s paths to the caller.
    #[test]
    fn observations_go_to_the_program_of_their_call() {
        let mut processes = started();
        processes.created(100, 101, Creation::Fork);
        // A call starts in the inherited shell, and its thread's process runs
        // another program by the time it returns.
        let at_entry = processes.current(101).unwrap();
        // A failed `exec` belongs to its caller, the inherited shell.
        let failed = processes.current(101).unwrap();
        processes.observations(failed).unwrap().record(
            path("/usr/local/bin/cat"),
            AccessKind::Probe,
            Outcome::Error("ENOENT".to_owned()),
        );
        let cat = processes
            .exec(101, 101, Some(path("/bin/cat")), args(&["cat", "x"]))
            .unwrap();
        processes.observations(cat).unwrap().record_kinds(
            path("/bin/cat"),
            AccessKinds::only(AccessKind::Exec),
            &Outcome::Ok,
        );
        read(&mut processes, at_entry, "/started-before");
        let now = processes.current(101).unwrap();
        read(&mut processes, now, "/x");
        let tree = processes.into_tree();
        let child = tree.process(2).unwrap();
        assert_eq!(programs(child), [
            ("/bin/sh".to_owned(), true),
            ("/bin/cat".to_owned(), false)
        ]);
        assert_eq!(observed(&child.programs[0]), [
            "/started-before",
            "/usr/local/bin/cat"
        ]);
        assert_eq!(observed(&child.programs[1]), ["/bin/cat", "/x"]);
    }

    /// An inherited program appears only when something is attributed to it, and
    /// so does the request's program before the command's first `exec`; a program
    /// that an `exec` started always appears.
    #[test]
    fn inherited_programs_appear_only_with_attributions() {
        let mut processes = Processes::new(100, path("/bin/sh"), args(&["sh"]));
        processes.created(100, 101, Creation::Fork);
        processes.created(100, 102, Creation::Fork);
        processes.created(100, 103, Creation::Fork);
        processes.created(100, 104, Creation::Fork);
        let connected = processes.current(102).unwrap();
        processes.connect(connected, [Connection::UnixAbstract(b"x".to_vec())]);
        let unknown = processes.current(103).unwrap();
        processes.unknown(unknown, 400);
        let incomplete = processes.current(104).unwrap();
        processes.incomplete(incomplete, IncompleteReason::Other {
            detail: "lost".to_owned(),
        });
        processes.exec(101, 101, None, Vec::new());
        let tree = processes.into_tree();
        // The command never ran `exec`, and nothing is attributed to it.
        assert_eq!(tree.processes[0].programs, Vec::<Program>::new());
        // An `exec` whose path is unknown names the program that made it.
        assert_eq!(programs(tree.process(2).unwrap()), [(
            "/bin/sh".to_owned(),
            false
        )]);
        for number in [3, 4, 5] {
            assert_eq!(programs(tree.process(number).unwrap()), [(
                "/bin/sh".to_owned(),
                true
            )]);
        }
    }

    /// A process ID that a new process gets after an exit within the run makes a
    /// new process with a new number, and a thread's exit leaves its process.
    #[test]
    fn a_reused_process_id_makes_a_new_process() {
        let mut processes = started();
        processes.created(100, 101, Creation::Fork);
        processes.created(101, 102, Creation::Clone { thread: true });
        processes.exited(102, Termination::Exited(0));
        assert!(processes.current(101).is_some());
        processes.exited(101, Termination::Exited(3));
        assert_eq!(processes.current(101), None);
        processes.created(100, 101, Creation::Fork);
        processes.exec(101, 101, Some(path("/bin/true")), args(&["true"]));
        processes.exited(101, Termination::Exited(0));
        let tree = processes.into_tree();
        let shape: Vec<(u32, Option<u32>, Option<Termination>)> = tree
            .processes
            .iter()
            .map(|process| (process.number, process.pid, process.status))
            .collect();
        assert_eq!(shape, [
            (1, Some(100), None),
            (2, Some(101), Some(Termination::Exited(3))),
            (3, Some(101), Some(Termination::Exited(0))),
        ]);
        assert_eq!(programs(tree.process(3).unwrap()), [(
            "/bin/true".to_owned(),
            false
        )]);
    }

    /// A process whose first stop comes before its creation event gets its parent,
    /// and the program it inherited, when the event arrives; what it did before
    /// belongs to that program. A thread whose first stop comes first joins its
    /// process.
    #[test]
    fn an_early_first_stop_gets_its_parent_from_the_event() {
        let mut processes = started();
        processes.created(100, 101, Creation::Fork);
        processes.exec(101, 101, Some(path("/bin/make")), args(&["make", "-j"]));
        processes.first_stop(102, Some(102));
        assert!(processes.awaits_creation(102));
        let early = processes.current(102).unwrap();
        read(&mut processes, early, "/early");
        // It even exits before the event.
        processes.exited(102, Termination::Exited(0));
        processes.created(101, 102, Creation::Fork);
        assert!(!processes.awaits_creation(102));
        processes.first_stop(103, Some(101));
        let thread = processes.current(103).unwrap();
        read(&mut processes, thread, "/thread");
        processes.created(101, 103, Creation::Clone { thread: true });
        // A process whose creation never arrives keeps no parent.
        processes.first_stop(104, None);
        let tree = processes.into_tree();
        let late = tree.process(3).unwrap();
        assert_eq!(late.parent, Some(2));
        assert_eq!(late.status, Some(Termination::Exited(0)));
        assert_eq!(programs(late), [("/bin/make".to_owned(), true)]);
        assert_eq!(late.programs[0].args, args(&["make", "-j"]));
        assert_eq!(observed(&late.programs[0]), ["/early"]);
        let make = tree.process(2).unwrap();
        assert_eq!(observed(&make.programs[0]), ["/thread"]);
        let orphan = tree.process(4).unwrap();
        assert_eq!((orphan.parent, orphan.programs.len()), (None, 0));
        assert_eq!(tree.processes.len(), 4);
    }

    /// Statuses, connections, unknown numbers and every incomplete reason land on
    /// the right process and program, and only a leader's exit gives a status.
    #[test]
    fn everything_lands_on_its_process_and_program() {
        let mut processes = started();
        processes.created(100, 101, Creation::Fork);
        let make = processes
            .exec(101, 101, Some(path("/bin/make")), Vec::new())
            .unwrap();
        processes.created(101, 105, Creation::Clone { thread: true });
        let thread = processes.current(105).unwrap();
        assert_eq!(thread, make);
        processes.connect(thread, [Connection::UnixAbstract(b"make".to_vec())]);
        processes.unknown(thread, 400);
        processes.unknown(make, 400);
        for detail in ["first", "second"] {
            processes.incomplete(make, IncompleteReason::Other {
                detail: detail.to_owned(),
            });
        }
        let shell = processes.current(100).unwrap();
        processes.incomplete(shell, IncompleteReason::UntraceableSystemCall {
            detail: "made `chroot`".to_owned(),
        });
        processes.unknown(shell, 1_000_000);
        processes.exited(105, Termination::Exited(7));
        processes.exited(101, Termination::Signaled(9));
        processes.exited(100, Termination::Exited(0));
        let tree = processes.into_tree();
        let root = &tree.processes[0];
        assert_eq!(root.status, Some(Termination::Exited(0)));
        assert_eq!(root.programs[0].unknown_system_calls, [1_000_000].into());
        assert_eq!(root.programs[0].incomplete.len(), 1);
        assert!(root.programs[0].connections.is_empty());
        let child = tree.process(2).unwrap();
        assert_eq!(child.status, Some(Termination::Signaled(9)));
        let program = &child.programs[0];
        assert_eq!(program.unknown_system_calls, [400].into());
        assert_eq!(
            program.connections,
            [Connection::UnixAbstract(b"make".to_vec())].into()
        );
        let details: Vec<String> = program.incomplete.iter().map(ToString::to_string).collect();
        assert_eq!(details, ["first", "second"]);
        assert_eq!(tree.unknown_system_calls(), [400, 1_000_000].into());
    }

    /// A failed start's tree has the command's process without a status, listing
    /// the request's program with what the attempt touched, or alone when the loop
    /// followed nothing.
    #[test]
    fn a_failed_start_lists_the_request_program() {
        let request = RunRequest {
            program: path("/bin/missing"),
            args: args(&["missing", "-v"]),
            env: BTreeMap::new(),
            cwd: path("/"),
            stdin: None,
            capture: CaptureMode::Split,
            echo: false,
            tree: true,
        };
        let mut processes = Processes::new(100, request.program.clone(), request.args.clone());
        let attempt = processes.current(100).unwrap();
        processes.observations(attempt).unwrap().record(
            path("/bin/missing"),
            AccessKind::Probe,
            Outcome::Error("ENOENT".to_owned()),
        );
        processes.exited(100, Termination::Exited(127));
        let tree = failed_start(Some(processes.into_tree()), &request, Some(100));
        let [command] = tree.processes.as_slice() else {
            panic!("expected one process: {tree:?}");
        };
        assert_eq!((command.pid, command.status), (Some(100), None));
        assert_eq!(programs(command), [("/bin/missing".to_owned(), false)]);
        assert_eq!(command.programs[0].args, request.args);
        assert_eq!(observed(&command.programs[0]), ["/bin/missing"]);

        let untouched = Processes::new(100, request.program.clone(), request.args.clone());
        let tree = failed_start(Some(untouched.into_tree()), &request, Some(100));
        assert_eq!(programs(&tree.processes[0]), [(
            "/bin/missing".to_owned(),
            false
        )]);
        let never_forked = failed_start(None, &request, None);
        assert_eq!(never_forked.processes.len(), 1);
        assert_eq!(never_forked.processes[0].pid, None);
        assert_eq!(never_forked.processes[0].programs[0].path, request.program);
    }
}
