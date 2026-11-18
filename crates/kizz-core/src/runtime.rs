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
//! Each positional argument of `run()` is a string, or a list or tuple of strings,
//! lists or tuples, which expands in place, in order and at any depth, into the
//! step's command. The expansion walks the values with its own stack, fails on a
//! list or tuple that contains itself, and stops past a million values, so it never
//! recurses without bound and always ends.
//!
//! The task environment starts empty, and only the `env` functions change it;
//! `env.caller` and `env.inherit` read the caller environment that
//! [`TaskProgram::run`] is given, never the process environment. The task
//! environment and the caller environment exist only while the task runs, so while
//! a module is being loaded, the `env` functions other than `env.read` are errors,
//! like `run()`. The `fs` functions and `env.read` work there too, and every file
//! access they make, like each module read, is recorded against the task, as is
//! every file that `fs.write` or `fs.write_json` changes.
//!
//! `run()` returns a value of the runtime's own Starlark type, `run_result`, with
//! the attributes `rc`, `signal`, `stdout` and `stderr` or `output`, and `cached`.
//! The value notes when the program reads `rc` or `signal`, or encodes the value
//! with `json.encode` or `fs.write_json`, which encode it as an object of its
//! attributes, as they encoded the struct that `run()` returned before. So when the
//! program ends, [`TaskRun::unchecked`] lists every step that exited with a
//! non-zero code or was killed by a signal, whose `run()` had no `expect`, and
//! whose result the program never checked ("Exit codes and failure").
//!
//! The runtime never panics ("Kizz never panics"): the state of the running task
//! and of a module being loaded lives in `Cell`s, which the private `cells` module
//! takes out, changes and puts back, where `RefCell` borrows would panic while the
//! cell is borrowed; and the blank lines that put the block at its line of the
//! `kizz.md` are pushed into space reserved with `String::try_reserve_exact`, where
//! `str::repeat` would panic on a length that overflows.

use std::{
    cell::Cell,
    collections::{BTreeMap, HashMap, HashSet},
    fmt, fs,
    io::{self, ErrorKind},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use allocative::Allocative;
use kizz_store::MachinePart;
use kizz_trace::{
    AbsolutePath, AccessKind, Accesses, CaptureMode, CapturedOutput, NotAbsoluteError, Termination,
};
use starlark::{
    PrintHandler,
    any::ProvidesStaticType,
    collections::SmallMap,
    docs::{DocFunction, DocItem, DocMember},
    environment::{FrozenModule, Globals, GlobalsBuilder, LibraryExtension, Module},
    eval::{Evaluator, FileLoader},
    pagable::{
        StarlarkDeserialize, StarlarkDeserializeContext, StarlarkSerialize,
        StarlarkSerializeContext,
    },
    starlark_module,
    syntax::{AstModule, Dialect},
    values::{
        AllocValue, Heap, StarlarkValue, Value, ValueIdentity, ValueLike,
        dict::{Dict, UnpackDictEntries},
        list::ListRef,
        list_or_tuple::UnpackListOrTuple,
        none::{NoneOr, NoneType},
        starlark_value,
        structs::AllocStruct,
        tuple::{TupleRef, UnpackTuple},
    },
};

use crate::{
    cells,
    dotenv::{self, DotenvError},
    files::{AccessOutcomes, FileChanges, FileError, Files},
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
    /// The name that the block's `name=` option gives the task, if it has one.
    pub name: Option<String>,
    /// The nearest heading above the block, the task's heading, if there is one.
    pub heading: Option<String>,
    /// The targets on the block, in the order written.
    pub targets: Vec<String>,
    /// The requested targets that selected the task.
    pub selected: Vec<String>,
    /// The task's arguments, which programs read as `task.args`: empty under
    /// `kizz build`. They are not part of the task program hash.
    pub args: Vec<String>,
    /// Whether the block's `cache=never` option marks the task, so every command of
    /// the task runs as if it were marked `always`.
    pub cache_never: bool,
    /// The block's Starlark source.
    pub source: String,
}

impl TaskInfo {
    /// The information of `task`, selected by the requested targets `selected`, with
    /// no arguments, as `kizz build` runs it.
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
            name: task.options.name.clone(),
            heading: task.heading.clone(),
            targets: task.targets.clone(),
            selected: selected.to_vec(),
            args: Vec::new(),
            cache_never: task.options.cache_never,
            source: task.source.clone(),
        })
    }

    /// The information of `task` as `kizz run` runs it: with the arguments `args`,
    /// which its program reads as `task.args`, and an empty `task.selected`,
    /// because no target selected the task.
    ///
    /// # Errors
    ///
    /// Returns [`NotAbsoluteError`] when the task's `kizz.md` path is not absolute.
    pub fn for_run(
        task: &Task,
        args: Vec<String>,
    ) -> Result<Self, NotAbsoluteError> {
        Ok(Self {
            args,
            ..Self::new(task, &[])?
        })
    }

    /// The name that prefixes the task's `print` output: its name, else its heading,
    /// else `path/kizz.md:LINE`.
    #[must_use]
    pub fn display_name(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.heading.clone())
            .unwrap_or_else(|| format!("{}:{}", self.relative_file.display(), self.line))
    }
}

/// Where the lines that task programs `print` go.
pub trait PrintSink {
    /// Show one line of `print` output, already prefixed with the task's
    /// [`TaskInfo::display_name`]: its name, else its heading, else its location.
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
        write_captured(f, &self.output)
    }
}

/// Write the captured output `output` under a header for each stream.
fn write_captured(
    f: &mut fmt::Formatter<'_>,
    output: &CapturedOutput,
) -> fmt::Result {
    match output {
        CapturedOutput::Split { stdout, stderr } => {
            write_output(f, "stdout", stdout)?;
            write_output(f, "stderr", stderr)
        },
        CapturedOutput::Merged { output } => write_output(f, "output", output),
    }
}

/// A step whose command exited with a non-zero code or was killed by a signal,
/// whose `run()` had no `expect`, and whose result the task program never checked:
/// it never read the result's `rc` or `signal` and never encoded the result
/// ("Exit codes and failure"). A result from the cache counts like any other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncheckedExit {
    /// The position of the step in the task's run, counting from zero: the number
    /// of `run()` calls that reached the executor before it.
    pub position: u32,
    /// The step's argument vector.
    pub argv: Vec<String>,
    /// How the command ended: never with exit code 0.
    pub termination: Termination,
    /// The command's raw captured output.
    pub output: CapturedOutput,
    /// Whether the result came from the cache.
    pub cached: bool,
}

impl UncheckedExit {
    /// The command as messages name it: its arguments joined with spaces.
    #[must_use]
    pub fn command(&self) -> String {
        self.argv.join(" ")
    }
}

impl fmt::Display for UncheckedExit {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        writeln!(
            f,
            "command `{}`: {}, which the task program never checked: its `run()` has no \
             `expect`, and the program never read the result's `rc` or `signal`",
            self.command(),
            self.termination,
        )?;
        write_captured(f, &self.output)
    }
}

