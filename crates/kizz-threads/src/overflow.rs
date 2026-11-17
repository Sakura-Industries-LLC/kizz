// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Kizz's handler of stack overflows ("Kizz never panics").
//!
//! Kizz does not prevent a stack overflow, such as one in Starlark's garbage
//! collector when a task program nests values too deeply. It catches the overflow
//! instead and stops in a controlled way, where Rust's runtime would abort the
//! process.
//!
//! * [`install`] sets up the calling thread, the main thread, and installs the handler
//!   for `SIGSEGV` and `SIGBUS`, with `SA_SIGINFO` and `SA_ONSTACK`, after it has kept
//!   the earlier action of each signal. It installs the handler for both signals or for
//!   neither, and its error names the call that failed.
//! * Setting up a thread gives it an alternate signal stack of its own, of 64 KiB plus
//!   the kernel's `AT_MINSIGSTKSZ`, above an inaccessible guard page, and records the
//!   guard region of its stack in a thread-local value of plain integers. Once the
//!   handler is installed, [`crate::spawn`] and [`crate::spawn_scoped`] set up each
//!   thread at the start of its work, and restore its earlier alternate stack and unmap
//!   kizz's one when the work ends; the main thread keeps its own. A thread whose
//!   alternate stack or guard region cannot be set up runs without them, silently.
//! * [`note_task`] and [`note_lock`] keep the location of the task whose program kizz
//!   evaluates and the path of the lock file that it holds in static buffers of atomic
//!   bytes, guarded by a sequence counter, until the [`Note`] is dropped.
//!
//! The handler acts only on a signal that the kernel raised for a fault
//! (`si_code` above 0), in the process that installed it, at an address in the
//! guard region of the faulting thread. It then writes a fixed error to standard
//! error, naming the noted task when there is one, deletes the noted lock file, and
//! exits with the status that [`install`] was given. It makes only calls that are
//! safe in a signal handler (signal-safety(7)): `write`, `unlink`, `_exit`,
//! `sigaction` and `getpid`. It allocates nothing, takes no lock, starts nothing and
//! never returns into the overflowed code.
//!
//! Any other fault goes to the action that was there before: the handler restores
//! it with `sigaction`, keeping `errno`, and returns, so the fault repeats under
//! `std`'s handler, which reports its own overflow or restores the default action. A
//! real crash is never swallowed.
//!
//! In builds with `debug_assertions`, [`fault_outside_guards`] makes the calling
//! thread fault outside every guard region, for the `kizz` binary's end-to-end test.

use std::{
    cell::Cell,
    ffi::c_void,
    fmt, io,
    os::unix::ffi::OsStrExt as _,
    path::Path,
    ptr,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU64, AtomicUsize, Ordering, fence},
    },
};

/// The size of kizz's alternate signal stack before the kernel's
/// `AT_MINSIGSTKSZ` is added: room for the handler's buffers, many times over.
const ALT_STACK_BASE: usize = 65_536;

/// The most bytes of a task's location that a note keeps; a longer text is cut at
/// a character boundary.
const TASK_CAPACITY: usize = 1_024;

/// The most bytes of a lock file's path that a note keeps; with the closing zero
/// byte the path fills [`PATH_BUFFER`] bytes at most, and a longer path is not
/// noted.
const LOCK_CAPACITY: usize = 4_095;

/// The size of the handler's copy of the lock file's path, the closing zero byte
/// included.
const PATH_BUFFER: usize = 4_096;

/// The size of the handler's message buffer: the longest message, with a task
/// location of [`TASK_CAPACITY`] bytes, fits in it.
const MESSAGE_CAPACITY: usize = 1_280;

/// How many calls of `write` the handler makes at most for its message, retrying
/// a short or interrupted write.
const WRITE_ATTEMPTS: u32 = 8;

/// What the message starts with when a task is noted.
const TASK_PREFIX: &[u8] = b"error: the program of task ";

/// What the message ends with when a task is noted.
const TASK_SUFFIX: &[u8] =
    b" overflowed the stack, so kizz stopped; its values are probably nested too deeply\n";

/// The message when no task is noted.
const NO_TASK: &[u8] = b"error: kizz overflowed its stack and stopped\n";

/// The location of the task whose program kizz evaluates, while it is noted.
static TASK: Slot<TASK_CAPACITY> = Slot::new();

/// The path of the lock file that kizz holds, while it is noted.
static LOCK: Slot<LOCK_CAPACITY> = Slot::new();

/// The identity of the next note; 0 is no note.
static NEXT_NOTE: AtomicU64 = AtomicU64::new(1);

/// Whether the handler is installed, so that new threads are set up.
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// The process that installed the handler, which alone it acts in.
static INSTALLER: AtomicI32 = AtomicI32::new(0);

