//! Shared plumbing for the standalone `blocktest` and `enginetest` runners: fixture
//! discovery, the worker pool, the result output and the per-block executor report.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use clap::ArgAction;
use ef_tests_blockchain::test_runner::DROPPED_ACCESS_LIST_TARGET;
use ethrex_vm::BAL_EXECUTION_TARGET;
use regex::Regex;
use serde::Serialize;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

/// Options every runner takes.
#[derive(clap::Args, Debug)]
pub struct RunnerArgs {
    /// Fixture files or directories (searched recursively), all run in one process.
    #[arg(value_name = "PATH", required_unless_present = "path")]
    pub paths: Vec<PathBuf>,

    /// A fixture file or directory, run together with any PATH; may repeat.
    #[arg(short, long, value_name = "PATH", action = ArgAction::Append)]
    pub path: Vec<PathBuf>,

    /// Number of fixture files to run at once.
    #[arg(short, long, default_value_t = 1)]
    pub workers: usize,

    /// Only run fixtures whose name matches this regex.
    #[arg(long, value_name = "REGEX")]
    pub run: Option<Regex>,

    /// Print the results to stdout as a JSON array.
    #[arg(long)]
    pub json: bool,

    /// Run every block on the sequential executor, as the node flag of the same name does.
    #[arg(long, env = "ETHREX_NO_BAL_PARALLEL_EXEC")]
    pub no_bal_parallel_exec: bool,

    /// Print the executor chosen for each block as one JSON line on stderr.
    #[arg(long)]
    pub bal_report: bool,
}

impl RunnerArgs {
    /// Every fixture path given, by `--path` or as an argument.
    pub fn fixture_paths(&self) -> impl Iterator<Item = &Path> {
        self.path.iter().chain(&self.paths).map(PathBuf::as_path)
    }

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
    /// Every block or payload the client rejected, with its error as the client
    /// gave it. The runner does not check these against the fixture's expected
    /// exception; whoever reads the results can.
    pub rejections: Vec<Rejection>,
}

/// A block (in `blocks`) or payload (in `engineNewPayloads`) the client rejected.
#[derive(Debug, Serialize)]
pub struct Rejection {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    pub error: String,
}

impl From<ef_tests_blockchain::test_runner::Rejection> for Rejection {
    fn from(r: ef_tests_blockchain::test_runner::Rejection) -> Self {
        Self {
            index: r.index,
            hash: r.hash.map(|hash| format!("{hash:#x}")),
            error: r.error,
        }
    }
}

impl From<ef_tests_engine::Rejection> for Rejection {
    fn from(r: ef_tests_engine::Rejection) -> Self {
        Self {
            index: r.index,
            hash: r.hash.map(|hash| format!("{hash:#x}")),
            error: r.error,
        }
    }
}

impl FixtureResult {
    pub fn new(name: String, fork: String, error: Option<String>) -> Self {
        Self {
            name,
            pass: error.is_none(),
            fork,
            error: error.unwrap_or_default(),
            rejections: Vec::new(),
        }
    }

    /// A failed fixture from a `catch_unwind` result, so a panicking check
    /// fails that fixture instead of aborting the run.
    pub fn from_outcome(
        name: String,
        fork: String,
        outcome: Result<Result<(), String>, Box<dyn Any + Send>>,
        rejections: Vec<Rejection>,
    ) -> Self {
        let error = match outcome {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(panic) => Some(panic_message(panic)),
        };
        Self {
            rejections,
            ..Self::new(name, fork, error)
        }
    }
}

fn panic_message(panic: Box<dyn Any + Send>) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "panicked".to_string())
}

