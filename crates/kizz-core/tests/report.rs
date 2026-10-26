// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Tests of observation reports, built from fake process trees: the golden output
//! of each of the four renderings of a full fixture and of a report of one
//! command as `kizz execute` writes it; the compact observations against the
//! combined hierarchical ones; the JSON fields against [`JSON_FIELDS`]; text
//! against JSON; inputs in another order; the totals; completeness; outcomes; and
//! the numbering of processes across commands.
//!
//! The full fixture holds four commands and a task program:
//!
//! * `sh build.sh`, with a tree of three processes: the shell, a child that inherits the
//!   shell and then runs `cc` and `ld` through `exec`, and a grandchild that inherits
//!   `cc`. Between them they have failed probes with error names, an outcome `unknown`,
//!   every access kind, connections of the three kinds, a reason of every tracer kind and
//!   an unknown system call, and a path and an argument that are not valid UTF-8;
//! * `cc --version`, which came from the cache;
//! * `missing-tool x`, which the lookup did not find, with its placeholder process;
//! * `/bin/true`, whose tracer failed before it started;
//! * the task program, which read, checked, listed and wrote paths, one of them read by a
//!   command too and one listed by a command too.

#![allow(
    clippy::disallowed_macros,
    clippy::print_stderr,
    reason = "test code: the tests assert and print the output that differs from a golden"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    ffi::{OsStr, OsString},
    fmt, io,
    os::unix::ffi::OsStrExt as _,
};

use kizz_core::{
    CommandInput, CommandOutcome, Execution, JSON_FIELDS, Report, ReportFormat, ReportMode,
    TaskProgramInput,
};
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, Connection, IncompleteReason, Observations, Outcome,
    Process, ProcessTree, Program, Termination, TraceError,
};
use serde::de::{DeserializeSeed, Deserializer, Error as DeError, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};

/// The result of a test.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// One observation of a fixture: the path's bytes, the access kind, and the
/// outcome, `ok`, `unknown` or an error name.
type Entry = (&'static [u8], AccessKind, &'static str);

/// Each observed path of a JSON report, with each access kind and its outcomes.
type ObservationMap = BTreeMap<String, BTreeMap<String, BTreeSet<String>>>;

/// A process of a report by its number, process ID and parent's number.
type Numbering = (u32, Option<u32>, Option<u32>);

/// Every rendering: each format in each mode.
const RENDERINGS: [(ReportFormat, ReportMode); 4] = [
    (ReportFormat::Text, ReportMode::Hierarchical),
    (ReportFormat::Text, ReportMode::Compact),
    (ReportFormat::Json, ReportMode::Hierarchical),
    (ReportFormat::Json, ReportMode::Compact),
];

/// The arguments of the full fixture's first command, the last one not valid
/// UTF-8.
const SHELL_ARGS: &[&[u8]] = &[b"sh", b"build.sh", b"two words", b"", b"caf\xFF"];

/// The arguments of the full fixture's compiler.
const CC_ARGS: &[&[u8]] = &[b"cc", b"-o", b"hello world", b"main.c"];

/// The arguments of the full fixture's linker.
const LD_ARGS: &[&[u8]] = &[b"ld", b"-o", b"hello world"];

/// The absolute path of `bytes`.
fn absolute(bytes: &[u8]) -> TestResult<AbsolutePath> {
    Ok(AbsolutePath::new(OsStr::from_bytes(bytes))?)
}

/// The outcome named `name`: `ok`, `unknown`, or an error name.
fn outcome(name: &str) -> Outcome {
    match name {
        "ok" => Outcome::Ok,
        "unknown" => Outcome::Unknown,
        other => Outcome::Error(other.to_owned()),
    }
}

/// The observations of `entries`, recorded in their order, or in reverse order
/// when `reversed` is set.
fn observations(
    entries: &[Entry],
    reversed: bool,
) -> TestResult<Observations> {
    let mut ordered: Vec<&Entry> = entries.iter().collect();
    if reversed {
        ordered.reverse();
    }
    let mut observations = Observations::new();
    for (path, kind, name) in ordered {
        observations.record(absolute(path)?, *kind, outcome(name));
    }
    Ok(observations)
}

/// The arguments of `words`, as bytes.
fn args(words: &[&[u8]]) -> Vec<OsString> {
    words
        .iter()
        .map(|word| OsStr::from_bytes(word).to_owned())
        .collect()
}

/// The program at `path` with `words` as its arguments, inherited or not, which
/// observed `entries`, recorded as [`observations`] records them.
fn program(
    path: &[u8],
    words: &[&[u8]],
    inherited: bool,
    entries: &[Entry],
    reversed: bool,
) -> TestResult<Program> {
    let mut program = Program::new(absolute(path)?, args(words), inherited);
    program.observations = observations(entries, reversed)?;
    Ok(program)
}

/// The process numbered `number`, with its process ID, parent, programs and
/// status.
const fn process(
    number: u32,
    pid: u32,
    parent: Option<u32>,
    programs: Vec<Program>,
    status: Termination,
) -> Process {
    Process {
        number,
        pid: Some(pid),
        parent,
        programs,
        status: Some(status),
    }
}

/// `items` in their order, or in reverse order when `reversed` is set.
fn ordered<T>(
    mut items: Vec<T>,
    reversed: bool,
) -> Vec<T> {
    if reversed {
        items.reverse();
    }
    items
}

/// The compiler of the full fixture's second process: failed probes with two error
/// names, an outcome `unknown`, a listed directory, an extended attribute, an IP
/// and a Unix connection, and an unknown system call.
fn compiler(reversed: bool) -> TestResult<Program> {
    let mut compiler = program(
        b"/usr/bin/cc",
        CC_ARGS,
        false,
        &[
            (b"/usr/bin/cc", AccessKind::Exec, "ok"),
            (b"/work/main.c", AccessKind::Read, "ok"),
            (b"/work/missing.h", AccessKind::Probe, "ENOENT"),
            (b"/work/missing.h", AccessKind::Probe, "EACCES"),
            (b"/work", AccessKind::List, "ok"),
            (b"/work/main.c", AccessKind::Xattr, "ENODATA"),
            (b"/work/slow.h", AccessKind::Read, "unknown"),
        ],
        reversed,
    )?;
    compiler.connections = ordered(
        vec![
            Connection::Ip("127.0.0.1:80".parse()?),
            Connection::UnixPath(absolute(b"/run/cc.sock")?),
        ],
        reversed,
    )
    .into_iter()
    .collect();
    compiler.unknown_system_calls = BTreeSet::from([335]);
    Ok(compiler)
}

/// The process tree of the full fixture's first command, with its processes and
/// each program's observations and reasons in reverse order when `reversed` is
/// set.
fn shell_tree(reversed: bool) -> TestResult<ProcessTree> {
    let mut shell = program(
        b"/bin/sh",
        SHELL_ARGS,
        false,
        &[
            (b"/bin/sh", AccessKind::Exec, "ok"),
            (b"/work/build.sh", AccessKind::Read, "ok"),
        ],
        reversed,
    )?;
    shell.incomplete = vec![IncompleteReason::Other {
        detail: "the tracer lost a process".to_owned(),
    }];
    let inherited_shell = program(
        b"/bin/sh",
        SHELL_ARGS,
        true,
        &[(b"/work/log.txt", AccessKind::Write, "ok")],
        reversed,
    )?;
    let linker = program(
        b"/usr/bin/ld",
        LD_ARGS,
        false,
        &[
            (b"/usr/bin/ld", AccessKind::Exec, "ok"),
            (b"/lib/ld.so", AccessKind::Read, "ok"),
            (b"/work/hello world", AccessKind::Write, "ok"),
            (b"/work/caf\xFF", AccessKind::Probe, "ENOENT"),
        ],
        reversed,
    )?;
    let mut inherited_compiler = program(
        b"/usr/bin/cc",
        CC_ARGS,
        true,
        &[(b"/tmp/cc.tmp", AccessKind::Write, "ok")],
        reversed,
    )?;
    inherited_compiler.connections =
        BTreeSet::from([Connection::UnixAbstract(b"cc-lock".to_vec())]);
    inherited_compiler.incomplete = ordered(
        vec![
            IncompleteReason::NonNativeSystemCall {
                detail: "int 0x80 call 5".to_owned(),
            },
            IncompleteReason::UntraceableSystemCall {
                detail: "process 102 called `chroot`".to_owned(),
            },
        ],
        reversed,
    );
    let processes = vec![
        process(1, 100, None, vec![shell], Termination::Signaled(15)),
        process(
            2,
            101,
            Some(1),
            vec![inherited_shell, compiler(reversed)?, linker],
            Termination::Exited(0),
        ),
        process(
            3,
            102,
            Some(2),
            vec![inherited_compiler],
            Termination::Signaled(9),
        ),
    ];
    Ok(ProcessTree {
        processes: ordered(processes, reversed),
    })
}

