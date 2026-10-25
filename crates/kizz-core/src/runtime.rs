// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The Starlark runtime of task programs.
//!
//! A task program is a task block plus every module it loads, directly or
//! indirectly. [`TaskProgram::load`] reads and evaluates the modules, records each
//! module read as an access of the task, and computes the [`ProgramHash`] that
//! covers the block and every module. [`TaskProgram::run`] then evaluates the block,
//! handing each `run()` call to a [`StepExecutor`].
//!
//! Programs use the extended Starlark dialect, so top-level `if` and `for` work.
//! The globals are the standard ones with `json.encode` and `json.decode`, plus
//! `run`, `env`, `fs`, `print`, and `fail`; `load` is a statement, and `task`
//! describes the running task. Functions from modules act on the task that calls
//! them, and calling `run()` while a module is being loaded, outside any task, is
//! an error.
//!
//! The task environment starts empty, and only the `env` functions change it;
//! `env.caller` and `env.inherit` read the caller environment that
//! [`TaskProgram::run`] is given, never the process environment. The task
//! environment and the caller environment exist only while the task runs, so while
//! a module is being loaded, the `env` functions other than `env.read` are errors,
//! like `run()`. The `fs` functions and `env.read` work there too, and every file
//! access they make, like each module read, is recorded against the task, as is
//! every file that `fs.write` or `fs.write_json` changes.

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    fmt, fs,
    io::ErrorKind,
    path::PathBuf,
};

use kizz_store::MachinePart;
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, CaptureMode, CapturedOutput, NotAbsoluteError, Termination,
};
use starlark::{
    PrintHandler,
    any::ProvidesStaticType,
    environment::{FrozenModule, Globals, GlobalsBuilder, LibraryExtension, Module},
    eval::{Evaluator, FileLoader},
    starlark_module,
    syntax::{AstModule, Dialect},
    values::{
        Heap, Value,
        dict::{AllocDict, UnpackDictEntries},
        list_or_tuple::UnpackListOrTuple,
        none::{NoneOr, NoneType},
        structs::AllocStruct,
        tuple::UnpackTuple,
    },
};

use crate::{
    dotenv::{self, DotenvError},
    files::{FileChanges, FileError, Files},
    step::{StepError, StepExecutor, StepOutcome, StepSpec},
    tasks::Task,
};

/// The context string from which the program hash key is derived. A change to how
/// the hash is computed changes the version at its end.
const PROGRAM_HASH_CONTEXT: &str = "kizz task program hash, version 1";

/// The running task, as task programs see it through `task` and as messages name
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskInfo {
    /// The absolute directory of the task's `kizz.md`, the task directory.
    pub dir: AbsolutePath,
    /// The absolute path of the task's `kizz.md`.
    pub file: AbsolutePath,
    /// The path of the task's `kizz.md`, relative to the directory discovery started
    /// in, as messages show it.
    pub relative_file: PathBuf,
    /// The line of the block's opening fence, counting from 1.
    pub line: usize,
    /// The index of the task block in its `kizz.md`, counting from zero.
    pub index: u32,
    /// The nearest heading above the block, which names the task, if there is one.
    pub name: Option<String>,
    /// The targets on the block, in the order written.
    pub targets: Vec<String>,
    /// The requested targets that selected the task.
    pub selected: Vec<String>,
    /// The block's Starlark source.
    pub source: String,
}

impl TaskInfo {
    /// The information of `task`, selected by the requested targets `selected`.
    ///
    /// # Errors
    ///
    /// Returns [`NotAbsoluteError`] when the task's `kizz.md` path is not absolute.
    pub fn new(
        task: &Task,
        selected: &[String],
    ) -> Result<Self, NotAbsoluteError> {
        let file = AbsolutePath::new(&task.id.file)?;
        let dir = parent_dir(&file);
        Ok(Self {
            dir,
            file,
            relative_file: task.relative_file.clone(),
            line: task.line,
            index: task.id.index,
            name: task.heading.clone(),
            targets: task.targets.clone(),
            selected: selected.to_vec(),
            source: task.source.clone(),
        })
    }

    /// The name that prefixes the task's `print` output: its heading, or
    /// `path/kizz.md:LINE` when the block has no heading above it.
    #[must_use]
    pub fn display_name(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("{}:{}", self.relative_file.display(), self.line))
    }
}

/// Where the lines that task programs `print` go.
pub trait PrintSink {
    /// Show one line of `print` output, already prefixed with the task name.
    fn print_line(
        &self,
        line: &str,
    );
}

/// The BLAKE3 hash of a task program: the block and every module the task loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProgramHash([u8; 32]);

impl ProgramHash {
    /// The hash with these bytes, as a stored task record holds it.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The hash's bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for ProgramHash {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// A step whose exit status was not one its `expect` list accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnexpectedExit {
    /// The step's argument vector.
    pub argv: Vec<String>,
    /// How the command ended.
    pub termination: Termination,
    /// The acceptable exit codes.
    pub expect: Vec<i32>,
    /// The command's raw captured output.
    pub output: CapturedOutput,
}

impl fmt::Display for UnexpectedExit {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        let expected: Vec<String> = self.expect.iter().map(ToString::to_string).collect();
        writeln!(
            f,
            "command `{}`: {}, but `expect` accepts exit code {}",
            self.argv.join(" "),
            self.termination,
            if expected.is_empty() {
                "none".to_owned()
            } else {
                expected.join(" or ")
            },
        )?;
        match &self.output {
            CapturedOutput::Split { stdout, stderr } => {
                write_output(f, "stdout", stdout)?;
                write_output(f, "stderr", stderr)
            },
            CapturedOutput::Merged { output } => write_output(f, "output", output),
        }
    }
}

