// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Step keys.
//!
//! A step's key is a BLAKE3 hash, derived with the versioned context string
//! [`STEP_KEY_CONTEXT`], of a CBOR encoding, written with `minicbor`, of exactly
//! these values, in this order:
//!
//! 1. the task program hash, covering the block and every loaded module;
//! 2. the absolute working directory;
//! 3. the arguments;
//! 4. the environment passed to the process;
//! 5. the `stdin` content;
//! 6. the capture mode;
//! 7. the ignore patterns;
//! 8. the `track` patterns;
//! 9. whether `timestamps` is set;
//! 10. the `machine` names, sorted and without repeats;
//! 11. whether `untraced` is set;
//! 12. the occurrence number, which counts identical commands earlier in the same task
//!     run.
//!
//! The encoding is deterministic: one definite-length array of the twelve values,
//! definite lengths throughout, and the environment as a map sorted by name. So the
//! same step always produces the same key. `always`, `stream`, and `expect` are not
//! in the key, because they change what kizz does with a step, not what the command
//! does. `timestamps` is in the key, because a record made without it keeps the
//! timestamps of its latest cache hit. The `machine` names are in the key, like the
//! `track` patterns, because they decide what the cache check compares.

use std::{collections::HashMap, convert::Infallible, os::unix::ffi::OsStrExt as _};

use kizz_store::StepKey;
use kizz_trace::CaptureMode;
use minicbor::{Encoder, encode};

use crate::{runtime::ProgramHash, step::StepSpec};

/// The context string from which step keys are derived. A change to the values in
/// the key or to their encoding changes the version at its end.
pub const STEP_KEY_CONTEXT: &str = "kizz step key, version 4";

/// The number of values in the key's encoding.
const KEY_VALUES: u64 = 12;

/// A step whose key values could not be encoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot encode the step key of `{command}`: {message}")]
pub struct KeyError {
    /// The command, as messages show it.
    pub command: String,
    /// What went wrong.
    pub message: String,
}

/// The error of an encoder that writes into memory.
type EncodeError = encode::Error<Infallible>;

/// The length of a list, as CBOR encodes it.
fn length(len: usize) -> Result<u64, EncodeError> {
    u64::try_from(len).map_err(|_| encode::Error::message("a list is too long to encode"))
}

/// The code of a capture mode in the key.
const fn capture_code(capture: CaptureMode) -> u8 {
    match capture {
        CaptureMode::Split => 0,
        CaptureMode::Merged => 1,
    }
}

/// Encode a list of strings as a definite-length array.
fn strings(
    e: &mut Encoder<Vec<u8>>,
    items: &[String],
) -> Result<(), EncodeError> {
    e.array(length(items.len())?)?;
    for item in items {
        e.str(item)?;
    }
    Ok(())
}

/// The encoding of the key values of `spec` in the task program `program`, up to and
/// excluding the occurrence number. Identical commands have identical bytes here.
fn encode_identity(
    program: ProgramHash,
    spec: &StepSpec,
) -> Result<Vec<u8>, EncodeError> {
    let mut e = Encoder::new(Vec::new());
    e.array(KEY_VALUES)?;
    e.bytes(program.as_bytes())?;
    e.bytes(spec.cwd.as_path().as_os_str().as_bytes())?;
    strings(&mut e, &spec.argv)?;
    e.map(length(spec.env.len())?)?;
    for (name, value) in &spec.env {
        e.str(name)?.str(value)?;
    }
    match &spec.stdin {
        Some(text) => e.str(text)?,
        None => e.null()?,
    };
    e.u8(capture_code(spec.capture))?;
    strings(&mut e, &spec.ignore)?;
    strings(&mut e, &spec.track)?;
    e.bool(spec.timestamps)?;
    strings(&mut e, &spec.machine)?;
    e.bool(spec.untraced)?;
    Ok(e.into_writer())
}

/// The key of a step from the encoding of its identity and its occurrence number.
fn derive(
    identity: Vec<u8>,
    occurrence: u32,
) -> Result<StepKey, EncodeError> {
    let mut e = Encoder::new(identity);
    e.u32(occurrence)?;
    Ok(StepKey(blake3::derive_key(STEP_KEY_CONTEXT, e.writer())))
}

/// The error for `spec` when its key cannot be encoded.
fn key_error(
    spec: &StepSpec,
    error: &EncodeError,
) -> KeyError {
    KeyError {
        command: spec.command(),
        message: error.to_string(),
    }
}

