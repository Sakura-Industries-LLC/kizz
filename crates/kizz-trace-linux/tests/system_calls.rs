// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Linux tracer reports every path that the real processes it starts touch,
//! with the kinds of access: system-call decoding, path resolution against the
//! working directory and directory descriptors, and the access kinds of every
//! tracked system call.
//!
//! Each test works in its own temporary directory, by its canonical path, because
//! the kernel reports working directories and descriptors by their real paths.
//! Programs that the shell cannot express run this test executable again in a
//! helper mode: [`helper_process`] does nothing in a normal test run, and does the
//! work of its mode when the tracer starts the executable with the helper variable
//! set in the request's explicit environment.

use std::{
    collections::BTreeMap,
    error::Error,
    ffi::{CString, OsStr, OsString},
    fs, io, mem,
    os::unix::{
        ffi::OsStrExt as _,
        fs::{PermissionsExt as _, symlink},
    },
    path::{Path, PathBuf},
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

use kizz_trace::{
    AbsolutePath, AccessKind, AccessKinds, Cancellation, CaptureMode, CapturedOutput, FileMetadata,
    RunRequest, RunResult, StdioEcho, Termination, TraceError, Tracer,
};
use kizz_trace_linux::{LinuxTracer, TRACKED_SYSTEM_CALLS};
use libc::{c_int, c_long};

/// The variable that selects the helper mode of this test executable.
const HELPER_VARIABLE: &str = "KIZZ_TRACE_TEST_HELPER";
/// The search path of the shell commands in these tests.
const SEARCH_PATH: &str = "/usr/bin:/bin";

/// The result of a test helper.
type Outcome<T> = Result<T, Box<dyn Error>>;

/// A temporary directory, known by its canonical path.
struct Workspace {
    /// The directory, removed when the workspace is dropped.
    _dir: tempfile::TempDir,
    /// The canonical path of the directory.
    root: PathBuf,
}

impl Workspace {
    /// A new empty workspace.
    fn new() -> Outcome<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        Ok(Self { _dir: dir, root })
    }

    /// The path of `relative` in the workspace.
    fn path(
        &self,
        relative: &str,
    ) -> PathBuf {
        self.root.join(relative)
    }

    /// Create the files and directories that `entries` name, in order: a name
    /// that ends with `/` is a directory, a name with `->` is a symbolic link to
    /// the text after it, and any other name is a file.
    fn create(
        &self,
        entries: &[&str],
    ) -> Outcome<()> {
        for entry in entries {
            if let Some(directory) = entry.strip_suffix('/') {
                fs::create_dir_all(self.path(directory))?;
            } else if let Some((link, target)) = entry.split_once(" -> ") {
                symlink(target, self.path(link))?;
            } else {
                fs::write(self.path(entry), entry.as_bytes())?;
            }
        }
        Ok(())
    }

    /// The combined access kinds that `result` reports for `relative`.
    fn kinds(
        &self,
        result: &RunResult,
        relative: &str,
    ) -> Outcome<Option<AccessKinds>> {
        Ok(result
            .accesses
            .get(&AbsolutePath::new(self.path(relative))?))
    }
}

/// A request to run `program` with `args` in `cwd`, with only `PATH` set.
fn request(
    program: &Path,
    args: &[&str],
    cwd: &Path,
) -> Outcome<RunRequest> {
    Ok(RunRequest {
        program: AbsolutePath::new(program)?,
        args: args.iter().map(OsString::from).collect(),
        env: BTreeMap::from([(OsString::from("PATH"), OsString::from(SEARCH_PATH))]),
        cwd: AbsolutePath::new(cwd)?,
        stdin: None,
        capture: CaptureMode::Split,
        echo: false,
    })
}

/// A request to run a shell script in `cwd`.
fn shell(
    script: &str,
    cwd: &Path,
) -> Outcome<RunRequest> {
    request(Path::new("/bin/sh"), &["sh", "-c", script], cwd)
}

/// A request to run this test executable in a helper mode, in `cwd`.
fn helper(
    mode: &str,
    cwd: &Path,
) -> Outcome<RunRequest> {
    let executable = std::env::current_exe()?;
    let mut request = request(
        &executable,
        &[
            "system_calls",
            "--exact",
            "helper_process",
            "--nocapture",
            "--test-threads=1",
            "--quiet",
        ],
        cwd,
    )?;
    request.env = BTreeMap::from([(OsString::from(HELPER_VARIABLE), OsString::from(mode))]);
    Ok(request)
}

/// Run a request without echo or cancellation.
fn run(request: &RunRequest) -> Result<RunResult, TraceError> {
    LinuxTracer::new().run(request, &StdioEcho, &Cancellation::new())
}

/// Run a request that must exit with code 0 and trace completely.
fn run_successfully(request: &RunRequest) -> Outcome<RunResult> {
    let result = run(request)?;
    if result.termination != Termination::Exited(0) || !result.is_complete() {
        return Err(format!(
            "the run ended with {} and {:?}; its output was {:?}",
            result.termination, result.incomplete, result.output
        )
        .into());
    }
    Ok(result)
}

/// The standard output of a split capture.
fn stdout(output: &CapturedOutput) -> Outcome<String> {
    match output {
        CapturedOutput::Split { stdout, .. } => Ok(String::from_utf8(stdout.clone())?),
        CapturedOutput::Merged { .. } => Err("expected split output".into()),
    }
}

/// The set of `kinds`.
fn kinds(kinds: &[AccessKind]) -> AccessKinds {
    kinds.iter().copied().collect()
}

/// The work of a helper mode, run by this test executable when the tracer starts it
/// with [`HELPER_VARIABLE`] set. In a normal test run it does nothing.
#[test]
fn helper_process() {
    let Some(mode) = std::env::var_os(HELPER_VARIABLE) else {
        return;
    };
    match mode.to_str().unwrap() {
        "threads" => helper_threads().unwrap(),
        "own_working_directory" => helper_own_working_directory().unwrap(),
        "directory_descriptor" => helper_directory_descriptor().unwrap(),
        "every_call" => helper_every_call().unwrap(),
        "interrupted_open" => helper_interrupted_open().unwrap(),
        "restarted_open" => helper_restarted_open().unwrap(),
        "deleted_suffix" => helper_deleted_suffix().unwrap(),
        "in_root" => helper_in_root().unwrap(),
        "in_root_traversal" => helper_in_root_traversal().unwrap(),
        "in_root_parent" => helper_in_root_parent(true).unwrap(),
        "in_root_parent_control" => helper_in_root_parent(false).unwrap(),
        "unknown_format" => helper_unknown_format().unwrap(),
        other => panic!("unknown helper mode {other}"),
    }
}

/// Read `thread0.txt` through `thread3.txt`, each from its own thread.
fn helper_threads() -> Outcome<()> {
    let handles: Vec<_> = (0..4)
        .map(|index| thread::spawn(move || fs::read(format!("thread{index}.txt"))))
        .collect();
    for handle in handles {
        handle.join().map_err(|_| "a thread panicked")??;
    }
    Ok(())
}

/// Read `file.txt` from a thread that has its own working directory, `own`, and
/// then from the main thread, whose working directory is unchanged.
fn helper_own_working_directory() -> Outcome<()> {
    let reader = thread::spawn(|| -> io::Result<Vec<u8>> {
        // SAFETY: `unshare` has no memory-safety requirements; with `CLONE_FS` it
        // gives this thread its own working directory.
        if unsafe { libc::unshare(libc::CLONE_FS) } != 0 {
            return Err(io::Error::last_os_error());
        }
        std::env::set_current_dir("own")?;
        fs::read("file.txt")
    });
    reader.join().map_err(|_| "the thread panicked")??;
    fs::read("file.txt")?;
    Ok(())
}