/// Write one captured stream under a header, decoded as UTF-8 with invalid bytes
/// replaced.
fn write_output(
    f: &mut fmt::Formatter<'_>,
    name: &str,
    bytes: &[u8],
) -> fmt::Result {
    writeln!(f, "--- {name} ---")?;
    let text = String::from_utf8_lossy(bytes);
    if text.ends_with('\n') || text.is_empty() {
        write!(f, "{text}")
    } else {
        writeln!(f, "{text}")
    }
}

/// Why a task failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskFailure {
    /// The program failed: `fail()`, a Starlark error, a module that cannot be
    /// loaded, a step the executor could not run, or `run()` called while a module
    /// loads. The message includes the Starlark stack when there is one.
    #[error("{message}")]
    Program {
        /// The error message.
        message: String,
    },
    /// A step's exit status was not in its `expect` list.
    #[error("{exit}{message}")]
    UnexpectedExit {
        /// The command, its exit status, and its captured output.
        exit: Box<UnexpectedExit>,
        /// The Starlark error message with the stack of the `run()` call.
        message: String,
    },
}

impl TaskFailure {
    /// A program failure with `message`.
    fn program(message: impl fmt::Display) -> Self {
        Self::Program {
            message: message.to_string(),
        }
    }
}

/// Errors that `run()`, the `env` and `fs` functions, and module loading raise
/// inside Starlark.
#[derive(Debug, thiserror::Error)]
enum RunError {
    /// A function that needs the running task was called while a module was being
    /// loaded, outside any task.
    #[error("{call} cannot be called while {module} is being loaded, outside any task")]
    WhileLoading {
        /// The function, such as `run()`.
        call: &'static str,
        /// The module being loaded, as messages show it.
        module: String,
    },
    /// `run()` was called with no program.
    #[error("run() needs a program: run(\"program\", \"argument\", ...)")]
    NoProgram,
    /// `capture` was neither `"split"` nor `"merged"`.
    #[error("run(): capture must be \"split\" or \"merged\", not \"{0}\"")]
    Capture(String),
    /// `machine` named a part that the machine state does not have.
    #[error(
        "run(): machine has no part \"{0}\"; the parts are {parts}",
        parts = MachinePart::names_text()
    )]
    MachinePart(String),
    /// The executor could not run the step.
    #[error("run(): {0}")]
    Step(#[from] StepError),
    /// The exit status was not in the `expect` list.
    #[error("run(): command `{command}`: {termination}, which `expect` does not accept")]
    Unexpected {
        /// The command.
        command: String,
        /// How it ended.
        termination: Termination,
    },
    /// A loader was asked for a module it was not given.
    #[error("module `{0}` was not loaded")]
    NotLoaded(String),
    /// A file operation of an `env` or `fs` function failed.
    #[error("{call}: {error}")]
    File {
        /// The function, such as `fs.read()`.
        call: &'static str,
        /// What went wrong.
        error: FileError,
    },
    /// A dotenv file has a syntax error.
    #[error("{call}: {path}:{}: {}", .error.line, .error.message)]
    Dotenv {
        /// The function, `env.load_file()` or `env.read()`.
        call: &'static str,
        /// The dotenv file.
        path: AbsolutePath,
        /// The line and what is wrong with it.
        error: DotenvError,
    },
    /// A variable name or value cannot be part of an environment.
    #[error("{call}: invalid variable {name:?}: {reason}")]
    Variable {
        /// The function, such as `env.set()`.
        call: &'static str,
        /// The variable's name.
        name: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A value cannot be written as JSON.
    #[error("fs.write_json(): {0}")]
    NotJson(String),
    /// `indent` was negative.
    #[error("fs.write_json(): indent must be 0 or more, not {0}")]
    Indent(i32),
    /// A JSON number has no Starlark value.
    #[error("fs.read_json(): the number {0} is out of range")]
    NumberRange(String),
}

/// The state of the task whose program is being evaluated, reached by `run()`
/// and the `env` and `fs` functions through the evaluator's `extra`.
#[derive(ProvidesStaticType)]
struct TaskState<'a> {
    /// The task.
    task: &'a TaskInfo,
    /// Runs the task's steps.
    executor: &'a dyn StepExecutor,
    /// The caller environment, which `env.caller` and `env.inherit` read.
    caller: &'a BTreeMap<String, String>,
    /// The task environment, which starts empty.
    env: RefCell<BTreeMap<String, String>>,
    /// Every file access the program makes.
    accesses: RefCell<Accesses>,
    /// Every file the program changes.
    changes: RefCell<FileChanges>,
    /// The step whose exit status `expect` did not accept, if any.
    unexpected: RefCell<Option<UnexpectedExit>>,
}

/// Marks an evaluator that loads a module, outside any task.
#[derive(ProvidesStaticType)]
struct LoadingModule {
    /// The module being loaded.
    path: AbsolutePath,
    /// The directory of the task that loads it, against which `fs` paths resolve.
    dir: AbsolutePath,
    /// Every file access the module's top-level code makes.
    accesses: RefCell<Accesses>,
    /// Every file the module's top-level code changes.
    changes: RefCell<FileChanges>,
}

/// The error for `call`, a function that needs the running task, when `eval`
/// evaluates no task.
fn outside_task(
    eval: &Evaluator<'_, '_, '_>,
    call: &'static str,
) -> starlark::Error {
    let module = eval
        .extra
        .and_then(|extra| extra.downcast_ref::<LoadingModule>())
        .map_or_else(
            || "a module".to_owned(),
            |loading| format!("the module {}", loading.path),
        );
    starlark::Error::new_other(RunError::WhileLoading { call, module })
}