/// The status that the handler exits with.
static STATUS: AtomicI32 = AtomicI32::new(2);

/// The action of `SIGSEGV` before the handler, written before the handler is
/// installed.
static EARLIER_SEGV: OnceLock<libc::sigaction> = OnceLock::new();

/// The action of `SIGBUS` before the handler, written before the handler is
/// installed.
static EARLIER_BUS: OnceLock<libc::sigaction> = OnceLock::new();

thread_local! {
    /// The guard region of the calling thread's stack, or [`Region::NONE`] when the
    /// thread is not set up: plain integers with a `const` initializer and no
    /// destructor, which the handler reads without allocation, lazy initialization
    /// or a panic.
    static REGION: Cell<Region> = const { Cell::new(Region::NONE) };
}

/// A range of addresses, from `start` up to but not including `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Region {
    /// The first address of the region.
    start: usize,
    /// The first address after the region.
    end: usize,
}

impl Region {
    /// No region: it contains no address.
    const NONE: Self = Self { start: 0, end: 0 };

    /// Whether `address` lies in the region.
    const fn contains(
        self,
        address: usize,
    ) -> bool {
        self.start <= address && address < self.end
    }
}

/// The guard region of a stack whose lowest address is `lowest`, with a guard of
/// `guard_size` bytes and pages of `page` bytes: from `lowest` minus the guard to
/// `lowest` plus the guard, where the guard is the larger of `guard_size` and
/// `page`, because the system may report the guard below or above `lowest`.
/// `None` when the guard is empty or the arithmetic would overflow.
fn guard_region(
    lowest: usize,
    guard_size: usize,
    page: usize,
) -> Option<Region> {
    let guard = guard_size.max(page);
    if guard == 0 {
        return None;
    }
    Some(Region {
        start: lowest.checked_sub(guard)?,
        end: lowest.checked_add(guard)?,
    })
}

/// The size of an alternate signal stack, [`ALT_STACK_BASE`] plus `minimum`, the
/// kernel's `AT_MINSIGSTKSZ`, rounded up to whole pages of `page` bytes; `None`
/// when `page` is 0 or the arithmetic would overflow.
fn alt_stack_size(
    page: usize,
    minimum: usize,
) -> Option<usize> {
    ALT_STACK_BASE
        .checked_add(minimum)?
        .checked_next_multiple_of(page)
}

/// The longest start of `text` that holds at most `capacity` bytes and ends at a
/// character boundary.
fn cut(
    text: &str,
    capacity: usize,
) -> &str {
    (0..=capacity.min(text.len()))
        .rev()
        .find(|&end| text.is_char_boundary(end))
        .and_then(|end| text.get(..end))
        .unwrap_or_default()
}

/// Write the handler's message into `out` and return its length: the message that
/// names `task` when it is a noted location, and the message without a task
/// otherwise. A message longer than `out` is cut, which no noted task causes in a
/// buffer of [`MESSAGE_CAPACITY`] bytes.
fn compose(
    task: Option<&[u8]>,
    out: &mut [u8],
) -> usize {
    let parts: [&[u8]; 3] = match task {
        Some(task) if !task.is_empty() => [TASK_PREFIX, task, TASK_SUFFIX],
        _ => [NO_TASK, &[], &[]],
    };
    let mut length: usize = 0;
    for (slot, byte) in out
        .iter_mut()
        .zip(parts.iter().flat_map(|part| part.iter()))
    {
        *slot = *byte;
        length = length.saturating_add(1);
    }
    length
}

/// A static buffer that holds one note of at most `N` bytes, written by kizz's
/// threads and read by the handler without a lock.
///
/// A writer makes the sequence counter odd, changes the bytes, the length and the
/// owner, and makes it even again; writers wait for each other on the counter. A
/// reader copies the note and uses the copy only when the counter was even and
/// unchanged across the copy, so a copy that a write overlapped is no note.
struct Slot<const N: usize> {
    /// The sequence counter: odd while a writer changes the note.
    seq: AtomicU64,
    /// The length of the note, 0 when there is none.
    len: AtomicUsize,
    /// The identity of the note's [`Note`], 0 when there is none.
    owner: AtomicU64,
    /// The bytes of the note.
    bytes: [AtomicU8; N],
}