/// The commands and the task program of the full fixture, with every input in
/// reverse order when `reversed` is set.
fn full_fixture(reversed: bool) -> TestResult<(Vec<CommandInput>, Option<TaskProgramInput>)> {
    let commands = vec![
        CommandInput {
            argv: args(SHELL_ARGS),
            program: Some(absolute(b"/bin/sh")?),
            cwd: absolute(b"/work")?,
            execution: Execution::Ran {
                outcome: CommandOutcome::Signaled(15),
                tree: Some(shell_tree(reversed)?),
                lookup: observations(
                    &[
                        (b"/usr/local/bin/sh", AccessKind::Probe, "ENOENT"),
                        (b"/bin/sh", AccessKind::Probe, "ok"),
                    ],
                    reversed,
                )?,
            },
        },
        CommandInput {
            argv: args(&[b"cc", b"--version"]),
            program: Some(absolute(b"/usr/bin/cc")?),
            cwd: absolute(b"/work")?,
            execution: Execution::Cached {
                outcome: CommandOutcome::Exited(0),
            },
        },
        CommandInput {
            argv: args(&[b"missing-tool", b"x"]),
            program: None,
            cwd: absolute(b"/work")?,
            execution: Execution::Ran {
                outcome: CommandOutcome::NotFound("missing-tool: not found in PATH".to_owned()),
                tree: None,
                lookup: observations(
                    &[
                        (b"/usr/local/bin/missing-tool", AccessKind::Probe, "ENOENT"),
                        (b"/usr/bin/missing-tool", AccessKind::Probe, "ENOENT"),
                    ],
                    reversed,
                )?,
            },
        },
        CommandInput {
            argv: args(&[b"/bin/true"]),
            program: Some(absolute(b"/bin/true")?),
            cwd: absolute(b"/")?,
            execution: Execution::Ran {
                outcome: CommandOutcome::Failed("cannot trace: ptrace failed".to_owned()),
                tree: None,
                lookup: Observations::new(),
            },
        },
    ];
    let task_program = TaskProgramInput {
        task: "kizz.md:3 (Build)".to_owned(),
        observations: observations(
            &[
                (b"/work/kizz.md", AccessKind::Read, "ok"),
                (b"/work/.env", AccessKind::Probe, "ENOENT"),
                (b"/work/main.c", AccessKind::Read, "ok"),
                (b"/work", AccessKind::List, "ok"),
                (b"/work/gen.txt", AccessKind::Write, "ok"),
            ],
            reversed,
        )?,
    };
    Ok((commands, Some(task_program)))
}

/// The report of the full fixture.
fn full_report(reversed: bool) -> TestResult<Report> {
    let (commands, task_program) = full_fixture(reversed)?;
    Ok(Report::new(commands, task_program))
}

/// The one command of a report that `kizz execute` writes for `cc -c main.c`:
/// the compiler, and the assembler that its child runs, with the lookup's probes.
fn execute_fixture(reversed: bool) -> TestResult<Vec<CommandInput>> {
    let compiler = program(
        b"/usr/bin/cc",
        &[b"cc", b"-c", b"main.c"],
        false,
        &[
            (b"/usr/bin/cc", AccessKind::Exec, "ok"),
            (b"/work/main.c", AccessKind::Read, "ok"),
            (b"/work/main.o", AccessKind::Write, "ok"),
        ],
        reversed,
    )?;
    let assembler = program(
        b"/usr/bin/as",
        &[b"as", b"-o", b"main.o", b"/tmp/main.s"],
        false,
        &[
            (b"/usr/bin/as", AccessKind::Exec, "ok"),
            (b"/tmp/main.s", AccessKind::Read, "ok"),
        ],
        reversed,
    )?;
    let processes = vec![
        process(1, 4242, None, vec![compiler], Termination::Exited(0)),
        process(2, 4243, Some(1), vec![assembler], Termination::Exited(0)),
    ];
    Ok(vec![CommandInput {
        argv: args(&[b"cc", b"-c", b"main.c"]),
        program: Some(absolute(b"/usr/bin/cc")?),
        cwd: absolute(b"/work")?,
        execution: Execution::Ran {
            outcome: CommandOutcome::Exited(0),
            tree: Some(ProcessTree {
                processes: ordered(processes, reversed),
            }),
            lookup: observations(
                &[
                    (b"/usr/local/bin/cc", AccessKind::Probe, "ENOENT"),
                    (b"/usr/bin/cc", AccessKind::Probe, "ok"),
                ],
                reversed,
            )?,
        },
    }])
}

/// The report of the one-command fixture, as `kizz execute` writes it.
fn execute_report(reversed: bool) -> TestResult<Report> {
    Ok(Report::new(execute_fixture(reversed)?, None))
}

/// Both fixtures' reports, by name.
fn every_report() -> TestResult<Vec<(&'static str, Report)>> {
    Ok(vec![
        ("full", full_report(false)?),
        ("execute", execute_report(false)?),
    ])
}

/// Compare `actual` with its golden `expected`; on a difference, print `actual`
/// whole and name the first line that differs.
fn golden(
    name: &str,
    actual: &str,
    expected: &str,
) -> TestResult {
    if actual == expected {
        return Ok(());
    }
    eprintln!("{name}: the output differs from its golden; the output is:\n{actual}");
    let difference = actual
        .lines()
        .zip(expected.lines())
        .enumerate()
        .find(|(_, (actual, expected))| actual != expected);
    Err(match difference {
        Some((index, (actual, expected))) => {
            format!(
                "{name}: line {} is {actual:?}, and the golden has {expected:?}",
                index.saturating_add(1)
            )
        },
        None => {
            format!(
                "{name}: the output has {} lines, and the golden has {}",
                actual.lines().count(),
                expected.lines().count()
            )
        },
    }
    .into())
}

/// The golden hierarchical text of the full fixture.
const FULL_HIERARCHICAL_TEXT: &str = r"kizz observation report, version 1, hierarchical
trace: incomplete
totals: commands 4, cached 1, processes 5, programs 5, paths 19, connections 3
paths by access: read 5, probe 7, list 1, write 4, exec 3, xattr 1

command 1: sh build.sh 'two words' '' 'caf�'
  program: /bin/sh
  directory: /work
  outcome: signaled with 15
  trace: incomplete
    other, process 1: the tracer lost a process
    unknown system call, process 2: the tracer does not know system call 335
    non-native system call, process 3: int 0x80 call 5
    untraceable system call, process 3: process 102 called `chroot`
  process 1, pid 100, signaled with 15
    program /bin/sh: sh build.sh 'two words' '' 'caf�'
      read:
        /work/build.sh: ok
      probe:
        /bin/sh: ok
        /usr/local/bin/sh: ENOENT
      exec:
        /bin/sh: ok
    process 2, pid 101, child of process 1, exited with 0
      program /bin/sh, inherited: sh build.sh 'two words' '' 'caf�'
        write:
          /work/log.txt: ok
      program /usr/bin/cc: cc -o 'hello world' main.c
        read:
          /work/main.c: ok
          /work/slow.h: unknown
        probe:
          /work/missing.h: EACCES, ENOENT
        list:
          /work: ok
        exec:
          /usr/bin/cc: ok
        xattr:
          /work/main.c: ENODATA
        connections:
          ip 127.0.0.1:80
          unix /run/cc.sock
      program /usr/bin/ld: ld -o 'hello world'
        read:
          /lib/ld.so: ok
        probe:
          /work/caf�: ENOENT
        write:
          /work/hello world: ok
        exec:
          /usr/bin/ld: ok
      process 3, pid 102, child of process 2, signaled with 9
        program /usr/bin/cc, inherited: cc -o 'hello world' main.c
          write:
            /tmp/cc.tmp: ok
          connections:
            abstract cc-lock

command 2: cc --version
  program: /usr/bin/cc
  directory: /work
  outcome: exited with 0
  trace: cached, not traced

command 3: missing-tool x
  program: not found
  directory: /work
  outcome: not found: missing-tool: not found in PATH
  trace: complete
  process 4, not started
    program missing-tool: missing-tool x
      probe:
        /usr/bin/missing-tool: ENOENT
        /usr/local/bin/missing-tool: ENOENT

