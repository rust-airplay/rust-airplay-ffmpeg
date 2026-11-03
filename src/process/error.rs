// SPDX-License-Identifier: GPL-3.0-or-later
//! Why a program could not be run, or stopped once it was.

use std::fmt;

use rust_airplay::RenderError;

/// What went wrong with a program this backend starts.
///
/// The distinction the renderer boundary draws -- the machine cannot, or the request
/// could not be carried out -- is drawn here, because this is where the evidence is: the
/// program that was not found is a machine without FFmpeg, and the program that said
/// "Invalid argument" is a command line that was wrong.
#[derive(Debug)]
pub(crate) enum LaunchError {
    /// The program is not on this machine, or not where the caller said it was.
    NotFound {
        /// What was tried, for a message that says what to install.
        tried: Vec<String>,
    },
    /// The program is here and would not start.
    StartFailed {
        /// What the operating system said.
        reason: String,
    },
    /// The machine cannot serve this stream: no display, no sound card, no device.
    Environment {
        /// What the program said about it.
        reason: String,
    },
    /// The program refused what it was asked to do.
    Rejected {
        /// What the program said about it.
        reason: String,
    },
    /// The program stopped while it was running.
    Stopped {
        /// What the program said about it.
        reason: String,
    },
}

impl fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { tried } => {
                write!(formatter, "no program was found: {}", tried.join(", "))
            }
            Self::StartFailed { reason } => write!(formatter, "{reason}"),
            Self::Environment { reason } => write!(formatter, "{reason}"),
            Self::Rejected { reason } => write!(formatter, "{reason}"),
            Self::Stopped { reason } => write!(formatter, "the program stopped: {reason}"),
        }
    }
}

/// The renderer error this becomes.
///
/// A program that is missing or a machine that cannot serve the stream is
/// [`RenderError::Unavailable`]: the session continues without that stream and the
/// receiver says why. A program that refused its arguments, or stopped once it was
/// running, is [`RenderError::Failed`]: the request was wrong, and trying it again
/// cannot help.
pub(crate) fn as_render_error(error: LaunchError) -> RenderError {
    match error {
        LaunchError::NotFound { tried } => {
            RenderError::Unavailable(format!("no program was found: {}", tried.join(", ")))
        }
        LaunchError::StartFailed { reason }
        | LaunchError::Environment { reason }
        | LaunchError::Rejected { reason } => RenderError::Unavailable(reason),
        LaunchError::Stopped { reason } => RenderError::Failed(reason),
    }
}
