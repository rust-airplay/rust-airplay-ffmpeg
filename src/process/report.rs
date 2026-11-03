// SPDX-License-Identifier: GPL-3.0-or-later
//! What a program said, kept in memory.
//!
//! `ffmpeg` and `ffplay` report everything -- a container they cannot parse, a display
//! they cannot open, a frame they cannot decode -- on standard error, and a backend that
//! discards it reports "the stream stopped" without saying why. A thread of its own
//! drains the pipe into the last few kilobytes, because a pipe nobody reads fills up and
//! blocks the program writing to it, which would turn a warning into a hang.
//!
//! It is kept in memory rather than in a file: a backend writes a file somewhere it has
//! to choose without being asked, and a stream that ends is exactly when the file would
//! be read.

use std::collections::VecDeque;
use std::io::Read;
use std::process::ChildStderr;
use std::sync::{Arc, Mutex};

/// How much of a program's output is kept.
///
/// The last words are the ones that say why it stopped, and a decoder that has run for
/// an hour has said a great deal before that.
const TAIL_BYTES: usize = 8 * 1024;

/// How many lines a report quotes.
const TAIL_LINES: usize = 12;

/// What a running program has said so far.
#[derive(Clone, Debug, Default)]
pub(crate) struct Report {
    tail: Arc<Mutex<VecDeque<u8>>>,
}

impl Report {
    /// Starts the thread that drains a child's standard error.
    ///
    /// A child started with a piped standard error always has one; a report with no
    /// drain says nothing, which is what a program that says nothing deserves.
    pub(crate) fn start(stderr: Option<ChildStderr>) -> Self {
        let report = Self::default();
        let Some(mut stderr) = stderr else {
            return report;
        };
        let tail = Arc::clone(&report.tail);
        let started = std::thread::Builder::new()
            .name("airplay-ffmpeg-report".to_owned())
            .spawn(move || drain(&mut stderr, &tail));
        if let Err(error) = started {
            // Not fatal: the stream still runs, and what the program said is lost
            // instead of the stream.
            log::debug!("the report thread cannot start: {error}");
        }
        report
    }

    /// What the program said, as the last few lines of it.
    ///
    /// An empty string is a program that said nothing, not a failure to read: a stream
    /// that stops silently is reported as having stopped silently.
    pub(crate) fn read(&self) -> String {
        let tail = self.tail.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let bytes: Vec<u8> = tail.iter().copied().collect();
        drop(tail);
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> =
            text.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
        let start = lines.len().saturating_sub(TAIL_LINES);
        lines[start..].join("; ")
    }
}

/// Reads a child's standard error until it closes, keeping the tail of it.
fn drain(stderr: &mut ChildStderr, tail: &Mutex<VecDeque<u8>>) {
    let mut buffer = [0u8; 4096];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) => return,
            Ok(read) => {
                let mut kept = tail.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                for byte in &buffer[..read] {
                    if kept.len() == TAIL_BYTES {
                        kept.pop_front();
                    }
                    kept.push_back(*byte);
                }
            }
            Err(error) => {
                log::debug!("a program's report stopped: {error}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Report;

    #[test]
    fn a_report_nobody_wrote_to_says_nothing() {
        assert_eq!(Report::default().read(), "");
    }
}
