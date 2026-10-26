// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! A trivial fake implements the [`Tracer`] trait, as the engine's tests will.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::print_stderr,
    clippy::print_stdout,
    reason = "test code: the tests assert, lock and print"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

use kizz_trace::{
    AbsolutePath, AccessKind, AccessKinds, Accesses, Cancellation, CaptureMode, CapturedOutput,
    Connection, Disk, EchoSink, ExtendedAttributes, FileSystem, FirstReads, Kernel, MachineState,
    Memory, OutputStream, ProcessTree, ResourceLimit, RunRequest, RunResult, Termination,
    TraceError, Tracer,
};

/// The signal number the fake reports for a cancelled request.
const SIGKILL: i32 = 9;

/// What the fake does when it runs one program.
#[derive(Debug, Clone)]
struct Script {
    /// The chunks of output the program writes, in order.
    output: Vec<(OutputStream, Vec<u8>)>,
    /// The accesses the program makes, in order.
    accesses: Vec<(AbsolutePath, AccessKind)>,
    /// How the program ends.
    termination: Termination,
    /// The system calls the program makes that the tracer does not know.
    unknown_system_calls: BTreeSet<u32>,
    /// The connections the program makes.
    connections: BTreeSet<Connection>,
}

/// A tracer that runs nothing and reports scripted behavior.
#[derive(Debug, Default)]
struct FakeTracer {
    /// The script of each known program.
    scripts: BTreeMap<AbsolutePath, Script>,
    /// Every request the fake received, in order.
    requests: Mutex<Vec<RunRequest>>,
    /// The extended attributes of each path, by the path and whether a final link
    /// is followed, which the attribute query answers.
    attributes: BTreeMap<(PathBuf, bool), ExtendedAttributes>,
    /// The machine state that the machine query answers.
    machine: MachineState,
    /// Every working directory the machine query was asked about, in order.
    machine_queries: Mutex<Vec<PathBuf>>,
}

impl FakeTracer {
    /// Add the script of one program.
    fn with_script(
        mut self,
        program: AbsolutePath,
        script: Script,
    ) -> Self {
        self.scripts.insert(program, script);
        self
    }