/// Call `f` with the state of the task that `eval` evaluates.
///
/// # Errors
///
/// Fails when `eval` loads a module, outside any task, or when `f` fails.
fn with_task<R>(
    eval: &Evaluator<'_, '_, '_>,
    call: &'static str,
    f: impl FnOnce(&TaskState<'_>) -> Result<R, RunError>,
) -> starlark::Result<R> {
    let state = eval
        .extra
        .and_then(|extra| extra.downcast_ref::<TaskState>())
        .ok_or_else(|| outside_task(eval, call))?;
    f(state).map_err(starlark::Error::new_other)
}

/// Call `f` with the file operations of the task that `eval` evaluates or loads a
/// module for, so every access and every change is recorded against that task.
///
/// # Errors
///
/// Fails when `f` fails.
fn with_files<R>(
    eval: &Evaluator<'_, '_, '_>,
    call: &'static str,
    f: impl FnOnce(&mut Files<'_>) -> Result<R, RunError>,
) -> starlark::Result<R> {
    let extra = eval.extra;
    let result = if let Some(state) = extra.and_then(|extra| extra.downcast_ref::<TaskState>()) {
        f(&mut Files::new(
            &state.task.dir,
            &mut state.accesses.borrow_mut(),
            &mut state.changes.borrow_mut(),
        ))
    } else if let Some(loading) = extra.and_then(|extra| extra.downcast_ref::<LoadingModule>()) {
        f(&mut Files::new(
            &loading.dir,
            &mut loading.accesses.borrow_mut(),
            &mut loading.changes.borrow_mut(),
        ))
    } else {
        return Err(outside_task(eval, call));
    };
    result.map_err(starlark::Error::new_other)
}

/// Check that `name` and `value` can be a variable of an environment: a name that
/// is not empty and holds no `=`, and neither holding a NUL character.
fn check_variable(
    call: &'static str,
    name: &str,
    value: &str,
) -> Result<(), RunError> {
    let reason = if name.is_empty() {
        "the name is empty"
    } else if name.contains('=') {
        "the name contains ="
    } else if name.contains('\0') || value.contains('\0') {
        "it contains a NUL character"
    } else {
        return Ok(());
    };
    Err(RunError::Variable {
        call,
        name: name.to_owned(),
        reason,
    })
}

/// Read the dotenv file at `path` for `call`: its variables, or `None` when it is
/// missing. The file is recorded as read, or probed when missing.
fn read_dotenv(
    files: &mut Files<'_>,
    call: &'static str,
    path: &str,
) -> Result<Option<BTreeMap<String, String>>, RunError> {
    let path = files.resolve(path);
    let Some(bytes) = files
        .read_bytes_if_present(&path)
        .map_err(|error| RunError::File { call, error })?
    else {
        return Ok(None);
    };
    dotenv::parse(&String::from_utf8_lossy(&bytes))
        .map(Some)
        .map_err(|error| RunError::Dotenv { call, path, error })
}

/// The variables of the dotenv file at `path`, which must exist, for `call`.
fn require_dotenv(
    files: &mut Files<'_>,
    call: &'static str,
    path: &str,
) -> Result<BTreeMap<String, String>, RunError> {
    read_dotenv(files, call, path)?.ok_or_else(|| {
        RunError::File {
            call,
            error: FileError::Io {
                action: "cannot read",
                path: files.resolve(path),
                source: std::io::Error::from(ErrorKind::NotFound),
            },
        }
    })
}

/// The modules one file loads, by the path its `load` statements give.
#[derive(Default)]
struct ModuleMap {
    /// The frozen modules.
    modules: HashMap<String, FrozenModule>,
}

impl FileLoader for ModuleMap {
    fn load(
        &self,
        path: &str,
    ) -> starlark::Result<FrozenModule> {
        self.modules
            .get(path)
            .cloned()
            .ok_or_else(|| starlark::Error::new_other(RunError::NotLoaded(path.to_owned())))
    }
}

/// Prefixes each line a program prints with the task name.
struct TaskPrinter<'a> {
    /// The task name.
    name: String,
    /// Where the lines go.
    sink: &'a dyn PrintSink,
}

impl PrintHandler for TaskPrinter<'_> {
    fn println(
        &self,
        text: &str,
    ) -> starlark::Result<()> {
        for line in text.split('\n') {
            self.sink.print_line(&format!("{}: {line}", self.name));
        }
        Ok(())
    }
}

