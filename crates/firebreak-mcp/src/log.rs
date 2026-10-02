//! The server's log, which goes to standard error.

use std::io::{self, Write};

/// Writes `message` as one line of the log.
///
/// Standard output carries the protocol, so the log goes to standard error. A log that cannot be
/// written to, such as a closed pipe, is ignored instead of panicking as `eprintln!` does: a
/// payment that was made must still be reported to the model.
pub fn line(message: &str) {
    let _ = writeln!(io::stderr(), "firebreak-mcp: {message}");
}