/// The message of a task that `--fail-unchecked` fails because its program left
/// the steps `exits` unchecked, under the command `invoked`, such as `kizz build`
/// or `kizz run`: a line that says so and names the command with the option, then
/// each command with its exit status and its captured output.
fn unchecked_message(
    exits: &[UncheckedExit],
    invoked: &str,
) -> String {
    let mut message = format!(
        "the task program left {} unchecked, and `{invoked} --fail-unchecked` makes that fail \
         the task\n",
        if exits.len() == 1 {
            "a non-zero exit".to_owned()
        } else {
            format!("{} non-zero exits", exits.len())
        }
    );
    for exit in exits {
        message.push_str(&exit.to_string());
    }
    message
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
    /// The program ended, and left steps unchecked, which fails the task with
    /// `kizz build --fail-unchecked` or `kizz run --fail-unchecked` ("Exit codes and
    /// failure").
    #[error("{}", unchecked_message(.exits, .invoked))]
    Unchecked {
        /// Each unchecked step, in the order the program ran them, with its command,
        /// its exit status, and its captured output.
        exits: Vec<UncheckedExit>,
        /// The command that was given `--fail-unchecked`, as the message names it:
        /// `kizz build` or `kizz run` (see [`Engine::command_name`]).
        ///
        /// [`Engine::command_name`]: crate::Engine::command_name
        invoked: &'static str,
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

/// The most values that the expansion of `run()`'s positional arguments visits:
/// the positional arguments, and every string, list and tuple inside them, alike.
/// The bound keeps the expansion of lists that share lists, which can double at each
/// level, under a million steps, and it refuses no command that Linux could start.
const MAX_ARGUMENT_VALUES: usize = 1_000_000;

/// Where a value among `run()`'s positional arguments is: the positional argument,
/// counted from 1, and the index path within it, as Starlark indexing writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ArgumentPlace {
    /// The positional argument, counted from 1.
    argument: usize,
    /// The index into each list or tuple on the way to the value, outermost first;
    /// empty for the positional argument itself.
    path: Vec<usize>,
}

impl fmt::Display for ArgumentPlace {
    /// `3` for a positional argument itself, and `2 at [1][0]` for `x[1][0]` where
    /// `x` is the second positional argument.
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(f, "{}", self.argument)?;
        if !self.path.is_empty() {
            f.write_str(" at ")?;
            for index in &self.path {
                write!(f, "[{index}]")?;
            }
        }
        Ok(())
    }
}

/// A list or tuple that the expansion of `run()`'s positional arguments is inside.
struct ArgumentFrame<'v> {
    /// The elements of the list or tuple.
    items: &'v [Value<'v>],
    /// The index of the next element to visit.
    next: usize,
    /// The identity of the list or tuple, which the cycle check holds while the
    /// expansion is inside it.
    identity: ValueIdentity<'v>,
}

/// The elements of `value` and its type, `list` or `tuple`, when it is a list or a
/// tuple, frozen or not.
fn list_or_tuple_items(value: Value<'_>) -> Option<(&[Value<'_>], &'static str)> {
    if let Some(list) = ListRef::from_value(value) {
        return Some((list.content(), value.get_type()));
    }
    TupleRef::from_value(value).map(|tuple| (tuple.content(), value.get_type()))
}

/// The command that `run()`'s positional arguments `arguments` give: each string as
/// one argument, unchanged, and each list or tuple expanded in place, in order and at
/// any depth.
///
/// The walk uses an explicit stack of frames, never recursion, so no depth of nesting
/// can overflow the thread's stack. The identities of the lists and tuples on the
/// current path are held in a set, so a list or tuple met again inside itself is an
/// error, while the same list used twice side by side is not. Every visited value
/// counts towards [`MAX_ARGUMENT_VALUES`], so lists that share lists end promptly.
/// Errors come in the order of the walk.
///
/// # Errors
///
/// Returns [`RunError::ArgumentType`] for a value that is neither a string nor a list
/// or tuple, [`RunError::SelfContaining`] for a list or tuple met again inside
/// itself, and [`RunError::TooManyValues`] past the bound.
fn expand_arguments(arguments: &[Value<'_>]) -> Result<Vec<String>, RunError> {
    let mut argv = Vec::new();
    let mut visited: usize = 0;
    for (position, &argument) in arguments.iter().enumerate() {
        let number = position.saturating_add(1);
        let mut stack: Vec<ArgumentFrame<'_>> = Vec::new();
        let mut on_path: HashSet<ValueIdentity<'_>> = HashSet::new();
        let mut pending = Some(argument);
        loop {
            if let Some(value) = pending.take() {
                if visited >= MAX_ARGUMENT_VALUES {
                    return Err(RunError::TooManyValues);
                }
                visited = visited.saturating_add(1);
                let place = || {
                    ArgumentPlace {
                        argument: number,
                        path: stack
                            .iter()
                            .map(|frame| frame.next.saturating_sub(1))
                            .collect(),
                    }
                };
                if let Some(text) = value.unpack_str() {
                    argv.push(text.to_owned());
                } else if let Some((items, kind)) = list_or_tuple_items(value) {
                    let identity = value.identity();
                    if !on_path.insert(identity) {
                        return Err(RunError::SelfContaining {
                            place: place(),
                            kind,
                        });
                    }
                    stack.push(ArgumentFrame {
                        items,
                        next: 0,
                        identity,
                    });
                } else {
                    return Err(RunError::ArgumentType {
                        place: place(),
                        kind: value.get_type(),
                    });
                }
            }
            let Some(frame) = stack.last_mut() else {
                break;
            };
            if let Some(&item) = frame.items.get(frame.next) {
                frame.next = frame.next.saturating_add(1);
                pending = Some(item);
            } else {
                on_path.remove(&frame.identity);
                stack.pop();
            }
        }
    }
    Ok(argv)
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
    /// A positional argument of `run()`, or a value inside one, is neither a string
    /// nor a list or tuple.
    #[error("run(): argument {place} is of type {kind}, not a string, list or tuple")]
    ArgumentType {
        /// Where the value is.
        place: ArgumentPlace,
        /// The value's type, as Starlark's `type()` names it.
        kind: &'static str,
    },
    /// A list or tuple among the positional arguments of `run()` contains itself,
    /// directly or through other lists or tuples.
    #[error("run(): argument {place} is a {kind} that contains itself")]
    SelfContaining {
        /// Where the list or tuple is met again inside itself.
        place: ArgumentPlace,
        /// `list` or `tuple`.
        kind: &'static str,
    },
    /// The positional arguments of `run()` would expand to more values than
    /// [`MAX_ARGUMENT_VALUES`].
    #[error(
        "run(): the arguments expand to more than {max} strings, lists and tuples",
        max = MAX_ARGUMENT_VALUES
    )]
    TooManyValues,
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
    /// A JSON key has no Starlark dict key.
    #[error("fs.read_json(): the key {0:?} cannot be a dict key")]
    JsonKey(String),
    /// A `run()` result was to be stored as a frozen value, which never happens,
    /// because a result exists only while its task runs.
    #[error("a run() result exists only while its task runs, so it cannot be stored")]
    NotStored,
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
    env: Cell<BTreeMap<String, String>>,
    /// Every file access the program makes.
    accesses: Cell<Accesses>,
    /// Every file the program changes.
    changes: Cell<FileChanges>,
    /// How each file access of the program ended, when the run notes it for the
    /// report of `kizz run --output`, and otherwise `None`.
    outcomes: Cell<Option<AccessOutcomes>>,
    /// The step whose exit status `expect` did not accept, if any.
    unexpected: Cell<Option<UnexpectedExit>>,
    /// The number of `run()` calls that reached the executor so far, which is the
    /// position of the next step in the task's run.
    started: Cell<u32>,
    /// The steps that exited with a non-zero code or were killed by a signal and
    /// had no `expect`, in the order they ran, each with whether the program
    /// checked its result.
    watched: Cell<Vec<Watched>>,
}

