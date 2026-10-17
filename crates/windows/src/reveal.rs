//! Showing a file to the user.

use std::path::Path;
use std::process::Command;

/// The command that shows `path` selected in Explorer.
pub fn command(path: &Path) -> Command {
    let mut command = Command::new("explorer");
    command.arg("/select,").arg(path);
    command
}