command 4: /bin/true
  program: /bin/true
  directory: /
  outcome: failed: cannot trace: ptrace failed
  trace: incomplete
    tracer failed, process 5: cannot trace: ptrace failed
  process 5, not started
    program /bin/true: /bin/true

task program: kizz.md:3 (Build)
  read:
    /work/kizz.md: ok
    /work/main.c: ok
  probe:
    /work/.env: ENOENT
  list:
    /work: ok
  write:
    /work/gen.txt: ok
";

/// The golden compact text of the full fixture.
const FULL_COMPACT_TEXT: &str = r"kizz observation report, version 1, compact
trace: incomplete
totals: commands 4, cached 1, processes 5, programs 5, paths 19, connections 3
paths by access: read 5, probe 7, list 1, write 4, exec 3, xattr 1

command 1: sh build.sh 'two words' '' 'caf�'
  program: /bin/sh
  directory: /work
  outcome: signaled with 15
  trace: incomplete
    other: the tracer lost a process
    unknown system call: the tracer does not know system call 335
    non-native system call: int 0x80 call 5
    untraceable system call: process 102 called `chroot`

command 2: cc --version
  program: /usr/bin/cc
  directory: /work
  outcome: exited with 0
  trace: cached, not traced

command 3: missing-tool x
  program: not found
  directory: /work
  outcome: not found: missing-tool: not found in PATH
  trace: complete

command 4: /bin/true
  program: /bin/true
  directory: /
  outcome: failed: cannot trace: ptrace failed
  trace: incomplete
    tracer failed: cannot trace: ptrace failed

task program: kizz.md:3 (Build)

read:
  /lib/ld.so: ok
  /work/build.sh: ok
  /work/kizz.md: ok
  /work/main.c: ok
  /work/slow.h: unknown
probe:
  /bin/sh: ok
  /usr/bin/missing-tool: ENOENT
  /usr/local/bin/missing-tool: ENOENT
  /usr/local/bin/sh: ENOENT
  /work/.env: ENOENT
  /work/caf�: ENOENT
  /work/missing.h: EACCES, ENOENT
list:
  /work: ok
write:
  /tmp/cc.tmp: ok
  /work/gen.txt: ok
  /work/hello world: ok
  /work/log.txt: ok
exec:
  /bin/sh: ok
  /usr/bin/cc: ok
  /usr/bin/ld: ok
xattr:
  /work/main.c: ENODATA
connections:
  ip 127.0.0.1:80
  unix /run/cc.sock
  abstract cc-lock
";

/// The golden hierarchical JSON of the full fixture.
const FULL_HIERARCHICAL_JSON: &str = r#"{
  "format": "kizz-execute",
  "version": 1,
  "mode": "hierarchical",
  "complete": false,
  "totals": {
    "commands": 4,
    "cached": 1,
    "processes": 5,
    "programs": 5,
    "paths": 19,
    "connections": 3,
    "kinds": {
      "read": 5,
      "probe": 7,
      "list": 1,
      "write": 4,
      "exec": 3,
      "xattr": 1
    }
  },
  "commands": [
    {
      "argv": [
        "sh",
        "build.sh",
        "two words",
        "",
        "caf�"
      ],
      "program": "/bin/sh",
      "cwd": "/work",
      "cached": false,
      "outcome": {
        "kind": "signaled",
        "signal": 15
      },
      "complete": false,
      "incomplete": [
        {
          "kind": "other",
          "detail": "the tracer lost a process",
          "number": null,
          "process": 1
        },
        {
          "kind": "unknown_system_call",
          "detail": "the tracer does not know system call 335",
          "number": 335,
          "process": 2
        },
        {
          "kind": "non_native_system_call",
          "detail": "int 0x80 call 5",
          "number": null,
          "process": 3
        },
        {
          "kind": "untraceable_system_call",
          "detail": "process 102 called `chroot`",
          "number": null,
          "process": 3
        }
      ],
      "processes": [
        {
          "id": 1,
          "pid": 100,
          "parent": null,
          "exit": {
            "kind": "signaled",
            "signal": 15
          },
          "programs": [
            {
              "path": "/bin/sh",
              "args": [
                "sh",
                "build.sh",
                "two words",
                "",
                "caf�"
              ],
              "inherited": false,
              "observations": [
                {
                  "path": "/bin/sh",
                  "access": {
                    "probe": [
                      "ok"
                    ],
                    "exec": [
                      "ok"
                    ]
                  }
                },
                {
                  "path": "/usr/local/bin/sh",
                  "access": {
                    "probe": [
                      "ENOENT"
                    ]
                  }
                },
                {
                  "path": "/work/build.sh",
                  "access": {
                    "read": [
                      "ok"
                    ]
                  }
                }
              ],
              "connections": []
            }
          ]
        },
        {
          "id": 2,
          "pid": 101,
          "parent": 1,
          "exit": {
            "kind": "exited",
            "code": 0
          },
          "programs": [
            {
              "path": "/bin/sh",
              "args": [
                "sh",
                "build.sh",
                "two words",
                "",
                "caf�"
              ],
              "inherited": true,
              "observations": [
                {
                  "path": "/work/log.txt",
                  "access": {
                    "write": [
                      "ok"
                    ]
                  }
                }
              ],
              "connections": []
            },
            {
              "path": "/usr/bin/cc",
              "args": [
                "cc",
                "-o",
                "hello world",
                "main.c"
              ],
              "inherited": false,
              "observations": [
                {
                  "path": "/usr/bin/cc",
                  "access": {
                    "exec": [
                      "ok"
                    ]
                  }
                },
                {
                  "path": "/work",
                  "access": {
                    "list": [
                      "ok"
                    ]
                  }
                },
                {
                  "path": "/work/main.c",
                  "access": {
                    "read": [
                      "ok"
                    ],
                    "xattr": [
                      "ENODATA"
                    ]
                  }
                },
                {
                  "path": "/work/missing.h",
                  "access": {
                    "probe": [
                      "EACCES",
                      "ENOENT"
                    ]
                  }
                },
                {
                  "path": "/work/slow.h",
                  "access": {
                    "read": [
                      "unknown"
                    ]
                  }
                }
              ],
              "connections": [
                {
                  "kind": "ip",
                  "address": "127.0.0.1:80"
                },
                {
                  "kind": "unix",
                  "path": "/run/cc.sock"
                }
              ]
            },
            {
              "path": "/usr/bin/ld",
              "args": [
                "ld",
                "-o",
                "hello world"
              ],
              "inherited": false,
              "observations": [
                {
                  "path": "/lib/ld.so",
                  "access": {
                    "read": [
                      "ok"
                    ]
                  }
                },
                {
                  "path": "/usr/bin/ld",
                  "access": {
                    "exec": [
                      "ok"
                    ]
                  }
                },
                {
                  "path": "/work/caf�",
                  "access": {
                    "probe": [
                      "ENOENT"
                    ]
                  }
                },
                {
                  "path": "/work/hello world",
                  "access": {
                    "write": [
                      "ok"
                    ]
                  }
                }
              ],
              "connections": []
            }
          ]
        },
        {
          "id": 3,
          "pid": 102,
          "parent": 2,
          "exit": {
            "kind": "signaled",
            "signal": 9
          },
          "programs": [
            {
              "path": "/usr/bin/cc",
              "args": [
                "cc",
                "-o",
                "hello world",
                "main.c"
              ],
              "inherited": true,
              "observations": [
                {
                  "path": "/tmp/cc.tmp",
                  "access": {
                    "write": [
                      "ok"
                    ]
                  }
                }
              ],
              "connections": [
                {
                  "kind": "abstract",
                  "name": "cc-lock"
                }
              ]
            }
          ]
        }
      ]
    },
    {
      "argv": [
        "cc",
        "--version"
      ],
      "program": "/usr/bin/cc",
      "cwd": "/work",
      "cached": true,
      "outcome": {
        "kind": "exited",
        "code": 0
      },
      "complete": null,
      "incomplete": [],
      "processes": []
    },
    {
      "argv": [
        "missing-tool",
        "x"
      ],
      "program": null,
      "cwd": "/work",
      "cached": false,
      "outcome": {
        "kind": "not_found",
        "error": "missing-tool: not found in PATH"
      },
      "complete": true,
      "incomplete": [],
      "processes": [
        {
          "id": 4,
          "pid": null,
          "parent": null,
          "exit": null,
          "programs": [
            {
              "path": "missing-tool",
              "args": [
                "missing-tool",
                "x"
              ],
              "inherited": false,
              "observations": [
                {
                  "path": "/usr/bin/missing-tool",
                  "access": {
                    "probe": [
                      "ENOENT"
                    ]
                  }
                },
                {
                  "path": "/usr/local/bin/missing-tool",
                  "access": {
                    "probe": [
                      "ENOENT"
                    ]
                  }
                }
              ],
              "connections": []
            }
          ]
        }
      ]
    },
    {
      "argv": [
        "/bin/true"
      ],
      "program": "/bin/true",
      "cwd": "/",
      "cached": false,
      "outcome": {
        "kind": "failed",
        "error": "cannot trace: ptrace failed"
      },
      "complete": false,
      "incomplete": [
        {
          "kind": "tracer_failed",
          "detail": "cannot trace: ptrace failed",
          "number": null,
          "process": 5
        }
      ],
      "processes": [
        {
          "id": 5,
          "pid": null,
          "parent": null,
          "exit": null,
          "programs": [
            {
              "path": "/bin/true",
              "args": [
                "/bin/true"
              ],
              "inherited": false,
              "observations": [],
              "connections": []
            }
          ]
        }
      ]
    }
  ],
  "task_program": {
    "task": "kizz.md:3 (Build)",
    "observations": [
      {
        "path": "/work",
        "access": {
          "list": [
            "ok"
          ]
        }
      },
      {
        "path": "/work/.env",
        "access": {
          "probe": [
            "ENOENT"
          ]
        }
      },
      {
        "path": "/work/gen.txt",
        "access": {
          "write": [
            "ok"
          ]
        }
      },
      {
        "path": "/work/kizz.md",
        "access": {
          "read": [
            "ok"
          ]
        }
      },
      {
        "path": "/work/main.c",
        "access": {
          "read": [
            "ok"
          ]
        }
      }
    ]
  }
}
"#;

