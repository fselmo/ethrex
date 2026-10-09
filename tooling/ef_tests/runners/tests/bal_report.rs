//! `--bal-report` prints the executor ethrex chose for each block, read from the
//! event the client emits, and `--no-bal-parallel-exec` and `--bal-withhold` reach
//! that choice.
//!
//! The fixtures are one Amsterdam block each, from execution-specs:
//! `test_bal_post_execution_calls_net_storage_at_last_index` (valid, with its
//! access list) and `test_bal_invalid_missing_system_contract_entry`
//! (`history_storage`), whose header commits to a list execution does not match.
//! `blocktest_mismatched_list.json` is the latter's tests@v21.0.1 `blockchain_test`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Map, Value};

const VALID_BLOCK: &str = "0x52f2953e505ce739d7c992a9d2f9d89f5f34b5a97f3a7e77dfe6484e3e687e41";
const MISMATCHED_BLOCK: &str = "0xff1a7dc74576788b6c2e5b039a0a623b943c4cea817c15276c223daee6578d80";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The `balExecution` lines the runner prints for `fixture`, after checking
/// every fixture passed.
fn bal_report(bin: &str, fixture: &Path, extra_args: &[&str]) -> Vec<Map<String, Value>> {
    let output = Command::new(bin)
        .args(["--json", "--bal-report", "--path"])
        .arg(fixture)
        .args(extra_args)
        .env_remove("ETHREX_NO_BAL_PARALLEL_EXEC")
        .output()
        .expect("failed to run the binary");
    let results: Vec<Value> =
        serde_json::from_slice(&output.stdout).expect("stdout is one JSON array");
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0]["pass"], true, "{}", results[0]);
    String::from_utf8(output.stderr)
        .expect("stderr is not UTF-8")
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str(line).expect("a report line is one JSON object"))
        .collect()
}

/// The one line the report holds for the fixture's block.
fn expected(hash: &str, path: &str, reason: &str) -> Vec<Map<String, Value>> {
    let line = serde_json::json!({
        "event": "balExecution",
        "block": 1,
        "hash": hash,
        "path": path,
        "reason": reason,
    });
    vec![line.as_object().unwrap().clone()]
}

#[test]
fn blocktest_reports_the_parallel_executor() {
    let bin = env!("CARGO_BIN_EXE_blocktest");
    let fixture = fixture("blocktest_bal_report.json");
    assert_eq!(
        bal_report(bin, &fixture, &[]),
        expected(VALID_BLOCK, "parallel", "")
    );
}

#[test]
fn blocktest_switch_reaches_the_executor() {
    let bin = env!("CARGO_BIN_EXE_blocktest");
    let fixture = fixture("blocktest_bal_report.json");
    assert_eq!(
        bal_report(bin, &fixture, &["--no-bal-parallel-exec"]),
        expected(VALID_BLOCK, "sequential", "disabled")
    );
}

/// A list the header does not commit to is dropped, and the block's line says so
/// whichever executor runs it.
#[test]
fn blocktest_reports_a_dropped_list() {
    let mut fixtures: Map<String, Value> = serde_json::from_str(
        &std::fs::read_to_string(fixture("blocktest_bal_report.json")).unwrap(),
    )
    .unwrap();
    let block = &mut fixtures["listed"]["blocks"][0];
    block["blockAccessList"].as_array_mut().unwrap().pop();
    let shortened = Path::new(env!("CARGO_TARGET_TMPDIR")).join("bal_report_dropped_list.json");
    std::fs::write(&shortened, Value::Object(fixtures).to_string()).unwrap();

    let bin = env!("CARGO_BIN_EXE_blocktest");
    for args in [&[][..], &["--no-bal-parallel-exec"]] {
        assert_eq!(
            bal_report(bin, &shortened, args),
            expected(VALID_BLOCK, "sequential", "bad-access-list")
        );
    }
}

/// A withheld block prints one line: the path of the two-pass check's run on the
/// list the import built, or `sequential` when no such run follows.
#[test]
fn blocktest_reports_a_withheld_list() {
    let bin = env!("CARGO_BIN_EXE_blocktest");
    let fixture = fixture("blocktest_bal_report.json");
    assert_eq!(
        bal_report(bin, &fixture, &["--bal-withhold"]),
        expected(VALID_BLOCK, "parallel", "withheld")
    );
    assert_eq!(
        bal_report(bin, &fixture, &["--bal-withhold", "--no-bal-parallel-exec"]),
        expected(VALID_BLOCK, "sequential", "withheld")
    );
}

/// A block the fixture expects to be rejected is not run again, so its one line is
/// the import's.
#[test]
fn blocktest_reports_a_withheld_rejected_block() {
    let bin = env!("CARGO_BIN_EXE_blocktest");
    let fixture = fixture("blocktest_mismatched_list.json");
    assert_eq!(
        bal_report(bin, &fixture, &["--bal-withhold"]),
        expected(MISMATCHED_BLOCK, "sequential", "withheld")
    );
}

#[test]
fn enginetest_reports_the_parallel_executor() {
    let bin = env!("CARGO_BIN_EXE_enginetest");
    let fixture = fixture("enginetest_bal_report.json");
    assert_eq!(
        bal_report(bin, &fixture, &[]),
        expected(VALID_BLOCK, "parallel", "")
    );
}

#[test]
fn enginetest_switch_reaches_the_executor() {
    let bin = env!("CARGO_BIN_EXE_enginetest");
    let fixture = fixture("enginetest_bal_report.json");
    assert_eq!(
        bal_report(bin, &fixture, &["--no-bal-parallel-exec"]),
        expected(VALID_BLOCK, "sequential", "disabled")
    );
}

/// The switch chooses the executor only: a list that does not match execution
/// makes the payload INVALID on either path, and the fixture expects that.
#[test]
fn enginetest_rejects_a_mismatched_list_in_both_modes() {
    let bin = env!("CARGO_BIN_EXE_enginetest");
    let fixture = fixture("enginetest_mismatched_list.json");
    assert_eq!(
        bal_report(bin, &fixture, &[]),
        expected(MISMATCHED_BLOCK, "parallel", "")
    );
    assert_eq!(
        bal_report(bin, &fixture, &["--no-bal-parallel-exec"]),
        expected(MISMATCHED_BLOCK, "sequential", "disabled")
    );
}