impl<const N: usize> Slot<N> {
    /// A slot that holds no note.
    const fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            len: AtomicUsize::new(0),
            owner: AtomicU64::new(0),
            bytes: [const { AtomicU8::new(0) }; N],
        }
    }

    /// Make the counter odd once no other writer holds it.
    fn begin(&self) {
        loop {
            let seq = self.seq.load(Ordering::Relaxed);
            if (seq & 1) == 0
                && self
                    .seq
                    .compare_exchange_weak(
                        seq,
                        seq.wrapping_add(1),
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
            {
                break;
            }
            std::hint::spin_loop();
        }
        fence(Ordering::Release);
    }

    /// Make the counter even again.
    fn end(&self) {
        self.seq.fetch_add(1, Ordering::Release);
    }

    /// Replace the note with the first `N` bytes of `bytes`, and return the
    /// identity of the new note.
    fn write(
        &self,
        bytes: &[u8],
    ) -> u64 {
        let id = NEXT_NOTE.fetch_add(1, Ordering::Relaxed);
        self.begin();
        let mut length: usize = 0;
        for (slot, byte) in self.bytes.iter().zip(bytes) {
            slot.store(*byte, Ordering::Relaxed);
            length = length.saturating_add(1);
        }
        self.len.store(length, Ordering::Relaxed);
        self.owner.store(id, Ordering::Relaxed);
        self.end();
        id
    }

    /// Remove the note when it is still the one with the identity `id`, so that an
    /// older note never removes a newer one.
    fn clear(
        &self,
        id: u64,
    ) {
        self.begin();
        if self.owner.load(Ordering::Relaxed) == id {
            self.len.store(0, Ordering::Relaxed);
            self.owner.store(0, Ordering::Relaxed);
        }
        self.end();
    }

    /// Copy the note into the start of `out`, calling `between` after the copy and
    /// before the check, and return its length; `None` when there is no note, when
    /// it does not fit in `out`, or when a write overlapped the copy.
    ///
    /// Only atomic loads and the copy into `out` happen here, so the handler can
    /// call it.
    fn read(
        &self,
        out: &mut [u8],
        between: impl FnOnce(),
    ) -> Option<usize> {
        let before = self.seq.load(Ordering::Acquire);
        if (before & 1) != 0 {
            return None;
        }
        let length = self.len.load(Ordering::Relaxed);
        for (slot, byte) in out.iter_mut().zip(&self.bytes).take(length) {
            *slot = byte.load(Ordering::Relaxed);
        }
        between();
        fence(Ordering::Acquire);
        let after = self.seq.load(Ordering::Relaxed);
        (after == before && length > 0 && length <= out.len()).then_some(length)
    }
}

/// A note of [`note_task`] or [`note_lock`], kept until it is dropped.
#[derive(Debug)]
#[must_use = "the note is removed when it is dropped"]
pub struct Note {
    /// The identity of the note, 0 when nothing was noted.
    id: u64,
    /// Removes the note from its slot.
    clear: fn(u64),
}

impl Drop for Note {
    fn drop(&mut self) {
        if self.id != 0 {
            (self.clear)(self.id);
        }
    }
}

/// Remove the task note with the identity `id`.
fn clear_task(id: u64) {
    TASK.clear(id);
}

/// Remove the lock note with the identity `id`.
fn clear_lock(id: u64) {
    LOCK.clear(id);
}

/// Note `text`, the location of the task whose program kizz evaluates, such as
/// `kizz.md:12 (Deep)`, so that the handler names it, until the note is dropped.
/// A text longer than 1,024 bytes is cut at a character boundary.
pub fn note_task(text: &str) -> Note {
    Note {
        id: TASK.write(cut(text, TASK_CAPACITY).as_bytes()),
        clear: clear_task,
    }
}

/// Note `path`, the lock file that kizz holds, so that the handler deletes it,
/// until the note is dropped. A path longer than 4,095 bytes, or one that holds a
/// zero byte, is not noted; the next build then takes the file over.
pub fn note_lock(path: &Path) -> Note {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || bytes.len() > LOCK_CAPACITY || bytes.contains(&0) {
        return Note {
            id: 0,
            clear: clear_lock,
        };
    }
    Note {
        id: LOCK.write(bytes),
        clear: clear_lock,
    }
}

/// Why [`install`] could not install the handler.
#[derive(Debug)]
pub struct InstallError {
    /// The call that failed, such as `sigaltstack`.
    call: &'static str,
    /// The system's error.
    error: io::Error,
}

impl InstallError {
    /// The error of `call`, which set `errno`.
    fn last(call: &'static str) -> Self {
        Self {
            call,
            error: io::Error::last_os_error(),
        }
    }

    /// The error of `call`, which returned the error number `code`.
    fn code(
        call: &'static str,
        code: i32,
    ) -> Self {
        Self {
            call,
            error: io::Error::from_raw_os_error(code),
        }
    }

    /// The error of `call`, which gave a value that cannot be used.
    fn invalid(
        call: &'static str,
        what: &'static str,
    ) -> Self {
        Self {
            call,
            error: io::Error::new(io::ErrorKind::InvalidData, what),
        }
    }
}