/// The golden compact JSON of the full fixture.
const FULL_COMPACT_JSON: &str = r#"{
  "format": "kizz-execute",
  "version": 1,
  "mode": "compact",
  "complete": false,
  "totals": {
    "commands": 4,
    "cached": 1,
    "processes": 5,
    "programs": 5,
    "paths": 19,
    "connections": 3,
    "kinds": {
      "read": 5,
      "probe": 7,
      "list": 1,
      "write": 4,
      "exec": 3,
      "xattr": 1
    }
  },
  "commands": [
    {
      "argv": [
        "sh",
        "build.sh",
        "two words",
        "",
        "caf�"
      ],
      "program": "/bin/sh",
      "cwd": "/work",
      "cached": false,
      "outcome": {
        "kind": "signaled",
        "signal": 15
      },
      "complete": false,
      "incomplete": [
        {
          "kind": "other",
          "detail": "the tracer lost a process",
          "number": null
        },
        {
          "kind": "unknown_system_call",
          "detail": "the tracer does not know system call 335",
          "number": 335
        },
        {
          "kind": "non_native_system_call",
          "detail": "int 0x80 call 5",
          "number": null
        },
        {
          "kind": "untraceable_system_call",
          "detail": "process 102 called `chroot`",
          "number": null
        }
      ]
    },
    {
      "argv": [
        "cc",
        "--version"
      ],
      "program": "/usr/bin/cc",
      "cwd": "/work",
      "cached": true,
      "outcome": {
        "kind": "exited",
        "code": 0
      },
      "complete": null,
      "incomplete": []
    },
    {
      "argv": [
        "missing-tool",
        "x"
      ],
      "program": null,
      "cwd": "/work",
      "cached": false,
      "outcome": {
        "kind": "not_found",
        "error": "missing-tool: not found in PATH"
      },
      "complete": true,
      "incomplete": []
    },
    {
      "argv": [
        "/bin/true"
      ],
      "program": "/bin/true",
      "cwd": "/",
      "cached": false,
      "outcome": {
        "kind": "failed",
        "error": "cannot trace: ptrace failed"
      },
      "complete": false,
      "incomplete": [
        {
          "kind": "tracer_failed",
          "detail": "cannot trace: ptrace failed",
          "number": null
        }
      ]
    }
  ],
  "task_program": {
    "task": "kizz.md:3 (Build)"
  },
  "observations": [
    {
      "path": "/bin/sh",
      "access": {
        "probe": [
          "ok"
        ],
        "exec": [
          "ok"
        ]
      }
    },
    {
      "path": "/lib/ld.so",
      "access": {
        "read": [
          "ok"
        ]
      }
    },
    {
      "path": "/tmp/cc.tmp",
      "access": {
        "write": [
          "ok"
        ]
      }
    },
    {
      "path": "/usr/bin/cc",
      "access": {
        "exec": [
          "ok"
        ]
      }
    },
    {
      "path": "/usr/bin/ld",
      "access": {
        "exec": [
          "ok"
        ]
      }
    },
    {
      "path": "/usr/bin/missing-tool",
      "access": {
        "probe": [
          "ENOENT"
        ]
      }
    },
    {
      "path": "/usr/local/bin/missing-tool",
      "access": {
        "probe": [
          "ENOENT"
        ]
      }
    },
    {
      "path": "/usr/local/bin/sh",
      "access": {
        "probe": [
          "ENOENT"
        ]
      }
    },
    {
      "path": "/work",
      "access": {
        "list": [
          "ok"
        ]
      }
    },
    {
      "path": "/work/.env",
      "access": {
        "probe": [
          "ENOENT"
        ]
      }
    },
    {
      "path": "/work/build.sh",
      "access": {
        "read": [
          "ok"
        ]
      }
    },
    {
      "path": "/work/caf�",
      "access": {
        "probe": [
          "ENOENT"
        ]
      }
    },
    {
      "path": "/work/gen.txt",
      "access": {
        "write": [
          "ok"
        ]
      }
    },
    {
      "path": "/work/hello world",
      "access": {
        "write": [
          "ok"
        ]
      }
    },
    {
      "path": "/work/kizz.md",
      "access": {
        "read": [
          "ok"
        ]
      }
    },
    {
      "path": "/work/log.txt",
      "access": {
        "write": [
          "ok"
        ]
      }
    },
    {
      "path": "/work/main.c",
      "access": {
        "read": [
          "ok"
        ],
        "xattr": [
          "ENODATA"
        ]
      }
    },
    {
      "path": "/work/missing.h",
      "access": {
        "probe": [
          "EACCES",
          "ENOENT"
        ]
      }
    },
    {
      "path": "/work/slow.h",
      "access": {
        "read": [
          "unknown"
        ]
      }
    }
  ],
  "connections": [
    {
      "kind": "ip",
      "address": "127.0.0.1:80"
    },
    {
      "kind": "unix",
      "path": "/run/cc.sock"
    },
    {
      "kind": "abstract",
      "name": "cc-lock"
    }
  ]
}
"#;

/// The golden hierarchical text of the one-command fixture.
const EXECUTE_HIERARCHICAL_TEXT: &str = r"kizz observation report, version 1, hierarchical
trace: complete
totals: commands 1, cached 0, processes 2, programs 2, paths 6, connections 0
paths by access: read 2, probe 2, list 0, write 1, exec 2, xattr 0

command 1: cc -c main.c
  program: /usr/bin/cc
  directory: /work
  outcome: exited with 0
  trace: complete
  process 1, pid 4242, exited with 0
    program /usr/bin/cc: cc -c main.c
      read:
        /work/main.c: ok
      probe:
        /usr/bin/cc: ok
        /usr/local/bin/cc: ENOENT
      write:
        /work/main.o: ok
      exec:
        /usr/bin/cc: ok
    process 2, pid 4243, child of process 1, exited with 0
      program /usr/bin/as: as -o main.o /tmp/main.s
        read:
          /tmp/main.s: ok
        exec:
          /usr/bin/as: ok
";

/// The golden compact text of the one-command fixture.
const EXECUTE_COMPACT_TEXT: &str = r"kizz observation report, version 1, compact
trace: complete
totals: commands 1, cached 0, processes 2, programs 2, paths 6, connections 0
paths by access: read 2, probe 2, list 0, write 1, exec 2, xattr 0

command 1: cc -c main.c
  program: /usr/bin/cc
  directory: /work
  outcome: exited with 0
  trace: complete

read:
  /tmp/main.s: ok
  /work/main.c: ok
