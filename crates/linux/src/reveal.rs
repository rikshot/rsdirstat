//! Showing a file to the user.

use std::path::Path;
use std::process::Command;

/// The command that opens the folder holding `path` in the desktop's file manager, which
/// `xdg-open` can't be asked to select it in.
pub fn command(path: &Path) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(path.parent().unwrap_or(path));
    command
}