impl fmt::Display for InstallError {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(formatter, "{} failed: {}", self.call, self.error)
    }
}

// The message holds the system's error, so the error has no source of its own.
impl std::error::Error for InstallError {}

/// The size of a page, from `sysconf`.
fn page_size() -> Result<usize, InstallError> {
    // SAFETY: `sysconf` has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(page)
        .ok()
        .filter(|&page| page > 0)
        .ok_or_else(|| InstallError::invalid("sysconf", "no page size"))
}

/// The guard region of the calling thread's stack, from `pthread_getattr_np`,
/// `pthread_attr_getstack` and `pthread_attr_getguardsize`.
fn stack_region() -> Result<Region, InstallError> {
    let page = page_size()?;
    let mut attributes = std::mem::MaybeUninit::<libc::pthread_attr_t>::uninit();
    // SAFETY: `attributes` is valid for writing, and `pthread_self` names the calling
    // thread, which is alive.
    let status = unsafe { libc::pthread_getattr_np(libc::pthread_self(), attributes.as_mut_ptr()) };
    if status != 0 {
        return Err(InstallError::code("pthread_getattr_np", status));
    }
    let mut lowest: *mut c_void = ptr::null_mut();
    let mut size: usize = 0;
    let mut guard: usize = 0;
    // SAFETY: `pthread_getattr_np` initialized `attributes`, and the other pointers
    // are valid for writing.
    let stack =
        unsafe { libc::pthread_attr_getstack(attributes.as_ptr(), &raw mut lowest, &raw mut size) };
    // SAFETY: as above.
    let guarded = unsafe { libc::pthread_attr_getguardsize(attributes.as_ptr(), &raw mut guard) };
    // SAFETY: `attributes` was initialized by `pthread_getattr_np` and is destroyed
    // once.
    unsafe { libc::pthread_attr_destroy(attributes.as_mut_ptr()) };
    if stack != 0 {
        return Err(InstallError::code("pthread_attr_getstack", stack));
    }
    if guarded != 0 {
        return Err(InstallError::code("pthread_attr_getguardsize", guarded));
    }
    guard_region(lowest.addr(), guard, page)
        .ok_or_else(|| InstallError::invalid("pthread_attr_getstack", "the guard region overflows"))
}

/// Kizz's alternate signal stack of a thread, and the one that it replaced.
struct AltStack {
    /// The start of the mapping, its guard page.
    base: *mut c_void,
    /// The length of the mapping, the guard page included.
    length: usize,
    /// The thread's alternate stack before kizz's.
    earlier: libc::stack_t,
}