probe:
  /usr/bin/cc: ok
  /usr/local/bin/cc: ENOENT
write:
  /work/main.o: ok
exec:
  /usr/bin/as: ok
  /usr/bin/cc: ok
";

/// The golden hierarchical JSON of the one-command fixture.
const EXECUTE_HIERARCHICAL_JSON: &str = r#"{
  "format": "kizz-execute",
  "version": 1,
  "mode": "hierarchical",
  "complete": true,
  "totals": {
    "commands": 1,
    "cached": 0,
    "processes": 2,
    "programs": 2,
    "paths": 6,
    "connections": 0,
    "kinds": {
      "read": 2,
      "probe": 2,
      "list": 0,
      "write": 1,
      "exec": 2,
      "xattr": 0
    }
  },
  "commands": [
    {
      "argv": [
        "cc",
        "-c",
        "main.c"
      ],
      "program": "/usr/bin/cc",
      "cwd": "/work",
      "cached": false,
      "outcome": {
        "kind": "exited",
        "code": 0
      },
      "complete": true,
      "incomplete": [],
      "processes": [
        {
          "id": 1,
          "pid": 4242,
          "parent": null,
          "exit": {
            "kind": "exited",
            "code": 0
          },
          "programs": [
            {
              "path": "/usr/bin/cc",
              "args": [
                "cc",
                "-c",
                "main.c"
              ],
              "inherited": false,
              "observations": [
                {
                  "path": "/usr/bin/cc",
                  "access": {
                    "probe": [
                      "ok"
                    ],
                    "exec": [
                      "ok"
                    ]
                  }
                },
                {
                  "path": "/usr/local/bin/cc",
                  "access": {
                    "probe": [
                      "ENOENT"
                    ]
                  }
                },
                {
                  "path": "/work/main.c",
                  "access": {
                    "read": [
                      "ok"
                    ]
                  }
                },
                {
                  "path": "/work/main.o",
                  "access": {
                    "write": [
                      "ok"
                    ]
                  }
                }
              ],
              "connections": []
            }
          ]
        },
        {
          "id": 2,
          "pid": 4243,
          "parent": 1,
          "exit": {
            "kind": "exited",
            "code": 0
          },
          "programs": [
            {
              "path": "/usr/bin/as",
              "args": [
                "as",
                "-o",
                "main.o",
                "/tmp/main.s"
              ],
              "inherited": false,
              "observations": [
                {
                  "path": "/tmp/main.s",
                  "access": {
                    "read": [
                      "ok"
                    ]
                  }
                },
                {
                  "path": "/usr/bin/as",
                  "access": {
                    "exec": [
                      "ok"
                    ]
                  }
                }
              ],
              "connections": []
            }
          ]
        }
      ]
    }
  ],
  "task_program": null
}
"#;

/// The golden compact JSON of the one-command fixture.
const EXECUTE_COMPACT_JSON: &str = r#"{
  "format": "kizz-execute",
  "version": 1,
  "mode": "compact",
  "complete": true,
  "totals": {
    "commands": 1,
    "cached": 0,
    "processes": 2,
    "programs": 2,
    "paths": 6,
    "connections": 0,
    "kinds": {
      "read": 2,
      "probe": 2,
      "list": 0,
      "write": 1,
      "exec": 2,
      "xattr": 0
    }
  },
  "commands": [
    {
      "argv": [
        "cc",
        "-c",
        "main.c"
      ],
      "program": "/usr/bin/cc",
      "cwd": "/work",
      "cached": false,
      "outcome": {
        "kind": "exited",
        "code": 0
      },
      "complete": true,
      "incomplete": []
    }
  ],
  "task_program": null,
  "observations": [
    {
      "path": "/tmp/main.s",
      "access": {
        "read": [
          "ok"
        ]
      }
    },
    {
      "path": "/usr/bin/as",
      "access": {
        "exec": [
          "ok"
        ]
      }
    },
    {
      "path": "/usr/bin/cc",
      "access": {
        "probe": [
          "ok"
        ],
        "exec": [
          "ok"
        ]
      }
    },
    {
      "path": "/usr/local/bin/cc",
      "access": {
        "probe": [
          "ENOENT"
        ]
      }
    },
    {
      "path": "/work/main.c",
      "access": {
        "read": [
          "ok"
        ]
      }
    },
    {
      "path": "/work/main.o",
      "access": {
        "write": [
          "ok"
        ]
      }
    }
  ],
  "connections": []
}
"#;

/// Each of the four renderings of the full fixture is its golden.
#[test]
fn full_fixture_renders_as_its_golden_output() -> TestResult {
    let report = full_report(false)?;
    let expected_outputs = [
        FULL_HIERARCHICAL_TEXT,
        FULL_COMPACT_TEXT,
        FULL_HIERARCHICAL_JSON,
        FULL_COMPACT_JSON,
    ];
    for ((format, mode), expected) in RENDERINGS.into_iter().zip(expected_outputs) {
        golden(
            &format!("full {format:?} {mode:?}"),
            &report.render(format, mode),
            expected,
        )?;
    }
    Ok(())
}

/// Each of the four renderings of a report of one command, as `kizz execute`
/// writes it, is its golden.
#[test]
fn one_command_renders_as_kizz_execute_writes_it() -> TestResult {
    let report = execute_report(false)?;
    let expected_outputs = [
        EXECUTE_HIERARCHICAL_TEXT,
        EXECUTE_COMPACT_TEXT,
        EXECUTE_HIERARCHICAL_JSON,
        EXECUTE_COMPACT_JSON,
    ];
    for ((format, mode), expected) in RENDERINGS.into_iter().zip(expected_outputs) {
        golden(
            &format!("execute {format:?} {mode:?}"),
            &report.render(format, mode),
            expected,
        )?;
    }
    Ok(())
}

/// `render` gives what `text` and `json` give.
#[test]
fn render_gives_text_and_json() -> TestResult {
    let report = full_report(false)?;
    for mode in [ReportMode::Hierarchical, ReportMode::Compact] {
        assert_eq!(report.render(ReportFormat::Text, mode), report.text(mode));
        assert_eq!(report.render(ReportFormat::Json, mode), report.json(mode));
    }
    Ok(())
}

/// The member `key` of `value`.
fn member<'a>(
    value: &'a Value,
    key: &str,
) -> TestResult<&'a Value> {
    value
        .get(key)
        .ok_or_else(|| format!("no member {key} in {value}").into())
}

/// The array that is the member `key` of `value`.
fn items<'a>(
    value: &'a Value,
    key: &str,
) -> TestResult<&'a Vec<Value>> {
    member(value, key)?
        .as_array()
        .ok_or_else(|| format!("{key} is not an array in {value}").into())
}

/// The string `value`.
fn string(value: &Value) -> TestResult<&str> {
    value
        .as_str()
        .ok_or_else(|| format!("{value} is not a string").into())
}

/// Add each observation of the JSON array `observations` to `map`.
fn add_observations(
    map: &mut ObservationMap,
    observations: &[Value],
) -> TestResult {
    for observation in observations {
        let path = string(member(observation, "path")?)?;
        let access = member(observation, "access")?
            .as_object()
            .ok_or("access is not an object")?;
        let kinds = map.entry(path.to_owned()).or_default();
        for (kind, outcomes) in access {
            let outcomes = outcomes.as_array().ok_or("outcomes are not an array")?;
            let entry = kinds.entry(kind.clone()).or_default();
            for outcome in outcomes {
                entry.insert(string(outcome)?.to_owned());
            }
        }
    }
    Ok(())
}

/// A JSON connection as text shows it: its kind, then its address, path or name.
fn connection_line(connection: &Value) -> TestResult<String> {
    let kind = string(member(connection, "kind")?)?;
    let target = ["address", "path", "name"]
        .into_iter()
        .find_map(|key| connection.get(key))
        .ok_or("a connection has no address, path or name")?;
    Ok(format!("{kind} {}", string(target)?))
}

/// Every program of every process of every command of the hierarchical JSON
/// report `report`.
fn json_programs(report: &Value) -> TestResult<Vec<&Value>> {
    let mut programs = Vec::new();
    for command in items(report, "commands")? {
        for process in items(command, "processes")? {
            programs.extend(items(process, "programs")?);
        }
    }
    Ok(programs)
}

