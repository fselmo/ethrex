//! Shared plumbing for the standalone `blocktest` and `enginetest` runners: fixture
//! discovery, the worker pool and the result output.

use std::any::Any;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use regex::Regex;
use serde::Serialize;

/// Options every runner takes.
#[derive(clap::Args, Debug)]
pub struct RunnerArgs {
    /// Fixture directory (searched recursively) or a single .json file.
    #[arg(short, long, value_name = "PATH")]
    pub path: PathBuf,

    /// Number of fixture files to run at once.
    #[arg(short, long, default_value_t = 1)]
    pub workers: usize,

    /// Only run fixtures whose name matches this regex.
    #[arg(long, value_name = "REGEX")]
    pub run: Option<Regex>,

    /// Print the results to stdout as a JSON array.
    #[arg(long)]
    pub json: bool,
}

impl RunnerArgs {
    pub fn selects(&self, fixture_name: &str) -> bool {
        self.run.as_ref().is_none_or(|re| re.is_match(fixture_name))
    }
}

/// The verdict on one fixture.
#[derive(Debug, Serialize)]
pub struct FixtureResult {
    pub name: String,
    pub pass: bool,
    pub fork: String,
    pub error: String,
}

impl FixtureResult {
    pub fn new(name: String, fork: String, error: Option<String>) -> Self {
        Self {
            name,
            pass: error.is_none(),
            fork,
            error: error.unwrap_or_default(),
        }
    }

    /// A failed fixture from a `catch_unwind` result, so a panicking check
    /// fails that fixture instead of aborting the run.
    pub fn from_outcome(
        name: String,
        fork: String,
        outcome: Result<Result<(), String>, Box<dyn Any + Send>>,
    ) -> Self {
        let error = match outcome {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(panic) => Some(panic_message(panic)),
        };
        Self::new(name, fork, error)
    }
}

fn panic_message(panic: Box<dyn Any + Send>) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "panicked".to_string())
}

/// All `.json` files under `path`, or `path` itself if it is a file.
pub fn collect_json_files(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    let mut files = Vec::new();
    let mut dirs = vec![path.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("cannot read directory {}", dir.display());
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|ext| ext == "json") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Run `run_file` over every file from `workers` threads, returning the results
/// sorted by fixture name.
///
/// Plain threads rather than a rayon pool: block execution uses rayon itself, and
/// running it from inside a pool's worker would hand it that pool's threads.
pub fn run_files<F>(files: &[PathBuf], workers: usize, run_file: F) -> Vec<FixtureResult>
where
    F: Fn(&Path) -> Vec<FixtureResult> + Sync,
{
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..workers.max(1) {
            std::thread::Builder::new()
                .stack_size(64 * 1024 * 1024)
                .spawn_scoped(s, || {
                    while let Some(file) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let file_results = run_file(file);
                        results
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .extend(file_results);
                    }
                })
                .expect("failed to spawn a worker thread");
        }
    });
    let mut results = results.into_inner().unwrap_or_else(|e| e.into_inner());
    results.sort_by(|a, b| a.name.cmp(&b.name));
    results
}

/// Print the results (a JSON array on stdout with `json`, failures on stderr
/// otherwise) and a summary on stderr, and return the exit code.
pub fn report(results: &[FixtureResult], json: bool, started: Instant) -> ExitCode {
    if json {
        match serde_json::to_string_pretty(results) {
            Ok(out) => println!("{out}"),
            Err(e) => {
                eprintln!("failed to serialize the results: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        for r in results.iter().filter(|r| !r.pass) {
            eprintln!("FAIL: {} -- {}", r.name, r.error);
        }
    }
    let failed = results.iter().filter(|r| !r.pass).count();
    eprintln!(
        "\nTotal: {} | Passed: {} | Failed: {} | Time: {:.2}s",
        results.len(),
        results.len() - failed,
        failed,
        started.elapsed().as_secs_f64(),
    );
    if failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
