// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! End-to-end tests of `kizz execute` with the real tracer ("Observing any command
//! with `kizz execute`").
//!
//! Each test runs the built binary in a temporary directory that holds no
//! `kizz.md` and no repository, with an explicit environment: `KIZZ_CACHE_DIR`, set
//! to a temporary directory, and `PATH`, plus what a test adds. The one exception
//! is [`no_cache_directory_is_created`], which gives no `KIZZ_CACHE_DIR` and sets
//! `HOME` and `XDG_CACHE_HOME` to temporary directories instead, so it cannot reach
//! the real cache either. The tests cover the standard streams, the exit statuses,
//! the environment, the working directory and the lookup, the signals and job
//! control, the cache, the banner, the command-line errors, the report's file, a
//! nested `kizz execute`, the four renderings, and a file name with a newline.
//!
//! Every wait is bounded: a run that does not end in time is killed, and the test
//! fails.

#![allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and start plain std threads"
)]

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    ffi::{OsStr, OsString},
    fs,
    io::{self, Write as _},
    os::unix::{ffi::OsStrExt as _, fs::PermissionsExt as _, process::CommandExt as _},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;
use tempfile::TempDir;

/// The result of a test or a test helper.
type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// The `PATH` that every run of `kizz` gets, unless a test says otherwise.
const PATH: &str = "/usr/bin:/bin";

/// How long a test waits for something that a process does.
const PATIENCE: Duration = Duration::from_secs(60);

/// How long a test waits for a second delivery of a signal that must arrive once.
const SETTLE: &str = "0.3";

/// The exit status of a Rust process that panicked.
const PANIC_STATUS: i32 = 101;

/// The temporary directories of one test: the working directory of `kizz` and the
/// cache that `KIZZ_CACHE_DIR` names.
struct Fixture {
    /// The working directory, canonical.
    dir: PathBuf,
    /// The cache directory.
    cache: PathBuf,
    /// The temporary directory that holds both.
    _root: TempDir,
}

impl Fixture {
    /// A working directory and a cache, both empty.
    fn new() -> TestResult<Self> {
        let root = tempfile::tempdir()?;
        let base = root.path().canonicalize()?;
        let dir = base.join("work");
        let cache = base.join("cache");
        fs::create_dir(&dir)?;
        fs::create_dir(&cache)?;
        Ok(Self {
            dir,
            cache,
            _root: root,
        })
    }

    /// The environment of a run: `KIZZ_CACHE_DIR` and `PATH`.
    fn env(&self) -> Vec<(OsString, OsString)> {
        vec![
            (
                OsString::from("KIZZ_CACHE_DIR"),
                self.cache.clone().into_os_string(),
            ),
            (OsString::from("PATH"), OsString::from(PATH)),
        ]
    }

    /// `kizz` with `args`, in the working directory, with `env` as its whole
    /// environment and no standard input.
    fn command_with(
        &self,
        env: &[(OsString, OsString)],
        args: &[&OsStr],
    ) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kizz"));
        command
            .args(args)
            .env_clear()
            .envs(env.iter().map(|(name, value)| (name, value)))
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// `kizz` with `args`, as [`Fixture::command_with`] runs it with
    /// [`Fixture::env`].
    fn command(
        &self,
        args: &[&str],
    ) -> Command {
        let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        self.command_with(&self.env(), &args)
    }

    /// The path of `name` in the working directory.
    fn path(
        &self,
        name: &str,
    ) -> PathBuf {
        self.dir.join(name)
    }

    /// The JSON report in `name`, parsed.
    fn json(
        &self,
        name: &str,
    ) -> TestResult<Value> {
        Ok(serde_json::from_str(&fs::read_to_string(self.path(name))?)?)
    }
}

/// What one run of `kizz` produced.
#[derive(Debug)]
struct Run {
    /// The exit code, or `None` when a signal killed the process.
    status: Option<i32>,
    /// Standard output, as bytes.
    stdout: Vec<u8>,
    /// Standard error, with invalid UTF-8 replaced.
    stderr: String,
}

/// Wait for `child` to end and take its output; kill it when it does not end in
/// time.
fn finish(child: Child) -> TestResult<Run> {
    let pid = libc::pid_t::try_from(child.id())?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || drop(sender.send(child.wait_with_output())));
    let output: Output = if let Ok(output) = receiver.recv_timeout(PATIENCE) {
        output?
    } else {
        // SAFETY: `kill` has no memory-safety requirements.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        return Err("kizz did not finish in time".into());
    };
    let run = Run {
        status: output.status.code(),
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    assert_ne!(run.status, Some(PANIC_STATUS), "{run:?}");
    assert!(!run.stderr.contains("panicked"), "{run:?}");
    Ok(run)
}

