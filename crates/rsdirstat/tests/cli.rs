use std::fs;
use std::process::Command;

fn rsdirstat_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_rsdirstat-cli"))
}

fn create_test_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    fs::create_dir(root.join("big_dir")).unwrap();
    fs::create_dir(root.join("small_dir")).unwrap();

    fs::write(root.join("big_dir").join("large.bin"), vec![0u8; 8192]).unwrap();
    fs::write(root.join("big_dir").join("medium.bin"), vec![0u8; 4096]).unwrap();
    fs::write(root.join("small_dir").join("tiny.txt"), "hello").unwrap();

    dir
}

#[test]
fn cli_default_shows_directories() {
    let dir = create_test_dir();
    let output = rsdirstat_bin().arg(dir.path()).output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("big_dir"), "output should contain big_dir:\n{stdout}");
}

#[test]
fn cli_files_flag_shows_files() {
    let dir = create_test_dir();
    let output = rsdirstat_bin().arg(dir.path()).arg("--files").output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("large.bin"),
        "output should contain large.bin:\n{stdout}"
    );
}

#[test]
fn cli_top_limits_output() {
    let dir = create_test_dir();
    let output = rsdirstat_bin().arg(dir.path()).arg("--top").arg("1").output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "expected 1 line with --top 1, got {}:\n{stdout}",
        lines.len()
    );
}

#[test]
fn cli_largest_dir_appears_first() {
    let dir = create_test_dir();
    let output = rsdirstat_bin().arg(dir.path()).output().unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    // big_dir should appear before small_dir (root may appear first since it's largest)
    let big_pos = lines.iter().position(|l| l.contains("big_dir"));
    let small_pos = lines.iter().position(|l| l.contains("small_dir"));
    assert!(big_pos.is_some(), "big_dir should appear in output:\n{stdout}");
    assert!(small_pos.is_some(), "small_dir should appear in output:\n{stdout}");
    assert!(big_pos < small_pos, "big_dir should appear before small_dir:\n{stdout}");
}

#[test]
fn cli_nonexistent_path_fails() {
    let output = rsdirstat_bin().arg("/nonexistent/path/xyz").output().unwrap();
    assert!(!output.status.success());
}
