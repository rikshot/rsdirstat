//! Showing a file to the user.

use std::path::Path;
use std::process::Command;

/// The command that shows `path` selected in Finder.
pub fn command(path: &Path) -> Command {
    let mut command = Command::new("open");
    command.arg("-R").arg(path);
    command
}