/// The observations and connections of the hierarchical JSON report `report`,
/// combined over every program and the task program.
fn combined_hierarchical(report: &Value) -> TestResult<(ObservationMap, BTreeSet<String>)> {
    let mut observations = ObservationMap::new();
    let mut connections = BTreeSet::new();
    for program in json_programs(report)? {
        add_observations(&mut observations, items(program, "observations")?)?;
        for connection in items(program, "connections")? {
            connections.insert(connection_line(connection)?);
        }
    }
    let task_program = member(report, "task_program")?;
    if !task_program.is_null() {
        add_observations(&mut observations, items(task_program, "observations")?)?;
    }
    Ok((observations, connections))
}

/// The observations and connections of the compact JSON report `report`.
fn compact_observations(report: &Value) -> TestResult<(ObservationMap, BTreeSet<String>)> {
    let mut observations = ObservationMap::new();
    add_observations(&mut observations, items(report, "observations")?)?;
    let connections = items(report, "connections")?
        .iter()
        .map(connection_line)
        .collect::<TestResult<_>>()?;
    Ok((observations, connections))
}

/// For every fixture, the compact report's observations and connections are
/// exactly the combined observations and connections of the hierarchical report,
/// in the JSON and in the model, and no path appears twice in the compact list.
#[test]
fn compact_observations_are_the_combined_hierarchical_ones() -> TestResult {
    for (name, report) in every_report()? {
        let hierarchical: Value = serde_json::from_str(&report.json(ReportMode::Hierarchical))?;
        let compact: Value = serde_json::from_str(&report.json(ReportMode::Compact))?;
        let combined = combined_hierarchical(&hierarchical)?;
        assert_eq!(combined, compact_observations(&compact)?, "{name}");
        assert!(!combined.0.is_empty(), "{name}");
        assert_eq!(
            items(&compact, "observations")?.len(),
            combined.0.len(),
            "{name}: each path once"
        );
        let mut model = Observations::new();
        let mut connections = BTreeSet::new();
        for command in &report.commands {
            for process in &command.processes {
                for program in &process.programs {
                    model.merge(&program.observations);
                    connections.extend(program.connections.iter().cloned());
                }
            }
        }
        if let Some(task_program) = &report.task_program {
            model.merge(&task_program.observations);
        }
        assert_eq!(model, report.observations, "{name}");
        assert_eq!(connections, report.connections, "{name}");
    }
    Ok(())
}

/// The members of every object of a JSON document, each object named by its path
/// as [`JSON_FIELDS`] names paths, in the order they appear.
struct Walk<'a> {
    /// The path of the value being walked.
    path: String,
    /// Each object walked so far, with its path and its members' names in order.
    objects: &'a mut Vec<(String, Vec<String>)>,
}

impl<'de> DeserializeSeed<'de> for Walk<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Walk<'_> {
    type Value = ();

    fn expecting(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: DeError>(
        self,
        _value: bool,
    ) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E: DeError>(
        self,
        _value: i64,
    ) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E: DeError>(
        self,
        _value: u64,
    ) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E: DeError>(
        self,
        _value: f64,
    ) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E: DeError>(
        self,
        _value: &str,
    ) -> Result<(), E> {
        Ok(())
    }

    fn visit_unit<E: DeError>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(
        self,
        mut seq: A,
    ) -> Result<(), A::Error> {
        let path = format!("{}[]", self.path);
        while seq
            .next_element_seed(Walk {
                path: path.clone(),
                objects: &mut *self.objects,
            })?
            .is_some()
        {}
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(
        self,
        mut map: A,
    ) -> Result<(), A::Error> {
        let mut keys = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            let path = if self.path.is_empty() {
                key.clone()
            } else {
                format!("{}.{key}", self.path)
            };
            map.next_value_seed(Walk {
                path,
                objects: &mut *self.objects,
            })?;
            keys.push(key);
        }
        self.objects.push((self.path, keys));
        Ok(())
    }
}

/// Every object of the JSON document `json`, with its path and its members' names
/// in order.
fn json_objects(json: &str) -> TestResult<Vec<(String, Vec<String>)>> {
    let mut objects = Vec::new();
    let mut deserializer = serde_json::Deserializer::from_str(json);
    Walk {
        path: String::new(),
        objects: &mut objects,
    }
    .deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(objects)
}

/// The path of the object that holds the field at `path`, and the field's name.
fn split_field(path: &str) -> (&str, &str) {
    path.rsplit_once('.').unwrap_or(("", path))
}

/// Whether the object at `path` holds only the members that its kind needs: an
/// outcome, a process's exit, a connection, or an observation's access.
fn is_variant_object(path: &str) -> bool {
    matches!(split_field(path).1, "outcome" | "exit" | "access") || path.ends_with("connections[]")
}

/// Check that each object of the JSON report `json` in `mode` holds only the
/// fields that [`JSON_FIELDS`] lists for its path, in their order, and every one
/// unless its kind needs only some; return the paths of the fields it holds.
fn checked_fields(
    name: &str,
    json: &str,
    mode: ReportMode,
) -> TestResult<BTreeSet<String>> {
    let listed: Vec<&str> = mode.json_fields().collect();
    let mut seen = BTreeSet::new();
    for (path, keys) in json_objects(json)? {
        let allowed: Vec<&str> = listed
            .iter()
            .map(|field| split_field(field))
            .filter(|(parent, _)| *parent == path)
            .map(|(_, key)| key)
            .collect();
        let mut previous = None;
        for key in &keys {
            let position = allowed
                .iter()
                .position(|allowed| allowed == key)
                .ok_or_else(|| {
                    format!("{name} {mode:?}: {path} holds {key}, which is not listed")
                })?;
            if previous.is_some_and(|previous| position <= previous) {
                return Err(format!("{name} {mode:?}: {path} holds {key} out of order").into());
            }
            previous = Some(position);
            seen.insert(if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            });
        }
        if !is_variant_object(&path) {
            assert_eq!(keys, allowed, "{name} {mode:?}: the members of {path}");
        }
    }
    Ok(seen)
}

/// For every fixture, the JSON of each mode parses, and each object holds exactly
/// the fields that [`JSON_FIELDS`] lists for that mode, in their order; the full
/// fixture holds every listed field, apart from the task program's `exec` and
/// `xattr`, which no task program can have: its `fs` functions and module loads
/// only read, check, list and write paths.
#[test]
fn json_holds_exactly_the_listed_fields() -> TestResult {
    for (name, report) in every_report()? {
        for mode in [ReportMode::Hierarchical, ReportMode::Compact] {
            let seen = checked_fields(name, &report.json(mode), mode)?;
            let listed: BTreeSet<String> = mode.json_fields().map(str::to_owned).collect();
            assert!(seen.is_subset(&listed), "{name} {mode:?}");
            if name == "full" {
                let never = [
                    "task_program.observations[].access.exec",
                    "task_program.observations[].access.xattr",
                ];
                let expected: BTreeSet<String> = listed
                    .iter()
                    .filter(|field| !never.contains(&field.as_str()))
                    .cloned()
                    .collect();
                assert_eq!(seen, expected, "{name} {mode:?}: every field appears");
            }
        }
    }
    Ok(())
}

/// The field list names each path once, every field in at least one mode, and
/// each field's parent before it in the same modes.
#[test]
fn field_list_is_consistent() {
    let mut paths = BTreeSet::new();
    for field in JSON_FIELDS {
        assert!(paths.insert(field.path), "{} is listed twice", field.path);
        assert!(field.hierarchical || field.compact, "{}", field.path);
        let (parent, _) = split_field(field.path);
        let parent = parent.strip_suffix("[]").unwrap_or(parent);
        if !parent.is_empty() {
            let listed = JSON_FIELDS
                .iter()
                .take_while(|other| other.path != field.path)
                .find(|other| other.path == parent);
            assert!(
                listed.is_some_and(|listed| {
                    (listed.hierarchical || !field.hierarchical)
                        && (listed.compact || !field.compact)
                }),
                "{}: its parent {parent} is listed before it in the same modes",
                field.path
            );
        }
    }
    assert!(
        JSON_FIELDS
            .iter()
            .any(|field| field.path == "commands[].outcome.kind")
    );
}

/// The entries of every access-kind and `connections` group of the text `text`,
/// by group, sorted.
fn text_groups(text: &str) -> BTreeMap<String, Vec<String>> {
    let headers: Vec<String> = AccessKind::ALL
        .iter()
        .map(|kind| kind.name().to_owned())
        .chain(["connections".to_owned()])
        .collect();
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current: Option<(String, usize)> = None;
    for line in text.lines() {
        let content = line.trim_start();
        let depth = line.len().saturating_sub(content.len());
        if let Some(header) = content
            .strip_suffix(':')
            .filter(|header| headers.iter().any(|name| name == header))
        {
            current = Some((header.to_owned(), depth));
            continue;
        }
        match &current {
            Some((header, header_depth)) if depth > *header_depth => {
                groups
                    .entry(header.clone())
                    .or_default()
                    .push(content.to_owned());
            },
            _ => current = None,
        }
    }
    for entries in groups.values_mut() {
        entries.sort();
    }
    groups
}

