//! Every fixture path given runs, in one process with one set of results.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A fresh directory holding `fixtures/a.json` and `b.json`. Neither parses, so
/// each is one failed result named after its file, which is all these tests need.
fn fixtures(test: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("fixtures")).unwrap();
    std::fs::write(root.join("fixtures/a.json"), "not json").unwrap();
    std::fs::write(root.join("b.json"), "not json").unwrap();
    root
}

fn run(bin: &str, args: &[&Path]) -> Output {
    Command::new(bin)
        .arg("--json")
        .args(args)
        .output()
        .expect("failed to run the binary")
}

/// The fixture names in the one JSON array on stdout.
fn names(output: &Output) -> Vec<String> {
    let results: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).expect("stdout is one JSON array");
    results
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect()
}

fn runs_every_path(bin: &str, test: &str) {
    let root = fixtures(test);
    let (a, b) = (root.join("fixtures/a.json"), root.join("b.json"));
    // Results are sorted by name, and `b.json` sorts before `fixtures/`.
    let expected = [b.display().to_string(), a.display().to_string()];

    let output = run(bin, &[&a, &b]);
    assert_eq!(names(&output), expected);
    assert!(!output.status.success(), "a failed fixture exits non-zero");

    // A directory and a file, and a file also found under that directory.
    assert_eq!(
        names(&run(bin, &[&root.join("fixtures"), &b, &a])),
        expected
    );
}

fn rejects_a_missing_path(bin: &str, test: &str) {
    let root = fixtures(test);
    let output = run(bin, &[&root.join("b.json"), &root.join("missing")]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no such fixture file or directory"),
        "{stderr}"
    );
}

#[test]
fn blocktest_runs_every_path() {
    runs_every_path(env!("CARGO_BIN_EXE_blocktest"), "blocktest_runs_every_path");
}

#[test]
fn enginetest_runs_every_path() {
    runs_every_path(
        env!("CARGO_BIN_EXE_enginetest"),
        "enginetest_runs_every_path",
    );
}

#[test]
fn blocktest_rejects_a_missing_path() {
    rejects_a_missing_path(env!("CARGO_BIN_EXE_blocktest"), "blocktest_missing");
}

#[test]
fn enginetest_rejects_a_missing_path() {
    rejects_a_missing_path(env!("CARGO_BIN_EXE_enginetest"), "enginetest_missing");
}