impl AltStack {
    /// Map an alternate signal stack above an inaccessible guard page and make it
    /// the calling thread's, keeping the earlier one.
    fn set_up() -> Result<Self, InstallError> {
        let page = page_size()?;
        // SAFETY: `getauxval` has no preconditions; it gives 0 for an unknown entry.
        let minimum = unsafe { libc::getauxval(libc::AT_MINSIGSTKSZ) };
        let size = alt_stack_size(page, usize::try_from(minimum).unwrap_or(0))
            .ok_or_else(|| InstallError::invalid("getauxval", "the signal stack size overflows"))?;
        let length = size
            .checked_add(page)
            .ok_or_else(|| InstallError::invalid("getauxval", "the signal stack size overflows"))?;
        // SAFETY: an anonymous private mapping at an address of the kernel's choosing
        // touches no existing memory.
        let base = unsafe {
            libc::mmap(
                ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_STACK,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(InstallError::last("mmap"));
        }
        // SAFETY: the first page lies in the mapping just made, which nothing else uses.
        if unsafe { libc::mprotect(base, page, libc::PROT_NONE) } != 0 {
            let error = InstallError::last("mprotect");
            // SAFETY: the mapping was made above, and nothing refers to it.
            unsafe { libc::munmap(base, length) };
            return Err(error);
        }
        let stack = libc::stack_t {
            ss_sp: base.cast::<u8>().wrapping_add(page).cast(),
            ss_flags: 0,
            ss_size: size,
        };
        // SAFETY: a zeroed `stack_t` is valid, and `sigaltstack` overwrites it.
        let mut earlier: libc::stack_t = unsafe { std::mem::zeroed() };
        // SAFETY: `stack` describes the accessible part of the mapping, which stays
        // mapped until `restore`, and `earlier` is valid for writing.
        if unsafe { libc::sigaltstack(&raw const stack, &raw mut earlier) } != 0 {
            let error = InstallError::last("sigaltstack");
            // SAFETY: the mapping was made above, and the thread does not use it.
            unsafe { libc::munmap(base, length) };
            return Err(error);
        }
        Ok(Self {
            base,
            length,
            earlier,
        })
    }

    /// Give the calling thread its earlier alternate stack back and unmap kizz's;
    /// when the earlier one cannot be restored, kizz's stays mapped, because the
    /// thread still uses it.
    fn restore(self) {
        // SAFETY: `earlier` is what `sigaltstack` reported for this thread.
        if unsafe { libc::sigaltstack(&raw const self.earlier, ptr::null_mut()) } == 0 {
            // SAFETY: the thread no longer uses the mapping, which nothing else refers
            // to.
            unsafe { libc::munmap(self.base, self.length) };
        }
    }
}

/// What setting up a thread did, undone when the thread's work ends.
pub(crate) struct ThreadGuard {
    /// Kizz's alternate stack of the thread, when it has one.
    stack: Option<AltStack>,
    /// Whether the thread's guard region was recorded.
    region: bool,
}

impl Drop for ThreadGuard {
    fn drop(&mut self) {
        if self.region {
            let _cleared: Result<(), std::thread::AccessError> =
                REGION.try_with(|region| region.set(Region::NONE));
        }
        if let Some(stack) = self.stack.take() {
            stack.restore();
        }
    }
}

/// Set up the calling thread, a thread that [`crate::spawn`] or
/// [`crate::spawn_scoped`] started, once the handler is installed: give it kizz's
/// alternate stack and record its guard region, each when it can be done. Nothing
/// happens when the handler is not installed.
pub(crate) fn guard_thread() -> ThreadGuard {
    if !INSTALLED.load(Ordering::Acquire) {
        return ThreadGuard {
            stack: None,
            region: false,
        };
    }
    let region =
        stack_region().is_ok_and(|region| REGION.try_with(|cell| cell.set(region)).is_ok());
    ThreadGuard {
        stack: AltStack::set_up().ok(),
        region,
    }
}

/// The earlier action of `signal`, when it was kept.
fn earlier(signal: libc::c_int) -> Option<&'static libc::sigaction> {
    match signal {
        libc::SIGSEGV => EARLIER_SEGV.get(),
        libc::SIGBUS => EARLIER_BUS.get(),
        _ => None,
    }
}

/// An action with every field zero: the default action, with an empty mask.
fn zeroed_action() -> libc::sigaction {
    // SAFETY: every field of `sigaction` is an integer, an integer array or an
    // optional function pointer, for which zero is a valid value.
    unsafe { std::mem::zeroed() }
}

/// Keep the current action of `signal` in `kept`, then install the handler for it.
fn take_over(
    signal: libc::c_int,
    kept: &OnceLock<libc::sigaction>,
) -> Result<(), InstallError> {
    let mut current = zeroed_action();
    // SAFETY: a null new action only reads the current one into `current`.
    if unsafe { libc::sigaction(signal, ptr::null(), &raw mut current) } != 0 {
        return Err(InstallError::last("sigaction"));
    }
    let _kept: &libc::sigaction = kept.get_or_init(|| current);
    let mut action = zeroed_action();
    let handler: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut c_void) = handle;
    action.sa_sigaction = handler as libc::sighandler_t;
    action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
    // SAFETY: `sa_mask` is a valid `sigset_t` to empty.
    unsafe { libc::sigemptyset(&raw mut action.sa_mask) };
    // SAFETY: `action` is a complete action whose handler has the `SA_SIGINFO`
    // signature.
    if unsafe { libc::sigaction(signal, &raw const action, ptr::null_mut()) } != 0 {
        return Err(InstallError::last("sigaction"));
    }
    Ok(())
}

/// Give `signal` its kept earlier action back, or the default action when none was
/// kept.
fn give_back(signal: libc::c_int) {
    let fallback = zeroed_action();
    let action = earlier(signal).unwrap_or(&fallback);
    // SAFETY: `action` is an action that `sigaction` reported, or the default action.
    unsafe { libc::sigaction(signal, ptr::from_ref(action), ptr::null_mut()) };
}