/// A step that is unchecked unless the program checks its result before it ends:
/// reads its `rc` or `signal`, or encodes it.
struct Watched {
    /// The step, as it is reported when it stays unchecked.
    exit: UncheckedExit,
    /// Set when the program reads the result's `rc` or `signal`, or encodes the
    /// result; shared with the [`RunResult`] that `run()` returned for the step.
    checked: Arc<AtomicBool>,
}

/// Marks an evaluator that loads a module, outside any task.
#[derive(ProvidesStaticType)]
struct LoadingModule {
    /// The module being loaded.
    path: AbsolutePath,
    /// The directory of the task that loads it, against which `fs` paths resolve.
    dir: AbsolutePath,
    /// Every file access the module's top-level code makes.
    accesses: Cell<Accesses>,
    /// Every file the module's top-level code changes.
    changes: Cell<FileChanges>,
    /// How each file access of the module's top-level code ended, when the run
    /// notes it, and otherwise `None`.
    outcomes: Cell<Option<AccessOutcomes>>,
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
/// module for, so every access and every change is recorded against that task, and
/// the outcome of every access is noted when the run notes outcomes.
///
/// The accesses, the changes and the outcomes are taken out of their cells for the
/// length of `f`, which only does file operations and reaches none of the cells,
/// and put back after it.
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
    let (dir, accesses, changes, outcomes) = if let Some(state) =
        extra.and_then(|extra| extra.downcast_ref::<TaskState>())
    {
        (
            &state.task.dir,
            &state.accesses,
            &state.changes,
            &state.outcomes,
        )
    } else if let Some(loading) = extra.and_then(|extra| extra.downcast_ref::<LoadingModule>()) {
        (
            &loading.dir,
            &loading.accesses,
            &loading.changes,
            &loading.outcomes,
        )
    } else {
        return Err(outside_task(eval, call));
    };
    let result = cells::update(accesses, |accesses| {
        cells::update(changes, |changes| {
            cells::update(outcomes, |outcomes| {
                f(&mut Files::new(dir, accesses, changes).noting(outcomes.as_mut()))
            })
        })
    });
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

/// Prefixes each line a program prints with the task's
/// [`TaskInfo::display_name`]: its name, else its heading, else its location.
struct TaskPrinter<'a> {
    /// The prefix: the task's name, else its heading, else `path/kizz.md:LINE`.
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
    /// The positional arguments are the program, then its arguments. Each is a
    /// string, or a list or tuple of strings, lists or tuples, which expands in
    /// place, in order and at any depth, so `[]` adds nothing. With
    /// `expect = None`, `run()` always returns; with a list of exit codes, the task
    /// fails when the exit code is not in it or a signal killed the command. A
    /// non-zero exit or a signal without `expect` is unchecked unless the program
    /// reads the result's `rc` or `signal`, or encodes the result, before it ends.
    #[expect(
        clippy::too_many_arguments,
        reason = "run() takes every parameter that \"Running commands\" lists, as Starlark \
                  keywords"
    )]
    fn run<'v>(
        #[starlark(args)] argv: UnpackTuple<Value<'v>>,
        #[starlark(require = named, default = NoneOr::None)] stdin: NoneOr<&str>,
        #[starlark(require = named, default = NoneOr::None)] env: NoneOr<
            UnpackDictEntries<&str, NoneOr<&str>>,
        >,
        #[starlark(require = named, default = NoneOr::None)] cwd: NoneOr<&str>,
        #[starlark(require = named, default = "split")] capture: &str,
        #[starlark(require = named, default = false)] stream: bool,
        #[starlark(require = named, default = false)] always: bool,
        #[starlark(require = named, default = false)] untraced: bool,
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
        let argv = expand_arguments(&argv.items).map_err(starlark::Error::new_other)?;
        if argv.is_empty() {
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
        let mut step_env = cells::cloned(&state.env);
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
            argv,
            stdin: stdin.into_option().map(str::to_owned),
            env: step_env,
            cwd: cwd
                .into_option()
                .map_or_else(|| state.task.dir.clone(), |cwd| state.task.dir.join(cwd)),
            capture,
            stream,
            always,
            untraced,
            ignore: ignore.items,
            track: track.items,
            timestamps,
            machine,
            expect: expect.into_option().map(|codes| codes.items),
        };
        let position = state.started.get();
        state.started.set(position.saturating_add(1));
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
            state.unexpected.set(Some(UnexpectedExit {
                argv: step.argv,
                termination: outcome.termination,
                expect: expect.clone(),
                output: outcome.output,
            }));
            return Err(starlark::Error::new_other(error));
        }
        let result = RunResult::new(&outcome);
        if step.expect.is_none() && outcome.termination != Termination::Exited(0) {
            let watched = Watched {
                exit: UncheckedExit {
                    position,
                    argv: step.argv,
                    termination: outcome.termination,
                    output: outcome.output,
                    cached: outcome.cached,
                },
                checked: Arc::clone(&result.checked),
            };
            cells::update(&state.watched, |steps| steps.push(watched));
        }
        Ok(eval.heap().alloc(result))
    }
}

/// The output of a `run()` result in the shape of its capture mode, decoded as
/// UTF-8 with invalid bytes replaced.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
enum ResultOutput {
    /// With `capture = "split"`: standard output and standard error apart.
    Split {
        /// Standard output.
        stdout: String,
        /// Standard error.
        stderr: String,
    },
    /// With `capture = "merged"`: both streams in the order they were written.
    Merged {
        /// Both streams.
        output: String,
    },
}

/// The value `run()` returns, of the Starlark type `run_result`: the attributes
/// `rc`, `signal`, `stdout` and `stderr` or `output` for the capture mode, and
/// `cached`, with the values a struct of them had, so `dir()`, `hasattr` and
/// `getattr` work as for that struct.
///
/// The value notes when the program checks it: reads `rc` or `signal`, as an
/// attribute or with `getattr`, or encodes the value with `json.encode` or
/// `fs.write_json`, also inside an encoded list or dict ("Exit codes and
/// failure"); `hasattr`, `dir()`, printing the value, and reading the output or
/// `cached` do not.
#[derive(Debug, ProvidesStaticType, Allocative)]
struct RunResult {
    /// The exit code, or `None` when a signal killed the command.
    rc: Option<i32>,
    /// The signal that killed the command, or `None` when it exited.
    signal: Option<i32>,
    /// The captured output.
    output: ResultOutput,
    /// Whether the result came from the cache.
    cached: bool,
    /// Set once the program reads `rc` or `signal` or encodes the value; shared with
    /// the runtime's [`Watched`] entry of the step, when it has one.
    checked: Arc<AtomicBool>,
}

