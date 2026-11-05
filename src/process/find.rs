// SPDX-License-Identifier: GPL-3.0-or-later
//! Finding the program a stream runs in.

use std::process::{Command, Stdio};

use super::LaunchError;

/// Whether a program can be started, which is all this backend needs to know about it.
///
/// It asks the program for its version: a name that answers is a name that works, and
/// one that does not is passed over. The exit status is not inspected -- `ffplay -version`
/// is not the same flag as `--version` for every program -- only whether the process
/// could be started at all.
///
/// # Errors
///
/// Returns [`LaunchError::NotFound`] when the program cannot be started.
pub(crate) fn probe(program: &str) -> Result<(), LaunchError> {
    let started = Command::new(program)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match started {
        Ok(_) => Ok(()),
        Err(error) => Err(LaunchError::NotFound { tried: vec![format!("{program} ({error})")] }),
    }
}

/// What a program prints when a flag makes it list something.
///
/// `-hwaccels` is the question this exists for: the methods a build has are the only
/// reason to ask for hardware decoding, because a build without one refuses the option
/// rather than falling back to software.
///
/// Returns `None` when the program cannot be started or says nothing, which the caller
/// reads as "no hardware acceleration" and does the same thing with as an empty list.
pub(crate) fn listing(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}
