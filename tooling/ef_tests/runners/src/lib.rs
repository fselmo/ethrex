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
use ef_tests_blockchain::test_runner::{DROPPED_ACCESS_LIST_TARGET, WITHHELD_ACCESS_LIST_TARGET};
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

    /// Run without the per-block precompile result cache, as the node flag of the same
    /// name does.
    #[arg(long, env = "ETHREX_NO_PRECOMPILE_CACHE")]
    pub no_precompile_cache: bool,

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

    /// How `enginetest` executes each fixture's payloads.
    pub fn enginetest_options(&self) -> ef_tests_engine::RunOptions {
        ef_tests_engine::RunOptions {
            bal_parallel_exec: !self.no_bal_parallel_exec,
            precompile_cache: !self.no_precompile_cache,
            ..ef_tests_engine::RunOptions::from_env()
        }
    }
}

/// Options `blocktest` takes.
#[derive(clap::Args, Debug)]
pub struct BlocktestArgs {
    #[command(flatten)]
    pub runner: RunnerArgs,

    /// Import every block without its access list, so ethrex runs it sequentially and
    /// checks the list it builds against the header, then run it again in parallel on
    /// that list.
    #[arg(long)]
    pub bal_withhold: bool,
}

impl BlocktestArgs {
    /// How `blocktest` executes each fixture's blocks.
    pub fn blocktest_options(&self) -> ef_tests_blockchain::test_runner::RunOptions {
        ef_tests_blockchain::test_runner::RunOptions {
            bal_parallel_exec: !self.runner.no_bal_parallel_exec,
            precompile_cache: !self.runner.no_precompile_cache,
            withhold_access_lists: self.bal_withhold,
        }
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
///
/// A block the runner imported without its list (`--bal-withhold`) also gets one line,
/// with `reason` `withheld`, printed once its outcome is known: with the `path` of the
/// two-pass check's parallel run when that runs it, and `sequential` otherwise. When
/// that run rejects a block the import accepted, a
/// `{"event":"balFallback","block":N,"hash":"0x…","parallelError":"…","sequentialResult":"valid","sequentialError":""}`
/// line follows.
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
        .with_target(DROPPED_ACCESS_LIST_TARGET, Level::DEBUG)
        .with_target(WITHHELD_ACCESS_LIST_TARGET, Level::DEBUG);
    args.bal_report.then(|| {
        BalExecutionReport {
            writer,
            pending: Mutex::default(),
        }
        .with_filter(filter)
    })
}

struct BalExecutionReport<W> {
    writer: W,
    pending: Mutex<Pending>,
}

#[derive(Default)]
struct Pending {
    /// The runner's events waiting for their block's line, by kind and block hash.
    events: HashMap<(&'static str, String), usize>,
    /// Lines of withheld blocks waiting for the block's outcome, by block hash.
    held: HashMap<String, Vec<BalExecution>>,
}

impl Pending {
    fn add(&mut self, kind: &'static str, hash: String) {
        *self.events.entry((kind, hash)).or_default() += 1;
    }

    /// Use up one of the `kind` events waiting for the block `hash`, if any.
    fn take(&mut self, kind: &'static str, hash: &str) -> bool {
        let key = (kind, hash.to_string());
        let Some(count) = self.events.get_mut(&key) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            self.events.remove(&key);
        }
        true
    }

