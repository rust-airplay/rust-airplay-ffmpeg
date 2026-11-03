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