/// Open the directory `base` and look up paths relative to its descriptor.
fn helper_directory_descriptor() -> Outcome<()> {
    let base = open("base", libc::O_RDONLY | libc::O_DIRECTORY)?;
    for name in ["inner.txt", "../outer.txt"] {
        let name = CString::new(name)?;
        // SAFETY: `name` is a NUL-terminated string.
        let fd = unsafe { libc::openat(base, name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    let missing = CString::new("missing")?;
    // SAFETY: `stat` is plain data, for which all zero bytes are a valid value.
    let mut stat: libc::stat = unsafe { mem::zeroed() };
    // SAFETY: `missing` is a NUL-terminated string and `stat` is writable.
    let ret = unsafe { libc::fstatat(base, missing.as_ptr(), &raw mut stat, 0) };
    if ret == 0 {
        return Err("base/missing exists".into());
    }
    Ok(())
}

/// Open `path` with `flags`, keeping the descriptor open for the helper's life.
fn open(
    path: &str,
    flags: c_int,
) -> Outcome<c_int> {
    let path = CString::new(path)?;
    // SAFETY: `path` is a NUL-terminated string.
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    if fd < 0 {
        return Err(format!("could not open {path:?}: {}", io::Error::last_os_error()).into());
    }
    Ok(fd)
}

/// The memory that the raw system calls of [`helper_every_call`] point to, kept
/// alive for the helper's life.
#[derive(Default)]
struct Strings {
    /// Every string passed so far.
    strings: Vec<CString>,
    /// Every buffer passed so far.
    buffers: Vec<Vec<u8>>,
}

impl Strings {
    /// The address of a new NUL-terminated copy of `text`.
    fn text(
        &mut self,
        text: &str,
    ) -> Outcome<c_long> {
        let string = CString::new(text)?;
        let address = address(string.as_ptr());
        self.strings.push(string);
        Ok(address)
    }
}

/// The address of `pointer`, as a system-call argument.
fn address<T>(pointer: *const T) -> c_long {
    c_long::try_from(pointer.expose_provenance()).unwrap_or_default()
}

/// Make the system call `number` with `arguments`, padded with zeros to six.
fn raw(
    number: c_long,
    arguments: &[c_long],
) -> io::Result<c_long> {
    let mut all = [0; 6];
    for (slot, argument) in all.iter_mut().zip(arguments) {
        *slot = *argument;
    }
    let [a0, a1, a2, a3, a4, a5] = all;
    // SAFETY: every caller passes the arguments the call expects, with pointers to
    // live memory of the right size; extra arguments are ignored by the kernel.
    let ret = unsafe { libc::syscall(number, a0, a1, a2, a3, a4, a5) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ret)
}

/// Make the system call `name` with `arguments`, and fail when it fails.
fn sys(
    name: &str,
    arguments: &[c_long],
) -> Outcome<c_long> {
    raw(number(name)?, arguments).map_err(|error| format!("{name} failed: {error}").into())
}

/// Make the system call `name` with `arguments`, which must fail with `ENOENT`.
fn sys_missing(
    name: &str,
    arguments: &[c_long],
) -> Outcome<()> {
    match raw(number(name)?, arguments) {
        Ok(_) => Err(format!("{name} found the path").into()),
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(()),
        Err(error) => Err(format!("{name} failed: {error}").into()),
    }
}

/// The `x86_64` number of the tracked system call `name`.
fn number(name: &str) -> Outcome<c_long> {
    Ok(TRACKED_SYSTEM_CALLS
        .iter()
        .find(|call| call.name == name)
        .ok_or_else(|| format!("{name} is not tracked"))?
        .number)
}

/// A flag or mode constant as a system-call argument.
fn flag(bits: c_int) -> c_long {
    c_long::from(bits)
}

/// `AT_FDCWD` as a system-call argument.
fn cwd() -> c_long {
    flag(libc::AT_FDCWD)
}

/// The address of a zeroed buffer of `size` bytes that stays alive in `strings`.
fn zeroed(
    strings: &mut Strings,
    size: usize,
) -> c_long {
    let mut buffer = vec![0_u8; size];
    let address = address(buffer.as_mut_ptr().cast_const());
    strings.buffers.push(buffer);
    address
}

/// Fork a child that makes the system calls `calls`, each a name and its
/// arguments, padded with zeros, and wait for it to exit with code 0. The child
/// makes only system calls, on data prepared before the fork; the last call must
/// be an `exec` that succeeds.
fn in_child(calls: &[(&str, &[c_long])]) -> Outcome<()> {
    let mut prepared = Vec::new();
    for (name, arguments) in calls {
        let mut all = [0; 6];
        for (slot, argument) in all.iter_mut().zip(*arguments) {
            *slot = *argument;
        }
        prepared.push((number(name)?, all));
    }
    // SAFETY: the child only makes raw system calls and exits.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        for (number, [a0, a1, a2, a3, a4, a5]) in &prepared {
            // SAFETY: the arguments point to data prepared before the fork. A call
            // that fails returns, and a successful `exec` does not.
            unsafe { libc::syscall(*number, *a0, *a1, *a2, *a3, *a4, *a5) };
        }
        // SAFETY: `_exit` has no requirements.
        unsafe { libc::_exit(1) };
    }
    if pid < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut status: c_int = 0;
    // SAFETY: `status` is a valid `int` for `waitpid` to fill.
    if unsafe { libc::waitpid(pid, &raw mut status, 0) } != pid {
        return Err(io::Error::last_os_error().into());
    }
    if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        return Err(format!("the child ended with status {status:#x}").into());
    }
    Ok(())
}

/// The `struct open_how` of `openat2`.
#[repr(C)]
struct OpenHow {
    /// The open flags.
    flags: u64,
    /// The mode of a created file.
    mode: u64,
    /// The `RESOLVE_*` flags.
    resolve: u64,
}

/// Make every tracked system call once, on the fixtures of
/// [`every_tracked_system_call_is_decoded_in_a_real_process`], with relative paths,
/// the working directory, and the directory descriptor of `sub`.
fn helper_every_call() -> Outcome<()> {
    let mut s = Strings::default();
    let sub = c_long::from(open("sub", libc::O_RDONLY | libc::O_DIRECTORY)?);
    open_files(&mut s, sub)?;
    check_files(&mut s)?;
    links_and_listings(&mut s, sub)?;
    working_directory(&mut s)?;
    changes(&mut s, sub)?;
    programs(&mut s, sub)
}

/// Open files.
fn open_files(
    s: &mut Strings,
    sub: c_long,
) -> Outcome<()> {
    let cwd = cwd();
    sys("open", &[s.text("open.r")?, flag(libc::O_RDONLY)])?;
    let create = flag(libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC);
    sys("open", &[s.text("open.w")?, create, 0o644])?;
    sys("open", &[s.text("open.p")?, flag(libc::O_PATH)])?;
    sys("openat", &[cwd, s.text("openat.rw")?, flag(libc::O_RDWR)])?;
    let how = OpenHow {
        flags: 0,
        mode: 0,
        resolve: 0,
    };
    let how_size = c_long::try_from(mem::size_of::<OpenHow>())?;
    let how_address = address(&raw const how);
    sys("openat2", &[
        sub,
        s.text("openat2.r")?,
        how_address,
        how_size,
    ])?;
    sys("creat", &[s.text("creat.w")?, 0o644])?;
    Ok(())
}

/// Check the metadata and existence of files.
fn check_files(s: &mut Strings) -> Outcome<()> {
    let cwd = cwd();
    let buffer = zeroed(s, mem::size_of::<libc::stat>());
    let extended = zeroed(s, mem::size_of::<libc::statx>());
    sys("stat", &[s.text("stat.p")?, buffer])?;
    sys("lstat", &[s.text("lstat.p")?, buffer])?;
    sys("newfstatat", &[cwd, s.text("newfstatat.p")?, buffer, 0])?;
    let empty = c_long::from(open("empty.link", libc::O_PATH)?);
    let empty_path = flag(libc::AT_EMPTY_PATH);
    sys("newfstatat", &[empty, s.text("")?, buffer, empty_path])?;
    let mask = c_long::from(libc::STATX_BASIC_STATS);
    sys("statx", &[cwd, s.text("statx.p")?, 0, mask, extended])?;
    let exists = flag(libc::F_OK);
    sys("access", &[s.text("access.p")?, exists])?;
    sys("faccessat", &[cwd, s.text("faccessat.p")?, exists])?;
    sys("faccessat2", &[cwd, s.text("faccessat2.p")?, exists, 0])?;
    sys_missing("stat", &[s.text("missing.p")?, buffer])
}