/// Install kizz's handler of stack overflows for the process, which exits with
/// `status` after an overflow, and set up the calling thread, which must be the main
/// thread, first. The main thread keeps its alternate stack.
///
/// A second call does nothing.
///
/// # Errors
///
/// An [`InstallError`] that names the call that failed and the system's error,
/// after the parts that succeeded are undone, so the handler is installed for both
/// signals or for neither.
pub fn install(status: u8) -> Result<(), InstallError> {
    if INSTALLED.load(Ordering::Acquire) {
        return Ok(());
    }
    let region = stack_region()?;
    let stack = AltStack::set_up()?;
    if REGION.try_with(|cell| cell.set(region)).is_err() {
        stack.restore();
        return Err(InstallError::invalid(
            "thread_local",
            "the thread is ending",
        ));
    }
    STATUS.store(i32::from(status), Ordering::Relaxed);
    // SAFETY: `getpid` has no preconditions and cannot fail.
    INSTALLER.store(unsafe { libc::getpid() }, Ordering::Relaxed);
    let undo = |stack: AltStack| {
        let _cleared: Result<(), std::thread::AccessError> =
            REGION.try_with(|cell| cell.set(Region::NONE));
        stack.restore();
    };
    if let Err(error) = take_over(libc::SIGSEGV, &EARLIER_SEGV) {
        undo(stack);
        return Err(error);
    }
    if let Err(error) = take_over(libc::SIGBUS, &EARLIER_BUS) {
        give_back(libc::SIGSEGV);
        undo(stack);
        return Err(error);
    }
    INSTALLED.store(true, Ordering::Release);
    Ok(())
}

/// The handler of `SIGSEGV` and `SIGBUS`: stop kizz after an overflow of the
/// faulting thread's stack, and hand every other fault to the earlier action.
extern "C" fn handle(
    signal: libc::c_int,
    info: *mut libc::siginfo_t,
    _context: *mut c_void,
) {
    if overflowed(info) {
        stop();
    }
    // SAFETY: `__errno_location` gives the calling thread's `errno`, which is valid
    // for the thread's life.
    let errno = unsafe { *libc::__errno_location() };
    give_back(signal);
    // SAFETY: as above.
    unsafe { *libc::__errno_location() = errno };
}

/// Whether `info` reports a fault that the kernel raised, in the process that
/// installed the handler, at an address in the calling thread's guard region.
fn overflowed(info: *const libc::siginfo_t) -> bool {
    if info.is_null() {
        return false;
    }
    // SAFETY: the kernel passes a valid `siginfo_t` to an `SA_SIGINFO` handler, and
    // `si_addr` is the faulting address of `SIGSEGV` and `SIGBUS`.
    let (code, address) = unsafe { ((*info).si_code, (*info).si_addr().addr()) };
    if code <= 0 {
        return false;
    }
    // SAFETY: `getpid` has no preconditions, and signal-safety(7) lists it.
    if unsafe { libc::getpid() } != INSTALLER.load(Ordering::Relaxed) {
        return false;
    }
    REGION
        .try_with(Cell::get)
        .is_ok_and(|region| region.contains(address))
}

/// Write the message, delete the noted lock file, and exit, with only `write`,
/// `unlink` and `_exit`.
#[inline(never)]
fn stop() -> ! {
    let mut task = [0_u8; TASK_CAPACITY];
    let noted = TASK.read(&mut task, || {});
    let mut message = [0_u8; MESSAGE_CAPACITY];
    let length = compose(noted.and_then(|length| task.get(..length)), &mut message);
    write_error(message.get(..length).unwrap_or_default());
    let mut path = [0_u8; PATH_BUFFER];
    let locked = path
        .get_mut(..LOCK_CAPACITY)
        .and_then(|start| LOCK.read(start, || {}));
    if locked.is_some() {
        // SAFETY: `path` holds the noted path and, after at most `LOCK_CAPACITY`
        // bytes, a zero byte, because the buffer is one byte longer and starts zeroed.
        unsafe { libc::unlink(path.as_ptr().cast()) };
    }
    // SAFETY: `_exit` ends the process at once, without running anything else.
    unsafe { libc::_exit(STATUS.load(Ordering::Relaxed)) }
}

/// Write `bytes` to standard error with `write`, retrying a short or interrupted
/// write at most [`WRITE_ATTEMPTS`] times in all, and dropping a write that fails.
fn write_error(bytes: &[u8]) {
    let mut rest = bytes;
    for _attempt in 0..WRITE_ATTEMPTS {
        if rest.is_empty() {
            return;
        }
        // SAFETY: the pointer and length describe `rest`, which outlives the call.
        let written = unsafe { libc::write(libc::STDERR_FILENO, rest.as_ptr().cast(), rest.len()) };
        let Ok(count) = usize::try_from(written) else {
            // SAFETY: as in `handle`.
            if unsafe { *libc::__errno_location() } == libc::EINTR {
                continue;
            }
            return;
        };
        if count == 0 {
            return;
        }
        rest = rest.get(count..).unwrap_or_default();
    }
}