/// The globals of task programs and modules.
#[starlark_module]
fn kizz_globals(builder: &mut GlobalsBuilder) {
    /// Run a command and return its result.
    ///
    /// The positional arguments are the program, then its arguments. With
    /// `expect = None`, `run()` always returns; with a list of exit codes, the task
    /// fails when the exit code is not in it or a signal killed the command.
    #[expect(
        clippy::too_many_arguments,
        reason = "run() takes every parameter that \"Running commands\" lists, as Starlark \
                  keywords"
    )]
    fn run<'v>(
        #[starlark(args)] argv: UnpackTuple<&str>,
        #[starlark(require = named, default = NoneOr::None)] stdin: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] env: NoneOr<
            UnpackDictEntries<&str, NoneOr<&str>>,
        >,
        #[starlark(require = named, default = NoneOr::None)] cwd: NoneOr<&str>,
        #[starlark(require = named, default = "split")] capture: &str,
        #[starlark(require = named, default = false)] stream: bool,
        #[starlark(require = named, default = false)] always: bool,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        ignore: UnpackListOrTuple<String>,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        track: UnpackListOrTuple<String>,
        #[starlark(require = named, default = false)] timestamps: bool,
        #[starlark(require = named, default = UnpackListOrTuple::default())]
        machine: UnpackListOrTuple<String>,
        #[starlark(require = named, default = NoneOr::None)] expect: NoneOr<UnpackListOrTuple<i32>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let Some(state) = eval
            .extra
            .and_then(|extra| extra.downcast_ref::<TaskState>())
        else {
            return Err(outside_task(eval, "run()"));
        };
        if argv.items.is_empty() {
            return Err(starlark::Error::new_other(RunError::NoProgram));
        }
        let capture = match capture {
            "split" => CaptureMode::Split,
            "merged" => CaptureMode::Merged,
            other => {
                return Err(starlark::Error::new_other(RunError::Capture(
                    other.to_owned(),
                )));
            },
        };
        let mut machine = machine.items;
        if let Some(unknown) = machine
            .iter()
            .find(|name| MachinePart::from_name(name).is_none())
        {
            return Err(starlark::Error::new_other(RunError::MachinePart(
                unknown.clone(),
            )));
        }
        machine.sort();
        machine.dedup();
        let mut step_env = state.env.borrow().clone();
        if let NoneOr::Other(entries) = env {
            for (name, value) in entries.entries {
                match value {
                    NoneOr::Other(value) => {
                        step_env.insert(name.to_owned(), value.to_owned());
                    },
                    NoneOr::None => {
                        step_env.remove(name);
                    },
                }
            }
        }
        let step = StepSpec {
            argv: argv.items.iter().map(|&arg| arg.to_owned()).collect(),
            stdin: stdin.into_option().map(str::to_owned),
            env: step_env,
            cwd: cwd
                .into_option()
                .map_or_else(|| state.task.dir.clone(), |cwd| state.task.dir.join(cwd)),
            capture,
            stream,
            always,
            ignore: ignore.items,
            track: track.items,
            timestamps,
            machine,
            expect: expect.into_option().map(|codes| codes.items),
        };
        let outcome = state
            .executor
            .execute(&step)
            .map_err(|error| starlark::Error::new_other(RunError::from(error)))?;
        if let Some(expect) = &step.expect
            && !matches!(outcome.termination, Termination::Exited(code) if expect.contains(&code))
        {
            let error = RunError::Unexpected {
                command: step.command(),
                termination: outcome.termination,
            };
            *state.unexpected.borrow_mut() = Some(UnexpectedExit {
                argv: step.argv,
                termination: outcome.termination,
                expect: expect.clone(),
                output: outcome.output,
            });
            return Err(starlark::Error::new_other(error));
        }
        Ok(result_value(eval.heap(), &outcome))
    }
}

/// The value `run()` returns: `rc`, `signal`, `stdout` and `stderr` or `output`
/// for the capture mode, and `cached`.
fn result_value<'v>(
    heap: Heap<'v>,
    outcome: &StepOutcome,
) -> Value<'v> {
    let int_or_none =
        |value: Option<i32>| value.map_or_else(Value::new_none, |value| heap.alloc(value));
    let text = |bytes: &[u8]| heap.alloc(String::from_utf8_lossy(bytes).as_ref());
    let mut fields: Vec<(&str, Value<'v>)> = vec![
        ("rc", int_or_none(outcome.termination.code())),
        ("signal", int_or_none(outcome.termination.signal())),
    ];
    match &outcome.output {
        CapturedOutput::Split { stdout, stderr } => {
            fields.push(("stdout", text(stdout)));
            fields.push(("stderr", text(stderr)));
        },
        CapturedOutput::Merged { output } => fields.push(("output", text(output))),
    }
    fields.push(("cached", Value::new_bool(outcome.cached)));
    heap.alloc(AllocStruct(fields))
}

