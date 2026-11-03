// SPDX-License-Identifier: GPL-3.0-or-later
//! The pipe into a program's standard input.
//!
//! A program reads its input from a descriptor, and writing to that descriptor can block
//! for as long as the program declines to read. Writing therefore happens on a thread of
//! its own, behind a queue a few buffers deep: a push hands one buffer over and waits for
//! that buffer to be taken, so a player that has stopped reading becomes a refused frame
//! rather than a session that never returns.
//!
//! Every wait here has a deadline, including the wait for room in the queue: a full queue
//! means the writer thread is stuck in a write, so offering it another job is the same
//! wait as the write itself, and it has to give up the same way.

use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use super::LaunchError;
use super::report::Report;

/// How long a push waits for the writer to take one buffer.
///
/// A program that has stopped reading is not one that will start again: past this the
/// frame is refused and the stream reports why, rather than holding the session that sent
/// it.
pub(super) const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// How many buffers the queue may hold.
///
/// A few rather than unbounded: a player that stops reading must not let a session
/// accumulate its whole stream in memory.
const QUEUE: usize = 4;

/// How long closing the pipe waits for the writer to get to it.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

/// One buffer for a program, and the answer to whether it was taken.
enum Job {
    Data(Vec<u8>, Sender<Result<(), String>>),
    Close(Sender<()>),
}

/// The queue into a program's standard input.
#[derive(Clone, Debug)]
pub(super) struct Writer {
    jobs: SyncSender<Job>,
}

impl Writer {
    /// Starts the program and the thread that writes to it.
    ///
    /// The arguments are passed separately rather than as one string, because that is
    /// the form which survives every platform's quoting rules: a file path may hold a
    /// space, and a shell that split a joined line would split it too.
    pub(super) fn spawn(
        program: &str,
        args: &[String],
        stdout: Stdio,
        environment: &[(String, String)],
    ) -> Result<(Child, Self, Report), LaunchError> {
        let mut child = Command::new(program)
            .envs(environment.iter().cloned())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(stdout)
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| LaunchError::StartFailed { reason: format!("{program}: {error}") })?;
        let report = Report::start(child.stderr.take());
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(LaunchError::StartFailed {
                reason: format!("{program} has no standard input"),
            });
        };
        let writer = match Self::from_stdin(stdin) {
            Ok(writer) => writer,
            Err(error) => {
                // The child exists whether or not the thread does, and a program with no
                // writer is a process nobody will ever feed or stop.
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        Ok((child, writer, report))
    }

    /// The queue for a pipe that has just been opened.
    ///
    /// # Errors
    ///
    /// Returns [`LaunchError::StartFailed`] when the thread that writes to it cannot be
    /// started, which leaves the caller owning a child it has to stop.
    pub(super) fn from_stdin(stdin: ChildStdin) -> Result<Self, LaunchError> {
        let (jobs, receiving) = std::sync::mpsc::sync_channel::<Job>(QUEUE);
        std::thread::Builder::new()
            .name("airplay-ffmpeg-writer".to_owned())
            .spawn(move || write_loop(stdin, receiving))
            .map_err(|error| LaunchError::StartFailed {
                reason: format!("the writer cannot start: {error}"),
            })?;
        Ok(Self { jobs })
    }

    /// Writes one buffer, waiting at most [`WRITE_TIMEOUT`] for the program to take it.
    ///
    /// The buffer is moved rather than copied: a video access unit is tens of kilobytes
    /// and a session is tens of them a second, which is worth not copying twice.
    pub(super) fn push(&self, bytes: Vec<u8>) -> Result<(), String> {
        let deadline = Instant::now() + WRITE_TIMEOUT;
        let (acknowledge, answer) = std::sync::mpsc::channel();
        enqueue(&self.jobs, Job::Data(bytes, acknowledge), deadline)?;
        let left = deadline.saturating_duration_since(Instant::now());
        match answer.recv_timeout(left) {
            Ok(outcome) => outcome,
            Err(_) => {
                Err(format!("the program did not take a frame within {}s", WRITE_TIMEOUT.as_secs()))
            }
        }
    }

    /// Closes the pipe, which is the end of stream for a program that reads a descriptor.
    pub(super) fn close(&self) {
        let (closed, closed_rx) = std::sync::mpsc::channel();
        let deadline = Instant::now() + CLOSE_TIMEOUT;
        if enqueue(&self.jobs, Job::Close(closed), deadline).is_ok() {
            let _ = closed_rx.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        }
    }
}

/// Hands a job to the writer thread, waiting at most until `deadline` for room.
///
/// This is the part that has to be bounded: `SyncSender::send` waits as long as the
/// writer keeps the queue full, which is exactly as long as the program refuses to read,
/// which is the case this module exists to survive.
fn enqueue(jobs: &SyncSender<Job>, mut job: Job, deadline: Instant) -> Result<(), String> {
    loop {
        match jobs.try_send(job) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(returned)) => {
                job = returned;
                if Instant::now() >= deadline {
                    return Err("the program is not taking frames: its writer is still queued behind a write".to_owned());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err("the program is no longer reading".to_owned());
            }
        }
    }
}

/// Writes what the queue holds, until the queue ends or a write fails.
fn write_loop(mut stdin: ChildStdin, jobs: Receiver<Job>) {
    while let Ok(job) = jobs.recv() {
        match job {
            Job::Data(bytes, acknowledge) => {
                let outcome = stdin.write_all(&bytes).map_err(|error| error.to_string());
                let failed = outcome.is_err();
                let _ = acknowledge.send(outcome);
                if failed {
                    return;
                }
            }
            Job::Close(closed) => {
                drop(stdin);
                let _ = closed.send(());
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Job, Writer, enqueue};
    use std::time::{Duration, Instant};

    #[test]
    fn a_full_queue_is_refused_rather_than_waited_out() {
        // A writer that has not taken a single job is a program that is not reading: the
        // second job must give up by its deadline instead of blocking the caller for as
        // long as the program stays stuck.
        let (jobs, _receiving) = std::sync::mpsc::sync_channel::<Job>(1);
        let writer = Writer { jobs };
        let (acknowledge, _answer) = std::sync::mpsc::channel();
        enqueue(
            &writer.jobs,
            Job::Data(vec![1, 2, 3], acknowledge),
            Instant::now() + Duration::from_secs(1),
        )
        .expect("the first job fits");
        let (acknowledge, _answer) = std::sync::mpsc::channel();
        let started = Instant::now();
        let error = enqueue(
            &writer.jobs,
            Job::Data(vec![4, 5, 6], acknowledge),
            started + Duration::from_millis(60),
        )
        .expect_err("a full queue is an error");
        assert!(started.elapsed() < Duration::from_secs(2), "{error}");
        assert!(error.contains("not taking frames"), "{error}");
    }

    #[test]
    fn a_queue_with_no_writer_says_so() {
        let (jobs, receiving) = std::sync::mpsc::sync_channel::<Job>(1);
        drop(receiving);
        let (acknowledge, _answer) = std::sync::mpsc::channel();
        let error = enqueue(
            &jobs,
            Job::Data(Vec::new(), acknowledge),
            Instant::now() + Duration::from_millis(50),
        )
        .expect_err("a dropped writer is an error");
        assert!(error.contains("no longer reading"), "{error}");
    }
}