/// Fault outside every guard region, by reading a page that is mapped without
/// access, so that the end-to-end test sees the handler hand the fault on; only in
/// builds with `debug_assertions`, which tests use and release builds lack. It
/// returns only when the page cannot be mapped.
#[cfg(debug_assertions)]
pub fn fault_outside_guards() {
    let Ok(page) = page_size() else {
        return;
    };
    // SAFETY: an anonymous private mapping at an address of the kernel's choosing
    // touches no existing memory.
    let mapped = unsafe {
        libc::mmap(
            ptr::null_mut(),
            page,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        return;
    }
    // SAFETY: the read faults on purpose: the page has no access, so the kernel raises
    // `SIGSEGV` before any value is read, and the fault ends the process.
    let _byte: u8 = unsafe { ptr::read_volatile(mapped.cast::<u8>()) };
    // SAFETY: the mapping was made above, and nothing refers to it.
    unsafe { libc::munmap(mapped, page) };
}

/// Unit tests of the guard region, the notes and the messages.
#[cfg(test)]
#[allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert, compute expected sizes and repeat text"
)]
mod tests {
    use std::path::PathBuf;

    use super::{
        LOCK, LOCK_CAPACITY, MESSAGE_CAPACITY, NO_TASK, PATH_BUFFER, Region, Slot, TASK,
        TASK_CAPACITY, TASK_PREFIX, TASK_SUFFIX, alt_stack_size, compose, cut, guard_region,
        note_lock, note_task,
    };

    /// The size of a page in the tests of the guard region.
    const PAGE: usize = 4_096;

    /// The region runs from the lowest address minus the guard up to, not including,
    /// the lowest address plus the guard, with its first and last addresses inside
    /// and the first ones outside it outside.
    #[test]
    fn the_guard_region_surrounds_the_lowest_address() {
        let region = guard_region(0x10_0000, PAGE, PAGE).unwrap();
        assert_eq!(region, Region {
            start: 0x10_0000 - PAGE,
            end: 0x10_0000 + PAGE,
        });
        assert!(region.contains(0x10_0000 - PAGE));
        assert!(region.contains(0x10_0000));
        assert!(region.contains(0x10_0000 + PAGE - 1));
        assert!(!region.contains(0x10_0000 - PAGE - 1));
        assert!(!region.contains(0x10_0000 + PAGE));
    }

    /// The guard is the larger of the guard size and the page size, so a guard size
    /// of zero gives a page, and a page size of zero with it gives no region.
    #[test]
    fn the_guard_is_at_least_a_page() {
        assert_eq!(
            guard_region(0x10_0000, 0, PAGE),
            guard_region(0x10_0000, PAGE, PAGE)
        );
        assert_eq!(guard_region(0x10_0000, 4 * PAGE, PAGE).unwrap(), Region {
            start: 0x10_0000 - 4 * PAGE,
            end: 0x10_0000 + 4 * PAGE,
        });
        assert_eq!(guard_region(0x10_0000, 0, 0), None);
    }

    /// Addresses and sizes near 0 and `usize::MAX` give no region where the
    /// arithmetic would overflow, and an exact region otherwise.
    #[test]
    fn the_guard_region_never_overflows() {
        assert_eq!(guard_region(PAGE - 1, PAGE, PAGE), None);
        assert_eq!(guard_region(0, PAGE, PAGE), None);
        let low = guard_region(PAGE, PAGE, PAGE).unwrap();
        assert!(low.contains(0));
        assert!(!low.contains(2 * PAGE));
        assert_eq!(guard_region(usize::MAX - PAGE + 1, PAGE, PAGE), None);
        assert_eq!(guard_region(usize::MAX, 0, PAGE), None);
        assert_eq!(guard_region(0x10_0000, usize::MAX, PAGE), None);
        let high = guard_region(usize::MAX - PAGE, PAGE, PAGE).unwrap();
        assert!(high.contains(usize::MAX - 1));
        assert!(!high.contains(usize::MAX));
        assert!(!Region::NONE.contains(0));
        assert!(!Region::NONE.contains(usize::MAX));
    }

    /// The alternate stack holds 64 KiB plus the kernel's minimum, in whole pages,
    /// and a size that would overflow, or a page size of zero, gives none.
    #[test]
    fn the_alternate_stack_is_whole_pages() {
        assert_eq!(alt_stack_size(PAGE, 0), Some(65_536));
        assert_eq!(alt_stack_size(PAGE, 3_000), Some(65_536 + 4_096));
        assert_eq!(alt_stack_size(PAGE, PAGE), Some(65_536 + 4_096));
        assert_eq!(alt_stack_size(0, 0), None);
        assert_eq!(alt_stack_size(PAGE, usize::MAX), None);
        assert_eq!(alt_stack_size(PAGE, usize::MAX - 65_536), None);
    }

