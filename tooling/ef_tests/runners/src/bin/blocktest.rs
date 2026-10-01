//! Run `blockchain_test` fixtures through ethrex's block import.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use clap::Parser;
use ef_tests_blockchain::test_runner::{SKIPPED_TESTS, run_ef_test, skip_reason};
use ef_tests_blockchain::types::TestUnit;
use ef_tests_runners::{FixtureResult, RunnerArgs, collect_json_files, report, run_files};

#[derive(Parser, Debug)]
#[command(
    name = "blocktest",
    about = "Run blockchain_test fixtures through ethrex"
)]
struct Cli {
    #[command(flatten)]
    runner: RunnerArgs,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let started = Instant::now();
    let rt = tokio::runtime::Runtime::new().expect("failed to build the tokio runtime");
    let files = collect_json_files(&cli.runner.path);
    let results = run_files(&files, cli.runner.workers, |file| {
        run_file(file, &cli.runner, &rt)
    });
    report(&results, cli.runner.json, started)
}

fn run_file(file: &Path, args: &RunnerArgs, rt: &tokio::runtime::Runtime) -> Vec<FixtureResult> {
    let tests: HashMap<String, TestUnit> = match std::fs::read_to_string(file)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(tests) => tests,
        Err(e) => {
            let name = file.display().to_string();
            return vec![FixtureResult::new(name, String::new(), Some(e))];
        }
    };
    let mut tests: Vec<_> = tests.into_iter().collect();
    tests.sort_by(|a, b| a.0.cmp(&b.0));
    tests
        .into_iter()
        .filter(|(name, test)| {
            args.selects(name) && skip_reason(name, test, Some(SKIPPED_TESTS)).is_none()
        })
        .map(|(name, test)| {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                rt.block_on(run_ef_test(&name, &test, false))
            }));
            FixtureResult::from_outcome(name, format!("{:?}", test.network), outcome)
        })
        .collect()
}