impl RunResult {
    /// The result of a step that ended as `outcome`, not checked yet.
    fn new(outcome: &StepOutcome) -> Self {
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        Self {
            rc: outcome.termination.code(),
            signal: outcome.termination.signal(),
            output: match &outcome.output {
                CapturedOutput::Split { stdout, stderr } => {
                    ResultOutput::Split {
                        stdout: text(stdout),
                        stderr: text(stderr),
                    }
                },
                CapturedOutput::Merged { output } => {
                    ResultOutput::Merged {
                        output: text(output),
                    }
                },
            },
            cached: outcome.cached,
            checked: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The names of the attributes, in the order the struct had them:
    /// [`SPLIT_RESULT_ATTRIBUTES`] or [`MERGED_RESULT_ATTRIBUTES`], for the capture
    /// mode.
    const fn attributes(&self) -> &'static [&'static str] {
        match self.output {
            ResultOutput::Split { .. } => SPLIT_RESULT_ATTRIBUTES,
            ResultOutput::Merged { .. } => MERGED_RESULT_ATTRIBUTES,
        }
    }

    /// The value of the attribute `name`, or `None` for a name that
    /// [`Self::attributes`] lacks: the constants decide which attributes a result
    /// has, for `get_attr` and for the JSON encoding alike. Reading `rc` or `signal`
    /// checks the result.
    fn attribute(
        &self,
        name: &str,
    ) -> Option<AttributeValue<'_>> {
        if !self.attributes().contains(&name) {
            return None;
        }
        match (name, &self.output) {
            ("rc", _) => {
                self.checked.store(true, Ordering::Relaxed);
                Some(AttributeValue::Int(self.rc))
            },
            ("signal", _) => {
                self.checked.store(true, Ordering::Relaxed);
                Some(AttributeValue::Int(self.signal))
            },
            ("stdout", ResultOutput::Split { stdout, .. }) => Some(AttributeValue::Text(stdout)),
            ("stderr", ResultOutput::Split { stderr, .. }) => Some(AttributeValue::Text(stderr)),
            ("output", ResultOutput::Merged { output }) => Some(AttributeValue::Text(output)),
            ("cached", _) => Some(AttributeValue::Bool(self.cached)),
            _ => None,
        }
    }

    /// Whether `other` has the same exit status, output and `cached`, whether or
    /// not either was checked.
    fn same_values(
        &self,
        other: &Self,
    ) -> bool {
        self.rc == other.rc
            && self.signal == other.signal
            && self.output == other.output
            && self.cached == other.cached
    }
}

impl fmt::Display for RunResult {
    fn fmt(
        &self,
        f: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        /// An optional integer as Starlark shows it.
        fn int(value: Option<i32>) -> String {
            value.map_or_else(|| "None".to_owned(), |value| value.to_string())
        }
        write!(
            f,
            "run_result(rc={}, signal={}, ",
            int(self.rc),
            int(self.signal)
        )?;
        match &self.output {
            ResultOutput::Split { stdout, stderr } => {
                write!(f, "stdout={stdout:?}, stderr={stderr:?}, ")?;
            },
            ResultOutput::Merged { output } => write!(f, "output={output:?}, ")?,
        }
        write!(f, "cached={})", if self.cached { "True" } else { "False" })
    }
}

impl<'v> AllocValue<'v> for RunResult {
    fn alloc_value(
        self,
        heap: Heap<'v>,
    ) -> Value<'v> {
        heap.alloc_simple(self)
    }
}

/// The value of one attribute of a `run()` result, which `get_attr` turns into a
/// Starlark value and the JSON encoding writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttributeValue<'a> {
    /// An integer or `None`, such as `rc` and `signal`.
    Int(Option<i32>),
    /// A string, such as `stdout`.
    Text(&'a str),
    /// A boolean, such as `cached`.
    Bool(bool),
}

/// The JSON encoding of `json.encode` and `fs.write_json`: an object of the
/// attributes of [`RunResult::attributes`], in their order, `rc`, `signal`,
/// `stdout` and `stderr` or `output`, and `cached`, with the same values, as the
/// struct that `run()` returned before encoded. Encoding the value checks it,
/// because its exit status then leaves the program on purpose ("Exit codes and
/// failure"). A listed name without a value is an error, never a panic.
impl serde::Serialize for RunResult {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error as _, SerializeMap as _};

        self.checked.store(true, Ordering::Relaxed);
        let mut map = serializer.serialize_map(Some(self.attributes().len()))?;
        for &name in self.attributes() {
            match self.attribute(name) {
                Some(AttributeValue::Int(value)) => map.serialize_entry(name, &value)?,
                Some(AttributeValue::Text(value)) => map.serialize_entry(name, value)?,
                Some(AttributeValue::Bool(value)) => map.serialize_entry(name, &value)?,
                None => {
                    return Err(S::Error::custom(format!(
                        "the run() result has no value for its attribute {name}"
                    )));
                },
            }
        }
        map.end()
    }
}

/// A `run()` result exists only while its task runs and is never stored as a
/// frozen value, so this storage, which is not JSON, refuses it.
impl StarlarkSerialize for RunResult {
    fn starlark_serialize(
        &self,
        _ctx: &mut dyn StarlarkSerializeContext,
    ) -> starlark::Result<()> {
        Err(starlark::Error::new_other(RunError::NotStored))
    }
}

/// A `run()` result is never stored, so it is never read back from storage either.
impl StarlarkDeserialize for RunResult {
    fn starlark_deserialize(
        _ctx: &mut dyn StarlarkDeserializeContext<'_>
    ) -> starlark::Result<Self> {
        Err(starlark::Error::new_other(RunError::NotStored))
    }
}