/// Every path that is a file, and every `.json` file under the paths that are
/// directories, sorted and each once. A path that does not exist, or a directory
/// that cannot be read, is an error rather than no fixtures.
pub fn collect_json_files<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_file() {
            files.push(path.to_path_buf());
            continue;
        }
        if !path.is_dir() {
            return Err(format!(
                "no such fixture file or directory: {}",
                path.display()
            ));
        }
        let mut dirs = vec![path.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            let entries = std::fs::read_dir(&dir)
                .map_err(|e| format!("cannot read directory {}: {e}", dir.display()))?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|ext| ext == "json") {
                    files.push(path);
                }
            }
        }
    }
    // A file named twice, or also found under a directory given, runs once.
    let mut seen = HashSet::new();
    files.retain(|file| seen.insert(file.canonicalize().unwrap_or_else(|_| file.clone())));
    files.sort();
    Ok(files)
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

/// With `--bal-report`, print the executor ethrex picks for each block as one
/// JSON line on stderr:
/// `{"event":"balExecution","block":N,"hash":"0x…","path":"parallel|sequential","reason":"…"}`.
/// `reason` is `bad-access-list` for a block whose delivered list the runner dropped
/// (the client then sees no list, so it would say `no-access-list`). Otherwise it is
/// empty for `parallel`, and for `sequential` the first gate that ruled parallel out.
/// Without the flag no subscriber is installed.
pub fn install_bal_report(args: &RunnerArgs) {
    if let Some(layer) = bal_report_layer(args, std::io::stderr) {
        tracing_subscriber::registry().with(layer).init();
    }
}

fn bal_report_layer<S, W>(args: &RunnerArgs, writer: W) -> Option<impl Layer<S>>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let filter = Targets::new()
        .with_target(BAL_EXECUTION_TARGET, Level::DEBUG)
        .with_target(DROPPED_ACCESS_LIST_TARGET, Level::DEBUG);
    args.bal_report.then(|| {
        BalExecutionReport {
            writer,
            dropped: Mutex::default(),
        }
        .with_filter(filter)
    })
}

struct BalExecutionReport<W> {
    writer: W,
    /// Delivered lists the runner dropped whose blocks have not run yet, by block hash.
    dropped: Mutex<HashMap<String, usize>>,
}

impl<S, W> Layer<S> for BalExecutionReport<W>
where
    S: Subscriber,
    W: for<'a> MakeWriter<'a> + 'static,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut line = BalExecution {
            event: "balExecution",
            ..Default::default()
        };
        event.record(&mut line);
        let mut dropped = self.dropped.lock().unwrap_or_else(|e| e.into_inner());
        if event.metadata().target() == DROPPED_ACCESS_LIST_TARGET {
            *dropped.entry(line.hash).or_default() += 1;
            return;
        }
        // Execution runs on a thread of its own, so the event can't be tied to the
        // import that dropped the list; the hash and the client's reason can. A block
        // whose list was dropped reaches the client with none, and ethrex's first gate
        // (with rayon) is the access list, so it always says `no-access-list`. Another
        // worker may run the same block with its list at the same time (a fixture and
        // its clean twin), and its event, which names any other reason, is left alone.
        if line.reason == "no-access-list"
            && let Some(pending) = dropped.get_mut(&line.hash)
        {
            *pending -= 1;
            if *pending == 0 {
                dropped.remove(&line.hash);
            }
            line.reason = "bad-access-list".to_string();
        }
        drop(dropped);
        if let Ok(mut line) = serde_json::to_string(&line) {
            line.push('\n');
            // One write per line, so lines from different workers don't interleave.
            let _ = self.writer.make_writer().write_all(line.as_bytes());
        }
    }
}

#[derive(Default, Serialize)]
struct BalExecution {
    event: &'static str,
    block: u64,
    hash: String,
    path: String,
    reason: String,
}