/// The `env` functions, which read the caller environment and dotenv files and
/// change the task environment.
#[starlark_module]
fn env_functions(builder: &mut GlobalsBuilder) {
    /// Read the variable `name` from the caller environment, or `default` when it
    /// is unset, without changing the task environment.
    fn caller<'v>(
        #[starlark(require = pos)] name: &str,
        default: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let value = with_task(eval, "env.caller()", |state| {
            Ok(state.caller.get(name).cloned())
        })?;
        Ok(value.map_or_else(
            || default.unwrap_or_else(Value::new_none),
            |value| eval.heap().alloc(value),
        ))
    }

    /// Copy the named variables from the caller environment into the task
    /// environment, skipping the unset ones.
    fn inherit(
        #[starlark(args)] names: UnpackTuple<&str>,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<NoneType> {
        with_task(eval, "env.inherit()", |state| {
            let mut env = state.env.borrow_mut();
            for name in names.items {
                if let Some(value) = state.caller.get(name) {
                    env.insert(name.to_owned(), value.clone());
                }
            }
            Ok(NoneType)
        })
    }

    /// Read the dotenv file at `path`, fill in `defaults` for the variables it does
    /// not set, add the result to the task environment, and return it. The file is
    /// recorded as a dependency of the task; with `required = False`, a missing
    /// file means only the defaults apply.
    fn load_file<'v>(
        #[starlark(require = pos)] path: &str,
        #[starlark(default = UnpackDictEntries::default())] defaults: UnpackDictEntries<&str, &str>,
        #[starlark(default = true)] required: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let call = "env.load_file()";
        let vars = with_task(eval, call, |state| {
            let mut vars = BTreeMap::new();
            for (name, value) in defaults.entries {
                check_variable(call, name, value)?;
                vars.insert(name.to_owned(), value.to_owned());
            }
            let mut accesses = state.accesses.borrow_mut();
            let mut changes = state.changes.borrow_mut();
            let mut files = Files::new(&state.task.dir, &mut accesses, &mut changes);
            let file = if required {
                Some(require_dotenv(&mut files, call, path)?)
            } else {
                read_dotenv(&mut files, call, path)?
            };
            vars.extend(file.unwrap_or_default());
            state.env.borrow_mut().extend(vars.clone());
            Ok(vars)
        })?;
        Ok(eval.heap().alloc(vars))
    }

    /// Read the dotenv file at `path` into a dict, without changing the task
    /// environment.
    fn read<'v>(
        #[starlark(require = pos)] path: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let vars = with_files(eval, "env.read()", |files| {
            require_dotenv(files, "env.read()", path)
        })?;
        Ok(eval.heap().alloc(vars))
    }

    /// Set the variable `name` of the task environment to `value`.
    fn set(
        #[starlark(require = pos)] name: &str,
        #[starlark(require = pos)] value: &str,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<NoneType> {
        with_task(eval, "env.set()", |state| {
            check_variable("env.set()", name, value)?;
            state
                .env
                .borrow_mut()
                .insert(name.to_owned(), value.to_owned());
            Ok(NoneType)
        })
    }

    /// Set every variable of the dict `values` in the task environment.
    fn update(
        #[starlark(require = pos)] values: UnpackDictEntries<&str, &str>,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<NoneType> {
        with_task(eval, "env.update()", |state| {
            for (name, value) in &values.entries {
                check_variable("env.update()", name, value)?;
            }
            state.env.borrow_mut().extend(
                values
                    .entries
                    .iter()
                    .map(|&(name, value)| (name.to_owned(), value.to_owned())),
            );
            Ok(NoneType)
        })
    }

    /// Remove the variable `name` from the task environment, if it is set.
    fn unset(
        #[starlark(require = pos)] name: &str,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<NoneType> {
        with_task(eval, "env.unset()", |state| {
            state.env.borrow_mut().remove(name);
            Ok(NoneType)
        })
    }

    /// Read the variable `name` from the task environment, or `default` when it
    /// is unset.
    fn get<'v>(
        #[starlark(require = pos)] name: &str,
        default: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let value = with_task(eval, "env.get()", |state| {
            Ok(state.env.borrow().get(name).cloned())
        })?;
        Ok(value.map_or_else(
            || default.unwrap_or_else(Value::new_none),
            |value| eval.heap().alloc(value),
        ))
    }

    /// The task environment as a new dict.
    fn dict<'v>(eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        let env = with_task(eval, "env.dict()", |state| Ok(state.env.borrow().clone()))?;
        Ok(eval.heap().alloc(env))
    }

    /// Empty the task environment.
    fn clear(eval: &mut Evaluator<'_, '_, '_>) -> starlark::Result<NoneType> {
        with_task(eval, "env.clear()", |state| {
            state.env.borrow_mut().clear();
            Ok(NoneType)
        })
    }
}

/// The `fs` functions, which read and write files and inspect the file system.
/// Paths are relative to the task directory, and every access is recorded.
#[starlark_module]
fn fs_functions(builder: &mut GlobalsBuilder) {
    /// The text of the file at `path`, with bytes that are not valid UTF-8
    /// replaced.
    fn read(
        #[starlark(require = pos)] path: &str,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<String> {
        with_files(eval, "fs.read()", |files| {
            files.read_text(path).map_err(|error| {
                RunError::File {
                    call: "fs.read()",
                    error,
                }
            })
        })
    }

    /// Write `text` to the file at `path`, creating parent directories, unless the
    /// file already holds exactly `text`.
    fn write(
        #[starlark(require = pos)] path: &str,
        #[starlark(require = pos)] text: &str,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<NoneType> {
        with_files(eval, "fs.write()", |files| {
            files
                .write_bytes(path, text.as_bytes())
                .map(|_| NoneType)
                .map_err(|error| {
                    RunError::File {
                        call: "fs.write()",
                        error,
                    }
                })
        })
    }

    /// The JSON value in the file at `path`, as a Starlark value.
    fn read_json<'v>(
        #[starlark(require = pos)] path: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let value = with_files(eval, "fs.read_json()", |files| {
            files.read_json(path).map_err(|error| {
                RunError::File {
                    call: "fs.read_json()",
                    error,
                }
            })
        })?;
        json_value(eval.heap(), &value).map_err(starlark::Error::new_other)
    }

    /// Write `value` to the file at `path` as stable JSON, with sorted keys,
    /// `indent` spaces per level, and a trailing newline, unless the file already
    /// holds exactly that text.
    fn write_json<'v>(
        #[starlark(require = pos)] path: &str,
        #[starlark(require = pos)] value: Value<'v>,
        #[starlark(default = 2)] indent: i32,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<NoneType> {
        let indent = usize::try_from(indent)
            .map_err(|_| starlark::Error::new_other(RunError::Indent(indent)))?;
        let json = value
            .to_json_value()
            .map_err(|error| starlark::Error::new_other(RunError::NotJson(error.to_string())))?;
        with_files(eval, "fs.write_json()", |files| {
            files
                .write_json(path, &json, indent)
                .map(|_| NoneType)
                .map_err(|error| {
                    RunError::File {
                        call: "fs.write_json()",
                        error,
                    }
                })
        })
    }

    /// Whether `path` exists, following symbolic links.
    fn exists(
        #[starlark(require = pos)] path: &str,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<bool> {
        with_files(eval, "fs.exists()", |files| Ok(files.exists(path)))
    }

    /// The names of the entries of the directory at `path`, sorted.
    fn list(
        #[starlark(require = pos, default = ".")] path: &str,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<Vec<String>> {
        with_files(eval, "fs.list()", |files| {
            files.list(path).map_err(|error| {
                RunError::File {
                    call: "fs.list()",
                    error,
                }
            })
        })
    }

    /// The paths that match the glob `pattern`, sorted.
    fn glob(
        #[starlark(require = pos)] pattern: &str,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<Vec<String>> {
        with_files(eval, "fs.glob()", |files| {
            files.glob(pattern).map_err(|error| {
                RunError::File {
                    call: "fs.glob()",
                    error,
                }
            })
        })
    }
}