/// The key of `spec`, the step at `occurrence` among identical commands of a run of
/// the task program `program`.
///
/// # Errors
///
/// Returns [`KeyError`] when a list in the step is too long to encode.
pub fn step_key(
    program: ProgramHash,
    spec: &StepSpec,
    occurrence: u32,
) -> Result<StepKey, KeyError> {
    encode_identity(program, spec)
        .and_then(|identity| derive(identity, occurrence))
        .map_err(|error| key_error(spec, &error))
}

/// Counts identical commands in one run of a task, so a command repeated in the
/// task gets one key for each occurrence.
#[derive(Debug, Default)]
pub struct Occurrences {
    /// How many times each identity has been seen in the run.
    seen: HashMap<Vec<u8>, u32>,
}

impl Occurrences {
    /// No commands seen yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The key and the occurrence number of the next step of the run, `spec` in the
    /// task program `program`, counting it.
    ///
    /// # Errors
    ///
    /// Returns [`KeyError`] when a list in the step is too long to encode.
    pub fn next_key(
        &mut self,
        program: ProgramHash,
        spec: &StepSpec,
    ) -> Result<(StepKey, u32), KeyError> {
        let identity = encode_identity(program, spec).map_err(|error| key_error(spec, &error))?;
        let count = self.seen.entry(identity.clone()).or_insert(0);
        let occurrence = *count;
        *count = count.saturating_add(1);
        let key = derive(identity, occurrence).map_err(|error| key_error(spec, &error))?;
        Ok((key, occurrence))
    }
}

/// Unit tests for the key encoding.
#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test code: the tests assert and lock"
)]
mod tests {
    use std::collections::BTreeMap;

    use kizz_trace::AbsolutePath;

    use super::*;

    /// A step that runs `echo hi` in `/work`.
    fn spec() -> StepSpec {
        StepSpec {
            argv: vec!["echo".to_owned(), "hi".to_owned()],
            stdin: None,
            env: BTreeMap::new(),
            cwd: AbsolutePath::new("/work").unwrap(),
            capture: CaptureMode::Split,
            stream: false,
            always: false,
            untraced: false,
            ignore: Vec::new(),
            track: Vec::new(),
            timestamps: false,
            machine: Vec::new(),
            expect: None,
        }
    }

    #[test]
    fn the_encoding_is_one_definite_array_in_the_documented_order() {
        let program = ProgramHash::from_bytes([7; 32]);
        let mut step = spec();
        step.env.insert("B".to_owned(), "2".to_owned());
        step.env.insert("A".to_owned(), "1".to_owned());
        let identity = encode_identity(program, &step).unwrap();
        let mut expected = vec![0x8C, 0x58, 0x20];
        expected.extend_from_slice(&[7; 32]);
        expected.extend_from_slice(b"\x45/work");
        expected.extend_from_slice(b"\x82\x64echo\x62hi");
        expected.extend_from_slice(b"\xa2\x61A\x611\x61B\x612");
        // No stdin, split capture, no ignore or track patterns, no `timestamps`, and
        // no `machine` names, and tracing enabled.
        expected.extend_from_slice(&[0xF6, 0x00, 0x80, 0x80, 0xF4, 0x80, 0xF4]);
        assert_eq!(identity, expected);
        step.timestamps = true;
        let with_timestamps = encode_identity(program, &step).unwrap();
        assert_eq!(with_timestamps.iter().rev().nth(2), Some(&0xF5));
        assert_eq!(with_timestamps.len(), identity.len());
        step.timestamps = false;
        step.machine = vec!["kernel".to_owned(), "memory".to_owned()];
        let with_machine = encode_identity(program, &step).unwrap();
        assert!(with_machine.ends_with(b"\xF4\x82\x66kernel\x66memory\xF4"));
        assert_ne!(
            step_key(program, &step, 0).unwrap(),
            step_key(program, &spec(), 0).unwrap()
        );
    }

    #[test]
    fn occurrences_count_identical_commands_only() {
        let program = ProgramHash::from_bytes([1; 32]);
        let mut occurrences = Occurrences::new();
        let (first, zero) = occurrences.next_key(program, &spec()).unwrap();
        let mut other = spec();
        other.argv.push("there".to_owned());
        let (_, other_zero) = occurrences.next_key(program, &other).unwrap();
        let (second, one) = occurrences.next_key(program, &spec()).unwrap();
        assert_eq!((zero, other_zero, one), (0, 0, 1));
        assert_ne!(first, second);
        assert_eq!(first, step_key(program, &spec(), 0).unwrap());
        assert_eq!(second, step_key(program, &spec(), 1).unwrap());
    }
}
