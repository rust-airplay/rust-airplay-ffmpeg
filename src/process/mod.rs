// SPDX-License-Identifier: GPL-3.0-or-later
//! Running FFmpeg programs and feeding them.
//!
//! Every stage of this backend is a program: `ffmpeg` decodes a stream, a player plays
//! it, and the backend's job is to start them, write to them, and recognise when they
//! have stopped. Both media reach a program as a byte stream on its standard input --
//! video because H.264 and HEVC carry start codes, audio because the frames are wrapped
//! in a container that carries their lengths -- and a decoder that crashes on one frame
//! takes only its own process with it.
//!
//! What happens here is bounded on purpose. A writer thread drains a small queue, so a
//! push gives up rather than blocking a session's receive loop forever, and every wait
//! for a process has a deadline after which the process is killed.

mod error;
mod find;
mod report;
mod writer;

use std::process::{Child, ChildStdout, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(crate) use error::{LaunchError, as_render_error};
pub(crate) use find::{listing, probe};
use report::Report;
use writer::Writer;

/// How long a closed program gets to play out what it has.
///
/// A player drains its queue before its output ends, so this is the length of a stream's
/// tail rather than a startup cost. Past it the process is killed: a receiver that hangs
/// on teardown is worse than one that cuts the last frame.
const FINISH_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a killed program gets to be reaped.
const REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a fresh program gets to say it was refused.
///
/// Measured rather than guessed: `ffmpeg` rejects an argument it cannot parse or an
/// input format it does not have within the first hundred milliseconds, because what
/// takes the time is the process starting rather than the stream being opened. A stream
/// that is still running after this is a stream that started.
const SETTLE: Duration = Duration::from_millis(150);

/// One running program.
///
/// It owns the process, the queue into it and what the program has said so far. The
/// standard input is owned by a writer thread rather than by this struct, so that no lock
/// is held across a write that can block.
#[derive(Debug)]
pub(crate) struct Process {
    program: String,
    args: Vec<String>,
    /// The variables the program was started with, so a restart starts it the same way.
    environment: Vec<(String, String)>,
    report: Mutex<Report>,
    child: Mutex<Child>,
    writer: Mutex<Option<Writer>>,
    finished: AtomicBool,
}

impl Process {
    /// Starts a program whose output goes nowhere.
    ///
    /// # Errors
    ///
    /// Returns [`LaunchError::StartFailed`] when the program cannot be started, and what
    /// it said when it exits before settling -- which is how a player with no display or
    /// an argument it cannot parse is reported at open rather than at the first frame.
    pub(crate) fn start(
        program: &str,
        args: &[String],
        environment: &[(String, String)],
    ) -> Result<Self, LaunchError> {
        Self::spawn(program, args, Stdio::null(), environment).map(|(process, _)| process)
    }

    /// Starts a program whose standard output is a pipe this process reads.
    ///
    /// The decoder of an audio stream is this one: it decodes the frames it is handed and
    /// writes PCM back, and the gain stage is between it and the sink.
    ///
    /// # Errors
    ///
    /// The same as [`Process::start`].
    pub(crate) fn start_piped(
        program: &str,
        args: &[String],
        environment: &[(String, String)],
    ) -> Result<(Self, ChildStdout), LaunchError> {
        let (process, stdout) = Self::spawn(program, args, Stdio::piped(), environment)?;
        match stdout {
            Some(stdout) => Ok((process, stdout)),
            // A process started with Stdio::piped always has one; a missing one is not a
            // stream this backend can feed.
            None => Err(LaunchError::StartFailed {
                reason: format!("{program} has no standard output"),
            }),
        }
    }

    /// Starts the program and waits for it to settle.
    fn spawn(
        program: &str,
        args: &[String],
        stdout: Stdio,
        environment: &[(String, String)],
    ) -> Result<(Self, Option<ChildStdout>), LaunchError> {
        let (mut child, writer, report) = Writer::spawn(program, args, stdout, environment)?;
        let taken = child.stdout.take();
        let process = Self {
            program: program.to_owned(),
            args: args.to_vec(),
            environment: environment.to_vec(),
            report: Mutex::new(report),
            child: Mutex::new(child),
            writer: Mutex::new(Some(writer)),
            finished: AtomicBool::new(false),
        };
        process.settle();
        if process.is_running() { Ok((process, taken)) } else { Err(process.stopped()) }
    }

    /// The command line this program was started from, for a message that says what was
    /// attempted.
    pub(crate) fn description(&self) -> String {
        format!("{} {}", self.program, self.args.join(" "))
    }

    /// Writes one buffer, waiting a bounded time for the program to take it.
    ///
    /// # Errors
    ///
    /// Returns [`LaunchError::Stopped`] when the program has gone or stopped reading,
    /// with what it said when it said anything.
    pub(crate) fn push(&self, bytes: Vec<u8>) -> Result<(), LaunchError> {
        let writer = lock(&self.writer).clone();
        let Some(writer) = writer else {
            return Err(self.stopped());
        };
        writer.push(bytes).map_err(|reason| LaunchError::Stopped { reason })
    }

    /// Tells the program that no more bytes are coming, and waits for it to finish.
    ///
    /// # Errors
    ///
    /// Returns [`LaunchError::Stopped`] when the program fails or had already gone.
    pub(crate) fn finish(&self) -> Result<(), LaunchError> {
        if self.finished.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // Closing the pipe is the end of stream for a program that reads a descriptor,
        // which is what lets a player drain what it has before it exits.
        if let Some(writer) = lock(&self.writer).take() {
            writer.close();
        }
        self.wait_exit(FINISH_TIMEOUT)
    }

    /// Stops the program now, as a teardown or a flush does.
    pub(crate) fn abort(&self) {
        lock(&self.writer).take();
        if let Err(error) = lock(&self.child).kill() {
            log::debug!("{} could not be killed: {error}", self.program);
        }
        let deadline = Instant::now() + REAP_TIMEOUT;
        while Instant::now() < deadline {
            match lock(&self.child).try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => return,
            }
        }
    }

    /// Starts the same program again, after stopping this one.
    ///
    /// This is what a flush costs: a decoder cannot be told to forget what it has queued,
    /// so the process that holds it is replaced. The frame a client sends after a flush is
    /// a key frame, which is what makes a fresh decoder correct rather than merely empty.
    ///
    /// # Errors
    ///
    /// Returns what [`Process::start`] returns, and leaves nothing running when it does.
    pub(crate) fn restart(&self, piped_output: bool) -> Result<Option<ChildStdout>, LaunchError> {
        self.abort();
        let stream = if piped_output { Stdio::piped() } else { Stdio::null() };
        let (mut child, writer, report) =
            Writer::spawn(&self.program, &self.args, stream, &self.environment)?;
        let taken = child.stdout.take();
        *lock(&self.report) = report;
        *lock(&self.child) = child;
        *lock(&self.writer) = Some(writer);
        self.finished.store(false, Ordering::SeqCst);
        self.settle();
        if self.is_running() { Ok(taken) } else { Err(self.stopped()) }
    }

    /// How the program ended, when it has.
    ///
    /// A program that exits on its own says why only in its exit status: a player told to
    /// quit, a window that was closed, a crash the display driver took it down with. The
    /// report is what it printed; this is what the operating system saw.
    pub(crate) fn exit_status(&self) -> Option<std::process::ExitStatus> {
        lock(&self.child).try_wait().ok().flatten()
    }

    /// Whether the program is still running.
    pub(crate) fn is_running(&self) -> bool {
        matches!(lock(&self.child).try_wait(), Ok(None))
    }

    /// Waits, briefly, for a program that was refused to say so.
    fn settle(&self) {
        let deadline = Instant::now() + SETTLE;
        while Instant::now() < deadline {
            if !self.is_running() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits for the process to end, killing it past the deadline.
    fn wait_exit(&self, budget: Duration) -> Result<(), LaunchError> {
        let deadline = Instant::now() + budget;
        loop {
            match lock(&self.child).try_wait() {
                Ok(Some(status)) => {
                    return if status.success() { Ok(()) } else { Err(self.stopped_after(status)) };
                }
                Ok(None) if Instant::now() >= deadline => {
                    self.abort();
                    return Err(LaunchError::Stopped {
                        reason: format!(
                            "{} did not exit within {}s",
                            self.program,
                            budget.as_secs()
                        ),
                    });
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => {
                    return Err(LaunchError::Stopped { reason: error.to_string() });
                }
            }
        }
    }

    /// What the program has said so far.
    ///
    /// A sink that is slow rather than stopped says why in its own output -- a display it
    /// cannot open, a decoder that refused the stream -- and that output is the only
    /// evidence there is; waiting for it to exit would come too late to help.
    pub(crate) fn report(&self) -> String {
        lock(&self.report).read()
    }

    /// What the program said, for a program that has stopped.
    fn stopped(&self) -> LaunchError {
        lock(&self.writer).take();
        let _ = lock(&self.child).kill();
        let _ = lock(&self.child).wait();
        self.said()
    }

    /// What the program said, for a program whose exit status is already known.
    fn stopped_after(&self, status: std::process::ExitStatus) -> LaunchError {
        match self.said() {
            LaunchError::Stopped { reason } => {
                LaunchError::Stopped { reason: format!("{reason} (exited {status})") }
            }
            other => other,
        }
    }

    /// Classifies what the program printed.
    ///
    /// The two programs say a machine cannot serve a stream in a small number of ways,
    /// and those are the words looked for: a display that cannot be opened, an audio
    /// device that is not there, an input format this build does not have. Everything
    /// else is the command line that was asked for, or a program that stopped.
    fn said(&self) -> LaunchError {
        let mut reason = lock(&self.report).read();
        if reason.is_empty() {
            reason = format!("{} exited without saying why", self.program);
        }
        const ENVIRONMENT: [&str; 5] = [
            "Could not initialize",
            "unable to open display",
            "No such device",
            "Device not found",
            "not found",
        ];
        const REJECTED: [&str; 5] = [
            "Invalid argument",
            "Unknown input format",
            "Option not found",
            "Error opening input",
            "Unrecognized option",
        ];
        if ENVIRONMENT.iter().any(|words| reason.contains(words)) {
            LaunchError::Environment { reason }
        } else if REJECTED.iter().any(|words| reason.contains(words)) {
            LaunchError::Rejected { reason }
        } else {
            LaunchError::Stopped { reason }
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // A program left running would hold a window or a sound card for as long as the
        // process lives, and a session that forgot to close one is not a reason to keep
        // playing.
        self.abort();
    }
}

/// Locks a lock this process owns, ignoring poisoning.
///
/// A process whose writer panicked once is still a process, and the alternative -- a
/// stream that can never be written to again -- is worse than one written to by whoever
/// is left.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