#[starlark_value(type = "run_result")]
impl<'v> StarlarkValue<'v> for RunResult {
    fn get_attr(
        &self,
        attribute: &str,
        heap: Heap<'v>,
    ) -> Option<Value<'v>> {
        self.attribute(attribute).map(|value| {
            match value {
                AttributeValue::Int(value) => {
                    value.map_or_else(Value::new_none, |value| heap.alloc(value))
                },
                AttributeValue::Text(value) => heap.alloc(value),
                AttributeValue::Bool(value) => Value::new_bool(value),
            }
        })
    }

    fn has_attr(
        &self,
        attribute: &str,
        _heap: Heap<'v>,
    ) -> bool {
        self.attributes().contains(&attribute)
    }

    fn dir_attr(&self) -> Vec<String> {
        self.attributes()
            .iter()
            .map(|&name| name.to_owned())
            .collect()
    }

    fn equals(
        &self,
        other: Value<'v>,
    ) -> starlark::Result<bool> {
        Ok(other
            .downcast_ref::<Self>()
            .is_some_and(|other| self.same_values(other)))
    }
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
            cells::update(&state.env, |env| {
                for name in names.items {
                    if let Some(value) = state.caller.get(name) {
                        env.insert(name.to_owned(), value.clone());
                    }
                }
            });
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
            let file = cells::update(&state.accesses, |accesses| {
                cells::update(&state.changes, |changes| {
                    cells::update(&state.outcomes, |outcomes| {
                        let mut files = Files::new(&state.task.dir, accesses, changes)
                            .noting(outcomes.as_mut());
                        if required {
                            require_dotenv(&mut files, call, path).map(Some)
                        } else {
                            read_dotenv(&mut files, call, path)
                        }
                    })
                })
            })?;
            vars.extend(file.unwrap_or_default());
            cells::update(&state.env, |env| env.extend(vars.clone()));
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
            cells::update(&state.env, |env| {
                env.insert(name.to_owned(), value.to_owned());
            });
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
            cells::update(&state.env, |env| {
                env.extend(
                    values
                        .entries
                        .iter()
                        .map(|&(name, value)| (name.to_owned(), value.to_owned())),
                );
            });
            Ok(NoneType)
        })
    }

    /// Remove the variable `name` from the task environment, if it is set.
    fn unset(
        #[starlark(require = pos)] name: &str,
        eval: &mut Evaluator<'_, '_, '_>,
    ) -> starlark::Result<NoneType> {
        with_task(eval, "env.unset()", |state| {
            cells::update(&state.env, |env| {
                env.remove(name);
            });
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
            Ok(cells::update(&state.env, |env| env.get(name).cloned()))
        })?;
        Ok(value.map_or_else(
            || default.unwrap_or_else(Value::new_none),
            |value| eval.heap().alloc(value),
        ))
    }

    /// The task environment as a new dict.
    fn dict<'v>(eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        let env = with_task(eval, "env.dict()", |state| Ok(cells::cloned(&state.env)))?;
        Ok(eval.heap().alloc(env))
    }

    /// Empty the task environment.
    fn clear(eval: &mut Evaluator<'_, '_, '_>) -> starlark::Result<NoneType> {
        with_task(eval, "env.clear()", |state| {
            cells::update(&state.env, BTreeMap::clear);
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
            // Each key is hashed with `get_hashed`, whose error is returned, where
            // `AllocDict` "panics if a key is not hashable" (its documentation).
            let mut map = SmallMap::new();
            for (key, member) in members {
                let hashed = heap
                    .alloc(key.as_str())
                    .get_hashed()
                    .map_err(|_not_hashed| RunError::JsonKey(key.clone()))?;
                map.insert_hashed(hashed, json_value(heap, member)?);
            }
            heap.alloc(Dict::new(map))
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

/// The name of the value that describes the running task, which every task
/// program's module holds besides the globals.
pub const TASK_VALUE: &str = "task";

/// The members of [`TASK_VALUE`], in the order the value holds them, under which
/// the runtime sets the values that describe the running task.
pub const TASK_MEMBERS: [&str; 8] = [
    "dir", "file", "line", "name", "heading", "targets", "selected", "args",
];

/// The attributes of a `run()` result with `capture = "split"`, in the order the
/// struct that `run()` returned before had them.
pub const SPLIT_RESULT_ATTRIBUTES: &[&str] = &["rc", "signal", "stdout", "stderr", "cached"];

/// The attributes of a `run()` result with `capture = "merged"`, in the order the
/// struct that `run()` returned before had them.
pub const MERGED_RESULT_ATTRIBUTES: &[&str] = &["rc", "signal", "output", "cached"];

/// What a name that task programs use is, which decides how documentation writes
/// it: a function with `()`, such as `run()`, and a module or a value without, such
/// as `env` or `task.args`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NameKind {
    /// A function, such as `run` or `env.inherit`.
    Function,
    /// A module of members, such as `env` or `task`.
    Module,
    /// A value, such as `task.args`.
    Value,
}

/// A name that task programs use beyond Starlark's standard globals.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProgramName {
    /// The name, after its module and a `.` for a member, such as `run`, `env` or
    /// `env.inherit`.
    pub name: String,
    /// What the name is.
    pub kind: NameKind,
}

impl ProgramName {
    /// The name as documentation writes it: `run()` or `env.inherit()` for a
    /// function, and `env` or `task.args` for a module or a value.
    #[must_use]
    pub fn written(&self) -> String {
        match self.kind {
            NameKind::Function => format!("{}()", self.name),
            NameKind::Module | NameKind::Value => self.name.clone(),
        }
    }
}

/// Everything that task programs use beyond Starlark's standard globals, read from
/// the runtime itself, so that the documentation can be checked against it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProgramNames {
    /// The globals that kizz adds to Starlark's standard ones, [`TASK_VALUE`]
    /// included, sorted.
    pub globals: Vec<ProgramName>,
    /// The members of the modules among the globals, `task`'s included, sorted.
    pub members: Vec<ProgramName>,
    /// The parameters of `run()`, in the order of its signature, the positional
    /// arguments first.
    pub run_parameters: Vec<String>,
    /// The attributes of a `run()` result in either capture mode, each once.
    pub result_attributes: Vec<String>,
}

/// What `item`, the documentation of a global or a member, says the name is.
const fn name_kind(item: &DocItem) -> NameKind {
    match item {
        DocItem::Module(_) => NameKind::Module,
        DocItem::Member(DocMember::Function(_)) => NameKind::Function,
        DocItem::Type(_) | DocItem::Member(DocMember::Property(_)) => NameKind::Value,
    }
}

/// The names of the parameters of `function`, in the order of its signature.
fn parameter_names(function: &DocFunction) -> Vec<String> {
    let params = &function.params;
    params
        .pos_only
        .iter()
        .chain(&params.pos_or_named)
        .chain(&params.args)
        .chain(&params.named_only)
        .chain(&params.kwargs)
        .map(|param| param.name.clone())
        .collect()
}

/// Everything that task programs use beyond Starlark's standard globals: the
/// globals of task programs that the standard ones lack, with the members of each
/// module, `run()`'s parameters from its signature, `task` with [`TASK_MEMBERS`],
/// and the result's attributes from [`SPLIT_RESULT_ATTRIBUTES`] and
/// [`MERGED_RESULT_ATTRIBUTES`].
#[must_use]
pub fn program_names() -> ProgramNames {
    let standard = GlobalsBuilder::standard().build();
    let standard: Vec<&str> = standard.iter().map(|(name, _value)| name).collect();
    let mut names = ProgramNames::default();
    for (name, item) in globals().documentation().members {
        if standard.contains(&name.as_str()) {
            continue;
        }
        if let DocItem::Module(module) = &item {
            for (member, member_item) in &module.members {
                names.members.push(ProgramName {
                    name: format!("{name}.{member}"),
                    kind: name_kind(member_item),
                });
            }
        }
        if let DocItem::Member(DocMember::Function(function)) = &item
            && name == "run"
        {
            names.run_parameters = parameter_names(function);
        }
        names.globals.push(ProgramName {
            kind: name_kind(&item),
            name,
        });
    }
    names.globals.push(ProgramName {
        name: TASK_VALUE.to_owned(),
        kind: NameKind::Module,
    });
    names.members.extend(TASK_MEMBERS.iter().map(|member| {
        ProgramName {
            name: format!("{TASK_VALUE}.{member}"),
            kind: NameKind::Value,
        }
    }));
    names.globals.sort();
    names.members.sort();
    for attribute in SPLIT_RESULT_ATTRIBUTES
        .iter()
        .chain(MERGED_RESULT_ATTRIBUTES)
    {
        if !names
            .result_attributes
            .iter()
            .any(|known| known == attribute)
        {
            names.result_attributes.push((*attribute).to_owned());
        }
    }
    names
}

/// Set [`TASK_VALUE`] in `module` to the values that describe `task`, under the
/// names of [`TASK_MEMBERS`]: `task.name` is the name that its `name=` option
/// gives, or `None`, `task.heading` its heading, or `None`, and `task.args` its
/// arguments.
fn set_task_value(
    module: &Module<'_>,
    task: &TaskInfo,
) {
    let heap = module.heap();
    let optional = |text: Option<&str>| text.map_or_else(Value::new_none, |text| heap.alloc(text));
    let values = [
        heap.alloc(task.dir.to_string()),
        heap.alloc(task.file.to_string()),
        heap.alloc(task.line),
        optional(task.name.as_deref()),
        optional(task.heading.as_deref()),
        heap.alloc(task.targets.clone()),
        heap.alloc(task.selected.clone()),
        heap.alloc(task.args.clone()),
    ];
    let value = heap.alloc(AllocStruct(TASK_MEMBERS.into_iter().zip(values)));
    module.set(TASK_VALUE, value);
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
    /// How each module read and each file access of the modules' top-level code
    /// ended, when the run notes it, and otherwise `None`.
    outcomes: Option<AccessOutcomes>,
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
                self.record(path, AccessKind::Read, None);
                source
            },
            Err(error) => {
                let kind = if error.kind() == ErrorKind::NotFound {
                    AccessKind::Probe
                } else {
                    AccessKind::Read
                };
                self.record(path, kind, Some(&error));
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
            self.outcomes.as_mut(),
        )?;
        self.sources.push((path.clone(), source));
        self.loaded.insert(path.clone(), module.clone());
        Ok(module)
    }

    /// Record that loading touched the module at `path` in the way `kind` names,
    /// and note that the read failed with `error`, or succeeded when it is `None`,
    /// when the run notes outcomes.
    fn record(
        &mut self,
        path: &AbsolutePath,
        kind: AccessKind,
        error: Option<&io::Error>,
    ) {
        if let Some(outcomes) = &mut self.outcomes {
            outcomes.note(path.clone(), kind, error);
        }
        self.accesses.record(path.clone(), kind);
    }
}