/// The Starlark value of the JSON `value`: `None`, a bool, an int for integers
/// that fit in 64 bits, a float for other numbers, a string, a list, or a dict.
fn json_value<'v>(
    heap: Heap<'v>,
    value: &serde_json::Value,
) -> Result<Value<'v>, RunError> {
    Ok(match value {
        serde_json::Value::Null => Value::new_none(),
        serde_json::Value::Bool(value) => Value::new_bool(*value),
        serde_json::Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                heap.alloc(int)
            } else if let Some(int) = number.as_u64() {
                heap.alloc(int)
            } else if let Some(float) = number.as_f64().filter(|float| float.is_finite()) {
                heap.alloc(float)
            } else {
                return Err(RunError::NumberRange(number.to_string()));
            }
        },
        serde_json::Value::String(text) => heap.alloc(text.as_str()),
        serde_json::Value::Array(items) => {
            let items = items
                .iter()
                .map(|item| json_value(heap, item))
                .collect::<Result<Vec<_>, _>>()?;
            heap.alloc(items)
        },
        serde_json::Value::Object(members) => {
            let members = members
                .iter()
                .map(|(key, member)| Ok((key.as_str(), json_value(heap, member)?)))
                .collect::<Result<Vec<_>, RunError>>()?;
            heap.alloc(AllocDict(members))
        },
    })
}

/// The globals of task programs and modules.
fn globals() -> Globals {
    GlobalsBuilder::extended_by(&[LibraryExtension::Json, LibraryExtension::Print])
        .with(kizz_globals)
        .with_namespace("env", env_functions)
        .with_namespace("fs", fs_functions)
        .build()
}

/// Set `task` in `module` to the values that describe `task`.
fn set_task_value(
    module: &Module<'_>,
    task: &TaskInfo,
) {
    let heap = module.heap();
    let name = task
        .name
        .as_deref()
        .map_or_else(Value::new_none, |name| heap.alloc(name));
    let value = heap.alloc(AllocStruct([
        ("dir", heap.alloc(task.dir.to_string())),
        ("file", heap.alloc(task.file.to_string())),
        ("line", heap.alloc(task.line)),
        ("name", name),
        ("targets", heap.alloc(task.targets.clone())),
        ("selected", heap.alloc(task.selected.clone())),
    ]));
    module.set("task", value);
}

/// The directory that holds `path`, or `path` itself for the root.
fn parent_dir(path: &AbsolutePath) -> AbsolutePath {
    path.as_path()
        .parent()
        .and_then(|parent| AbsolutePath::new(parent).ok())
        .unwrap_or_else(|| path.clone())
}

/// Loads the modules of one task program, depth first.
struct ModuleLoader<'a> {
    /// The task whose program is loaded.
    task: &'a TaskInfo,
    /// The globals.
    globals: &'a Globals,
    /// Where module `print` output goes.
    printer: &'a TaskPrinter<'a>,
    /// Every module read, and every module path found missing.
    accesses: Accesses,
    /// Every file the modules' top-level code changed.
    changes: FileChanges,
    /// The modules loaded so far, by absolute path.
    loaded: HashMap<AbsolutePath, FrozenModule>,
    /// The modules being loaded, outermost first, to find load cycles.
    loading: Vec<AbsolutePath>,
    /// The path and source of each loaded module, in the order loading finished.
    sources: Vec<(AbsolutePath, String)>,
}

impl ModuleLoader<'_> {
    /// Load every module that `ast`, in the file `from` in `dir`, loads.
    fn load_imports(
        &mut self,
        ast: &AstModule,
        dir: &AbsolutePath,
        from: &str,
    ) -> Result<ModuleMap, TaskFailure> {
        let ids: Vec<String> = ast
            .loads()
            .iter()
            .map(|load| load.module_id.to_owned())
            .collect();
        let mut imports = ModuleMap::default();
        for id in ids {
            let module = self.load_module(&dir.join(&id), &id, from)?;
            imports.modules.insert(id, module);
        }
        Ok(imports)
    }

    /// Load the module at `path`, which the file `from` loads as `id`.
    fn load_module(
        &mut self,
        path: &AbsolutePath,
        id: &str,
        from: &str,
    ) -> Result<FrozenModule, TaskFailure> {
        if let Some(module) = self.loaded.get(path) {
            return Ok(module.clone());
        }
        if self.loading.contains(path) {
            return Err(TaskFailure::program(format!(
                "{from}: load(\"{id}\"): the module {path} loads itself, directly or indirectly"
            )));
        }
        let source = match fs::read_to_string(path) {
            Ok(source) => {
                self.accesses.record(path.clone(), AccessKind::Read);
                source
            },
            Err(error) => {
                let kind = if error.kind() == ErrorKind::NotFound {
                    AccessKind::Probe
                } else {
                    AccessKind::Read
                };
                self.accesses.record(path.clone(), kind);
                return Err(TaskFailure::program(format!(
                    "{from}: load(\"{id}\"): cannot read {path}: {error}"
                )));
            },
        };
        let name = path.to_string();
        let ast = AstModule::parse(&name, source.clone(), &Dialect::Extended)
            .map_err(TaskFailure::program)?;
        self.loading.push(path.clone());
        let imports = self.load_imports(&ast, &parent_dir(path), &name);
        self.loading.pop();
        let imports = imports?;
        let module = evaluate_module(
            ast,
            path,
            &imports,
            self.task,
            self.globals,
            self.printer,
            &mut self.accesses,
            &mut self.changes,
        )?;
        self.sources.push((path.clone(), source));
        self.loaded.insert(path.clone(), module.clone());
        Ok(module)
    }
}