    /// One held line of the block `hash`, if any.
    fn release(&mut self, hash: &str) -> Option<BalExecution> {
        let lines = self.held.get_mut(hash)?;
        let line = lines.pop();
        if lines.is_empty() {
            self.held.remove(hash);
        }
        line
    }
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
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let target = event.metadata().target();
        if target == DROPPED_ACCESS_LIST_TARGET {
            pending.add("dropped", line.hash);
            return;
        }
        // A withheld block runs up to three times: the import, the two-pass check's
        // run that only collects its list, and the check's run on that list. The
        // import's line is held until the runner says which run decides the block.
        if target == WITHHELD_ACCESS_LIST_TARGET {
            match line.step.as_str() {
                "import" => pending.add("import", line.hash),
                "collect" => pending.add("collect", line.hash),
                "rerun" => {
                    pending.release(&line.hash);
                    pending.add("rerun", line.hash);
                }
                "done" => match pending.release(&line.hash) {
                    Some(held) => self.write(&held),
                    // The import rejected the block before executing it.
                    None => {
                        pending.take("import", &line.hash);
                    }
                },
                "rejected" => self.write(&BalFallback {
                    event: "balFallback",
                    block: line.block,
                    hash: &line.hash,
                    parallel_error: &line.error,
                    sequential_result: "valid",
                    sequential_error: "",
                }),
                _ => {}
            }
            return;
        }
        // Execution runs on a thread of its own, so the event can't be tied to the
        // import that dropped or withheld the list; the hash and the client's reason
        // can. Such a block reaches the client with no list, and ethrex's first gate
        // (with rayon) is the access list, so it always says `no-access-list`, as does
        // the run that collects a withheld block's list. Another worker may run the
        // same block with its list at the same time (a fixture and its clean twin),
        // and its event, which names any other reason, is left alone.
        if line.reason == "no-access-list" {
            if pending.take("import", &line.hash) {
                line.reason = "withheld".to_string();
                pending
                    .held
                    .entry(line.hash.clone())
                    .or_default()
                    .push(line);
                return;
            } else if pending.take("collect", &line.hash) {
                return;
            } else if pending.take("dropped", &line.hash) {
                line.reason = "bad-access-list".to_string();
            }
        } else if pending.take("rerun", &line.hash) {
            line.reason = "withheld".to_string();
        }
        drop(pending);
        self.write(&line);
    }
}