/// Evaluate and freeze one module, outside any task, recording the file accesses
/// of its top-level code in `accesses`, the files it changed in `changes`, and,
/// when `outcomes` is `Some`, how each access ended in it.
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
    outcomes: Option<&mut AccessOutcomes>,
) -> Result<FrozenModule, TaskFailure> {
    let marker = LoadingModule {
        path: path.clone(),
        dir: task.dir.clone(),
        accesses: Cell::new(Accesses::new()),
        changes: Cell::new(FileChanges::new()),
        outcomes: Cell::new(outcomes.is_some().then(AccessOutcomes::new)),
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
    if let (Some(outcomes), Some(noted)) = (outcomes, marker.outcomes.into_inner()) {
        outcomes.merge(noted);
    }
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

/// The result of [`TaskProgram::load`] and [`TaskProgram::load_noting`].
#[derive(Debug)]
pub struct ProgramLoad {
    /// Every module the task read and every module path it found missing, with the
    /// file accesses of the modules' top-level code.
    pub accesses: Accesses,
    /// Every file the modules' top-level code changed through `fs.write` and
    /// `fs.write_json`, in the order it first changed each.
    pub changes: FileChanges,
    /// How each of those accesses ended, when the load noted it, and otherwise
    /// `None`.
    pub outcomes: Option<AccessOutcomes>,
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
    /// Whether its run notes how each file access ended, as the load did.
    noting: bool,
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

/// `source` after `lines` blank lines, so Starlark's line numbers are those of
/// the `kizz.md`.
///
/// The space is reserved first with `String::try_reserve_exact`, which returns an
/// error where `str::repeat` panics on a length that overflows, so the pushes that
/// follow fit.
///
/// # Errors
///
/// A [`TaskFailure::Program`] when the padded source cannot be allocated.
fn padded_source(
    lines: usize,
    source: &str,
) -> Result<String, TaskFailure> {
    let mut padded = String::new();
    padded
        .try_reserve_exact(lines.saturating_add(source.len()))
        .map_err(|error| {
            TaskFailure::program(format_args!(
                "cannot place the task's source after {lines} blank lines: {error}"
            ))
        })?;
    for _ in 0..lines {
        padded.push('\n');
    }
    padded.push_str(source);
    Ok(padded)
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
        Self::load_noting(task, print, false)
    }

    /// Load the task program as [`TaskProgram::load`] does; when `noting` is set,
    /// also note how each module read and each file access ended, in
    /// [`ProgramLoad::outcomes`] and then in [`TaskRun::outcomes`] of its run, for
    /// the observation report of `kizz run --output`.
    #[must_use]
    pub fn load_noting(
        task: TaskInfo,
        print: &dyn PrintSink,
        noting: bool,
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
            outcomes: noting.then(AccessOutcomes::new),
            loaded: HashMap::new(),
            loading: Vec::new(),
            sources: Vec::new(),
        };
        let file_name = task.relative_file.display().to_string();
        // Blank lines before the source make Starlark's line numbers those of the
        // `kizz.md`.
        let parsed = padded_source(task.line, &task.source)
            .and_then(|padded| {
                AstModule::parse(&file_name, padded, &Dialect::Extended)
                    .map_err(TaskFailure::program)
            })
            .and_then(|ast| {
                let imports = loader.load_imports(&ast, &task.dir, &file_name)?;
                Ok((ast, imports))
            });
        let ModuleLoader {
            accesses,
            changes,
            outcomes,
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
                noting,
            }
        });
        ProgramLoad {
            accesses,
            changes,
            outcomes,
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
    /// whether or not it failed, and every step it left unchecked: when the
    /// program ends, whether it succeeded or failed, each step that exited with a
    /// non-zero code or was killed by a signal, whose `run()` had no `expect`, and
    /// whose result's `rc` and `signal` the program never read.
    #[must_use]
    pub fn run(
        self,
        caller: &BTreeMap<String, String>,
        executor: &dyn StepExecutor,
        print: &dyn PrintSink,
    ) -> TaskRun {
        let Self {
            task,
            ast,
            imports,
            noting,
            ..
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
            env: Cell::new(BTreeMap::new()),
            accesses: Cell::new(Accesses::new()),
            changes: Cell::new(FileChanges::new()),
            outcomes: Cell::new(noting.then(AccessOutcomes::new)),
            unexpected: Cell::new(None),
            started: Cell::new(0),
            watched: Cell::new(Vec::new()),
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
        let unchecked = state
            .watched
            .take()
            .into_iter()
            .filter(|watched| !watched.checked.load(Ordering::Relaxed))
            .map(|watched| watched.exit)
            .collect();
        TaskRun {
            accesses: state.accesses.into_inner(),
            changes: state.changes.into_inner(),
            outcomes: state.outcomes.into_inner(),
            result,
            unchecked,
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
    /// How each of those accesses ended, when the program was loaded with
    /// [`TaskProgram::load_noting`], and otherwise `None`.
    pub outcomes: Option<AccessOutcomes>,
    /// Whether the program succeeded, or why it failed. Unchecked steps never fail
    /// the run here; `kizz build --fail-unchecked` turns them into a failure.
    pub result: Result<(), TaskFailure>,
    /// Every step the program left unchecked, in the order they ran, with its
    /// command, how it ended, and its output; results from the cache included.
    pub unchecked: Vec<UncheckedExit>,
}

/// Unit tests for the runtime's own logic.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert, borrow cells and lock"
)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap, path::PathBuf};

    use kizz_trace::{AbsolutePath, CapturedOutput, Termination};

    use super::{PrintSink, TaskFailure, TaskInfo, TaskProgram, UnexpectedExit, padded_source};
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
            heading: None,
            targets: vec!["ci".to_owned()],
            selected: vec!["ci".to_owned()],
            args: Vec::new(),
            cache_never: false,
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

    /// A task's source after blank lines whose total length cannot be reserved is a
    /// program failure, where `str::repeat` would panic: `usize::MAX` lines saturate
    /// the length, which overflows the largest length a `String` can reserve, so the
    /// reservation fails at once, without allocating; a length that fits is built in
    /// full.
    #[test]
    fn padding_that_cannot_be_reserved_is_a_program_failure() {
        let Err(TaskFailure::Program { message }) = padded_source(usize::MAX, "run(\"a\")\n")
        else {
            panic!("the padding was reserved");
        };
        assert!(
            message.starts_with(&format!(
                "cannot place the task's source after {} blank lines: ",
                usize::MAX
            )),
            "{message}"
        );
        assert_eq!(padded_source(2, "x").unwrap(), "\n\nx");
        assert_eq!(padded_source(0, "x").unwrap(), "x");
    }
}

/// Unit tests of `expand_arguments`, the expansion of `run()`'s positional
/// arguments, on Starlark values that small programs build, or that a test
/// allocates directly: each form of the expansion, the errors with their exact
/// messages, the cycle check, the bound, and a nesting deep enough that a recursive
/// walk would overflow the stack.
#[cfg(test)]
#[allow(clippy::disallowed_macros, reason = "test code: the tests assert")]
mod argument_tests {
    use starlark::{
        environment::{Globals, Module},
        eval::Evaluator,
        syntax::{AstModule, Dialect},
        values::{Heap, tuple::TupleRef},
    };

    use super::{MAX_ARGUMENT_VALUES, expand_arguments};

    /// The expansion of the positional arguments that `source` gives, a Starlark
    /// program whose last statement is the tuple of the arguments, or the message of
    /// the expansion's error.
    fn expand(source: &str) -> Result<Vec<String>, String> {
        Module::with_temp_heap(|module| {
            let ast =
                AstModule::parse("arguments.star", source.to_owned(), &Dialect::Extended).unwrap();
            let globals = Globals::standard();
            let mut eval = Evaluator::new(&module);
            let value = eval.eval_module(ast, &globals).unwrap();
            let arguments = TupleRef::from_value(value).unwrap();
            expand_arguments(arguments.content()).map_err(|error| error.to_string())
        })
    }

    /// `items` as owned strings.
    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|&item| item.to_owned()).collect()
    }

    /// Lists and tuples expand in place, in order and at any depth, wherever they
    /// stand: first, last, in the middle, nested, or as the whole command; empty ones
    /// add nothing; each string, one with spaces and an empty one included, stays one
    /// argument; and the same list used twice, side by side, adds its strings twice.
    #[test]
    fn lists_and_tuples_expand_in_place_and_strings_stay_whole() {
        let this_is_test = strings(&["echo", "this", "is", "test"]);
        for source in [
            "(\"echo\", \"this\", \"is\", \"test\")",
            "(\"echo\", [\"this\", \"is\", \"test\"])",
            "([\"echo\", \"this\"], \"is\", \"test\")",
            "(\"echo\", [\"this\"], \"is\", (\"test\",))",
            "(\"echo\", [[\"this\"], (\"is\", [])], \"test\")",
            "((\"echo\",), [[\"this\"], (\"is\", [])], \"test\")",
            "([\"echo\", \"this\", \"is\", \"test\"],)",
            "(((\"echo\", [[[\"this\"]]]), \"is\"), [], (), [[]], \"test\")",
        ] {
            assert_eq!(expand(source), Ok(this_is_test.clone()), "{source}");
        }
        assert_eq!(
            expand("(\"printf\", [\"%s|\", \"a b\"], \"\", [\"\"])"),
            Ok(strings(&["printf", "%s|", "a b", "", ""]))
        );
        assert_eq!(
            expand("opt = [\"-v\"]\n(\"tool\", opt, \"x\", [opt, opt])"),
            Ok(strings(&["tool", "-v", "x", "-v", "-v"]))
        );
        assert_eq!(
            expand("flags = []\n(\"tool\", flags, \"x\")"),
            Ok(strings(&["tool", "x"]))
        );
    }

    /// Arguments of nothing but empty lists and tuples, and no arguments at all,
    /// expand to no string, which `run()` then refuses as a call without a program.
    #[test]
    fn empty_lists_and_tuples_expand_to_nothing() {
        for source in ["()", "([],)", "([], ())", "([[]],)", "(([(), [[]]],),)"] {
            assert_eq!(expand(source), Ok(Vec::new()), "{source}");
        }
    }

    /// A value that is neither a string nor a list or tuple fails with its argument,
    /// its index path within the lists, and its type, as Starlark's `type()` names
    /// it, and the first such value in the order of the walk is the one named.
    #[test]
    fn other_values_fail_with_their_place_and_type() {
        for (source, message) in [
            (
                "(\"p\", \"a\", 5)",
                "run(): argument 3 is of type int, not a string, list or tuple",
            ),
            (
                "(\"p\", True)",
                "run(): argument 2 is of type bool, not a string, list or tuple",
            ),
            (
                "(\"p\", [\"a\", [None]])",
                "run(): argument 2 at [1][0] is of type NoneType, not a string, list or tuple",
            ),
            (
                "([{\"a\": 1}],)",
                "run(): argument 1 at [0] is of type dict, not a string, list or tuple",
            ),
            (
                "(\"p\", (\"a\", range(3)))",
                "run(): argument 2 at [1] is of type range, not a string, list or tuple",
            ),
            (
                "([], 5)",
                "run(): argument 2 is of type int, not a string, list or tuple",
            ),
            (
                "a = [\"x\"]\na.append(a)\n(\"p\", [1], a)",
                "run(): argument 2 at [0] is of type int, not a string, list or tuple",
            ),
        ] {
            assert_eq!(expand(source), Err(message.to_owned()), "{source}");
        }
    }

    /// A list or tuple that contains itself, directly, through another list, or
    /// through a list inside a tuple, fails where it is met again, with its own
    /// type, and the walk ends.
    #[test]
    fn a_list_or_tuple_that_contains_itself_fails() {
        for (source, message) in [
            (
                "a = [\"x\"]\na.append(a)\n(\"p\", a)",
                "run(): argument 2 at [1] is a list that contains itself",
            ),
            (
                "a = []\nb = [a]\na.append(b)\n(\"p\", a)",
                "run(): argument 2 at [0][0] is a list that contains itself",
            ),
            (
                "l = []\nt = (\"a\", l)\nl.append(t)\n(\"p\", t)",
                "run(): argument 2 at [1][0] is a tuple that contains itself",
            ),
        ] {
            assert_eq!(expand(source), Err(message.to_owned()), "{source}");
        }
    }

    /// The expansion visits at most [`MAX_ARGUMENT_VALUES`] values: a list of
    /// 999,999 strings is 1,000,000 values and expands, one more string fails, and so
    /// do twenty-one doublings of a shared list, which would expand to 2,097,152
    /// strings, before the walk goes further.
    #[test]
    fn the_expansion_stops_past_a_million_values() {
        let limit = "run(): the arguments expand to more than 1000000 strings, lists and tuples";
        assert_eq!(MAX_ARGUMENT_VALUES, 1_000_000);
        assert_eq!(
            expand("([\"x\"] * 999999,)").as_ref().map(Vec::len),
            Ok(999_999)
        );
        assert_eq!(expand("([\"x\"] * 1000000,)"), Err(limit.to_owned()));
        assert_eq!(
            expand("a = [\"x\"]\nfor _ in range(21):\n    a = [a, a]\n(a,)"),
            Err(limit.to_owned())
        );
    }

    /// A string inside 100,000 nested lists, passed inside a tuple, expands to that
    /// string, because the walk keeps its own stack rather than recursing. The lists
    /// are allocated directly on a heap, where no garbage collection runs: Starlark's
    /// collector, which the evaluator runs between a module's top-level statements,
    /// copies nested values recursively, so building this nesting in a program can
    /// overflow the stack in the collector, before any `run()` call.
    #[test]
    fn a_deep_nesting_expands_without_recursion() {
        Heap::temp(|heap| {
            let mut nested = heap.alloc("deep");
            for _ in 0..100_000 {
                nested = heap.alloc(vec![nested]);
            }
            let arguments = [heap.alloc("p"), heap.alloc((nested,))];
            assert_eq!(
                expand_arguments(&arguments).map_err(|error| error.to_string()),
                Ok(strings(&["p", "deep"]))
            );
        });
    }
}