/// The group entries of `observations` and `connections`, as text shows them,
/// added to `groups`.
fn json_group_entries(
    groups: &mut BTreeMap<String, Vec<String>>,
    observations: &[Value],
    connections: &[Value],
) -> TestResult {
    for observation in observations {
        let path = string(member(observation, "path")?)?;
        for (kind, outcomes) in member(observation, "access")?
            .as_object()
            .ok_or("access is not an object")?
        {
            let outcomes: Vec<&str> = outcomes
                .as_array()
                .ok_or("outcomes are not an array")?
                .iter()
                .map(string)
                .collect::<TestResult<_>>()?;
            groups
                .entry(kind.clone())
                .or_default()
                .push(format!("{path}: {}", outcomes.join(", ")));
        }
    }
    for connection in connections {
        groups
            .entry("connections".to_owned())
            .or_default()
            .push(connection_line(connection)?);
    }
    Ok(())
}

/// The group entries that the JSON report `report` in `mode` should show as text.
fn json_groups(
    report: &Value,
    mode: ReportMode,
) -> TestResult<BTreeMap<String, Vec<String>>> {
    let mut groups = BTreeMap::new();
    match mode {
        ReportMode::Hierarchical => {
            for program in json_programs(report)? {
                json_group_entries(
                    &mut groups,
                    items(program, "observations")?,
                    items(program, "connections")?,
                )?;
            }
            let task_program = member(report, "task_program")?;
            if !task_program.is_null() {
                json_group_entries(&mut groups, items(task_program, "observations")?, &[])?;
            }
        },
        ReportMode::Compact => {
            json_group_entries(
                &mut groups,
                items(report, "observations")?,
                items(report, "connections")?,
            )?;
        },
    }
    for entries in groups.values_mut() {
        entries.sort();
    }
    Ok(groups)
}

/// A JSON outcome as text shows it.
fn outcome_line(outcome: &Value) -> TestResult<String> {
    let kind = string(member(outcome, "kind")?)?;
    Ok(match kind {
        "exited" => format!("outcome: exited with {}", member(outcome, "code")?),
        "signaled" => format!("outcome: signaled with {}", member(outcome, "signal")?),
        other => {
            format!(
                "outcome: {}: {}",
                other.replace('_', " "),
                string(member(outcome, "error")?)?
            )
        },
    })
}

/// For every fixture and mode, the text and the JSON carry the same
/// observations, connections, completeness, totals and outcomes, and a
/// hierarchical text has a line for each process and program of the JSON.
#[test]
fn text_and_json_carry_the_same_observations() -> TestResult {
    for (name, report) in every_report()? {
        for mode in [ReportMode::Hierarchical, ReportMode::Compact] {
            let text = report.text(mode);
            let json: Value = serde_json::from_str(&report.json(mode))?;
            assert_eq!(
                text_groups(&text),
                json_groups(&json, mode)?,
                "{name} {mode:?}"
            );
            let lines: Vec<&str> = text.lines().collect();
            let complete = if member(&json, "complete")?.as_bool() == Some(true) {
                "trace: complete"
            } else {
                "trace: incomplete"
            };
            assert_eq!(lines.get(1).copied(), Some(complete), "{name} {mode:?}");
            let totals = member(&json, "totals")?;
            let count = |key: &str| member(totals, key).map(ToString::to_string);
            let expected_totals = format!(
                "totals: commands {}, cached {}, processes {}, programs {}, paths {}, connections {}",
                count("commands")?,
                count("cached")?,
                count("processes")?,
                count("programs")?,
                count("paths")?,
                count("connections")?
            );
            assert_eq!(
                lines.get(2).copied(),
                Some(expected_totals.as_str()),
                "{name} {mode:?}"
            );
            let kinds = member(totals, "kinds")?;
            let by_kind: Vec<String> = AccessKind::ALL
                .iter()
                .map(|kind| {
                    member(kinds, kind.name()).map(|count| format!("{} {count}", kind.name()))
                })
                .collect::<TestResult<_>>()?;
            let expected_kinds = format!("paths by access: {}", by_kind.join(", "));
            assert_eq!(
                lines.get(3).copied(),
                Some(expected_kinds.as_str()),
                "{name} {mode:?}"
            );
            let text_outcomes: Vec<&str> = lines
                .iter()
                .filter_map(|line| {
                    line.strip_prefix("  ")
                        .filter(|line| line.starts_with("outcome: "))
                })
                .collect();
            let json_outcomes: Vec<String> = items(&json, "commands")?
                .iter()
                .map(|command| outcome_line(member(command, "outcome")?))
                .collect::<TestResult<_>>()?;
            assert_eq!(text_outcomes, json_outcomes, "{name} {mode:?}");
            if mode == ReportMode::Hierarchical {
                let processes = lines
                    .iter()
                    .filter(|line| line.trim_start().starts_with("process "))
                    .count();
                let programs = lines
                    .iter()
                    .filter(|line| line.trim_start().starts_with("program "))
                    .count();
                let json_processes: usize = items(&json, "commands")?
                    .iter()
                    .map(|command| items(command, "processes").map(Vec::len))
                    .collect::<TestResult<Vec<_>>>()?
                    .into_iter()
                    .fold(0, usize::saturating_add);
                assert_eq!(processes, json_processes, "{name}: processes");
                assert_eq!(programs, json_programs(&json)?.len(), "{name}: programs");
            }
        }
    }
    Ok(())
}

/// The same model built from inputs in another order, with processes, reasons,
/// observations, connections and lookups reversed, gives byte-identical output in
/// every rendering.
#[test]
fn inputs_in_another_order_give_identical_output() -> TestResult {
    assert_ne!(full_fixture(false)?, full_fixture(true)?);
    assert_ne!(execute_fixture(false)?, execute_fixture(true)?);
    let pairs = [
        (full_report(false)?, full_report(true)?),
        (execute_report(false)?, execute_report(true)?),
    ];
    for (report, reordered) in pairs {
        for (format, mode) in RENDERINGS {
            assert_eq!(
                report.render(format, mode),
                reordered.render(format, mode),
                "{format:?} {mode:?}"
            );
        }
    }
    Ok(())
}

/// The totals count the commands, the cached commands, every process with the
/// placeholders, the programs that are not inherited, the paths once, the paths
/// of each kind, and the connections once.
#[test]
fn totals_count_as_the_spec_says() -> TestResult {
    let report = full_report(false)?;
    let totals = &report.totals;
    assert_eq!(
        (
            totals.commands,
            totals.cached,
            totals.processes,
            totals.programs
        ),
        (4, 1, 5, 5)
    );
    let all_programs: usize = report
        .commands
        .iter()
        .flat_map(|command| &command.processes)
        .map(|process| process.programs.len())
        .fold(0, usize::saturating_add);
    assert_eq!(all_programs, 7, "two inherited programs are not counted");
    assert_eq!((totals.paths, totals.connections), (19, 3));
    let kinds: Vec<usize> = AccessKind::ALL
        .iter()
        .map(|kind| totals.kind(*kind))
        .collect();
    assert_eq!(kinds, [5, 7, 1, 4, 3, 1]);
    assert_eq!(report.observations.len(), totals.paths);
    Ok(())
}

/// A command that ran with a tree of one process whose program has
/// `unknown_system_calls`, and with `lookup`.
fn one_process_command(
    unknown_system_calls: &[u32],
    lookup: Observations,
) -> TestResult<CommandInput> {
    let mut tool = program(b"/bin/tool", &[b"tool"], false, &[], false)?;
    tool.unknown_system_calls = unknown_system_calls.iter().copied().collect();
    Ok(CommandInput {
        argv: args(&[b"tool"]),
        program: Some(absolute(b"/bin/tool")?),
        cwd: absolute(b"/work")?,
        execution: Execution::Ran {
            outcome: CommandOutcome::Exited(0),
            tree: Some(ProcessTree {
                processes: vec![process(1, 10, None, vec![tool], Termination::Exited(0))],
            }),
            lookup,
        },
    })
}