/// Run `command` to its end, with `input` on its standard input when it is given.
fn run_kizz(
    command: &mut Command,
    input: Option<&[u8]>,
) -> TestResult<Run> {
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn()?;
    if let Some(input) = input {
        let mut stdin = child.stdin.take().ok_or("no standard input")?;
        stdin.write_all(input)?;
    }
    finish(child)
}

/// Wait until `condition` holds, failing with `what` when it does not in time.
fn wait_until(
    what: &str,
    condition: impl Fn() -> bool,
) -> TestResult {
    let deadline = Instant::now()
        .checked_add(PATIENCE)
        .ok_or("the deadline is out of range")?;
    while !condition() {
        if Instant::now() >= deadline {
            return Err(format!("{what} did not happen in time").into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// Send `signal` to the process, or with a negative number the process group,
/// `target`.
fn send(
    target: libc::pid_t,
    signal: libc::c_int,
) -> TestResult {
    // SAFETY: `kill` has no memory-safety requirements.
    if unsafe { libc::kill(target, signal) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

/// The status of the child `pid` that `waitpid` reports with `flags` and
/// `WNOHANG`, polled until it reports one, or `None` when it does not in time.
fn wait_status(
    pid: libc::pid_t,
    flags: libc::c_int,
) -> TestResult<libc::c_int> {
    let deadline = Instant::now()
        .checked_add(PATIENCE)
        .ok_or("the deadline is out of range")?;
    loop {
        let mut status = 0;
        // SAFETY: `status` is valid for writing.
        let waited = unsafe { libc::waitpid(pid, &raw mut status, flags | libc::WNOHANG) };
        if waited == pid {
            return Ok(status);
        }
        if waited < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if Instant::now() >= deadline {
            drop(send(group(pid), libc::SIGKILL));
            return Err("kizz did not change its state in time".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// The target of `kill` for the process group whose leader is `pid`: its negated
/// process ID.
const fn group(pid: libc::pid_t) -> libc::pid_t {
    pid.wrapping_neg()
}

/// The value at the JSON pointer `pointer` in `value`, or `null` when there is
/// none.
fn at<'a>(
    value: &'a Value,
    pointer: &str,
) -> &'a Value {
    value.pointer(pointer).unwrap_or(&Value::Null)
}

/// The report's command `index` in the JSON report `report`.
fn command(
    report: &Value,
    index: usize,
) -> &Value {
    at(report, &format!("/commands/{index}"))
}

/// The first program of the first process of the JSON report's first command.
fn first_program(report: &Value) -> &Value {
    at(report, "/commands/0/processes/0/programs/0")
}

/// The outcomes of `kind` of `path` in the JSON `observations`, as strings.
fn outcomes(
    observations: &Value,
    path: &Path,
    kind: &str,
) -> BTreeSet<String> {
    observations
        .as_array()
        .into_iter()
        .flatten()
        .filter(|observation| at(observation, "/path").as_str() == path.to_str())
        .flat_map(|observation| {
            observation
                .get("access")
                .and_then(|access| access.get(kind))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|outcome| outcome.as_str().map(str::to_owned))
        .collect()
}

/// The program's standard output and error and its exit code reach kizz's own
/// streams, and only they: kizz adds nothing, exits with the program's code, and
/// its report says `exited` with that code.
#[test]
fn the_program_keeps_the_streams_and_its_exit_code() -> TestResult {
    let fixture = Fixture::new()?;
    let run = run_kizz(
        &mut fixture.command(&[
            "execute",
            "--json",
            "--output=report.json",
            "--",
            "sh",
            "-c",
            "printf out; printf err >&2; exit 3",
        ]),
        None,
    )?;
    assert_eq!(run.status, Some(3), "{run:?}");
    assert_eq!(run.stdout, b"out", "{run:?}");
    assert_eq!(run.stderr, "err", "{run:?}");
    let report = fixture.json("report.json")?;
    assert_eq!(command(&report, 0)["outcome"]["kind"], "exited");
    assert_eq!(command(&report, 0)["outcome"]["code"], 3);
    Ok(())
}

/// Text that kizz gets on its standard input reaches `cat`, which writes it back
/// on kizz's standard output.
#[test]
fn standard_input_reaches_the_program() -> TestResult {
    let fixture = Fixture::new()?;
    let run = run_kizz(
        &mut fixture.command(&["execute", "--output=report.txt", "--", "cat"]),
        Some(b"typed\nlines\n"),
    )?;
    assert_eq!(run.status, Some(0), "{run:?}");
    assert_eq!(run.stdout, b"typed\nlines\n", "{run:?}");
    Ok(())
}

/// The program sees exactly the environment that kizz was given, a value that is
/// not valid UTF-8 and an empty one included, and the working directory that kizz
/// was started in.
#[test]
fn the_program_gets_the_caller_s_environment_and_directory() -> TestResult {
    let fixture = Fixture::new()?;
    let mut env = fixture.env();
    env.push((OsString::from("SPACED"), OsString::from("two words")));
    env.push((OsString::from("EMPTY"), OsString::new()));
    env.push((
        OsString::from("BYTES"),
        OsStr::from_bytes(b"caf\xFF").to_owned(),
    ));
    let args = [
        "execute",
        "--output=report.txt",
        "--",
        "cat",
        "/proc/self/environ",
    ]
    .map(OsStr::new);
    let run = run_kizz(&mut fixture.command_with(&env, &args), None)?;
    assert_eq!(run.status, Some(0), "{run:?}");
    let seen: BTreeSet<Vec<u8>> = run
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    let given: BTreeSet<Vec<u8>> = env
        .iter()
        .map(|(name, value)| [name.as_bytes(), b"=", value.as_bytes()].concat())
        .collect();
    assert_eq!(seen, given);

    let args = [
        "execute",
        "--output=report.txt",
        "--",
        "readlink",
        "/proc/self/cwd",
    ]
    .map(OsStr::new);
    let run = run_kizz(&mut fixture.command_with(&fixture.env(), &args), None)?;
    assert_eq!(run.status, Some(0), "{run:?}");
    assert_eq!(
        run.stdout,
        [fixture.dir.as_os_str().as_bytes(), b"\n"].concat()
    );
    Ok(())
}

/// With a `PATH` whose first directory lacks the program, the first program of
/// process 1 has a probe of that location with `ENOENT` and a probe of the found
/// program with `ok`.
#[test]
fn the_lookup_s_probes_are_observations_of_process_1() -> TestResult {
    let fixture = Fixture::new()?;
    let empty = fixture.path("empty");
    fs::create_dir(&empty)?;
    let found = ["/usr/bin/true", "/bin/true"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| {
            fs::metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .ok_or("no true program in /usr/bin or /bin")?;
    let mut env = fixture.env();
    env.retain(|(name, _)| name != "PATH");
    env.push((
        OsString::from("PATH"),
        OsString::from(format!("{}:{PATH}", empty.display())),
    ));
    let args = ["execute", "--json", "--output=report.json", "--", "true"].map(OsStr::new);
    let run = run_kizz(&mut fixture.command_with(&env, &args), None)?;
    assert_eq!(run.status, Some(0), "{run:?}");
    let report = fixture.json("report.json")?;
    let process = &command(&report, 0)["processes"][0];
    assert_eq!(process["id"], 1);
    let observations = &first_program(&report)["observations"];
    assert_eq!(
        outcomes(observations, &empty.join("true"), "probe"),
        BTreeSet::from(["ENOENT".to_owned()]),
        "{report}"
    );
    assert!(
        outcomes(observations, &found, "probe").contains("ok"),
        "{report}"
    );
    assert_eq!(command(&report, 0)["program"], found.display().to_string());
    Ok(())
}

/// A name that no directory of `PATH` holds gives status 127 and a report that
/// says `not_found`, with the placeholder process and its probes; without `PATH`,
/// the same, with a message that says that `PATH` is not set, and no probe.
#[test]
fn a_program_that_is_not_found_gives_127() -> TestResult {
    let fixture = Fixture::new()?;
    let name = "kizz-test-no-such-program";
    let run = run_kizz(
        &mut fixture.command(&["execute", "--json", "--output=report.json", "--", name, "x"]),
        None,
    )?;
    assert_eq!(run.status, Some(127), "{run:?}");
    assert!(run.stderr.contains(name), "{run:?}");
    let report = fixture.json("report.json")?;
    let found = command(&report, 0);
    assert_eq!(found["outcome"]["kind"], "not_found");
    assert_eq!(found["program"], Value::Null);
    let process = &found["processes"][0];
    assert_eq!(
        (&process["pid"], &process["exit"]),
        (&Value::Null, &Value::Null)
    );
    let program = first_program(&report);
    assert_eq!(program["path"], name);
    for directory in PATH.split(':') {
        assert_eq!(
            outcomes(
                &program["observations"],
                &Path::new(directory).join(name),
                "probe"
            ),
            BTreeSet::from(["ENOENT".to_owned()]),
            "{report}"
        );
    }

    let env: Vec<(OsString, OsString)> = fixture
        .env()
        .into_iter()
        .filter(|(name, _)| name != "PATH")
        .collect();
    let args = ["execute", "--json", "--output=report.json", "--", "sh"].map(OsStr::new);
    let run = run_kizz(&mut fixture.command_with(&env, &args), None)?;
    assert_eq!(run.status, Some(127), "{run:?}");
    let report = fixture.json("report.json")?;
    assert_eq!(command(&report, 0)["outcome"]["kind"], "not_found");
    let message = command(&report, 0)["outcome"]["error"]
        .as_str()
        .ok_or("no message")?;
    assert!(message.contains("PATH is not set"), "{message}");
    assert_eq!(
        first_program(&report)["observations"],
        Value::Array(Vec::new())
    );
    Ok(())
}

/// A file without execute permission, given by its path, gives status 126 and
/// `not_executable`.
#[test]
fn a_file_without_execute_permission_gives_126() -> TestResult {
    let fixture = Fixture::new()?;
    let script = fixture.path("script");
    fs::write(&script, "#!/bin/sh\nexit 0\n")?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o644))?;
    let run = run_kizz(
        &mut fixture.command(&[
            "execute",
            "--json",
            "--output=report.json",
            "--",
            "./script",
        ]),
        None,
    )?;
    assert_eq!(run.status, Some(126), "{run:?}");
    let report = fixture.json("report.json")?;
    assert_eq!(command(&report, 0)["outcome"]["kind"], "not_executable");
    Ok(())
}

/// A program that `SIGTERM` kills gives status 143 and `signaled` with 15.
#[test]
fn a_program_killed_by_a_signal_gives_128_plus_its_number() -> TestResult {
    let fixture = Fixture::new()?;
    let run = run_kizz(
        &mut fixture.command(&[
            "execute",
            "--json",
            "--output=report.json",
            "--",
            "sh",
            "-c",
            "kill -TERM $$",
        ]),
        None,
    )?;
    assert_eq!(run.status, Some(143), "{run:?}");
    let report = fixture.json("report.json")?;
    assert_eq!(command(&report, 0)["outcome"]["kind"], "signaled");
    assert_eq!(command(&report, 0)["outcome"]["signal"], 15);
    Ok(())
}

/// The script of a program that writes `ready` once it runs, then waits.
const WAITING: &str = "echo ready > ready; while :; do sleep 0.05; done";

/// Start `kizz execute` for `sh -c SCRIPT` in a process group of its own, with its
/// output going nowhere, and wait until the program wrote `ready`; return the child
/// and its process ID, which is also its group's.
fn start_in_own_group(
    fixture: &Fixture,
    script: &str,
) -> TestResult<(Child, libc::pid_t)> {
    let mut command =
        fixture.command(&["execute", "--output=report.txt", "--", "sh", "-c", script]);
    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let child = command.spawn()?;
    let pid = libc::pid_t::try_from(child.id())?;
    let ready = fixture.path("ready");
    if let Err(error) = wait_until("the program's start", || ready.exists()) {
        drop(send(group(pid), libc::SIGKILL));
        return Err(error);
    }
    Ok((child, pid))
}

/// The exit code of the child `pid` once it has exited, polled with `waitpid`;
/// `None` when a signal killed it.
fn exit_code(pid: libc::pid_t) -> TestResult<Option<i32>> {
    let status = wait_status(pid, 0)?;
    Ok(libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status)))
}

/// With kizz in a process group of its own, `SIGINT` sent to the whole group
/// reaches the program, which records it once with a trap and exits with 7, and
/// kizz survives it and exits with 7; a program that does not trap it dies of it,
/// and kizz exits with 130.
#[test]
fn sigint_to_the_group_reaches_the_program_and_not_kizz() -> TestResult {
    let fixture = Fixture::new()?;
    let trapped = format!("trap 'echo INT >> traps; exit 7' INT; {WAITING}");
    let (_child, pid) = start_in_own_group(&fixture, &trapped)?;
    send(group(pid), libc::SIGINT)?;
    assert_eq!(exit_code(pid)?, Some(7));
    assert_eq!(fs::read_to_string(fixture.path("traps"))?, "INT\n");
    assert!(fs::read_to_string(fixture.path("report.txt"))?.contains("outcome: exited with 7"));

    fs::remove_file(fixture.path("ready"))?;
    let (_child, pid) = start_in_own_group(&fixture, WAITING)?;
    send(group(pid), libc::SIGINT)?;
    assert_eq!(exit_code(pid)?, Some(130));
    assert!(fs::read_to_string(fixture.path("report.txt"))?.contains("outcome: signaled with 2"));
    Ok(())
}

/// `SIGTERM` and `SIGHUP` sent to kizz alone each reach the program once, which a
/// trap records; the program then ends by itself, and kizz exits with its status.
#[test]
fn sigterm_and_sighup_to_kizz_reach_the_program_once() -> TestResult {
    for (signal, name) in [(libc::SIGTERM, "TERM"), (libc::SIGHUP, "HUP")] {
        let fixture = Fixture::new()?;
        let script = format!(
            "trap 'echo {name} >> traps' {name}; echo ready > ready; while [ ! -s traps ]; do \
             sleep 0.05; done; sleep {SETTLE}; exit 0"
        );
        let (_child, pid) = start_in_own_group(&fixture, &script)?;
        send(pid, signal)?;
        assert_eq!(exit_code(pid)?, Some(0), "{name}");
        assert_eq!(
            fs::read_to_string(fixture.path("traps"))?,
            format!("{name}\n"),
            "{name}"
        );
    }
    Ok(())
}

/// The masks of the line that starts with `name` and a colon in the status that
/// `cat /proc/self/status` wrote.
fn mask(
    status: &str,
    name: &str,
) -> TestResult<u64> {
    let prefix = format!("{name}:");
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .ok_or_else(|| format!("no {name} line in {status}"))?;
    Ok(u64::from_str_radix(line.trim(), 16)?)
}

/// The bit of `signal` in a mask of `/proc/<pid>/status`.
fn bit(signal: libc::c_int) -> TestResult<u64> {
    let shift = u32::try_from(signal)?
        .checked_sub(1)
        .ok_or("there is no signal 0")?;
    Ok(1_u64
        .checked_shl(shift)
        .ok_or("the signal is out of range")?)
}

/// Kizz started without ignoring `SIGINT` runs a program in which `SIGINT` and
/// `SIGQUIT` are at their default action, neither ignored nor caught; kizz started
/// with `SIGINT` ignored runs one in which `SIGINT` is ignored.
#[test]
fn signals_ignored_at_the_start_stay_ignored() -> TestResult {
    let fixture = Fixture::new()?;
    let both = bit(libc::SIGINT)? | bit(libc::SIGQUIT)?;
    let args = [
        "execute",
        "--output=report.txt",
        "--",
        "cat",
        "/proc/self/status",
    ];
    let plain = run_kizz(&mut fixture.command(&args), None)?;
    assert_eq!(plain.status, Some(0), "{plain:?}");
    let status = String::from_utf8(plain.stdout)?;
    assert_eq!(mask(&status, "SigIgn")? & both, 0, "{status}");
    assert_eq!(mask(&status, "SigCgt")? & both, 0, "{status}");

    let mut command = fixture.command(&args);
    // SAFETY: `signal` is async-signal-safe, which `pre_exec` requires.
    unsafe {
        command.pre_exec(|| {
            if libc::signal(libc::SIGINT, libc::SIG_IGN) == libc::SIG_ERR {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let ignoring = run_kizz(&mut command, None)?;
    assert_eq!(ignoring.status, Some(0), "{ignoring:?}");
    let status = String::from_utf8(ignoring.stdout)?;
    assert_eq!(
        mask(&status, "SigIgn")? & both,
        bit(libc::SIGINT)?,
        "{status}"
    );
    assert_eq!(mask(&status, "SigCgt")? & both, 0, "{status}");
    Ok(())
}

/// The program's process group and session, read from its `/proc/self/stat`, are
/// kizz's.
#[test]
fn the_program_shares_kizz_s_group_and_session() -> TestResult {
    let fixture = Fixture::new()?;
    let mut command = fixture.command(&[
        "execute",
        "--output=report.txt",
        "--",
        "cat",
        "/proc/self/stat",
    ]);
    command.process_group(0);
    let child = command.spawn()?;
    let kizz = libc::pid_t::try_from(child.id())?;
    let run = finish(child)?;
    assert_eq!(run.status, Some(0), "{run:?}");
    let stat = String::from_utf8(run.stdout)?;
    let (_, rest) = stat.rsplit_once(')').ok_or("the stat line has no name")?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let group: libc::pid_t = fields.get(2).ok_or("no process group")?.parse()?;
    let session: libc::pid_t = fields.get(3).ok_or("no session")?.parse()?;
    // SAFETY: `getsid` has no requirements.
    let own_session = unsafe { libc::getsid(0) };
    assert_eq!((group, session), (kizz, own_session), "{stat}");
    Ok(())
}

/// `SIGTSTP` sent to the whole group stops kizz, which `waitpid` with `WUNTRACED`
/// sees, and `SIGCONT` lets the program finish, with its status and its report.
#[test]
fn ctrl_z_stops_kizz_and_sigcont_continues_it() -> TestResult {
    let fixture = Fixture::new()?;
    let (_child, pid) = start_in_own_group(&fixture, "echo ready > ready; sleep 0.5; exit 5")?;
    send(group(pid), libc::SIGTSTP)?;
    let stopped = wait_status(pid, libc::WUNTRACED)?;
    assert!(libc::WIFSTOPPED(stopped), "status {stopped:#x}");
    assert_eq!(libc::WSTOPSIG(stopped), libc::SIGTSTP);
    send(group(pid), libc::SIGCONT)?;
    assert_eq!(exit_code(pid)?, Some(5));
    assert!(fs::read_to_string(fixture.path("report.txt"))?.contains("outcome: exited with 5"));
    Ok(())
}

/// Every entry of `dir`, recursively, as paths relative to it.
fn entries(dir: &Path) -> TestResult<BTreeSet<PathBuf>> {
    let mut found = BTreeSet::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            found.insert(path.strip_prefix(dir)?.to_path_buf());
        }
    }
    Ok(found)
}

/// With `KIZZ_CACHE_DIR` set to a path that does not exist, it still does not exist
/// afterwards; without `KIZZ_CACHE_DIR`, `HOME` and `XDG_CACHE_HOME` hold no cache
/// directory afterwards; and the working directory, with no `kizz.md` and no
/// repository, holds only the report.
#[test]
fn no_cache_directory_is_created() -> TestResult {
    let fixture = Fixture::new()?;
    assert!(entries(&fixture.dir)?.is_empty());
    let missing = fixture.path("../missing-cache");
    let env = vec![
        (
            OsString::from("KIZZ_CACHE_DIR"),
            missing.clone().into_os_string(),
        ),
        (OsString::from("PATH"), OsString::from(PATH)),
    ];
    let args = ["execute", "--output=report.txt", "--", "true"].map(OsStr::new);
    let run = run_kizz(&mut fixture.command_with(&env, &args), None)?;
    assert_eq!(run.status, Some(0), "{run:?}");
    assert!(!missing.exists());

    let home = fixture.path("../home");
    let xdg = fixture.path("../xdg");
    fs::create_dir(&home)?;
    fs::create_dir(&xdg)?;
    let env = vec![
        (OsString::from("HOME"), home.clone().into_os_string()),
        (
            OsString::from("XDG_CACHE_HOME"),
            xdg.clone().into_os_string(),
        ),
        (OsString::from("PATH"), OsString::from(PATH)),
    ];
    let run = run_kizz(&mut fixture.command_with(&env, &args), None)?;
    assert_eq!(run.status, Some(0), "{run:?}");
    assert!(entries(&home)?.is_empty());
    assert!(entries(&xdg)?.is_empty());
    assert_eq!(
        entries(&fixture.dir)?,
        BTreeSet::from([PathBuf::from("report.txt")])
    );
    assert!(fs::read_to_string(fixture.path("report.txt"))?.contains("trace: complete"));
    Ok(())
}

/// Standard output holds no banner, and nothing at all for a program that writes
/// nothing, with or without `--quiet`.
#[test]
fn no_banner_is_printed() -> TestResult {
    let fixture = Fixture::new()?;
    for args in [
        &["execute", "--output=report.txt", "--", "true"][..],
        &["--quiet", "execute", "--output=report.txt", "--", "true"][..],
        &[
            "--color",
            "always",
            "execute",
            "--output=report.txt",
            "--",
            "true",
        ][..],
    ] {
        let run = run_kizz(&mut fixture.command(args), None)?;
        assert_eq!(run.status, Some(0), "{args:?}: {run:?}");
        assert!(run.stdout.is_empty(), "{args:?}: {run:?}");
        assert!(run.stderr.is_empty(), "{args:?}: {run:?}");
    }
    Ok(())
}

/// A missing `--output`, a missing `--`, a missing program, a missing directory for
/// `--output` and a directory as `--output` each exit with 2 before the program
/// runs, which a program that would create a file proves, and write no report.
#[test]
fn command_line_errors_exit_with_2_before_the_program_runs() -> TestResult {
    let fixture = Fixture::new()?;
    fs::create_dir(fixture.path("sub"))?;
    let creates = ["sh", "-c", ": > ran"];
    let cases: [Vec<&str>; 6] = [
        [&["execute", "--"][..], &creates[..]].concat(),
        [&["execute", "--output=report.txt"][..], &creates[..]].concat(),
        vec!["execute", "--output=report.txt", "--"],
        vec!["execute", "--output=report.txt"],
        [
            &["execute", "--output=missing/report.txt", "--"][..],
            &creates[..],
        ]
        .concat(),
        [&["execute", "--output=sub", "--"][..], &creates[..]].concat(),
    ];
    for args in cases {
        let run = run_kizz(&mut fixture.command(&args), None)?;
        assert_eq!(run.status, Some(2), "{args:?}: {run:?}");
        assert!(run.stdout.is_empty(), "{args:?}: {run:?}");
        assert!(!run.stderr.is_empty(), "{args:?}: {run:?}");
        assert!(!fixture.path("ran").exists(), "{args:?}");
        assert!(!fixture.path("report.txt").exists(), "{args:?}");
        assert!(entries(&fixture.path("sub"))?.is_empty(), "{args:?}");
    }
    Ok(())
}

/// An existing `--output` file is replaced by the report, and the program reads its
/// old content, not the report; no temporary file stays behind.
#[test]
fn the_report_replaces_its_file_after_the_program() -> TestResult {
    let fixture = Fixture::new()?;
    fs::write(fixture.path("report.txt"), "old content\n")?;
    let run = run_kizz(
        &mut fixture.command(&[
            "execute",
            "--output=report.txt",
            "--",
            "sh",
            "-c",
            "cat report.txt > seen.txt",
        ]),
        None,
    )?;
    assert_eq!(run.status, Some(0), "{run:?}");
    assert_eq!(
        fs::read_to_string(fixture.path("seen.txt"))?,
        "old content\n"
    );
    assert!(
        fs::read_to_string(fixture.path("report.txt"))?
            .starts_with("kizz observation report, version 1, hierarchical\n")
    );
    assert_eq!(
        entries(&fixture.dir)?,
        BTreeSet::from([PathBuf::from("report.txt"), PathBuf::from("seen.txt")])
    );
    Ok(())
}

/// A program that removes the directory of `--output` makes kizz exit with 125 and
/// print the error.
#[test]
fn a_report_that_cannot_be_written_gives_125() -> TestResult {
    let fixture = Fixture::new()?;
    fs::create_dir(fixture.path("out"))?;
    let run = run_kizz(
        &mut fixture.command(&[
            "execute",
            "--output=out/report.txt",
            "--",
            "rm",
            "-r",
            "out",
        ]),
        None,
    )?;
    assert_eq!(run.status, Some(125), "{run:?}");
    assert!(run.stderr.contains("cannot write the report"), "{run:?}");
    assert!(!fixture.path("out").exists());
    Ok(())
}

/// `kizz execute` run under another `kizz execute` cannot trace, so the inner kizz
/// exits with 125, which the outer one passes on, and its report says `failed`,
/// incomplete, with a `tracer_failed` reason.
#[test]
fn kizz_execute_under_kizz_execute_cannot_trace() -> TestResult {
    let fixture = Fixture::new()?;
    let kizz = env!("CARGO_BIN_EXE_kizz");
    let run = run_kizz(
        &mut fixture.command(&[
            "execute",
            "--json",
            "--output=outer.json",
            "--",
            kizz,
            "execute",
            "--json",
            "--output=inner.json",
            "--",
            "true",
        ]),
        None,
    )?;
    assert_eq!(run.status, Some(125), "{run:?}");
    let inner = fixture.json("inner.json")?;
    let command = command(&inner, 0);
    assert_eq!(command["outcome"]["kind"], "failed", "{inner}");
    assert_eq!(inner["complete"], false, "{inner}");
    assert_eq!(command["complete"], false, "{inner}");
    let reasons = command["incomplete"].as_array().ok_or("no reasons")?;
    assert!(
        reasons
            .iter()
            .any(|reason| reason["kind"] == "tracer_failed"),
        "{inner}"
    );
    let outer = fixture.json("outer.json")?;
    assert_eq!(outer["commands"][0]["outcome"]["code"], 125, "{outer}");
    Ok(())
}

/// The observations of a JSON report, by path, each with its kinds and outcomes:
/// combined over every program of a hierarchical report, or the compact list.
fn combined(report: &Value) -> BTreeMap<String, BTreeMap<String, BTreeSet<String>>> {
    let mut lists = Vec::new();
    if at(report, "/mode") == "compact" {
        lists.push(at(report, "/observations"));
    } else {
        for command in at(report, "/commands").as_array().into_iter().flatten() {
            for process in at(command, "/processes").as_array().into_iter().flatten() {
                for program in at(process, "/programs").as_array().into_iter().flatten() {
                    lists.push(at(program, "/observations"));
                }
            }
        }
    }
    let mut map: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();
    for observation in lists.into_iter().filter_map(Value::as_array).flatten() {
        let path = at(observation, "/path")
            .as_str()
            .unwrap_or_default()
            .to_owned();
        for (kind, outcomes) in at(observation, "/access").as_object().into_iter().flatten() {
            map.entry(path.clone())
                .or_default()
                .entry(kind.clone())
                .or_default()
                .extend(
                    outcomes
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|outcome| outcome.as_str().map(str::to_owned)),
                );
        }
    }
    map
}

/// Each of the four renderings is written as the report model shapes it: text and
/// JSON, hierarchical and compact; and the compact report holds exactly the
/// combined observations of the hierarchical one.
#[test]
fn each_rendering_is_written() -> TestResult {
    let fixture = Fixture::new()?;
    fs::write(fixture.path("in.txt"), "input\n")?;
    let program = ["--", "cat", "in.txt"];
    let mut reports = Vec::new();
    for (flags, name) in [
        (&[][..], "hierarchical.txt"),
        (&["--compact"][..], "compact.txt"),
        (&["--json"][..], "hierarchical.json"),
        (&["--json", "--compact"][..], "compact.json"),
    ] {
        let output = format!("--output={name}");
        let args: Vec<&str> = ["execute"]
            .into_iter()
            .chain(flags.iter().copied())
            .chain([output.as_str()])
            .chain(program)
            .collect();
        let run = run_kizz(&mut fixture.command(&args), None)?;
        assert_eq!(run.status, Some(0), "{name}: {run:?}");
        assert_eq!(run.stdout, b"input\n", "{name}");
        reports.push(fs::read_to_string(fixture.path(name))?);
    }
    let [
        hierarchical_text,
        compact_text,
        hierarchical_json,
        compact_json,
    ] = reports.as_slice()
    else {
        panic!("expected four reports");
    };
    assert!(
        hierarchical_text
            .starts_with("kizz observation report, version 1, hierarchical\ntrace: complete\n")
    );
    assert!(
        hierarchical_text.contains("\n  process 1, pid "),
        "{hierarchical_text}"
    );
    assert!(
        compact_text.starts_with("kizz observation report, version 1, compact\ntrace: complete\n")
    );
    assert!(!compact_text.contains("\n  process 1"), "{compact_text}");
    let in_file = fixture.path("in.txt");
    assert!(compact_text.contains("\nread:\n"), "{compact_text}");
    assert!(
        compact_text.contains(&format!("  {}: ok\n", in_file.display())),
        "{compact_text}"
    );
    let hierarchical: Value = serde_json::from_str(hierarchical_json)?;
    let compact: Value = serde_json::from_str(compact_json)?;
    for (report, mode) in [(&hierarchical, "hierarchical"), (&compact, "compact")] {
        assert_eq!(report["format"], "kizz-execute");
        assert_eq!(report["version"], 1);
        assert_eq!(report["mode"], mode);
        assert_eq!(report["complete"], true);
    }
    assert!(
        compact.get("processes").is_none() && compact["commands"][0].get("processes").is_none()
    );
    assert_eq!(
        outcomes(&compact["observations"], &in_file, "read"),
        BTreeSet::from(["ok".to_owned()])
    );
    let combined_hierarchical = combined(&hierarchical);
    assert!(!combined_hierarchical.is_empty());
    assert_eq!(combined_hierarchical, combined(&compact));
    Ok(())
}

/// A program that creates a file whose name holds a newline gives a text report in
/// which that path is one line, with `\n` in its name, and a JSON report whose
/// observation of it parses back to the exact name.
#[test]
fn a_file_name_with_a_newline_stays_on_one_line() -> TestResult {
    let fixture = Fixture::new()?;
    let script = "printf x > \"$(printf 'a\\nb')\"";
    for (flags, name) in [(&[][..], "report.txt"), (&["--json"][..], "report.json")] {
        let output = format!("--output={name}");
        let args: Vec<&str> = ["execute"]
            .into_iter()
            .chain(flags.iter().copied())
            .chain([output.as_str(), "--", "sh", "-c", script])
            .collect();
        let run = run_kizz(&mut fixture.command(&args), None)?;
        assert_eq!(run.status, Some(0), "{name}: {run:?}");
    }
    let created = fixture.path("a\nb");
    assert_eq!(fs::read_to_string(&created)?, "x");
    let text = fs::read_to_string(fixture.path("report.txt"))?;
    let shown = format!("{}/a\\nb: ", fixture.dir.display());
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_start)
        .filter(|line| line.starts_with(&shown))
        .collect();
    assert!(!lines.is_empty(), "{text}");
    assert!(!text.lines().any(|line| line.starts_with("b: ")), "{text}");
    let report = fixture.json("report.json")?;
    let observations = &first_program(&report)["observations"];
    let exact = created.to_str().ok_or("the path is not UTF-8")?;
    assert!(
        observations
            .as_array()
            .into_iter()
            .flatten()
            .any(|observation| observation["path"] == exact),
        "{report}"
    );
    Ok(())
}