/// Read and create links, and list directories through descriptors opened by
/// symbolic links.
fn links_and_listings(
    s: &mut Strings,
    sub: c_long,
) -> Outcome<()> {
    let cwd = cwd();
    let size = 4096;
    let buffer = zeroed(s, 4096);
    sys("readlink", &[s.text("readlink.l")?, buffer, size])?;
    sys("readlinkat", &[sub, s.text("readlinkat.l")?, buffer, size])?;
    sys("link", &[s.text("link.src")?, s.text("link.dst")?])?;
    sys("linkat", &[
        cwd,
        s.text("linkat.src")?,
        sub,
        s.text("linkat.dst")?,
        0,
    ])?;
    sys("symlink", &[
        s.text("symlink.target")?,
        s.text("symlink.l")?,
    ])?;
    let symlinkat = [s.text("symlinkat.target")?, sub, s.text("symlinkat.l")?];
    sys("symlinkat", &symlinkat)?;
    let directory = libc::O_RDONLY | libc::O_DIRECTORY;
    let listed = c_long::from(open("getdents.link", directory)?);
    sys("getdents", &[listed, buffer, size])?;
    let listed64 = c_long::from(open("getdents64.link", directory)?);
    sys("getdents64", &[listed64, buffer, size])?;
    Ok(())
}

/// Change the working directory by path and by descriptor, look a path up in it,
/// and change back.
fn working_directory(s: &mut Strings) -> Outcome<()> {
    let buffer = zeroed(s, mem::size_of::<libc::stat>());
    let directory = libc::O_RDONLY | libc::O_DIRECTORY;
    let inside = s.text("inside.p")?;
    sys("chdir", &[s.text("chdir.d")?])?;
    sys_missing("stat", &[inside, buffer])?;
    sys("chdir", &[s.text("..")?])?;
    let entered = c_long::from(open("fchdir.link", directory)?);
    let back = c_long::from(open(".", directory)?);
    sys("fchdir", &[entered])?;
    sys_missing("stat", &[inside, buffer])?;
    sys("fchdir", &[back])?;
    Ok(())
}

/// Rename, remove, create and change files, by path and through descriptors
/// opened by symbolic links.
fn changes(
    s: &mut Strings,
    sub: c_long,
) -> Outcome<()> {
    let cwd = cwd();
    let none = -1;
    sys("rename", &[s.text("rename.a")?, s.text("rename.b")?])?;
    sys("renameat", &[
        sub,
        s.text("renameat.a")?,
        sub,
        s.text("renameat.b")?,
    ])?;
    let renameat2 = [cwd, s.text("renameat2.a")?, cwd, s.text("renameat2.b")?, 0];
    sys("renameat2", &renameat2)?;
    sys("unlink", &[s.text("unlink.f")?])?;
    sys("unlinkat", &[cwd, s.text("unlinkat.f")?, 0])?;
    sys("rmdir", &[s.text("rmdir.d")?])?;
    sys("mkdir", &[s.text("mkdir.d")?, 0o755])?;
    sys("mkdirat", &[sub, s.text("mkdirat.d")?, 0o755])?;
    let fifo = c_long::from(libc::S_IFIFO | 0o644);
    sys("mknod", &[s.text("mknod.f")?, fifo, 0])?;
    sys("mknodat", &[sub, s.text("mknodat.f")?, fifo, 0])?;
    sys("truncate", &[s.text("truncate.f")?, 0])?;
    sys("chmod", &[s.text("chmod.f")?, 0o644])?;
    sys("fchmodat", &[cwd, s.text("fchmodat.f")?, 0o644])?;
    sys("chown", &[s.text("chown.f")?, none, none])?;
    sys("lchown", &[s.text("lchown.f")?, none, none])?;
    sys("fchownat", &[cwd, s.text("fchownat.f")?, none, none, 0])?;
    sys("utime", &[s.text("utime.f")?, 0])?;
    sys("utimes", &[s.text("utimes.f")?, 0])?;
    sys("utimensat", &[cwd, s.text("utimensat.f")?, 0, 0])?;
    sys("futimesat", &[cwd, s.text("futimesat.f")?, 0])?;
    let truncated = c_long::from(open("ftruncate.link", libc::O_WRONLY)?);
    sys("ftruncate", &[truncated, 0])?;
    let for_mode = c_long::from(open("fchmod.link", libc::O_RDONLY)?);
    sys("fchmod", &[for_mode, 0o644])?;
    let for_owner = c_long::from(open("fchown.link", libc::O_RDONLY)?);
    sys("fchown", &[for_owner, none, none])?;
    Ok(())
}

/// Run programs, each from a child that first tries a missing program: `execve`
/// by path, and `execveat` relative to `sub` and through a descriptor.
fn programs(
    s: &mut Strings,
    sub: c_long,
) -> Outcome<()> {
    let arguments: [c_long; 2] = [s.text("true")?, 0];
    let environment: [c_long; 1] = [0];
    let args = address(arguments.as_ptr());
    let env = address(environment.as_ptr());
    in_child(&[
        ("execve", &[s.text("execve.missing")?, args, env]),
        ("execve", &[s.text("/bin/true")?, args, env]),
    ])?;
    let program = c_long::from(open("/bin/true", libc::O_PATH)?);
    let empty_path = flag(libc::AT_EMPTY_PATH);
    in_child(&[
        ("execveat", &[
            sub,
            s.text("execveat.missing")?,
            args,
            env,
            0,
        ]),
        ("execveat", &[program, s.text("")?, args, env, empty_path]),
    ])
}

/// The number of `SIGUSR1` signals that [`count_signal`] caught.
static CAUGHT: AtomicUsize = AtomicUsize::new(0);

/// The longest a helper waits for another thread.
const PATIENCE: Duration = Duration::from_secs(20);

/// Count a caught `SIGUSR1`, with only an atomic operation, which is
/// async-signal-safe.
extern "C" fn count_signal(_signal: c_int) {
    CAUGHT.fetch_add(1, Ordering::SeqCst);
}