    /// A note is read back as written, and dropping it removes it; a long task text
    /// is cut at a character boundary, and a long path is not noted. The globals are
    /// used by this one test only.
    #[test]
    fn notes_are_read_back_until_dropped() {
        let mut out = [0_u8; TASK_CAPACITY];
        let task = note_task("kizz.md:12 (Deep)");
        assert_eq!(TASK.read(&mut out, || {}), Some(17));
        assert_eq!(out.get(..17).unwrap(), b"kizz.md:12 (Deep)");
        drop(task);
        assert_eq!(TASK.read(&mut out, || {}), None);

        let long = format!("{}é", "a".repeat(TASK_CAPACITY - 1));
        let task = note_task(&long);
        assert_eq!(TASK.read(&mut out, || {}), Some(TASK_CAPACITY - 1));
        assert!(
            out.get(..TASK_CAPACITY - 1)
                .unwrap()
                .iter()
                .all(|&byte| byte == b'a')
        );
        drop(task);

        let mut path = [0_u8; PATH_BUFFER];
        let lock = note_lock(&PathBuf::from("/tmp/cache/kizz.lock"));
        let start = path.get_mut(..LOCK_CAPACITY).unwrap();
        assert_eq!(LOCK.read(start, || {}), Some(20));
        assert_eq!(path.get(..21).unwrap(), b"/tmp/cache/kizz.lock\0");
        drop(lock);
        assert_eq!(LOCK.read(&mut path, || {}), None);

        let longest = PathBuf::from(format!("/{}", "p".repeat(LOCK_CAPACITY - 1)));
        let lock = note_lock(&longest);
        assert_eq!(LOCK.read(&mut path, || {}), Some(LOCK_CAPACITY));
        drop(lock);
        let too_long = PathBuf::from(format!("/{}", "p".repeat(LOCK_CAPACITY)));
        let lock = note_lock(&too_long);
        assert_eq!(LOCK.read(&mut path, || {}), None);
        drop(lock);
        assert_eq!(PATH_BUFFER, LOCK_CAPACITY + 1);
    }

    /// A long text is cut at the last character boundary within the capacity.
    #[test]
    fn long_texts_are_cut_at_a_character_boundary() {
        assert_eq!(cut("kizz.md:1", 100), "kizz.md:1");
        assert_eq!(cut("abc", 2), "ab");
        assert_eq!(cut("aé", 2), "a");
        assert_eq!(cut("aé", 3), "aé");
        assert_eq!(cut("é", 1), "");
        assert_eq!(cut("", 0), "");
    }

    /// A copy that a write overlapped is no note, and the next copy reads the new
    /// note.
    #[test]
    fn an_overlapped_copy_is_no_note() {
        let slot: Slot<16> = Slot::new();
        let mut out = [0_u8; 16];
        let _first = slot.write(b"first");
        assert_eq!(
            slot.read(&mut out, || {
                slot.write(b"second");
            }),
            None
        );
        assert_eq!(slot.read(&mut out, || {}), Some(6));
        assert_eq!(out.get(..6).unwrap(), b"second");
    }

    /// An older note's removal leaves a newer note in place, and a note that does not
    /// fit the reader's buffer is no note.
    #[test]
    fn only_the_current_note_is_removed() {
        let slot: Slot<16> = Slot::new();
        let mut out = [0_u8; 16];
        let older = slot.write(b"older");
        let newer = slot.write(b"newer");
        slot.clear(older);
        assert_eq!(slot.read(&mut out, || {}), Some(5));
        let mut short = [0_u8; 3];
        assert_eq!(slot.read(&mut short, || {}), None);
        slot.clear(newer);
        assert_eq!(slot.read(&mut out, || {}), None);
        let empty = slot.write(b"");
        assert_eq!(slot.read(&mut out, || {}), None);
        slot.clear(empty);
    }

    /// Both messages, byte for byte, and the longest one fits the handler's buffer.
    #[test]
    fn the_messages_are_exact() {
        let mut out = [0_u8; MESSAGE_CAPACITY];
        let length = compose(Some(b"kizz.md:12 (Deep)".as_slice()), &mut out);
        assert_eq!(
            out.get(..length).unwrap(),
            b"error: the program of task kizz.md:12 (Deep) overflowed the stack, so kizz stopped; \
              its values are probably nested too deeply\n"
        );
        let length = compose(None, &mut out);
        assert_eq!(
            out.get(..length).unwrap(),
            b"error: kizz overflowed its stack and stopped\n"
        );
        assert_eq!(compose(Some(b"".as_slice()), &mut out), NO_TASK.len());
        let longest = [b'x'; TASK_CAPACITY];
        let length = compose(Some(longest.as_slice()), &mut out);
        assert_eq!(
            length,
            TASK_PREFIX.len() + TASK_CAPACITY + TASK_SUFFIX.len()
        );
        assert!(length <= MESSAGE_CAPACITY);
        assert!(out.get(..length).unwrap().ends_with(TASK_SUFFIX));
    }
}