/// Unit tests of `program_names`, which the documentation coverage test reads, and
/// of the result-attribute constants that it lists.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and borrow cells"
)]
mod names_tests {
    use std::{cell::RefCell, collections::BTreeMap, path::PathBuf};

    use kizz_trace::{AbsolutePath, CapturedOutput, Termination};
    use starlark::values::{Heap, StarlarkValue as _};

    use super::{
        MERGED_RESULT_ATTRIBUTES, PrintSink, ProgramName, ProgramNames, RunResult,
        SPLIT_RESULT_ATTRIBUTES, TaskInfo, TaskProgram, program_names,
    };
    use crate::step::{StepError, StepExecutor, StepOutcome, StepSpec};

    /// Answers every step with exit status 0 and no output.
    struct Succeed;

    impl StepExecutor for Succeed {
        fn execute(
            &self,
            step: &StepSpec,
        ) -> Result<StepOutcome, StepError> {
            Ok(StepOutcome {
                termination: Termination::Exited(0),
                output: CapturedOutput::empty(step.capture),
                cached: false,
            })
        }
    }

    /// Keeps the lines that programs print.
    #[derive(Default)]
    struct Lines {
        /// The printed lines, in order, each with its prefix.
        lines: RefCell<Vec<String>>,
    }

    impl PrintSink for Lines {
        fn print_line(
            &self,
            line: &str,
        ) {
            self.lines.borrow_mut().push(line.to_owned());
        }
    }

