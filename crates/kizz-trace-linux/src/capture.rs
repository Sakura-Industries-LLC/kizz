// Copyright (c) 2026 Sakura Industries LLC.
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Standard input, output capture and live echo.
//!
//! `stdin` content goes through a pipe that a thread of the tracer writes, so a
//! program that writes before it reads never blocks the tracer; a request without
//! content connects `/dev/null`. Split capture gives standard output and standard
//! error a pipe each. Merged capture gives both one pipe, so the kernel keeps the
//! exact order of the writes. A thread reads each output pipe until every process
//! that holds it has exited, and echoes each chunk live when the request asks.

use std::{
    fs::File,
    io::{self, Read, Write},
    os::fd::OwnedFd,
};

use kizz_trace::{CaptureMode, CapturedOutput, EchoSink, OutputStream, RunRequest};

use crate::{spawn::ChildStdio, sys};

/// The size of each read from an output pipe.
const CHUNK_SIZE: usize = 64 * 1024;

/// The tracer's ends of the child's standard streams.
#[derive(Debug)]
pub(crate) struct TracerStdio {
    /// The write end of the `stdin` pipe, when the request has `stdin` content.
    pub stdin: Option<File>,
    /// The read end of each output pipe, with the stream it carries.
    pub outputs: Vec<(OutputStream, File)>,
}

/// Create the pipes of `request`: the child's ends and the tracer's ends.
///
/// # Errors
///
/// Returns the error of creating a pipe or opening `/dev/null`.
pub(crate) fn pipes(request: &RunRequest) -> io::Result<(ChildStdio, TracerStdio)> {
    let (child_stdin, stdin) = if request.stdin.is_some() {
        let (read, write) = sys::pipe()?;
        (read, Some(File::from(write)))
    } else {
        let null = File::open("/dev/null")?;
        (sys::above_stdio(OwnedFd::from(null))?, None)
    };
    let (child_stdout, child_stderr, outputs) = match request.capture {
        CaptureMode::Split => {
            let (stdout_read, stdout_write) = sys::pipe()?;
            let (stderr_read, stderr_write) = sys::pipe()?;
            (stdout_write, Some(stderr_write), vec![
                (OutputStream::Stdout, File::from(stdout_read)),
                (OutputStream::Stderr, File::from(stderr_read)),
            ])
        },
        CaptureMode::Merged => {
            let (read, write) = sys::pipe()?;
            (write, None, vec![(OutputStream::Merged, File::from(read))])
        },
    };
    Ok((
        ChildStdio {
            stdin: child_stdin,
            stdout: child_stdout,
            stderr: child_stderr,
        },
        TracerStdio { stdin, outputs },
    ))
}

/// Write all of `content` to the program's standard input, then close it.
///
/// A program that exits without reading all of its input closes the pipe, which is
/// not an error.
///
/// # Errors
///
/// Returns any other error of the write.
pub(crate) fn feed(
    mut stdin: File,
    content: &[u8],
) -> io::Result<()> {
    match stdin.write_all(content) {
        Err(error) if error.kind() != io::ErrorKind::BrokenPipe => Err(error),
        Ok(()) | Err(_) => Ok(()),
    }
}

/// Read one output pipe until every writer has closed it, echoing each chunk to
/// `echo` when it is set.
///
/// A failed echo never fails the capture.
///
/// # Errors
///
/// Returns the error of a read.
pub(crate) fn collect(
    mut pipe: File,
    stream: OutputStream,
    echo: Option<&dyn EchoSink>,
) -> io::Result<Vec<u8>> {
    let mut captured = Vec::new();
    let mut buffer = vec![0; CHUNK_SIZE];
    loop {
        let count = match pipe.read(&mut buffer) {
            Ok(0) => return Ok(captured),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let chunk = buffer.get(..count).unwrap_or_default();
        captured.extend_from_slice(chunk);
        if let Some(echo) = echo {
            drop(echo.echo(stream, chunk));
        }
    }
}

/// Assemble the captured output of each stream in the shape of `mode`.
pub(crate) fn assemble(
    mode: CaptureMode,
    streams: Vec<(OutputStream, Vec<u8>)>,
) -> CapturedOutput {
    let mut output = CapturedOutput::empty(mode);
    for (stream, bytes) in streams {
        let target = match (&mut output, stream) {
            (CapturedOutput::Split { stdout, .. }, OutputStream::Stdout) => Some(stdout),
            (CapturedOutput::Split { stderr, .. }, OutputStream::Stderr) => Some(stderr),
            (CapturedOutput::Merged { output }, OutputStream::Merged) => Some(output),
            (CapturedOutput::Split { .. }, OutputStream::Merged)
            | (CapturedOutput::Merged { .. }, OutputStream::Stdout | OutputStream::Stderr) => None,
        };
        if let Some(target) = target {
            target.extend_from_slice(&bytes);
        }
    }
    output
}

/// Unit tests for assembling captured output.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_streams_are_kept_apart() {
        let output = assemble(CaptureMode::Split, vec![
            (OutputStream::Stdout, b"out".to_vec()),
            (OutputStream::Stderr, b"err".to_vec()),
        ]);
        assert_eq!(output, CapturedOutput::Split {
            stdout: b"out".to_vec(),
            stderr: b"err".to_vec(),
        });
    }

    #[test]
    fn merged_stream_is_one_output() {
        let output = assemble(CaptureMode::Merged, vec![(
            OutputStream::Merged,
            b"both".to_vec(),
        )]);
        assert_eq!(output, CapturedOutput::Merged {
            output: b"both".to_vec(),
        });
    }
}