/// Catch `SIGUSR1` with [`count_signal`] and the `sigaction` flags `flags`.
fn catch_signal(flags: c_int) -> Outcome<()> {
    // SAFETY: `sigaction` is plain data, for which all zero bytes are a valid value.
    let mut action: libc::sigaction = unsafe { mem::zeroed() };
    let handler: extern "C" fn(c_int) = count_signal;
    action.sa_sigaction = handler as libc::sighandler_t;
    action.sa_flags = flags;
    // SAFETY: `action` is a valid action whose handler is async-signal-safe, and
    // the old action is not asked for.
    if unsafe { libc::sigaction(libc::SIGUSR1, &raw const action, ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

/// The ID of the calling thread, as a system-call argument.
fn thread_id() -> c_long {
    // SAFETY: `gettid` has no requirements.
    c_long::from(unsafe { libc::gettid() })
}

/// Wait until `done` holds, checking every millisecond, for at most [`PATIENCE`].
fn wait_until(
    what: &str,
    mut done: impl FnMut() -> Result<bool, String>,
) -> Result<(), String> {
    let deadline = Instant::now().checked_add(PATIENCE).ok_or("no deadline")?;
    while !done()? {
        if Instant::now() >= deadline {
            return Err(format!("gave up waiting until {what}"));
        }
        thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

/// Wait until the thread `tid` of this process is in `openat` on the path at
/// `path`, as `/proc/self/task/<tid>/syscall` shows: its number, then its
/// arguments in hexadecimal.
fn wait_in_open(
    tid: c_long,
    path: c_long,
) -> Result<(), String> {
    let file = format!("/proc/self/task/{tid}/syscall");
    let number = libc::SYS_openat.to_string();
    let argument = format!("{path:#x}");
    wait_until("the thread is in openat", || {
        let line = fs::read_to_string(&file).map_err(|error| error.to_string())?;
        let mut fields = line.split_whitespace();
        Ok(fields.next() == Some(number.as_str()) && fields.nth(1) == Some(argument.as_str()))
    })
}

/// Wait until the thread `tid` is in `openat` on the path at `path`, and then send
/// it `SIGUSR1`, which interrupts the open.
fn interrupt(
    tid: c_long,
    path: c_long,
) -> Result<(), String> {
    wait_in_open(tid, path)?;
    let process = c_long::from(std::process::id());
    let signal = flag(libc::SIGUSR1);
    raw(libc::SYS_tgkill, &[process, tid, signal]).map_err(|error| error.to_string())?;
    Ok(())
}

/// Open the FIFO `fifo` for reading while another thread interrupts the open with
/// `SIGUSR1`, caught without `SA_RESTART`, so the open fails with `EINTR` and is not
/// restarted; the helper handles that and succeeds.
fn helper_interrupted_open() -> Outcome<()> {
    catch_signal(0)?;
    let mut s = Strings::default();
    let fifo = s.text("fifo")?;
    let tid = thread_id();
    let interrupter = thread::spawn(move || interrupt(tid, fifo));
    let opened = raw(libc::SYS_openat, &[
        cwd(),
        fifo,
        flag(libc::O_RDONLY | libc::O_CLOEXEC),
    ]);
    interrupter
        .join()
        .map_err(|_| "the interrupting thread panicked")??;
    match opened {
        Err(error) if error.raw_os_error() == Some(libc::EINTR) => Ok(()),
        other => Err(format!("the open was not interrupted: {other:?}").into()),
    }
}

/// Open the FIFO `fifo` for reading while another thread interrupts the open with
/// `SIGUSR1`, caught with `SA_RESTART`, waits until the restarted open is waiting
/// again, and then opens the FIFO for writing through the link `writer.link`, so
/// the restarted open succeeds.
fn helper_restarted_open() -> Outcome<()> {
    catch_signal(libc::SA_RESTART)?;
    let mut s = Strings::default();
    let fifo = s.text("fifo")?;
    let writer = s.text("writer.link")?;
    let tid = thread_id();
    let partner = thread::spawn(move || -> Result<c_long, String> {
        interrupt(tid, fifo)?;
        wait_until("the signal is caught", || {
            Ok(CAUGHT.load(Ordering::SeqCst) > 0)
        })?;
        // The thread caught the signal, so an open it is in now is the restart.
        wait_in_open(tid, fifo)?;
        raw(libc::SYS_openat, &[
            cwd(),
            writer,
            flag(libc::O_WRONLY | libc::O_CLOEXEC),
        ])
        .map_err(|error| error.to_string())
    });
    let opened = raw(libc::SYS_openat, &[
        cwd(),
        fifo,
        flag(libc::O_RDONLY | libc::O_CLOEXEC),
    ]);
    partner
        .join()
        .map_err(|_| "the partner thread panicked")??;
    opened?;
    Ok(())
}

/// Touch files and directories whose names end in ` (deleted)`, and objects that
/// were really removed or have no path, from the fixtures of
/// [`live_names_with_the_removed_suffix_are_recorded`].
fn helper_deleted_suffix() -> Outcome<()> {
    let mut s = Strings::default();
    let workspace = c_long::from(open(".", libc::O_RDONLY | libc::O_DIRECTORY)?);
    // A working directory with the suffix.
    sys("chdir", &[s.text("base (deleted)")?])?;
    fs::read("inner.txt")?;
    sys("fchdir", &[workspace])?;
    // A directory descriptor with the suffix, listed and used for a lookup.
    for entry in fs::read_dir("base (deleted)")? {
        entry?;
    }
    let base = c_long::from(open("base (deleted)", libc::O_RDONLY | libc::O_DIRECTORY)?);
    sys("openat", &[
        base,
        s.text("dirfd.txt")?,
        flag(libc::O_RDONLY),
    ])?;
    // A change through a descriptor of a file with the suffix.
    let file = c_long::from(open("file (deleted)", libc::O_WRONLY)?);
    sys("ftruncate", &[file, 0])?;
    // A removed file, a removed working directory, and a pipe name no path.
    let gone = c_long::from(open("gone.txt", libc::O_WRONLY)?);
    sys("unlink", &[s.text("gone.txt")?])?;
    sys("fchmod", &[gone, 0o600])?;
    sys("mkdir", &[s.text("gone.d")?, 0o755])?;
    sys("chdir", &[s.text("gone.d")?])?;
    sys("rmdir", &[s.text("../gone.d")?])?;
    sys_missing("stat", &[s.text("missing")?, zeroed(&mut s, 256)])?;
    sys("fchdir", &[workspace])?;
    let mut fds: [c_int; 2] = [-1, -1];
    // SAFETY: `fds` has room for the two descriptors `pipe` writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let [read_end, _] = fds;
    // Whether a pipe's mode can change does not matter, only that nothing is
    // recorded for it.
    let _changed = raw(number("fchmod")?, &[c_long::from(read_end), 0o600]).is_ok();
    Ok(())
}

/// The `RESOLVE_IN_ROOT` flag of `openat2`.
const RESOLVE_IN_ROOT: u64 = 0x10;

/// Open files with `openat2` and `RESOLVE_IN_ROOT` in the directory `root`, from
/// the fixtures of [`openat2_in_root_reports_the_objects_it_reaches`], and once
/// without it.
fn helper_in_root() -> Outcome<()> {
    let mut s = Strings::default();
    let root = c_long::from(open("root", libc::O_RDONLY | libc::O_DIRECTORY)?);
    let size = c_long::try_from(mem::size_of::<OpenHow>())?;
    // Flags of zero open for reading.
    let in_root = OpenHow {
        flags: 0,
        mode: 0,
        resolve: RESOLVE_IN_ROOT,
    };
    let rooted = address(&raw const in_root);
    for path in [
        "../file.txt",
        "/../dir/../file.txt",
        "abs.link/inner.txt",
        "final.link",
    ] {
        sys("openat2", &[root, s.text(path)?, rooted, size])?;
    }
    sys_missing("openat2", &[root, s.text("../missing")?, rooted, size])?;
    let plain = OpenHow {
        flags: 0,
        mode: 0,
        resolve: 0,
    };
    sys("openat2", &[
        root,
        s.text("../file.txt")?,
        address(&raw const plain),
        size,
    ])?;
    Ok(())
}

/// Make the system call `name` with `arguments`, which must fail with `errno`.
fn sys_failing(
    name: &str,
    arguments: &[c_long],
    errno: c_int,
) -> Outcome<()> {
    match raw(number(name)?, arguments) {
        Ok(_) => Err(format!("{name} succeeded").into()),
        Err(error) if error.raw_os_error() == Some(errno) => Ok(()),
        Err(error) => Err(format!("{name} failed otherwise: {error}").into()),
    }
}

/// Open paths with `openat2` and `RESOLVE_IN_ROOT` in the directory `root`, from
/// the fixtures of [`openat2_in_root_keeps_directory_and_final_component_rules`]:
/// through the regular file `plain`, which fails with `ENOTDIR`, and through the
/// link `link` followed by `.`, which `O_NOFOLLOW` does not stop.
fn helper_in_root_traversal() -> Outcome<()> {
    let mut s = Strings::default();
    let root = c_long::from(open("root", libc::O_RDONLY | libc::O_DIRECTORY)?);
    let size = c_long::try_from(mem::size_of::<OpenHow>())?;
    let read = OpenHow {
        flags: 0,
        mode: 0,
        resolve: RESOLVE_IN_ROOT,
    };
    for path in ["plain/../other", "plain/."] {
        sys_failing(
            "openat2",
            &[root, s.text(path)?, address(&raw const read), size],
            libc::ENOTDIR,
        )?;
    }
    let no_follow = OpenHow {
        flags: u64::try_from(libc::O_NOFOLLOW)?,
        mode: 0,
        resolve: RESOLVE_IN_ROOT,
    };
    sys("openat2", &[
        root,
        s.text("link/.")?,
        address(&raw const no_follow),
        size,
    ])?;
    Ok(())
}

/// Open the directories `ok`, `gone` and `flat`, from the fixtures of
/// [`openat2_in_root_records_directories_that_parent_components_leave`], and with
/// `lookup`, open `dir/../file` with `openat2` and `RESOLVE_IN_ROOT` in each: in
/// `ok`, where `dir` is a directory, it succeeds; in `gone`, where `dir` is
/// missing, it fails with `ENOENT`; and in `flat`, where `dir` is a regular file,
/// it fails with `ENOTDIR`. Without `lookup`, it is the control run that shows
/// that nothing else the helper does touches `dir`.
fn helper_in_root_parent(lookup: bool) -> Outcome<()> {
    let mut s = Strings::default();
    let size = c_long::try_from(mem::size_of::<OpenHow>())?;
    let read = OpenHow {
        flags: 0,
        mode: 0,
        resolve: RESOLVE_IN_ROOT,
    };
    let how = address(&raw const read);
    let directory = libc::O_RDONLY | libc::O_DIRECTORY;
    let ok = c_long::from(open("ok", directory)?);
    let gone = c_long::from(open("gone", directory)?);
    let flat = c_long::from(open("flat", directory)?);
    if !lookup {
        return Ok(());
    }
    sys("openat2", &[ok, s.text("dir/../file")?, how, size])?;
    sys_missing("openat2", &[gone, s.text("dir/../file")?, how, size])?;
    sys_failing(
        "openat2",
        &[flat, s.text("dir/../file")?, how, size],
        libc::ENOTDIR,
    )?;
    Ok(())
}

#[test]
fn cat_reports_a_read() {
    let workspace = Workspace::new().unwrap();
    workspace.create(&["input.txt"]).unwrap();
    let result = run_successfully(&shell("cat input.txt", &workspace.root).unwrap()).unwrap();
    assert_eq!(stdout(&result.output).unwrap(), "input.txt");
    let input = workspace.kinds(&result, "input.txt").unwrap().unwrap();
    assert!(input.contains(AccessKind::Read), "{input}");
    assert!(!input.contains(AccessKind::Write), "{input}");
}

#[test]
fn test_e_on_a_missing_path_reports_a_probe() {
    let workspace = Workspace::new().unwrap();
    let request = shell("test -e missing || echo absent", &workspace.root).unwrap();
    let result = run_successfully(&request).unwrap();
    assert_eq!(stdout(&result.output).unwrap(), "absent\n");
    assert_eq!(
        workspace.kinds(&result, "missing").unwrap(),
        Some(kinds(&[AccessKind::Probe]))
    );
}

#[test]
fn ls_reports_a_directory_listing() {
    let workspace = Workspace::new().unwrap();
    workspace
        .create(&["listed/", "listed/a", "listed/b"])
        .unwrap();
    let result = run_successfully(&shell("ls listed", &workspace.root).unwrap()).unwrap();
    assert_eq!(stdout(&result.output).unwrap(), "a\nb\n");
    let listed = workspace.kinds(&result, "listed").unwrap().unwrap();
    assert!(listed.contains(AccessKind::List), "{listed}");
}

#[test]
fn a_shell_script_that_creates_renames_and_removes_files_reports_each_write() {
    let workspace = Workspace::new().unwrap();
    let script = "echo one > created
        mv created renamed
        rm renamed
        mkdir made
        rmdir made
        touch touched
        ln -s touched linked
        chmod 600 touched";
    let result = run_successfully(&shell(script, &workspace.root).unwrap()).unwrap();
    for path in ["created", "renamed", "made", "touched", "linked"] {
        let written = workspace.kinds(&result, path).unwrap();
        assert!(
            written.is_some_and(|kinds| kinds.contains(AccessKind::Write)),
            "{path}: {written:?}"
        );
    }
}

#[test]
fn a_program_that_is_run_is_reported_with_the_exec_kind() {
    let workspace = Workspace::new().unwrap();
    let result =
        run_successfully(&shell("/bin/true; exec /bin/true", &workspace.root).unwrap()).unwrap();
    for program in ["/bin/sh", "/bin/true"] {
        let kinds = result.accesses.get(&AbsolutePath::new(program).unwrap());
        assert!(
            kinds.is_some_and(|kinds| kinds.contains(AccessKind::Exec)),
            "{program}: {kinds:?}"
        );
    }
}

#[test]
fn a_missing_program_is_reported_with_its_probe() {
    let workspace = Workspace::new().unwrap();
    let program = workspace.path("missing-program");
    let error =
        run(&request(&program, &["missing-program"], &workspace.root).unwrap()).unwrap_err();
    let TraceError::ProgramNotFound { accesses, .. } = error else {
        panic!("expected a missing program, got {error:?}");
    };
    assert_eq!(
        accesses.get(&AbsolutePath::new(&program).unwrap()),
        Some(kinds(&[AccessKind::Probe]))
    );
}

/// Write the executable file `relative` in `workspace` with `content`.
fn write_executable(
    workspace: &Workspace,
    relative: &str,
    content: &str,
) -> Outcome<PathBuf> {
    let path = workspace.path(relative);
    fs::write(&path, content)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

/// Run a request that runs a file the test just wrote, again while starting it
/// fails with `ETXTBSY`: a child that another test forks may hold a copy of the
/// descriptor that wrote the file until that child runs its own program.
fn run_written(request: &RunRequest) -> Result<RunResult, TraceError> {
    let mut tries = 0_u32;
    loop {
        match run(request) {
            Err(TraceError::StartFailed { ref source, .. })
                if source.raw_os_error() == Some(libc::ETXTBSY) && tries < 500 =>
            {
                tries = tries.saturating_add(1);
                thread::sleep(Duration::from_millis(10));
            },
            other => return other,
        }
    }
}

/// The little-endian field of `size` bytes, at most eight, at `offset` in `bytes`.
fn field(
    bytes: &[u8],
    offset: usize,
    size: usize,
) -> Outcome<usize> {
    let end = offset.checked_add(size).ok_or("the field overflows")?;
    let slice = bytes.get(offset..end).ok_or("the file is too short")?;
    let mut word = [0_u8; 8];
    word.get_mut(..size)
        .ok_or("the field is too wide")?
        .copy_from_slice(slice);
    Ok(usize::try_from(u64::from_le_bytes(word))?)
}

/// The loader that the 64-bit ELF executable at `path` names in its `PT_INTERP`
/// program header, read by the test itself.
fn elf_loader(path: &Path) -> Outcome<PathBuf> {
    let bytes = fs::read(path)?;
    let start = field(&bytes, 32, 8)?;
    let size = field(&bytes, 54, 2)?;
    let count = field(&bytes, 56, 2)?;
    for index in 0..count {
        let header = index
            .checked_mul(size)
            .and_then(|offset| offset.checked_add(start))
            .ok_or("the program headers overflow")?;
        if field(&bytes, header, 4)? != 3 {
            continue;
        }
        let offset = field(&bytes, header.checked_add(8).ok_or("overflow")?, 8)?;
        let length = field(&bytes, header.checked_add(32).ok_or("overflow")?, 8)?;
        let end = offset
            .checked_add(length)
            .ok_or("the loader path overflows")?;
        let name = bytes.get(offset..end).ok_or("the loader path is cut")?;
        let name = name.split(|byte| *byte == 0).next().unwrap_or_default();
        return Ok(PathBuf::from(OsStr::from_bytes(name)));
    }
    Err(format!("{} has no PT_INTERP program header", path.display()).into())
}

/// Assert that `result` records `path` with the exec kind.
fn assert_executed(
    result: &RunResult,
    path: &Path,
) {
    let kinds = AbsolutePath::new(path)
        .ok()
        .and_then(|path| result.accesses.get(&path));
    assert!(
        kinds.is_some_and(|kinds| kinds.contains(AccessKind::Exec)),
        "{}: {kinds:?}",
        path.display()
    );
}

/// A real chain of two scripts records both, `/bin/sh` and its loader with the
/// exec kind, from the `exec` alone.
#[test]
fn a_chain_of_scripts_records_each_interpreter_and_the_loader_from_the_exec_alone() {
    let workspace = Workspace::new().unwrap();
    // The first script names the second by a path relative to the working
    // directory, and the second names `/bin/sh`, which runs it and exits.
    let outer = write_executable(&workspace, "outer", "#!inner first\n").unwrap();
    let inner = write_executable(&workspace, "inner", "#! /bin/sh\nexit 0\n").unwrap();
    let result = run_written(&request(&outer, &["outer"], &workspace.root).unwrap()).unwrap();
    assert_eq!(result.termination, Termination::Exited(0));
    assert!(result.is_complete(), "{:?}", result.incomplete);
    let shell = Path::new("/bin/sh");
    let loader = elf_loader(shell).unwrap();
    for path in [&outer, &inner, shell, &loader] {
        assert_executed(&result, path);
    }
    // The first script is only run, never read by a process: its interpreter
    // reaches the tracer from the `exec` alone.
    assert_eq!(
        workspace.kinds(&result, "outer").unwrap(),
        Some(kinds(&[AccessKind::Exec]))
    );
}

/// A real dynamically linked program records its loader with the exec kind.
#[test]
fn a_dynamically_linked_program_records_its_loader() {
    let workspace = Workspace::new().unwrap();
    let program = Path::new("/bin/true");
    let loader = elf_loader(program).unwrap();
    let result = run_successfully(&request(program, &["true"], &workspace.root).unwrap()).unwrap();
    assert_executed(&result, program);
    assert_executed(&result, &loader);
}

/// A real script whose interpreter is missing records both the script and the
/// interpreter as probed.
#[test]
fn a_script_whose_interpreter_is_missing_records_the_interpreter_as_probed() {
    let workspace = Workspace::new().unwrap();
    let script = write_executable(&workspace, "broken", "#!missing-interpreter\n").unwrap();
    let error = run_written(&request(&script, &["broken"], &workspace.root).unwrap()).unwrap_err();
    let TraceError::ProgramNotFound { accesses, .. } = error else {
        panic!("expected a missing interpreter, got {error:?}");
    };
    let probe = Some(kinds(&[AccessKind::Probe]));
    for path in ["broken", "missing-interpreter"] {
        assert_eq!(
            accesses.get(&AbsolutePath::new(workspace.path(path)).unwrap()),
            probe,
            "{path}"
        );
    }
}

/// Run the file `data` in the working directory, again while it is busy, and
/// check that it fails with `ENOEXEC`.
fn helper_unknown_format() -> Outcome<()> {
    let path = CString::new("data")?;
    let argv = [path.as_ptr(), ptr::null()];
    let envp = [ptr::null()];
    loop {
        // SAFETY: `path` is a NUL-terminated string, and `argv` and `envp` are
        // null-terminated arrays of such strings.
        unsafe { libc::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENOEXEC) => return Ok(()),
            Some(libc::ETXTBSY) => thread::sleep(Duration::from_millis(10)),
            _ => return Err(format!("running data failed with {error}").into()),
        }
    }
}

/// A real file of no known format fails with `ENOEXEC`, is recorded as probed,
/// and leaves the run complete.
#[test]
fn a_file_of_no_known_format_fails_with_enoexec_and_records_a_probe() {
    let workspace = Workspace::new().unwrap();
    write_executable(&workspace, "data", "plain data, not a program\n").unwrap();
    // The run is complete: a program that fails to run is not followed.
    let result = run_successfully(&helper("unknown_format", &workspace.root).unwrap()).unwrap();
    assert_eq!(
        workspace.kinds(&result, "data").unwrap(),
        Some(kinds(&[AccessKind::Probe]))
    );
}

#[test]
fn commands_run_by_child_processes_are_traced() {
    let workspace = Workspace::new().unwrap();
    workspace
        .create(&["one.txt", "two.txt", "three.txt", "four.txt"])
        .unwrap();
    let script = "sh -c 'cat one.txt' > /dev/null
        (cat two.txt) > /dev/null
        cat three.txt | cat > /dev/null
        cat four.txt > /dev/null &
        wait";
    let result = run_successfully(&shell(script, &workspace.root).unwrap()).unwrap();
    for path in ["one.txt", "two.txt", "three.txt", "four.txt"] {
        let read = workspace.kinds(&result, path).unwrap();
        assert!(
            read.is_some_and(|kinds| kinds.contains(AccessKind::Read)),
            "{path}: {read:?}"
        );
    }
}

#[test]
fn commands_run_by_threads_are_traced() {
    let workspace = Workspace::new().unwrap();
    let files = ["thread0.txt", "thread1.txt", "thread2.txt", "thread3.txt"];
    workspace.create(&files).unwrap();
    let result = run_successfully(&helper("threads", &workspace.root).unwrap()).unwrap();
    for path in files {
        // Reading a whole file also checks its size through the descriptor.
        let read = workspace.kinds(&result, path).unwrap();
        assert!(
            read.is_some_and(|kinds| kinds.contains(AccessKind::Read)),
            "{path}: {read:?}"
        );
    }
}

#[test]
fn relative_paths_resolve_after_cd() {
    let workspace = Workspace::new().unwrap();
    workspace
        .create(&["top.txt", "sub/", "sub/inner/", "sub/inner/deep.txt"])
        .unwrap();
    let script =
        "cd sub && cat ../top.txt ./inner//deep.txt > /dev/null && cd inner && test -e nothing
        cd / && test -e relative-to-root
        exit 0";
    let result = run_successfully(&shell(script, &workspace.root).unwrap()).unwrap();
    let read = Some(kinds(&[AccessKind::Read]));
    // `..` components are kept, and `.` components and repeated separators go.
    assert_eq!(workspace.kinds(&result, "sub/../top.txt").unwrap(), read);
    assert_eq!(
        workspace.kinds(&result, "sub/inner/deep.txt").unwrap(),
        read
    );
    assert_eq!(
        workspace.kinds(&result, "sub/inner/nothing").unwrap(),
        Some(kinds(&[AccessKind::Probe]))
    );
    assert_eq!(
        result
            .accesses
            .get(&AbsolutePath::new("/relative-to-root").unwrap()),
        Some(kinds(&[AccessKind::Probe]))
    );
    assert_eq!(workspace.kinds(&result, "top.txt").unwrap(), None);
}

#[test]
fn each_thread_resolves_against_its_own_working_directory() {
    let workspace = Workspace::new().unwrap();
    workspace
        .create(&["file.txt", "own/", "own/file.txt"])
        .unwrap();
    let request = helper("own_working_directory", &workspace.root).unwrap();
    let result = run_successfully(&request).unwrap();
    for path in ["own/file.txt", "file.txt"] {
        let read = workspace.kinds(&result, path).unwrap();
        assert!(
            read.is_some_and(|kinds| kinds.contains(AccessKind::Read)),
            "{path}: {read:?}"
        );
    }
    assert_eq!(workspace.kinds(&result, "own/own/file.txt").unwrap(), None);
}

#[test]
fn relative_paths_resolve_through_directory_descriptors() {
    let workspace = Workspace::new().unwrap();
    workspace
        .create(&["outer.txt", "base/", "base/inner.txt"])
        .unwrap();
    let request = helper("directory_descriptor", &workspace.root).unwrap();
    let result = run_successfully(&request).unwrap();
    let read = Some(kinds(&[AccessKind::Read]));
    assert_eq!(workspace.kinds(&result, "base/inner.txt").unwrap(), read);
    assert_eq!(workspace.kinds(&result, "base/../outer.txt").unwrap(), read);
    assert_eq!(
        workspace.kinds(&result, "base/missing").unwrap(),
        Some(kinds(&[AccessKind::Probe]))
    );
    assert_eq!(workspace.kinds(&result, "inner.txt").unwrap(), None);
}

/// The fixtures of [`every_tracked_system_call_is_decoded_in_a_real_process`], in
/// the form [`Workspace::create`] takes.
const EVERY_CALL_FIXTURES: &[&str] = &[
    "sub/",
    "open.r",
    "open.p",
    "openat.rw",
    "sub/openat2.r",
    "stat.p",
    "lstat.p",
    "newfstatat.p",
    "empty.f",
    "empty.link -> empty.f",
    "statx.p",
    "access.p",
    "faccessat.p",
    "faccessat2.p",
    "readlink.l -> nowhere",
    "sub/readlinkat.l -> nowhere",
    "link.src",
    "linkat.src",
    "getdents.d/",
    "getdents.link -> getdents.d",
    "getdents64.d/",
    "getdents64.link -> getdents64.d",
    "chdir.d/",
    "fchdir.d/",
    "fchdir.link -> fchdir.d",
    "rename.a",
    "sub/renameat.a",
    "renameat2.a",
    "unlink.f",
    "unlinkat.f",
    "rmdir.d/",
    "truncate.f",
    "chmod.f",
    "fchmodat.f",
    "chown.f",
    "lchown.f",
    "fchownat.f",
    "utime.f",
    "utimes.f",
    "utimensat.f",
    "futimesat.f",
    "ftruncate.f",
    "ftruncate.link -> ftruncate.f",
    "fchmod.f",
    "fchmod.link -> fchmod.f",
    "fchown.f",
    "fchown.link -> fchown.f",
];

/// Only a read.
const READ: &[AccessKind] = &[AccessKind::Read];
/// Only a probe.
const PROBE: &[AccessKind] = &[AccessKind::Probe];
/// Only a listing.
const LIST: &[AccessKind] = &[AccessKind::List];
/// Only a write.
const WRITE: &[AccessKind] = &[AccessKind::Write];

/// The exact access kinds that [`helper_every_call`] gives each workspace path.
const EVERY_CALL_ACCESSES: &[(&str, &[AccessKind])] = &[
    ("open.r", READ),
    ("open.w", WRITE),
    ("open.p", PROBE),
    ("openat.rw", &[AccessKind::Read, AccessKind::Write]),
    ("sub/openat2.r", READ),
    ("creat.w", WRITE),
    ("stat.p", PROBE),
    ("lstat.p", PROBE),
    ("newfstatat.p", PROBE),
    ("empty.link", PROBE),
    ("empty.f", PROBE),
    ("statx.p", PROBE),
    ("access.p", PROBE),
    ("faccessat.p", PROBE),
    ("faccessat2.p", PROBE),
    ("missing.p", PROBE),
    ("readlink.l", READ),
    ("sub/readlinkat.l", READ),
    ("link.src", WRITE),
    ("link.dst", WRITE),
    ("linkat.src", WRITE),
    ("sub/linkat.dst", WRITE),
    ("symlink.l", WRITE),
    ("sub/symlinkat.l", WRITE),
    ("getdents.link", PROBE),
    ("getdents.d", LIST),
    ("getdents64.link", PROBE),
    ("getdents64.d", LIST),
    ("chdir.d", PROBE),
    ("chdir.d/inside.p", PROBE),
    ("chdir.d/..", PROBE),
    ("fchdir.link", PROBE),
    ("fchdir.d", PROBE),
    ("fchdir.d/inside.p", PROBE),
    ("rename.a", WRITE),
    ("rename.b", WRITE),
    ("sub/renameat.a", WRITE),
    ("sub/renameat.b", WRITE),
    ("renameat2.a", WRITE),
    ("renameat2.b", WRITE),
    ("unlink.f", WRITE),
    ("unlinkat.f", WRITE),
    ("rmdir.d", WRITE),
    ("mkdir.d", WRITE),
    ("sub/mkdirat.d", WRITE),
    ("mknod.f", WRITE),
    ("sub/mknodat.f", WRITE),
    ("truncate.f", WRITE),
    ("chmod.f", WRITE),
    ("fchmodat.f", WRITE),
    ("chown.f", WRITE),
    ("lchown.f", WRITE),
    ("fchownat.f", WRITE),
    ("utime.f", WRITE),
    ("utimes.f", WRITE),
    ("utimensat.f", WRITE),
    ("futimesat.f", WRITE),
    ("ftruncate.link", WRITE),
    ("ftruncate.f", WRITE),
    ("fchmod.link", READ),
    ("fchmod.f", WRITE),
    ("fchown.link", READ),
    ("fchown.f", WRITE),
    ("execve.missing", PROBE),
    ("sub/execveat.missing", PROBE),
];

#[test]
fn every_tracked_system_call_is_decoded_in_a_real_process() {
    let workspace = Workspace::new().unwrap();
    workspace.create(EVERY_CALL_FIXTURES).unwrap();
    let result = run_successfully(&helper("every_call", &workspace.root).unwrap()).unwrap();
    let mut wrong = Vec::new();
    for (path, expected) in EVERY_CALL_ACCESSES {
        let actual = workspace.kinds(&result, path).unwrap();
        if actual != Some(kinds(expected)) {
            wrong.push(format!(
                "{path}: expected {}, got {actual:?}",
                kinds(expected)
            ));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    // The target of a symbolic link is only text.
    for target in ["symlink.target", "symlinkat.target", "sub/symlinkat.target"] {
        assert_eq!(workspace.kinds(&result, target).unwrap(), None, "{target}");
    }
    // `execve` runs `/bin/true` by the path it was given, and `execveat` by the
    // path behind its descriptor.
    let real_true = fs::canonicalize("/bin/true").unwrap();
    for program in [Path::new("/bin/true"), &real_true] {
        let kinds = result.accesses.get(&AbsolutePath::new(program).unwrap());
        assert!(
            kinds.is_some_and(|kinds| kinds.contains(AccessKind::Exec)),
            "{}: {kinds:?}",
            program.display()
        );
    }
}

/// Make a FIFO at `path`.
fn make_fifo(path: &Path) -> Outcome<()> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `path` is a NUL-terminated string.
    if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[test]
fn an_open_that_a_signal_interrupts_without_restarting_records_its_probe() {
    let workspace = Workspace::new().unwrap();
    make_fifo(&workspace.path("fifo")).unwrap();
    let request = helper("interrupted_open", &workspace.root).unwrap();
    let result = run_successfully(&request).unwrap();
    assert_eq!(
        workspace.kinds(&result, "fifo").unwrap(),
        Some(kinds(PROBE))
    );
}

#[test]
fn an_open_that_restarts_after_a_signal_records_its_final_kinds() {
    let workspace = Workspace::new().unwrap();
    make_fifo(&workspace.path("fifo")).unwrap();
    workspace.create(&["writer.link -> fifo"]).unwrap();
    let request = helper("restarted_open", &workspace.root).unwrap();
    let result = run_successfully(&request).unwrap();
    // The interrupted attempt probed the FIFO, and the restarted one read it.
    assert_eq!(
        workspace.kinds(&result, "fifo").unwrap(),
        Some(kinds(&[AccessKind::Read, AccessKind::Probe]))
    );
    assert_eq!(
        workspace.kinds(&result, "writer.link").unwrap(),
        Some(kinds(WRITE))
    );
}

/// The fixtures of [`live_names_with_the_removed_suffix_are_recorded`].
const DELETED_SUFFIX_FIXTURES: &[&str] = &[
    "base (deleted)/",
    "base (deleted)/inner.txt",
    "base (deleted)/dirfd.txt",
    "file (deleted)",
    "gone.txt",
];

#[test]
fn live_names_with_the_removed_suffix_are_recorded() {
    let workspace = Workspace::new().unwrap();
    workspace.create(DELETED_SUFFIX_FIXTURES).unwrap();
    let request = helper("deleted_suffix", &workspace.root).unwrap();
    let result = run_successfully(&request).unwrap();
    let has = |path: &str, kind: AccessKind| {
        let recorded = workspace.kinds(&result, path).unwrap();
        assert!(
            recorded.is_some_and(|kinds| kinds.contains(kind)),
            "{path}: {recorded:?}"
        );
    };
    // Through the working directory, a directory descriptor, and a descriptor of
    // a file.
    has("base (deleted)/inner.txt", AccessKind::Read);
    has("base (deleted)", AccessKind::List);
    has("file (deleted)", AccessKind::Write);
    has("gone.txt", AccessKind::Write);
    assert_eq!(
        workspace
            .kinds(&result, "base (deleted)/dirfd.txt")
            .unwrap(),
        Some(kinds(READ))
    );
    // The removed file and working directory name no path.
    let live = [
        workspace.path("base (deleted)"),
        workspace.path("file (deleted)"),
    ];
    let stray: Vec<&Path> = result
        .accesses
        .iter()
        .map(|(path, _)| path.as_path())
        .filter(|path| path.as_os_str().as_bytes().ends_with(b" (deleted)"))
        .filter(|path| !live.iter().any(|live| path.starts_with(live)))
        .collect();
    assert!(stray.is_empty(), "{stray:?}");
    for path in [
        "gone.txt (deleted)",
        "gone.d (deleted)",
        "gone.d (deleted)/missing",
    ] {
        assert_eq!(workspace.kinds(&result, path).unwrap(), None, "{path}");
    }
}

/// The fixtures of [`openat2_in_root_reports_the_objects_it_reaches`]: the root
/// `root`, a file of the same name outside it, and links to absolute targets,
/// which name nothing outside the root.
const IN_ROOT_FIXTURES: &[&str] = &[
    "file.txt",
    "root/",
    "root/file.txt",
    "root/dir/",
    "root/dir/inner.txt",
    "root/abs.link -> /dir",
    "root/final.link -> /file.txt",
];

#[test]
fn openat2_in_root_reports_the_objects_it_reaches() {
    let workspace = Workspace::new().unwrap();
    workspace.create(IN_ROOT_FIXTURES).unwrap();
    let result = run_successfully(&helper("in_root", &workspace.root).unwrap()).unwrap();
    let read = Some(kinds(READ));
    let probe = Some(kinds(PROBE));
    // `..` at the root stays there, and absolute targets start at the root.
    assert_eq!(workspace.kinds(&result, "root/file.txt").unwrap(), read);
    assert_eq!(
        workspace.kinds(&result, "root/dir/inner.txt").unwrap(),
        read
    );
    assert_eq!(workspace.kinds(&result, "root/abs.link").unwrap(), probe);
    assert_eq!(workspace.kinds(&result, "root/final.link").unwrap(), probe);
    assert_eq!(workspace.kinds(&result, "root/missing").unwrap(), probe);
    // Without `RESOLVE_IN_ROOT`, `..` leaves the directory and is kept.
    assert_eq!(workspace.kinds(&result, "root/../file.txt").unwrap(), read);
    for path in ["file.txt", "missing", "root/abs.link/inner.txt"] {
        assert_eq!(workspace.kinds(&result, path).unwrap(), None, "{path}");
    }
    for host in ["/dir", "/dir/inner.txt", "/file.txt"] {
        let path = AbsolutePath::new(host).unwrap();
        assert_eq!(result.accesses.get(&path), None, "{host}");
    }
}

/// The fixtures of [`openat2_in_root_keeps_directory_and_final_component_rules`]:
/// the root `root` with the regular file `plain`, the file `other` beside it, and
/// the link `link` to the absolute target `/dir`, which in the root names
/// `root/dir`, not the `dir` outside it.
const IN_ROOT_TRAVERSAL_FIXTURES: &[&str] = &[
    "dir/",
    "root/",
    "root/plain",
    "root/other",
    "root/dir/",
    "root/link -> /dir",
];

#[test]
fn openat2_in_root_keeps_directory_and_final_component_rules() {
    let workspace = Workspace::new().unwrap();
    workspace.create(IN_ROOT_TRAVERSAL_FIXTURES).unwrap();
    // The run must be complete: the descriptor `openat2` returned names the
    // object the decoder found.
    let request = helper("in_root_traversal", &workspace.root).unwrap();
    let result = run_successfully(&request).unwrap();
    let probe = Some(kinds(PROBE));
    // `plain` is not a directory, so the lookup stops there, before `..` or `.`.
    assert_eq!(workspace.kinds(&result, "root/plain").unwrap(), probe);
    // `link` is an intermediate component of `link/.`, so it is followed despite
    // `O_NOFOLLOW`, and the directory it names in the root is opened.
    assert_eq!(workspace.kinds(&result, "root/link").unwrap(), probe);
    assert_eq!(
        workspace.kinds(&result, "root/dir").unwrap(),
        Some(kinds(READ))
    );
    for path in ["root/other", "other", "dir", "root/plain/../other"] {
        assert_eq!(workspace.kinds(&result, path).unwrap(), None, "{path}");
    }
}

/// The fixtures of the test
/// [`openat2_in_root_records_directories_that_parent_components_leave`]: three
/// roots, each with the file `file`, where `dir` is a directory in `ok`, missing
/// in `gone`, and a regular file in `flat`.
const IN_ROOT_PARENT_FIXTURES: &[&str] = &[
    "ok/",
    "ok/dir/",
    "ok/file",
    "gone/",
    "gone/file",
    "flat/",
    "flat/dir",
    "flat/file",
];

#[test]
fn openat2_in_root_records_directories_that_parent_components_leave() {
    let workspace = Workspace::new().unwrap();
    workspace.create(IN_ROOT_PARENT_FIXTURES).unwrap();
    // Every path recorded beneath the root `root`, relative to it.
    let beneath = |result: &RunResult, root: &str| {
        let base = workspace.path(root);
        result
            .accesses
            .iter()
            .filter_map(|(path, recorded)| {
                path.as_path()
                    .strip_prefix(&base)
                    .ok()
                    .filter(|relative| !relative.as_os_str().is_empty())
                    .map(|relative| (relative.to_path_buf(), *recorded))
            })
            .collect::<Vec<_>>()
    };
    let entries = |pairs: &[(&str, &[AccessKind])]| {
        pairs
            .iter()
            .map(|(path, recorded)| (PathBuf::from(*path), kinds(recorded)))
            .collect::<Vec<_>>()
    };
    // Neither the fixtures nor the rest of the helper supply a record beneath
    // the roots.
    let control = helper("in_root_parent_control", &workspace.root).unwrap();
    let control = run_successfully(&control).unwrap();
    for root in ["ok", "gone", "flat"] {
        assert_eq!(beneath(&control, root), entries(&[]), "{root}");
    }
    // The run must be complete: the descriptor that the successful `openat2`
    // returned names `ok/file`.
    let result = run_successfully(&helper("in_root_parent", &workspace.root).unwrap()).unwrap();
    // `dir` is a directory that `..` leaves, so it is a probe beside the read of
    // the file, and removing it or replacing it with a file changes a record.
    assert_eq!(
        beneath(&result, "ok"),
        entries(&[("dir", PROBE), ("file", READ)])
    );
    // Missing, or a regular file, `dir` is where the lookup stops.
    assert_eq!(beneath(&result, "gone"), entries(&[("dir", PROBE)]));
    assert_eq!(beneath(&result, "flat"), entries(&[("dir", PROBE)]));
    for path in ["dir", "file"] {
        assert_eq!(workspace.kinds(&result, path).unwrap(), None, "{path}");
    }
}

/// The first-read metadata that `result` reports for `relative` in `workspace`.
fn first_read(
    workspace: &Workspace,
    result: &RunResult,
    relative: &str,
) -> Outcome<Option<FileMetadata>> {
    Ok(result
        .first_reads
        .get(&AbsolutePath::new(workspace.path(relative))?)
        .copied())
}

/// The review's command reads `input` and then rewrites it: the first-read
/// metadata of `input` is what the test read before the run, not its final state.
#[test]
fn a_file_read_and_then_rewritten_reports_its_metadata_before_the_rewrite() {
    let workspace = Workspace::new().unwrap();
    fs::write(workspace.path("input"), "the old input\n").unwrap();
    let before = FileMetadata::at(&workspace.path("input")).unwrap();
    let script = "cat input > output; printf \"new\\n\" > input";
    let result = run_successfully(&shell(script, &workspace.root).unwrap()).unwrap();
    assert_eq!(
        fs::read_to_string(workspace.path("output")).unwrap(),
        "the old input\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.path("input")).unwrap(),
        "new\n"
    );
    assert_eq!(
        first_read(&workspace, &result, "input").unwrap(),
        Some(before)
    );
    assert_ne!(FileMetadata::at(&workspace.path("input")), Some(before));
    // `output` was created and truncated, and never read.
    assert_eq!(first_read(&workspace, &result, "output").unwrap(), None);
}

/// A file that the command writes and then reads reports no first read.
#[test]
fn a_file_written_and_then_read_reports_no_first_read() {
    let workspace = Workspace::new().unwrap();
    let script = "printf 'one ' > made; cat made; printf two >> made; cat made";
    let result = run_successfully(&shell(script, &workspace.root).unwrap()).unwrap();
    assert_eq!(stdout(&result.output).unwrap(), "one one two");
    let made = workspace.kinds(&result, "made").unwrap().unwrap();
    assert!(made.contains(AccessKind::Read), "{made}");
    assert_eq!(first_read(&workspace, &result, "made").unwrap(), None);
}

/// A script that runs reports its own first-read metadata, and so does its
/// interpreter, which the kernel loaded.
#[test]
fn a_script_that_runs_reports_its_own_first_read() {
    let workspace = Workspace::new().unwrap();
    let script = write_executable(&workspace, "script", "#!/bin/sh\nexit 0\n").unwrap();
    let before = FileMetadata::at(&script).unwrap();
    let result = run_written(&request(&script, &["script"], &workspace.root).unwrap()).unwrap();
    assert_eq!(result.termination, Termination::Exited(0));
    assert_eq!(
        first_read(&workspace, &result, "script").unwrap(),
        Some(before)
    );
    let interpreter = AbsolutePath::new("/bin/sh").unwrap();
    assert_eq!(
        result.first_reads.get(&interpreter).copied(),
        FileMetadata::at(Path::new("/bin/sh"))
    );
}