/// Evaluate and freeze one module, outside any task, recording the file accesses
/// of its top-level code in `accesses` and the files it changed in `changes`.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is a separate part of the loading context"
)]
fn evaluate_module(
    ast: AstModule,
    path: &AbsolutePath,
    imports: &ModuleMap,
    task: &TaskInfo,
    globals: &Globals,
    printer: &TaskPrinter<'_>,
    accesses: &mut Accesses,
    changes: &mut FileChanges,
) -> Result<FrozenModule, TaskFailure> {
    let marker = LoadingModule {
        path: path.clone(),
        dir: task.dir.clone(),
        accesses: RefCell::new(Accesses::new()),
        changes: RefCell::new(FileChanges::new()),
    };
    let result = Module::with_temp_heap(|module| {
        set_task_value(&module, task);
        {
            let mut eval = Evaluator::new(&module);
            eval.set_loader(imports);
            eval.set_print_handler(printer);
            eval.extra = Some(&marker);
            eval.eval_module(ast, globals)
                .map_err(TaskFailure::program)?;
        }
        module.freeze().map_err(|error| {
            TaskFailure::program(format!(
                "cannot freeze the module {path}: {}",
                error.err_msg
            ))
        })
    });
    accesses.merge(marker.accesses.into_inner());
    changes.merge(marker.changes.into_inner());
    result
}

/// Hash one length-prefixed field.
fn hash_field(
    hasher: &mut blake3::Hasher,
    bytes: &[u8],
) {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    hasher.update(&len.to_le_bytes());
    hasher.update(bytes);
}

/// The parts of a task program and the hash of each, so explanations can say
/// whether the block or a named loaded module changed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProgramParts {
    /// The BLAKE3 hash of the block's source.
    pub block: [u8; 32],
    /// The absolute path of every module the block loads, directly or indirectly,
    /// with the BLAKE3 hash of its source, sorted by path.
    pub modules: Vec<(PathBuf, [u8; 32])>,
}

/// The result of [`TaskProgram::load`].
#[derive(Debug)]
pub struct ProgramLoad {
    /// Every module the task read and every module path it found missing, with the
    /// file accesses of the modules' top-level code.
    pub accesses: Accesses,
    /// Every file the modules' top-level code changed through `fs.write` and
    /// `fs.write_json`, in the order it first changed each.
    pub changes: FileChanges,
    /// The loaded program, or why it could not be loaded.
    pub program: Result<TaskProgram, TaskFailure>,
}

/// A task program whose modules are loaded, ready to run.
pub struct TaskProgram {
    /// The task.
    task: TaskInfo,
    /// The parsed block.
    ast: AstModule,
    /// The modules the block loads.
    imports: ModuleMap,
    /// The hash of the block and every module it loads.
    hash: ProgramHash,
    /// The hashes of the block and of each module.
    parts: ProgramParts,
}

impl fmt::Debug for TaskProgram {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        f.debug_struct("TaskProgram")
            .field("task", &self.task)
            .field("hash", &self.hash)
            .field("parts", &self.parts)
            .finish_non_exhaustive()
    }
}

impl TaskProgram {
    /// Parse the task block and load every module it loads, directly or
    /// indirectly.
    ///
    /// A `load` path is relative to the loading file, or absolute. Each module is
    /// evaluated once per task, outside the task, with `print` output going to
    /// `print`.
    #[must_use]
    pub fn load(
        task: TaskInfo,
        print: &dyn PrintSink,
    ) -> ProgramLoad {
        let globals = globals();
        let printer = TaskPrinter {
            name: task.display_name(),
            sink: print,
        };
        let mut loader = ModuleLoader {
            task: &task,
            globals: &globals,
            printer: &printer,
            accesses: Accesses::new(),
            changes: FileChanges::new(),
            loaded: HashMap::new(),
            loading: Vec::new(),
            sources: Vec::new(),
        };
        let file_name = task.relative_file.display().to_string();
        // Blank lines before the source make Starlark's line numbers those of the
        // `kizz.md`.
        let padded = format!("{}{}", "\n".repeat(task.line), task.source);
        let parsed = AstModule::parse(&file_name, padded, &Dialect::Extended)
            .map_err(TaskFailure::program)
            .and_then(|ast| {
                let imports = loader.load_imports(&ast, &task.dir, &file_name)?;
                Ok((ast, imports))
            });
        let ModuleLoader {
            accesses,
            changes,
            sources,
            ..
        } = loader;
        let program = parsed.map(|(ast, imports)| {
            let mut hasher = blake3::Hasher::new_derive_key(PROGRAM_HASH_CONTEXT);
            hash_field(&mut hasher, task.source.as_bytes());
            for (path, source) in &sources {
                hash_field(&mut hasher, path.as_path().as_os_str().as_encoded_bytes());
                hash_field(&mut hasher, source.as_bytes());
            }
            let hash = ProgramHash(*hasher.finalize().as_bytes());
            let mut modules: Vec<(PathBuf, [u8; 32])> = sources
                .iter()
                .map(|(path, source)| {
                    (
                        path.as_path().to_path_buf(),
                        *blake3::hash(source.as_bytes()).as_bytes(),
                    )
                })
                .collect();
            modules.sort();
            let parts = ProgramParts {
                block: *blake3::hash(task.source.as_bytes()).as_bytes(),
                modules,
            };
            Self {
                task,
                ast,
                imports,
                hash,
                parts,
            }
        });
        ProgramLoad {
            accesses,
            changes,
            program,
        }
    }

    /// The hash of the block and every module it loads.
    #[must_use]
    pub const fn hash(&self) -> ProgramHash {
        self.hash
    }

    /// The hashes of the block and of each module it loads, so explanations can
    /// name the part that changed.
    #[must_use]
    pub const fn parts(&self) -> &ProgramParts {
        &self.parts
    }

    /// The task.
    #[must_use]
    pub const fn task(&self) -> &TaskInfo {
        &self.task
    }