    /// The members of `module` among `names`, without the module's name, sorted.
    fn members_of(
        names: &ProgramNames,
        module: &str,
    ) -> Vec<String> {
        let prefix = format!("{module}.");
        let mut members: Vec<String> = names
            .members
            .iter()
            .filter_map(|name| name.name.strip_prefix(&prefix).map(str::to_owned))
            .collect();
        members.sort();
        members
    }

    /// `names` as owned strings, sorted.
    fn sorted(names: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = names.iter().map(|&name| name.to_owned()).collect();
        names.sort();
        names
    }

    /// What a task program in `/work` sees, as `dir()` lists it, of `env`, `fs`,
    /// `json`, `task`, and a `run()` result in each capture mode, each sorted.
    fn seen_by_a_program() -> Vec<Vec<String>> {
        let task = TaskInfo {
            dir: AbsolutePath::new("/work").unwrap(),
            file: AbsolutePath::new("/work/kizz.md").unwrap(),
            relative_file: PathBuf::from("kizz.md"),
            line: 1,
            index: 0,
            name: None,
            heading: None,
            targets: vec!["ci".to_owned()],
            selected: vec!["ci".to_owned()],
            args: Vec::new(),
            cache_never: false,
            source: "print(json.encode([dir(env), dir(fs), dir(json), dir(task), \
                     dir(run(\"a\")), dir(run(\"a\", capture = \"merged\"))]))\n"
                .to_owned(),
        };
        let lines = Lines::default();
        let program = TaskProgram::load(task, &lines).program.unwrap();
        program
            .run(&BTreeMap::new(), &Succeed, &lines)
            .result
            .unwrap();
        let printed = lines.lines.borrow();
        let [line] = printed.as_slice() else {
            panic!("the program printed {printed:?}");
        };
        let (_task, text) = line.split_once(": ").unwrap();
        let seen: Vec<Vec<String>> = serde_json::from_str(text).unwrap();
        seen.into_iter()
            .map(|mut names| {
                names.sort();
                names
            })
            .collect()
    }

    /// The members, result attributes and `task` members that `program_names` lists
    /// are those that a task program sees with `dir()`; the globals are those that
    /// kizz adds to Starlark's standard ones, `task` included, and `run()`'s
    /// parameters come from its signature, the positional arguments first.
    #[test]
    fn program_names_are_what_programs_see() {
        let names = program_names();
        assert_eq!(seen_by_a_program(), [
            members_of(&names, "env"),
            members_of(&names, "fs"),
            members_of(&names, "json"),
            members_of(&names, "task"),
            sorted(SPLIT_RESULT_ATTRIBUTES),
            sorted(MERGED_RESULT_ATTRIBUTES),
        ]);
        let globals: Vec<String> = names.globals.iter().map(ProgramName::written).collect();
        assert_eq!(globals, ["env", "fs", "json", "print()", "run()", "task"]);
        let members: Vec<String> = names.members.iter().map(ProgramName::written).collect();
        assert!(members.contains(&"env.inherit()".to_owned()), "{members:?}");
        assert!(members.contains(&"task.args".to_owned()), "{members:?}");
        assert_eq!(names.run_parameters, [
            "argv",
            "stdin",
            "env",
            "cwd",
            "capture",
            "stream",
            "always",
            "untraced",
            "ignore",
            "track",
            "timestamps",
            "machine",
            "expect",
        ]);
        assert_eq!(names.result_attributes, [
            "rc", "signal", "stdout", "stderr", "cached", "output"
        ]);
    }

    /// The keys of a JSON object, in the order its text has them.
    struct Keys {
        /// The keys, in order.
        keys: Vec<String>,
    }

    impl<'de> serde::Deserialize<'de> for Keys {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            /// Collects the keys of an object and skips its values.
            struct KeysVisitor;

            impl<'de> serde::de::Visitor<'de> for KeysVisitor {
                type Value = Keys;

                fn expecting(
                    &self,
                    formatter: &mut std::fmt::Formatter<'_>,
                ) -> std::fmt::Result {
                    formatter.write_str("a JSON object")
                }

                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> Result<Keys, A::Error> {
                    let mut keys = Vec::new();
                    while let Some((key, serde::de::IgnoredAny)) =
                        map.next_entry::<String, serde::de::IgnoredAny>()?
                    {
                        keys.push(key);
                    }
                    Ok(Keys { keys })
                }
            }

            deserializer.deserialize_map(KeysVisitor)
        }
    }

    /// The attribute constants decide which attributes a `run()` result has: in
    /// each capture mode, `get_attr` and `has_attr` answer every name of the mode's
    /// constant and no other, the other mode's names and unknown names included, and
    /// the JSON encoding writes exactly the constant's names, in its order.
    #[test]
    fn the_attribute_constants_decide_what_a_result_has() {
        let modes = [
            (
                CapturedOutput::Split {
                    stdout: b"out".to_vec(),
                    stderr: b"err".to_vec(),
                },
                SPLIT_RESULT_ATTRIBUTES,
            ),
            (
                CapturedOutput::Merged {
                    output: b"both".to_vec(),
                },
                MERGED_RESULT_ATTRIBUTES,
            ),
        ];
        let mut names: Vec<&str> = SPLIT_RESULT_ATTRIBUTES
            .iter()
            .chain(MERGED_RESULT_ATTRIBUTES)
            .copied()
            .collect();
        names.extend(["duration", "checked", "RC", ""]);
        for (output, attributes) in modes {
            let result = RunResult::new(&StepOutcome {
                termination: Termination::Exited(3),
                output,
                cached: true,
            });
            Heap::temp(|heap| {
                for &name in &names {
                    let listed = attributes.contains(&name);
                    assert_eq!(
                        result.get_attr(name, heap).is_some(),
                        listed,
                        "get_attr of {name:?} with {attributes:?}"
                    );
                    assert_eq!(
                        result.has_attr(name, heap),
                        listed,
                        "has_attr of {name:?} with {attributes:?}"
                    );
                }
            });
            let encoded = serde_json::to_string(&result).unwrap();
            let keys: Keys = serde_json::from_str(&encoded).unwrap();
            assert_eq!(keys.keys, attributes, "{encoded}");
        }
    }
}