/// A command whose result came from the cache.
fn cached_command() -> TestResult<CommandInput> {
    Ok(CommandInput {
        argv: args(&[b"tool"]),
        program: Some(absolute(b"/bin/tool")?),
        cwd: absolute(b"/work")?,
        execution: Execution::Cached {
            outcome: CommandOutcome::Exited(0),
        },
    })
}

/// `complete` is false whenever a command has a reason, an unknown system call
/// alone included, and for a tracer failure; `null` for a cached command, which
/// does not count; and true for a report without commands.
#[test]
fn completeness_follows_the_reasons() -> TestResult {
    let unknown = Report::new(
        vec![one_process_command(&[999], Observations::new())?],
        None,
    );
    assert!(!unknown.complete());
    let command = unknown.commands.first().ok_or("no command")?;
    assert_eq!(command.complete(), Some(false));
    let reason = command.incomplete.first().ok_or("no reason")?;
    assert_eq!((reason.number, reason.process), (Some(999), Some(1)));
    let json: Value = serde_json::from_str(&unknown.json(ReportMode::Hierarchical))?;
    assert_eq!(member(&json, "complete")?, &json!(false));

    let cached = Report::new(
        vec![
            cached_command()?,
            one_process_command(&[], Observations::new())?,
        ],
        None,
    );
    assert!(cached.complete());
    let json: Value = serde_json::from_str(&cached.json(ReportMode::Compact))?;
    assert_eq!(member(&json, "complete")?, &json!(true));
    let commands = items(&json, "commands")?;
    let completes: Vec<&Value> = commands
        .iter()
        .map(|command| member(command, "complete"))
        .collect::<TestResult<_>>()?;
    assert_eq!(completes, [&json!(null), &json!(true)]);

    let failed = full_report(false)?;
    let last = failed.commands.last().ok_or("no command")?;
    assert_eq!(last.complete(), Some(false));
    assert!(Report::new(Vec::new(), None).complete());
    Ok(())
}

/// Each outcome has its `kind` and its `code`, `signal` or `error` in JSON, and
/// its line in text.
#[test]
fn outcomes_have_their_kinds() -> TestResult {
    let cases = [
        (
            CommandOutcome::Exited(3),
            json!({"kind": "exited", "code": 3}),
            "exited with 3",
        ),
        (
            CommandOutcome::Signaled(15),
            json!({"kind": "signaled", "signal": 15}),
            "signaled with 15",
        ),
        (
            CommandOutcome::NotFound("gone".to_owned()),
            json!({"kind": "not_found", "error": "gone"}),
            "not found: gone",
        ),
        (
            CommandOutcome::NotExecutable("denied".to_owned()),
            json!({"kind": "not_executable", "error": "denied"}),
            "not executable: denied",
        ),
        (
            CommandOutcome::StartFailed("busy".to_owned()),
            json!({"kind": "start_failed", "error": "busy"}),
            "start failed: busy",
        ),
        (
            CommandOutcome::Failed("broke".to_owned()),
            json!({"kind": "failed", "error": "broke"}),
            "failed: broke",
        ),
    ];
    for (outcome, expected_json, expected_text) in cases {
        let report = Report::new(
            vec![CommandInput {
                argv: args(&[b"tool"]),
                program: None,
                cwd: absolute(b"/work")?,
                execution: Execution::Ran {
                    outcome,
                    tree: None,
                    lookup: Observations::new(),
                },
            }],
            None,
        );
        let json: Value = serde_json::from_str(&report.json(ReportMode::Hierarchical))?;
        let command = items(&json, "commands")?.first().ok_or("no command")?;
        assert_eq!(member(command, "outcome")?, &expected_json);
        let text = report.text(ReportMode::Hierarchical);
        assert!(
            text.contains(&format!("\n  outcome: {expected_text}\n")),
            "{text}"
        );
    }
    Ok(())
}

/// A tracer's termination and each kind of tracer error map to their outcomes,
/// which keep the error's message.
#[test]
fn tracer_results_map_to_outcomes() -> TestResult {
    assert_eq!(
        CommandOutcome::from_termination(Termination::Exited(2)),
        CommandOutcome::Exited(2)
    );
    assert_eq!(
        CommandOutcome::from_termination(Termination::Signaled(9)),
        CommandOutcome::Signaled(9)
    );
    let program = absolute(b"/bin/tool")?;
    let errors = [
        (
            TraceError::ProgramNotFound {
                program: program.clone(),
                accesses: Accesses::new(),
                tree: None,
            },
            "not_found",
        ),
        (
            TraceError::ProgramNotExecutable {
                program: program.clone(),
                source: io::Error::from(io::ErrorKind::PermissionDenied),
                accesses: Accesses::new(),
                tree: None,
            },
            "not_executable",
        ),
        (
            TraceError::StartFailed {
                program,
                source: io::Error::from(io::ErrorKind::OutOfMemory),
                accesses: Accesses::new(),
                tree: None,
            },
            "start_failed",
        ),
        (
            TraceError::Tracer {
                message: "cannot attach".to_owned(),
                source: None,
            },
            "failed",
        ),
    ];
    for (error, kind) in errors {
        let outcome = CommandOutcome::from_error(&error);
        assert_eq!(outcome.kind(), kind);
        assert!(
            outcome.text().ends_with(&error.to_string()),
            "{}",
            outcome.text()
        );
    }
    Ok(())
}

/// Process numbers count from 1 across every command, parents follow their
/// processes' new numbers, a parent outside the tree becomes `null`, the lookup
/// goes into the first program of the first process, and a tree without
/// processes gets the placeholder process.
#[test]
fn processes_are_numbered_across_commands() -> TestResult {
    let first = CommandInput {
        argv: args(&[b"tool"]),
        program: Some(absolute(b"/bin/tool")?),
        cwd: absolute(b"/work")?,
        execution: Execution::Ran {
            outcome: CommandOutcome::Exited(0),
            tree: Some(ProcessTree {
                processes: vec![
                    process(
                        9,
                        30,
                        Some(4),
                        vec![program(b"/bin/child", &[b"child"], false, &[], false)?],
                        Termination::Exited(0),
                    ),
                    process(
                        4,
                        20,
                        None,
                        vec![program(b"/bin/tool", &[b"tool"], false, &[], false)?],
                        Termination::Exited(0),
                    ),
                    process(12, 40, Some(77), Vec::new(), Termination::Exited(1)),
                ],
            }),
            lookup: observations(&[(b"/bin/tool", AccessKind::Probe, "ok")], false)?,
        },
    };
    let mut second = one_process_command(&[], Observations::new())?;
    second.execution = Execution::Ran {
        outcome: CommandOutcome::Exited(0),
        tree: Some(ProcessTree::default()),
        lookup: observations(&[(b"/usr/bin/tool", AccessKind::Probe, "ENOENT")], false)?,
    };
    let report = Report::new(vec![first, cached_command()?, second], None);
    let numbering: Vec<Vec<Numbering>> = report
        .commands
        .iter()
        .map(|command| {
            command
                .processes
                .iter()
                .map(|process| (process.id, process.pid, process.parent))
                .collect()
        })
        .collect();
    assert_eq!(numbering, [
        vec![
            (1, Some(20), None),
            (2, Some(30), Some(1)),
            (3, Some(40), None)
        ],
        Vec::new(),
        vec![(4, None, None)],
    ]);
    let tool = report
        .commands
        .first()
        .and_then(|command| command.processes.first())
        .and_then(|process| process.programs.first())
        .ok_or("no first program")?;
    assert!(tool.observations.get(&absolute(b"/bin/tool")?).is_some());
    let placeholder = report
        .commands
        .last()
        .and_then(|command| command.processes.first())
        .and_then(|process| process.programs.first())
        .ok_or("no placeholder program")?;
    assert_eq!(placeholder.path.as_os_str(), "tool");
    assert!(
        placeholder
            .observations
            .get(&absolute(b"/usr/bin/tool")?)
            .is_some()
    );
    let text = report.text(ReportMode::Hierarchical);
    assert!(text.contains("\n  process 1, pid 20, exited with 0\n    program /bin/tool: tool\n      probe:\n        /bin/tool: ok\n    process 2, pid 30, child of process 1, exited with 0\n"), "{text}");
    assert!(
        text.contains("\n  process 3, pid 40, exited with 1\n"),
        "{text}"
    );
    assert!(
        text.contains("\n  process 4, not started\n    program tool: tool\n"),
        "{text}"
    );
    Ok(())
}