impl Visit for BalExecution {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "block" {
            self.block = value;
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "path" => self.path = value.to_string(),
            "reason" => self.reason = value.to_string(),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "hash" {
            self.hash = format!("{value:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        runner: RunnerArgs,
    }

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Buffer;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    enum Event {
        /// The runner dropped the block's delivered list.
        Dropped,
        /// The client ran the block on `path` for `reason`.
        Executed(&'static str, &'static str),
    }

    /// The report the runner prints, given its arguments, for these events of one
    /// block.
    fn report_for(args: &[&str], events: &[Event]) -> String {
        let cli = Cli::try_parse_from([&["runner", "--path", "fixtures"], args].concat())
            .expect("valid arguments");
        let buffer = Buffer::default();
        let subscriber =
            tracing_subscriber::registry().with(bal_report_layer(&cli.runner, buffer.clone()));
        tracing::subscriber::with_default(subscriber, || {
            for event in events {
                match *event {
                    Event::Dropped => tracing::debug!(
                        target: DROPPED_ACCESS_LIST_TARGET,
                        hash = %"0x01",
                        "Dropping the delivered access list"
                    ),
                    Event::Executed(path, reason) => tracing::debug!(
                        target: BAL_EXECUTION_TARGET,
                        block = 1u64,
                        hash = %"0x01",
                        path,
                        reason,
                        "Executing block"
                    ),
                }
            }
        });
        let bytes = buffer.0.lock().unwrap().clone();
        String::from_utf8(bytes).expect("utf-8 output")
    }

    fn line(path: &str, reason: &str) -> String {
        format!(
            "{{\"event\":\"balExecution\",\"block\":1,\"hash\":\"0x01\",\"path\":\"{path}\",\"reason\":\"{reason}\"}}\n"
        )
    }

    #[test]
    fn bal_report_prints_one_line_per_block() {
        assert_eq!(
            report_for(
                &["--bal-report"],
                &[Event::Executed("sequential", "disabled")]
            ),
            line("sequential", "disabled")
        );
    }

    #[test]
    fn bal_report_names_a_dropped_list() {
        assert_eq!(
            report_for(
                &["--bal-report"],
                &[
                    Event::Dropped,
                    Event::Executed("sequential", "no-access-list")
                ]
            ),
            line("sequential", "bad-access-list")
        );
        assert_eq!(
            report_for(
                &["--bal-report"],
                &[Event::Executed("sequential", "no-access-list")]
            ),
            line("sequential", "no-access-list")
        );
    }

    /// A fixture and its clean twin share the block hash and may run at once: the
    /// twin, which kept its list, keeps its own line.
    #[test]
    fn bal_report_leaves_the_same_block_with_its_list_alone() {
        for (path, reason) in [("parallel", ""), ("sequential", "disabled")] {
            assert_eq!(
                report_for(
                    &["--bal-report"],
                    &[
                        Event::Dropped,
                        Event::Executed(path, reason),
                        Event::Executed("sequential", "no-access-list"),
                    ]
                ),
                line(path, reason) + &line("sequential", "bad-access-list")
            );
        }
    }

    fn paths(args: &[&str]) -> Result<Vec<PathBuf>, clap::Error> {
        let cli = Cli::try_parse_from([&["runner"], args].concat())?;
        Ok(cli.runner.fixture_paths().map(Path::to_path_buf).collect())
    }

    #[test]
    fn takes_any_number_of_fixture_paths() {
        assert_eq!(
            paths(&["a.json", "dir", "-p", "b", "--path", "c"]).unwrap(),
            ["b", "c", "a.json", "dir"].map(PathBuf::from)
        );
        assert_eq!(paths(&["--path", "dir"]).unwrap(), [PathBuf::from("dir")]);
        assert!(paths(&["--json"]).is_err());
    }

    #[test]
    fn a_missing_fixture_path_is_an_error() {
        let missing = Path::new("no/such/fixtures");
        let err = collect_json_files([missing]).unwrap_err();
        assert!(err.contains("no/such/fixtures"), "{err}");
    }

    #[test]
    fn no_bal_report_without_the_flag() {
        assert_eq!(
            report_for(
                &[],
                &[
                    Event::Dropped,
                    Event::Executed("sequential", "no-access-list")
                ]
            ),
            ""
        );
    }
}
