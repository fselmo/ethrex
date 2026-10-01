//! Run `blockchain_test_engine` fixtures through ethrex's Engine API handlers,
//! in process: each payload goes to `engine_newPayloadV<n>` and each forkchoice
//! update to `engine_forkchoiceUpdatedV<n>`, at the versions the fixture names.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use clap::Parser;
use ef_tests_engine::{EngineFixtureFile, RunOptions, run_fixture};
use ef_tests_runners::{FixtureResult, RunnerArgs, collect_json_files, report, run_files};

#[derive(Parser, Debug)]
#[command(
    name = "enginetest",
    about = "Run blockchain_test_engine fixtures through ethrex's Engine API"
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
    let fixtures: EngineFixtureFile = match std::fs::read_to_string(file)
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(fixtures) => fixtures,
        Err(e) => {
            let name = file.display().to_string();
            return vec![FixtureResult::new(name, String::new(), Some(e))];
        }
    };
    let opts = RunOptions::from_env();
    let mut results = Vec::new();
    for (name, fixture) in fixtures.iter().filter(|(name, _)| args.selects(name)) {
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            rt.block_on(run_fixture(name, fixture, &opts))
        }));
        let outcome = match outcome {
            Ok(Err(failure)) if failure.is_skip() => continue,
            Ok(result) => Ok(result.map_err(|failure| failure.to_string())),
            Err(panic) => Err(panic),
        };
        results.push(FixtureResult::from_outcome(
            name.clone(),
            fixture.network.clone(),
            outcome,
        ));
    }
    results
}