    /// The requests received so far.
    fn requests(&self) -> Vec<RunRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Tracer for FakeTracer {
    fn run(
        &self,
        request: &RunRequest,
        echo: &dyn EchoSink,
        cancel: &Cancellation,
    ) -> Result<RunResult, TraceError> {
        self.requests
            .lock()
            .map_err(|_| {
                TraceError::Tracer {
                    message: "request log poisoned".to_owned(),
                    source: None,
                }
            })?
            .push(request.clone());

        let Some(script) = self.scripts.get(&request.program) else {
            let mut accesses = Accesses::new();
            accesses.record(request.program.clone(), AccessKind::Probe);
            return Err(TraceError::ProgramNotFound {
                program: request.program.clone(),
                tree: request
                    .tree
                    .then(|| ProcessTree::of_request(request, &accesses, None)),
                accesses,
            });
        };

        let mut accesses = Accesses::new();
        let mut first_reads = FirstReads::new();
        first_reads.note_at(&request.program, &accesses);
        accesses.record(request.program.clone(), AccessKind::Exec);
        if cancel.is_cancelled() {
            return Ok(RunResult {
                termination: Termination::Signaled(SIGKILL),
                output: CapturedOutput::empty(request.capture),
                tree: request.tree.then(|| {
                    ProcessTree::of_request(
                        request,
                        &accesses,
                        Some(Termination::Signaled(SIGKILL)),
                    )
                }),
                accesses,
                incomplete: None,
                first_reads,
                unknown_system_calls: BTreeSet::new(),
                connections: BTreeSet::new(),
            });
        }

        let mut output = CapturedOutput::empty(request.capture);
        for (stream, bytes) in &script.output {
            let echoed = match &mut output {
                CapturedOutput::Split { stdout, stderr } => {
                    if *stream == OutputStream::Stderr {
                        stderr.extend_from_slice(bytes);
                    } else {
                        stdout.extend_from_slice(bytes);
                    }
                    *stream
                },
                CapturedOutput::Merged { output } => {
                    output.extend_from_slice(bytes);
                    OutputStream::Merged
                },
            };
            if request.echo {
                // A failed echo never fails the run.
                echo.echo(echoed, bytes).ok();
            }
        }
        for (path, kind) in &script.accesses {
            // A read or a run notes the file's metadata first, as the Linux tracer
            // does.
            if matches!(kind, AccessKind::Read | AccessKind::Exec) {
                first_reads.note_at(path, &accesses);
            }
            accesses.record(path.clone(), *kind);
        }

        Ok(RunResult {
            termination: script.termination,
            output,
            tree: request
                .tree
                .then(|| ProcessTree::of_request(request, &accesses, Some(script.termination))),
            accesses,
            incomplete: None,
            first_reads,
            unknown_system_calls: script.unknown_system_calls.clone(),
            connections: script.connections.clone(),
        })
    }

    fn extended_attributes(
        &self,
        path: &Path,
        follow: bool,
    ) -> io::Result<ExtendedAttributes> {
        self.attributes
            .get(&(path.to_path_buf(), follow))
            .cloned()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }

    fn machine_state(
        &self,
        cwd: &Path,
    ) -> MachineState {
        self.machine_queries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(cwd.to_path_buf());
        self.machine.clone()
    }
}

/// An echo sink that keeps every chunk it receives.
#[derive(Debug, Default)]
struct RecordingEcho {
    /// The chunks received, in order.
    chunks: Mutex<Vec<(OutputStream, Vec<u8>)>>,
}

impl RecordingEcho {
    /// The chunks received so far.
    fn chunks(&self) -> Vec<(OutputStream, Vec<u8>)> {
        self.chunks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl EchoSink for RecordingEcho {
    fn echo(
        &self,
        stream: OutputStream,
        bytes: &[u8],
    ) -> io::Result<()> {
        self.chunks
            .lock()
            .map_err(|_| io::Error::other("echo log poisoned"))?
            .push((stream, bytes.to_vec()));
        Ok(())
    }
}

/// Build an absolute path for a test.
fn path(text: &str) -> Result<AbsolutePath, kizz_trace::NotAbsoluteError> {
    AbsolutePath::new(text)
}

/// The fake's script for `/bin/tool`: two chunks of output, three accesses.
fn tool_script() -> Result<Script, kizz_trace::NotAbsoluteError> {
    Ok(Script {
        output: vec![
            (OutputStream::Stdout, b"out ".to_vec()),
            (OutputStream::Stderr, b"err ".to_vec()),
            (OutputStream::Stdout, b"done".to_vec()),
        ],
        accesses: vec![
            (path("/work/input.txt")?, AccessKind::Read),
            (path("/work/./input.txt")?, AccessKind::Probe),
            (path("/work/out//result.txt")?, AccessKind::Write),
        ],
        termination: Termination::Exited(3),
        unknown_system_calls: BTreeSet::from([1_000_000, 400]),
        connections: BTreeSet::from([
            Connection::UnixPath(path("/run/tool.sock")?),
            Connection::Ip(SocketAddr::from(([127, 0, 0, 1], 8080))),
        ]),
    })
}

/// A request for `program` in `/work` with the given capture mode and echo.
fn request(
    program: &str,
    capture: CaptureMode,
    echo: bool,
) -> Result<RunRequest, kizz_trace::NotAbsoluteError> {
    Ok(RunRequest {
        program: path(program)?,
        args: vec![OsString::from("tool"), OsString::from("--flag")],
        env: BTreeMap::from([(OsString::from("PATH"), OsString::from("/bin"))]),
        cwd: path("/work")?,
        stdin: Some(b"input".to_vec()),
        capture,
        echo,
        tree: false,
    })
}

/// The fake runs through the trait object and reports output, accesses, the exit
/// code, and live echo.
#[test]
fn fake_tracer_reports_split_output_accesses_and_echo() {
    let fake =
        FakeTracer::default().with_script(path("/bin/tool").unwrap(), tool_script().unwrap());
    let tracer: &dyn Tracer = &fake;
    let echo = RecordingEcho::default();
    let request = request("/bin/tool", CaptureMode::Split, true).unwrap();

    let result = tracer.run(&request, &echo, &Cancellation::new()).unwrap();

    assert_eq!(result.termination, Termination::Exited(3));
    assert_eq!(result.termination.code(), Some(3));
    assert_eq!(result.output, CapturedOutput::Split {
        stdout: b"out done".to_vec(),
        stderr: b"err ".to_vec(),
    });
    assert!(result.is_complete());
    assert_eq!(
        result
            .unknown_system_calls
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        [400, 1_000_000]
    );
    assert_eq!(
        result
            .connections
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["127.0.0.1:8080", "Unix socket /run/tool.sock"]
    );
    assert_eq!(result.accesses.len(), 3);
    assert_eq!(
        result.accesses.get(&path("/work/input.txt").unwrap()),
        Some(AccessKinds::only(AccessKind::Read).with(AccessKind::Probe))
    );
    assert_eq!(
        result.accesses.get(&path("/work/out/result.txt").unwrap()),
        Some(AccessKinds::only(AccessKind::Write))
    );
    assert_eq!(
        result.accesses.get(&path("/bin/tool").unwrap()),
        Some(AccessKinds::only(AccessKind::Exec))
    );
    assert_eq!(echo.chunks(), vec![
        (OutputStream::Stdout, b"out ".to_vec()),
        (OutputStream::Stderr, b"err ".to_vec()),
        (OutputStream::Stdout, b"done".to_vec()),
    ]);
    assert_eq!(fake.requests(), vec![request]);
}

/// Merged capture keeps the order of both streams, and without echo the sink
/// receives nothing.
#[test]
fn fake_tracer_merges_output_without_echo() {
    let fake =
        FakeTracer::default().with_script(path("/bin/tool").unwrap(), tool_script().unwrap());
    let echo = RecordingEcho::default();
    let request = request("/bin/tool", CaptureMode::Merged, false).unwrap();

    let result = fake.run(&request, &echo, &Cancellation::new()).unwrap();

    assert_eq!(result.output, CapturedOutput::Merged {
        output: b"out err done".to_vec(),
    });
    assert_eq!(echo.chunks(), Vec::new());
}

/// A missing program is reported apart from other failures, with the paths
/// touched while looking for it.
#[test]
fn fake_tracer_reports_a_missing_program() {
    let fake = FakeTracer::default();
    let request = request("/bin/missing", CaptureMode::Split, false).unwrap();

    let error = fake
        .run(&request, &RecordingEcho::default(), &Cancellation::new())
        .unwrap_err();

    assert!(error.is_missing_program());
    assert!(!error.is_tracer_failure());
    assert!(matches!(error, TraceError::ProgramNotFound { .. }));
    assert_eq!(
        error
            .accesses()
            .and_then(|accesses| accesses.get(&path("/bin/missing").unwrap())),
        Some(AccessKinds::only(AccessKind::Probe))
    );
}

/// The fake gives the tree of one process only when the request asks for it, and
/// that tree combines to the run's accesses; a missing program's tree has no status.
#[test]
fn fake_tracer_gives_the_tree_that_a_request_asks_for() {
    let fake =
        FakeTracer::default().with_script(path("/bin/tool").unwrap(), tool_script().unwrap());
    let mut request = request("/bin/tool", CaptureMode::Split, false).unwrap();
    let without = fake
        .run(&request, &RecordingEcho::default(), &Cancellation::new())
        .unwrap();
    assert_eq!(without.tree, None);
    request.tree = true;
    let with = fake
        .run(&request, &RecordingEcho::default(), &Cancellation::new())
        .unwrap();
    let tree = with.tree.as_ref().unwrap();
    assert_eq!(tree.accesses(), with.accesses);
    assert_eq!(with.accesses, without.accesses);
    assert_eq!(
        tree.process(1).and_then(|process| process.status),
        Some(Termination::Exited(3))
    );

    request.program = path("/bin/missing").unwrap();
    let error = fake
        .run(&request, &RecordingEcho::default(), &Cancellation::new())
        .unwrap_err();
    let tree = error.tree().unwrap();
    assert_eq!(Some(&tree.accesses()), error.accesses());
    assert_eq!(tree.process(1).map(|process| process.status), Some(None));
}

/// A cancelled request ends with a signal and no exit code.
#[test]
fn fake_tracer_honors_cancellation() {
    let fake =
        FakeTracer::default().with_script(path("/bin/tool").unwrap(), tool_script().unwrap());
    let cancel = Cancellation::new();
    cancel.clone().cancel();
    let request = request("/bin/tool", CaptureMode::Split, true).unwrap();

    let result = fake
        .run(&request, &RecordingEcho::default(), &cancel)
        .unwrap();

    assert_eq!(result.termination, Termination::Signaled(SIGKILL));
    assert_eq!(result.termination.code(), None);
    assert_eq!(result.output, CapturedOutput::empty(CaptureMode::Split));
}

/// The attribute query answers through the trait object with the attributes the
/// test set, for a path with and without following a final link, and fails for a
/// path it does not know.
#[test]
fn fake_tracer_answers_the_attribute_query() {
    let mut fake = FakeTracer::default();
    let own = ExtendedAttributes::from([(b"user.link".to_vec(), b"1".to_vec())]);
    let pointee = ExtendedAttributes::from([
        (b"user.b".to_vec(), b"2".to_vec()),
        (b"user.a".to_vec(), Vec::new()),
    ]);
    fake.attributes
        .insert((PathBuf::from("/work/link"), false), own.clone());
    fake.attributes
        .insert((PathBuf::from("/work/link"), true), pointee.clone());
    let tracer: &dyn Tracer = &fake;

    let link = Path::new("/work/link");
    assert_eq!(tracer.extended_attributes(link, false).unwrap(), own);
    let followed = tracer.extended_attributes(link, true).unwrap();
    assert_eq!(followed, pointee);
    let names: Vec<&[u8]> = followed.keys().map(Vec::as_slice).collect();
    assert_eq!(names, [b"user.a".as_slice(), b"user.b".as_slice()]);
    let missing = tracer
        .extended_attributes(Path::new("/work/missing"), true)
        .unwrap_err();
    assert_eq!(missing.kind(), io::ErrorKind::NotFound);
}

/// The machine query answers through the trait object with the state the test
/// scripted, and the fake notes the working directory of each query.
#[test]
fn fake_tracer_answers_the_machine_query() {
    let state = MachineState {
        kernel: Some(Kernel {
            release: "6.12.0".to_owned(),
            version: "#1 SMP".to_owned(),
        }),
        limits: Some(vec![ResourceLimit {
            resource: "nofile".to_owned(),
            soft: Some(1_024),
            hard: None,
        }]),
        affinity: Some(vec![0, 1]),
        memory: Some(Memory {
            total: 8_589_934_592,
            available: 4_294_967_296,
        }),
        filesystem: Some(FileSystem {
            kind: 0xEF53,
            flags: 0x1020,
        }),
        disk: Some(Disk {
            size: 107_374_182_400,
            free: 42_949_672_960,
        }),
    };
    let fake = FakeTracer {
        machine: state.clone(),
        ..FakeTracer::default()
    };
    let tracer: &dyn Tracer = &fake;
    assert_eq!(tracer.machine_state(Path::new("/work")), state);
    assert_eq!(MachineState::default().kernel, None);
    assert_eq!(
        *fake
            .machine_queries
            .lock()
            .unwrap_or_else(PoisonError::into_inner),
        [PathBuf::from("/work")]
    );
}