    /// Evaluate the task block, handing each `run()` call to `executor` in program
    /// order, with `print` output going to `print`.
    ///
    /// The task environment starts empty. `caller` is the caller environment, the
    /// only one that `env.caller` and `env.inherit` read. The result holds every
    /// file access the program made while it ran, and every file it changed,
    /// whether or not it failed.
    #[must_use]
    pub fn run(
        self,
        caller: &BTreeMap<String, String>,
        executor: &dyn StepExecutor,
        print: &dyn PrintSink,
    ) -> TaskRun {
        let Self {
            task, ast, imports, ..
        } = self;
        let globals = globals();
        let printer = TaskPrinter {
            name: task.display_name(),
            sink: print,
        };
        let state = TaskState {
            task: &task,
            executor,
            caller,
            env: RefCell::new(BTreeMap::new()),
            accesses: RefCell::new(Accesses::new()),
            changes: RefCell::new(FileChanges::new()),
            unexpected: RefCell::new(None),
        };
        let result = Module::with_temp_heap(|module| {
            set_task_value(&module, &task);
            let mut eval = Evaluator::new(&module);
            eval.set_loader(&imports);
            eval.set_print_handler(&printer);
            eval.extra = Some(&state);
            eval.eval_module(ast, &globals).map(|_| ())
        });
        let result = result.map_err(|error| {
            let message = error.to_string();
            match state.unexpected.take() {
                Some(exit) => {
                    TaskFailure::UnexpectedExit {
                        exit: Box::new(exit),
                        message,
                    }
                },
                None => TaskFailure::Program { message },
            }
        });
        TaskRun {
            accesses: state.accesses.into_inner(),
            changes: state.changes.into_inner(),
            result,
        }
    }
}

/// The result of [`TaskProgram::run`].
#[derive(Debug)]
pub struct TaskRun {
    /// Every file access the program made while it ran, through the `fs`
    /// functions, `env.load_file`, and `env.read`, missing files and directory
    /// listings included.
    pub accesses: Accesses,
    /// Every file the program changed through `fs.write` and `fs.write_json` while
    /// it ran, in the order it first changed each, without repeats.
    pub changes: FileChanges,
    /// Whether the program succeeded, or why it failed.
    pub result: Result<(), TaskFailure>,
}

/// Unit tests for the runtime's own logic.
#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap, path::PathBuf};

    use kizz_trace::{AbsolutePath, CapturedOutput, Termination};

    use super::{PrintSink, TaskInfo, TaskProgram, UnexpectedExit};
    use crate::step::{StepError, StepExecutor, StepOutcome, StepSpec};

    /// Records each step and answers with exit status 0 and no output.
    #[derive(Default)]
    struct Recorder {
        /// The steps received, in order.
        steps: RefCell<Vec<StepSpec>>,
    }

    impl StepExecutor for Recorder {
        fn execute(
            &self,
            step: &StepSpec,
        ) -> Result<StepOutcome, StepError> {
            self.steps.borrow_mut().push(step.clone());
            Ok(StepOutcome {
                termination: Termination::Exited(0),
                output: CapturedOutput::empty(step.capture),
                cached: false,
            })
        }
    }

    /// Drops printed lines.
    struct NoPrint;

    impl PrintSink for NoPrint {
        fn print_line(
            &self,
            _line: &str,
        ) {
        }
    }

    /// A task in `/work` whose block is `source`.
    fn task(source: &str) -> TaskInfo {
        TaskInfo {
            dir: AbsolutePath::new("/work").unwrap(),
            file: AbsolutePath::new("/work/kizz.md").unwrap(),
            relative_file: PathBuf::from("kizz.md"),
            line: 1,
            index: 0,
            name: None,
            targets: vec!["ci".to_owned()],
            selected: vec!["ci".to_owned()],
            source: source.to_owned(),
        }
    }

    /// The per-run `env` applies to the task environment for one step only: a
    /// string sets a variable, and `None` removes it.
    #[test]
    fn per_run_env_sets_and_removes_task_variables() {
        let program = TaskProgram::load(
            task(
                "env.update({\"KEPT\": \"old\", \"REMOVED\": \"gone\"})\nrun(\"a\", env = \
                 {\"REMOVED\": None, \"ADDED\": \"new\", \"KEPT\": \"changed\"})\nrun(\"b\")\n",
            ),
            &NoPrint,
        )
        .program
        .unwrap();
        let recorder = Recorder::default();
        let start = BTreeMap::from([
            ("KEPT".to_owned(), "old".to_owned()),
            ("REMOVED".to_owned(), "gone".to_owned()),
        ]);
        program
            .run(&BTreeMap::new(), &recorder, &NoPrint)
            .result
            .unwrap();
        let envs: Vec<BTreeMap<String, String>> = recorder
            .steps
            .borrow()
            .iter()
            .map(|step| step.env.clone())
            .collect();
        assert_eq!(envs, [
            BTreeMap::from([
                ("ADDED".to_owned(), "new".to_owned()),
                ("KEPT".to_owned(), "changed".to_owned()),
            ]),
            start,
        ]);
    }

    /// An unexpected exit shows the command, its exit status, the accepted codes,
    /// and the merged output, with a newline added when the output lacks one.
    #[test]
    fn unexpected_exit_shows_command_status_and_output() {
        let exit = UnexpectedExit {
            argv: vec!["make".to_owned(), "all".to_owned()],
            termination: Termination::Signaled(9),
            expect: vec![0],
            output: CapturedOutput::Merged {
                output: b"partial".to_vec(),
            },
        };
        assert_eq!(
            exit.to_string(),
            "command `make all`: killed by signal 9, but `expect` accepts exit code 0\n--- \
             output ---\npartial\n"
        );
    }
}
