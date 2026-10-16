//! Each result lists the blocks or payloads the client rejected, with the
//! client's own error, and the runner does not check that error itself.
//!
//! The fixtures are tests@v21.0.0 fixtures, some edited: `rejected_twice`
//! repeats its invalid block, `rejected_for_another_reason` names an exception
//! other than the one the block is rejected for, and `json_rpc_error` drops the
//! payload's `blockHash` and expects the resulting error code.

use std::collections::HashMap;
use std::process::Command;

use serde_json::Value;

/// The results for `fixture`, by fixture name.
fn results(bin: &str, fixture: &str) -> HashMap<String, Value> {
    let path = format!("{}/tests/fixtures/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(bin)
        .args(["--json", &path])
        .env_remove("ETHREX_ENGINE_STRICT_EXCEPTIONS")
        .output()
        .expect("failed to run the binary");
    let results: Vec<Value> =
        serde_json::from_slice(&output.stdout).expect("stdout is one JSON array");
    results
        .into_iter()
        .map(|r| (r["name"].as_str().unwrap().to_string(), r))
        .collect()
}

/// `(index, error)` of each rejection in `result`, checking each has a hash
/// exactly when `hashed`.
fn rejections_in(result: &Value, hashed: bool) -> Vec<(u64, String)> {
    result["rejections"]
        .as_array()
        .expect("rejections is an array")
        .iter()
        .map(|r| {
            assert_eq!(r.get("hash").is_some(), hashed, "{r}");
            (
                r["index"].as_u64().unwrap(),
                r["error"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn assert_passes(result: &Value) {
    assert_eq!(result["pass"], true, "{result}");
}

/// ethrex's error for the fixtures' invalid base fee.
const BASE_FEE_ERROR: &str = "Base fee per gas is incorrect";

#[test]
fn blocktest_reports_each_rejected_block() {
    let results = results(env!("CARGO_BIN_EXE_blocktest"), "blocktest_rejections.json");

    let rejected = &results["rejected_twice"];
    assert_passes(rejected);
    let rejections = rejections_in(rejected, true);
    assert_eq!(
        rejections.iter().map(|r| r.0).collect::<Vec<_>>(),
        [0, 1],
        "{rejected}"
    );
    assert!(rejections[0].1.contains(BASE_FEE_ERROR), "{rejected}");

    // A block that does not decode has no hash, and its error is the decoder's.
    let undecodable = &results["undecodable"];
    assert_passes(undecodable);
    let rejections = rejections_in(undecodable, false);
    assert_eq!(rejections.len(), 1, "{undecodable}");
    assert_eq!(rejections[0].0, 0);
    assert!(
        rejections[0].1.starts_with("Error decoding field"),
        "{undecodable}"
    );

    let clean = &results["clean"];
    assert_passes(clean);
    assert_eq!(clean["rejections"], serde_json::json!([]));
}

#[test]
fn blocktest_does_not_check_the_rejection_reason() {
    let results = results(env!("CARGO_BIN_EXE_blocktest"), "blocktest_rejections.json");
    let wrong = &results["rejected_for_another_reason"];
    assert_passes(wrong);
    let rejections = rejections_in(wrong, true);
    assert_eq!(rejections.len(), 1, "{wrong}");
    assert!(rejections[0].1.contains(BASE_FEE_ERROR), "{wrong}");
}

#[test]
fn enginetest_reports_each_rejected_payload() {
    let results = results(
        env!("CARGO_BIN_EXE_enginetest"),
        "enginetest_rejections.json",
    );

    let rejected = &results["rejected_twice"];
    assert_passes(rejected);
    let rejections = rejections_in(rejected, true);
    assert_eq!(
        rejections.iter().map(|r| r.0).collect::<Vec<_>>(),
        [0, 1],
        "{rejected}"
    );
    assert!(rejections[0].1.contains(BASE_FEE_ERROR), "{rejected}");

    // A JSON-RPC error has no hash, and its error is `<code>: <message>`.
    let rpc_error = &results["json_rpc_error"];
    assert_passes(rpc_error);
    let rejections = rejections_in(rpc_error, false);
    assert_eq!(rejections.len(), 1, "{rpc_error}");
    assert_eq!(rejections[0].0, 0);
    assert!(rejections[0].1.starts_with("-32602: "), "{rpc_error}");

    let clean = &results["clean"];
    assert_passes(clean);
    assert_eq!(clean["rejections"], serde_json::json!([]));
}

#[test]
fn enginetest_does_not_check_the_rejection_reason() {
    let results = results(
        env!("CARGO_BIN_EXE_enginetest"),
        "enginetest_rejections.json",
    );
    let wrong = &results["rejected_for_another_reason"];
    assert_passes(wrong);
    let rejections = rejections_in(wrong, true);
    assert_eq!(rejections.len(), 1, "{wrong}");
    assert!(rejections[0].1.contains(BASE_FEE_ERROR), "{wrong}");
}
