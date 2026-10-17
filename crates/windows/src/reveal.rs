//! Showing a file to the user.

use std::ffi::OsString;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The command that shows `path` selected in Explorer.
pub fn command(path: &Path) -> Command {
    // Explorer splits its command line at commas outside double quotes, but `arg` quotes only an
    // argument with a space or tab in it, so `C:\a,b.txt` would reach Explorer in pieces and reveal
    // the wrong place. Quote the path here instead: a Windows file name can't contain `"`.
    let mut quoted = OsString::from("\"");
    quoted.push(path);
    quoted.push("\"");
    let mut command = Command::new(explorer());
    command.arg("/select,").raw_arg(quoted);
    command
}

/// Explorer by its full path: a bare `explorer` is looked for in rsdirstat's own folder first, where
/// a downloaded or planted `explorer.exe` would run instead.
fn explorer() -> PathBuf {
    std::env::var_os("SystemRoot").map_or_else(
        || PathBuf::from("explorer"),
        |root| Path::new(&root).join("explorer.exe"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_with_commas_reach_explorer_whole() {
        let command = command(Path::new(r"C:\data\2024,Q1\Smith,John.pdf"));
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["/select,", r#""C:\data\2024,Q1\Smith,John.pdf""#]);
        assert!(
            Path::new(command.get_program()).ends_with("explorer.exe"),
            "{:?}",
            command.get_program()
        );
    }
}