impl<W> BalExecutionReport<W>
where
    W: for<'a> MakeWriter<'a> + 'static,
{
    fn write(&self, line: &impl Serialize) {
        if let Ok(mut line) = serde_json::to_string(line) {
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
    /// The runner's events only.
    #[serde(skip)]
    step: String,
    #[serde(skip)]
    error: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BalFallback<'a> {
    event: &'static str,
    block: u64,
    hash: &'a str,
    parallel_error: &'a str,
    sequential_result: &'static str,
    sequential_error: &'static str,
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
            "step" => self.step = value.to_string(),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "hash" => self.hash = format!("{value:?}"),
            "error" => self.error = format!("{value:?}"),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use clap::Parser;
    use ef_tests_blockchain::test_runner::fixture_blockchain;
    use ef_tests_engine::EngineApiHarness;
    use ethrex_storage::{EngineType, Store};

    use super::*;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        runner: RunnerArgs,
    }

    #[derive(Parser)]
    struct BlocktestCli {
        #[command(flatten)]
        args: BlocktestArgs,
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

    #[derive(Clone, Copy)]
    enum Event {
        /// The runner dropped the block's delivered list.
        Dropped,
        /// The runner, withholding the block's list, reached this step.
        Withheld(&'static str),
        /// The two-pass check's run on the list rejected the withheld block.
        Rejected,
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
                    Event::Withheld(step) => tracing::debug!(
                        target: WITHHELD_ACCESS_LIST_TARGET,
                        block = 1u64,
                        hash = %"0x01",
                        step,
                        "Withheld access list"
                    ),
                    Event::Rejected => tracing::debug!(
                        target: WITHHELD_ACCESS_LIST_TARGET,
                        block = 1u64,
                        hash = %"0x01",
                        step = "rejected",
                        error = %"parallel failed",
                        "Withheld access list"
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

    /// The events of a withheld block the two-pass check runs again on its list.
    const WITHHELD_AND_RERUN: [Event; 6] = [
        Event::Withheld("import"),
        Event::Executed("sequential", "no-access-list"),
        Event::Withheld("collect"),
        Event::Executed("sequential", "no-access-list"),
        Event::Withheld("rerun"),
        Event::Executed("parallel", ""),
    ];

    /// A withheld block prints one line, with the path of the run that decides it.
    #[test]
    fn bal_report_prints_one_line_for_a_withheld_block() {
        assert_eq!(
            report_for(&["--bal-report"], &WITHHELD_AND_RERUN),
            line("parallel", "withheld")
        );
        assert_eq!(
            report_for(
                &["--bal-report"],
                &[
                    Event::Withheld("import"),
                    Event::Executed("sequential", "no-access-list"),
                    Event::Withheld("done"),
                ]
            ),
            line("sequential", "withheld")
        );
    }

    /// A withheld block the import rejects before executing it prints nothing. Nothing
    /// is left over from an earlier run of the same block for it to print, and nothing
    /// of it is left over to change a later run's line.
    #[test]
    fn bal_report_prints_nothing_for_a_withheld_block_never_executed() {
        let mut events = WITHHELD_AND_RERUN.to_vec();
        events.extend([
            Event::Withheld("import"),
            Event::Withheld("done"),
            Event::Executed("sequential", "no-access-list"),
        ]);
        assert_eq!(
            report_for(&["--bal-report"], &events),
            line("parallel", "withheld") + &line("sequential", "no-access-list")
        );
    }

    /// When the run on the list rejects a block the import accepted, a fallback line
    /// follows the block's line.
    #[test]
    fn bal_report_prints_a_fallback_when_the_runs_disagree() {
        let mut events = WITHHELD_AND_RERUN.to_vec();
        events.push(Event::Rejected);
        assert_eq!(
            report_for(&["--bal-report"], &events),
            line("parallel", "withheld")
                + "{\"event\":\"balFallback\",\"block\":1,\"hash\":\"0x01\",\"parallelError\":\"parallel failed\",\"sequentialResult\":\"valid\",\"sequentialError\":\"\"}\n"
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

    /// Each switch reaches the `Blockchain` both runners import blocks with, which
    /// reads it whenever it executes a block.
    #[tokio::test]
    async fn switches_reach_the_blockchain() {
        const GENESIS: &str = include_str!("../../../../fixtures/genesis/l1.json");
        for (args, bal_parallel_exec, precompile_cache) in [
            (&[][..], true, true),
            (&["--no-bal-parallel-exec"][..], false, true),
            (&["--no-precompile-cache"][..], true, false),
        ] {
            let cli = BlocktestCli::try_parse_from([&["runner", "fixtures"], args].concat())
                .expect("valid arguments");
            let store = Store::new("", EngineType::InMemory).expect("in-memory store");
            let blockchain = fixture_blockchain(store, &cli.args.blocktest_options());
            let genesis = serde_json::from_str(GENESIS).expect("genesis parses");
            let harness =
                EngineApiHarness::from_genesis(genesis, &cli.args.runner.enginetest_options())
                    .await
                    .expect("harness builds");
            for (runner, options) in [
                ("blocktest", &blockchain.options),
                ("enginetest", &harness.ctx.blockchain.options),
            ] {
                assert_eq!(
                    (
                        options.bal_parallel_exec_enabled,
                        options.precompile_cache_enabled
                    ),
                    (bal_parallel_exec, precompile_cache),
                    "{runner} {args:?}: (bal_parallel_exec, precompile_cache)"
                );
            }
        }
    }

    /// `--bal-withhold` reaches the options `blocktest` imports with, and
    /// `enginetest`, which takes the shared options alone, refuses it.
    #[test]
    fn bal_withhold_is_a_blocktest_switch() {
        for (args, withhold) in [(&[][..], false), (&["--bal-withhold"][..], true)] {
            let cli = BlocktestCli::try_parse_from([&["runner", "fixtures"], args].concat())
                .expect("valid arguments");
            assert_eq!(
                cli.args.blocktest_options().withhold_access_lists,
                withhold,
                "{args:?}"
            );
        }
        assert!(Cli::try_parse_from(["runner", "fixtures", "--bal-withhold"]).is_err());
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
