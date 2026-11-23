//! The `kioku` command line interface.
//!
//! Output channels: the commands that start a child process (`run`, `verify`) let
//! the child keep stdin, stdout and stderr, so kioku writes its own messages and
//! its `--json` events on stderr. The commands that start no child (`status`,
//! `explain`, `clean`) write on stdout.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde_json::json;

use kioku::fingerprint::Mode;
use kioku::runner::{self, Decision, Request};

/// Version of the JSON schema emitted by `--json`.
const SCHEMA: &str = "kioku/v1";

const AFTER_HELP: &str = "\
Examples:
  kioku run -- cargo test
  kioku run --trust-trace -- /absolute/path/to/command
  kioku run --require-output -- cargo test
  kioku quarantine reset -- /absolute/path/to/command
  kioku clean --all

By default commands always execute. --trust-trace asserts deterministic behavior
and stable inputs, including atime insensitivity. Unsupported observations execute
without reuse. Traced reuse is for exit status only; output is not replayed.
--require-output always executes. AST requests use bytes.
--verify-rate accepts a fraction between 0 and 1; 1 verifies every eligible hit.
Command argv must be UTF-8. With --json, run and verify write kioku JSON to
stderr and preserve child stdout; commands without a child write JSON to stdout.
Errors are written to stderr.

Exit codes: 0 success, 2 usage, 3 internal error, 126/127 launch failure;
otherwise the child exit code. A trace with unknown child status is not retried.
KIOKU_DIR overrides the store; KIOKU_DISABLE disables reuse.";

#[derive(Parser, Debug)]
#[command(
    name = "kioku",
    version,
    about = "Cache the result of a command on the content of the code",
    after_help = AFTER_HELP,
    disable_help_subcommand = true
)]
struct Cli {
    /// Machine readable output, schema kioku/v1 (run/verify: stderr; others: stdout; errors: stderr).
    #[arg(long, global = true)]
    json: bool,
    /// Do not print the messages of kioku.
    #[arg(long, global = true)]
    quiet: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run a command, reusing a cached success when nothing changed.
    ///
    /// Command argv must be UTF-8. JSON goes to stderr; child stdout is preserved.
    Run(CommandArgs),
    /// Always run the command and compare with what kioku would have decided.
    ///
    /// Command argv must be UTF-8. JSON goes to stderr; child stdout is preserved.
    Verify(CommandArgs),
    /// Explain why a command is a cache hit or a cache miss.
    ///
    /// Command argv must be UTF-8. JSON goes to stdout; errors go to stderr.
    Explain(CommandArgs),
    /// Show the state of the cache.
    Status,
    /// Remove the entries of the cache.
    Clean(CleanArgs),
    /// Manage persistent command quarantine.
    Quarantine(QuarantineArgs),
    /// Run only the tests that depend on what changed; every skipped test is journaled.
    ///
    /// Compares the working tree with the last known green state of the base branch (see
    /// `kioku select base`). A partial run is never a full green. Exit codes: 0 partial
    /// green, 1 red, 3 incomplete or selection error (the full suite was run), 5 full green.
    Test(TestArgs),
    /// Prepare and inspect the test selection (`base`, `learn`, `plan`).
    Select(SelectArgs),
}

#[derive(Args, Debug)]
struct SelectRepoArgs {
    /// Repository root.
    #[arg(long, value_name = "DIR", default_value = ".")]
    repo: PathBuf,
    /// Selection store (default: `$KIOKU_SELECT_STORE`, then `$XDG_CACHE_HOME/kioku/select`, then `~/.cache/kioku/select`).
    #[arg(long, value_name = "DIR")]
    store: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum TestRunner {
    Nextest,
    Vitest,
}

#[path = "select/vitest.rs"]
mod vitest;

#[derive(Args, Debug)]
struct TestArgs {
    #[command(flatten)]
    location: SelectRepoArgs,
    /// Test runner. Vitest runs the full suite as a conservative fallback.
    #[arg(long, value_enum, default_value = "nextest")]
    runner: TestRunner,
    /// Base reference (default: main, else master).
    #[arg(long, value_name = "REF")]
    base: Option<String>,
    /// `d1` proposal (JSON `{"add": [...], "skip": [...]}`): it can only add tests.
    #[arg(long, value_name = "FILE")]
    d1: Option<PathBuf>,
    /// Return the last nextest nonzero code; success may be partial.
    /// Both full and partial green return 0: check status and scope.
    /// Selection errors remain errors even if nextest succeeds.
    #[arg(long)]
    native_exit_code: bool,
    /// nextest filter expression; a filtered run is never a full green.
    #[arg(short = 'E', long = "filter", value_name = "EXPR")]
    filter: Option<String>,
    /// Cargo features (repeatable or comma separated).
    #[arg(long = "features", value_name = "FEATURE", value_delimiter = ',')]
    features: Vec<String>,
    /// Where to write the journal (default: target/kioku-select/journal.json).
    #[arg(long, value_name = "PATH")]
    journal: Option<PathBuf>,
    /// Timeout of each nextest run, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = kioku::select::exec::DEFAULT_TIMEOUT_SECS)]
    timeout: u64,
}

#[derive(Args, Debug)]
struct SelectArgs {
    #[command(subcommand)]
    action: SelectAction,
}

#[derive(Subcommand, Debug)]
enum SelectAction {
    /// Inventory the clean checked-out base: tests, `JUnit` results, dependency graph.
    Base {
        #[command(flatten)]
        location: SelectRepoArgs,
        /// Inventory runner (Vitest uses its local list and JSON reporter).
        #[arg(long, value_enum, default_value = "nextest")]
        runner: TestRunner,
        /// `JUnit` of a full run of the base (default: run `cargo nextest run`).
        #[arg(long, value_name = "FILE")]
        junit: Option<PathBuf>,
        /// Cargo features (repeatable or comma separated).
        #[arg(long = "features", value_name = "FEATURE", value_delimiter = ',')]
        features: Vec<String>,
        /// Timeout of the nextest run, in seconds.
        #[arg(long, value_name = "SECONDS", default_value_t = kioku::select::exec::DEFAULT_TIMEOUT_SECS)]
        timeout: u64,
    },
    /// Learn the edges the dependency graph misses, from per-test coverage of the base.
    Learn {
        #[command(flatten)]
        location: SelectRepoArgs,
        /// Base commit or reference (default: HEAD).
        #[arg(long, value_name = "REF")]
        base: Option<String>,
        /// Per-test coverage already measured (`lines.json`).
        #[arg(
            long,
            value_name = "FILE",
            conflicts_with = "run",
            required_unless_present = "run"
        )]
        coverage_json: Option<PathBuf>,
        /// Measure the coverage now (instrumented run of the base).
        #[arg(long)]
        run: bool,
        /// Timeout of the coverage run, in seconds.
        #[arg(long, value_name = "SECONDS", default_value_t = kioku::select::exec::DEFAULT_TIMEOUT_SECS)]
        timeout: u64,
    },
    /// Like `kioku test` without running anything (status `planned`).
    Plan {
        #[command(flatten)]
        location: SelectRepoArgs,
        /// Base reference (default: main, else master).
        #[arg(long, value_name = "REF")]
        base: Option<String>,
        /// `d1` proposal (JSON): it can only add tests.
        #[arg(long, value_name = "FILE")]
        d1: Option<PathBuf>,
        /// nextest filter expression.
        #[arg(short = 'E', long = "filter", value_name = "EXPR")]
        filter: Option<String>,
        /// Cargo features (repeatable or comma separated).
        #[arg(long = "features", value_name = "FEATURE", value_delimiter = ',')]
        features: Vec<String>,
        /// Also write the planned journal here.
        #[arg(long, value_name = "PATH")]
        journal: Option<PathBuf>,
    },
}

#[derive(Args, Debug)]
struct QuarantineArgs {
    #[command(subcommand)]
    action: QuarantineAction,
}

#[derive(Subcommand, Debug)]
enum QuarantineAction {
    /// Clear quarantine and force discovery and learning again.
    ///
    /// Command argv must be UTF-8. JSON goes to stdout; errors go to stderr.
    Reset(ResetArgs),
}

#[derive(Args, Debug)]
struct ResetArgs {
    /// Exact command in UTF-8, after `--`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    cmd: Vec<String>,
}

#[derive(Args, Debug, Clone)]
struct CommandArgs {
    /// Assert deterministic behavior and stable inputs for traced reuse.
    #[arg(long)]
    trust_trace: bool,
    /// Accepted; currently has no effect. The complete initial environment is already part of the traced identity.
    #[arg(long)]
    strict_env: bool,
    /// Execute the child when its output is required.
    #[arg(long)]
    require_output: bool,
    /// Fraction of eligible hits to verify by executing the child.
    #[arg(long, value_parser = parse_verify_rate, default_value_t = 0.0)]
    verify_rate: f64,
    /// Fingerprint mode: ast or bytes.
    #[arg(long, value_enum)]
    mode: Option<ModeArg>,
    /// Restrict the tracked inputs to these globs.
    #[arg(long = "inputs", value_name = "GLOB")]
    inputs: Vec<String>,
    /// Declare an environment variable as part of the cache key.
    #[arg(long = "env", value_name = "VAR")]
    env: Vec<String>,
    /// The command to run, after `--`.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        required = true,
        value_name = "CMD"
    )]
    cmd: Vec<String>,
}

#[derive(Args, Debug)]
struct CleanArgs {
    /// Remove the entries older than this duration, for example 7d.
    #[arg(long = "older-than", value_name = "DURATION")]
    older_than: Option<String>,
    /// Remove every entry.
    #[arg(long)]
    all: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ModeArg {
    /// Ignore whitespace and ordinary comments.
    Ast,
    /// Count every byte.
    Bytes,
}

impl From<ModeArg> for Mode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::Ast => Self::Ast,
            ModeArg::Bytes => Self::Bytes,
        }
    }
}

/// Where and how kioku prints.
struct Out {
    json: bool,
    quiet: bool,
    to_stderr: bool,
}

impl Out {
    fn human(&self, message: &str) {
        if self.json || self.quiet {
            return;
        }
        if self.to_stderr {
            eprintln!("{message}");
        } else {
            println!("{message}");
        }
    }

    fn event(&self, value: &serde_json::Value) {
        if !self.json {
            return;
        }
        let line = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
        if self.to_stderr {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
}

fn print_error(json: bool, kind: &str, message: &str) {
    if json {
        eprintln!(
            "{}",
            json!({"schema": SCHEMA, "error": {"kind": kind, "message": message}})
        );
    } else {
        eprintln!("kioku: {message}");
    }
}

fn print_command_error(json: bool, request: &Request, error: &runner::Error) {
    let configured = kioku::key::repository(&request.cwd)
        .ok()
        .and_then(|(root, _)| kioku::config::mode(&root, &request.argv).ok().flatten());
    let ast = request.mode.or(configured) == Some(Mode::Ast);
    if json {
        eprintln!(
            "{}",
            json!({"schema": SCHEMA, "error": {"kind": "runtime", "message": error.to_string()},
            "ast_contract": ast.then_some("caller-asserted-trivia-insensitive")})
        );
    } else {
        if ast {
            eprintln!(
                "kioku: caller-asserted-trivia-insensitive; source-text readers may produce false greens"
            );
        }
        eprintln!("kioku: {error}");
    }
}

fn print_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("kioku: warning: {warning}");
    }
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => return usage_error(&error),
    };
    dispatch(cli)
}

fn usage_error(error: &clap::Error) -> ExitCode {
    use clap::error::ErrorKind;
    match error.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
            print!("{error}");
            ExitCode::SUCCESS
        }
        _ => {
            let args: Vec<_> = std::env::args_os().take_while(|arg| arg != "--").collect();
            let wants_json = args.iter().any(|arg| arg == "--json");
            let ast = args.iter().any(|arg| arg == "--mode=ast")
                || args
                    .windows(2)
                    .any(|pair| pair[0] == "--mode" && pair[1] == "ast");
            if wants_json {
                let payload = json!({
                    "schema": SCHEMA,
                    "error": {"kind": "usage", "message": error.to_string()},
                    "ast_contract": ast.then_some("caller-asserted-trivia-insensitive"),
                });
                eprintln!("{payload}");
            } else {
                if ast {
                    eprintln!(
                        "kioku: caller-asserted-trivia-insensitive; source-text readers may produce false greens"
                    );
                }
                eprint!("{error}");
            }
            ExitCode::from(2)
        }
    }
}

fn dispatch(cli: Cli) -> ExitCode {
    match cli.command {
        Command::Run(args) => run_or_verify(cli.json, cli.quiet, &args, false),
        Command::Verify(args) => run_or_verify(cli.json, cli.quiet, &args, true),
        Command::Explain(args) => explain(cli.json, cli.quiet, &args),
        Command::Status => status(cli.json, cli.quiet),
        Command::Clean(args) => clean(cli.json, cli.quiet, &args),
        Command::Quarantine(args) => quarantine(cli.json, cli.quiet, &args),
        Command::Test(args) => select_test(cli.json, cli.quiet, &args),
        Command::Select(args) => select_dispatch(cli.json, cli.quiet, args.action),
    }
}

/// Exit code of a selection command that could not even start (usage or internal error).
fn select_failure(json: bool, error: &kioku::select::SelectError) -> ExitCode {
    print_error(json, "select", &error.to_string());
    ExitCode::from(3)
}

fn print_select_report(
    json: bool,
    quiet: bool,
    plan_only: bool,
    report: &kioku::select::exec::TestReport,
) -> ExitCode {
    for note in &report.notes {
        eprintln!("{note}");
    }
    if json {
        println!("{}", report.summary_json());
    } else if !quiet {
        for line in report.human_lines(plan_only) {
            println!("{line}");
        }
    }
    ExitCode::from(report.exit_code)
}

/// What `kioku test` and `kioku select plan` share.
struct RunSpec {
    base: Option<String>,
    d1: Option<PathBuf>,
    filter: Option<String>,
    features: Vec<String>,
    journal: Option<PathBuf>,
    timeout: u64,
    plan_only: bool,
}

fn test_options(
    location: &SelectRepoArgs,
    spec: RunSpec,
) -> Result<kioku::select::exec::TestOptions, kioku::select::SelectError> {
    Ok(kioku::select::exec::TestOptions {
        repo: location.repo.clone(),
        store: kioku::select::exec::Store::resolve(location.store.as_deref())?,
        base_ref: spec.base,
        d1: spec.d1,
        user_filter: spec.filter,
        features: spec.features,
        journal: spec.journal,
        timeout: Duration::from_secs(spec.timeout),
        plan_only: spec.plan_only,
    })
}

fn compatible_exit_code(code: u8, native: bool) -> u8 {
    if native && code == kioku::select::exec::EXIT_FULL_GREEN {
        kioku::select::exec::EXIT_PARTIAL_GREEN
    } else {
        code
    }
}

fn select_test(json: bool, quiet: bool, args: &TestArgs) -> ExitCode {
    if matches!(args.runner, TestRunner::Vitest) {
        if args.base.is_some()
            || args.d1.is_some()
            || args.filter.is_some()
            || !args.features.is_empty()
            || args.journal.is_some()
            || args.location.store.is_some()
            || args.timeout != kioku::select::exec::DEFAULT_TIMEOUT_SECS
        {
            print_error(
                json,
                "select",
                "Vitest fallback does not support selection options",
            );
            return ExitCode::from(2);
        }
        return vitest::run(&args.location.repo, json, quiet);
    }
    let options = test_options(
        &args.location,
        RunSpec {
            base: args.base.clone(),
            d1: args.d1.clone(),
            filter: args.filter.clone(),
            features: args.features.clone(),
            journal: args.journal.clone(),
            timeout: args.timeout,
            plan_only: false,
        },
    );
    match options.and_then(|options| kioku::select::exec::run_test(&options)) {
        Ok(mut report) => {
            report.exit_code = if args.native_exit_code {
                report
                    .native_exit_code
                    .filter(|code| *code != 0)
                    .unwrap_or_else(|| compatible_exit_code(report.exit_code, true))
            } else {
                report.exit_code
            };
            print_select_report(json, quiet, false, &report)
        }
        Err(error) => select_failure(json, &error),
    }
}

fn select_dispatch(json: bool, quiet: bool, action: SelectAction) -> ExitCode {
    match action {
        SelectAction::Base {
            location,
            runner,
            junit,
            features,
            timeout,
        } => {
            if matches!(runner, TestRunner::Vitest) {
                if junit.is_some()
                    || !features.is_empty()
                    || timeout != kioku::select::exec::DEFAULT_TIMEOUT_SECS
                {
                    print_error(
                        json,
                        "select",
                        "Vitest base requires native results and default options",
                    );
                    return ExitCode::from(2);
                }
                print_error(
                    json,
                    "select",
                    "Vitest base preparation unavailable; full execution only",
                );
                return ExitCode::from(3);
            }
            select_base(json, quiet, &location, junit, features, timeout)
        }
        SelectAction::Learn {
            location,
            base,
            coverage_json,
            run: _,
            timeout,
        } => select_learn(json, quiet, &location, base, coverage_json, timeout),
        SelectAction::Plan {
            location,
            base,
            d1,
            filter,
            features,
            journal,
        } => {
            let options = test_options(
                &location,
                RunSpec {
                    base,
                    d1,
                    filter,
                    features,
                    journal,
                    timeout: kioku::select::exec::DEFAULT_TIMEOUT_SECS,
                    plan_only: true,
                },
            );
            match options.and_then(|options| kioku::select::exec::run_test(&options)) {
                Ok(report) => print_select_report(json, quiet, true, &report),
                Err(error) => select_failure(json, &error),
            }
        }
    }
}

fn select_base(
    json: bool,
    quiet: bool,
    location: &SelectRepoArgs,
    junit: Option<PathBuf>,
    features: Vec<String>,
    timeout: u64,
) -> ExitCode {
    let outcome =
        kioku::select::exec::Store::resolve(location.store.as_deref()).and_then(|store| {
            kioku::select::exec::build_base(&kioku::select::exec::BaseOptions {
                repo: location.repo.clone(),
                store,
                junit,
                features,
                timeout: Duration::from_secs(timeout),
            })
        });
    match outcome {
        Ok(report) => {
            if json {
                println!(
                    "{}",
                    json!({
                        "schema": kioku::select::BASE_SCHEMA,
                        "commit": report.commit,
                        "binaries": report.binaries,
                        "tests": report.tests,
                        "green": report.green,
                        "graph_digest": report.graph_digest,
                        "path": report.path.display().to_string(),
                        "elapsed_ms": u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX),
                        "nextest_ms": u64::try_from(report.nextest_elapsed.as_millis()).unwrap_or(u64::MAX),
                    })
                );
            } else if !quiet {
                println!(
                    "kioku select base: {} — {} binaires, {} tests, {} ({:.1} s dont nextest {:.1} s) -> {}",
                    report.commit,
                    report.binaries,
                    report.tests,
                    if report.green { "VERT" } else { "NON VERT" },
                    report.elapsed.as_secs_f64(),
                    report.nextest_elapsed.as_secs_f64(),
                    report.path.display()
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => select_failure(json, &error),
    }
}

fn select_learn(
    json: bool,
    quiet: bool,
    location: &SelectRepoArgs,
    base: Option<String>,
    coverage_json: Option<PathBuf>,
    timeout: u64,
) -> ExitCode {
    let outcome =
        kioku::select::exec::Store::resolve(location.store.as_deref()).and_then(|store| {
            kioku::select::exec::learn_from_coverage(&kioku::select::exec::LearnOptions {
                repo: location.repo.clone(),
                store,
                base_ref: base,
                coverage_json,
                timeout: Duration::from_secs(timeout),
            })
        });
    match outcome {
        Ok(report) => {
            if json {
                println!(
                    "{}",
                    json!({
                        "schema": kioku::select::LEARNED_SCHEMA,
                        "base_commit": report.commit,
                        "tests_with_edges": report.tests_with_edges,
                        "edges": report.edges,
                        "alerts": report.alerts.len(),
                        "path": report.path.display().to_string(),
                    })
                );
            } else if !quiet {
                for alert in &report.alerts {
                    println!(
                        "  alerte : {} {} — {}",
                        alert.test.binary_id, alert.test.name, alert.message
                    );
                }
                println!(
                    "kioku select learn: {} — {} arêtes apprises sur {} tests, {} alertes -> {}",
                    report.commit,
                    report.edges,
                    report.tests_with_edges,
                    report.alerts.len(),
                    report.path.display()
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => select_failure(json, &error),
    }
}

fn quarantine(json: bool, quiet: bool, args: &QuarantineArgs) -> ExitCode {
    let QuarantineAction::Reset(reset) = &args.action;
    let request = Request {
        argv: reset.cmd.clone(),
        cwd: current_dir(),
        mode: None,
        inputs: Vec::new(),
        env: Vec::new(),
    };
    if let Err(error) = runner::reset_quarantine(&request) {
        print_error(json, "runtime", &error.to_string());
        return ExitCode::from(error_code(&error));
    }
    let out = Out {
        json,
        quiet,
        to_stderr: false,
    };
    out.event(&json!({"schema": SCHEMA, "event": "quarantine-reset", "policy": "trace-v1", "contract": "caller-asserted-stable-inputs", "mode_requested": null, "mode_effective": "bytes", "result": "exit-status", "output_replayed": false}));
    out.human("kioku: quarantine reset; new discovery and learning required");
    ExitCode::SUCCESS
}

fn parse_verify_rate(value: &str) -> Result<f64, String> {
    let rate: f64 = value
        .parse()
        .map_err(|_| "verify rate must be a number between 0 and 1".to_string())?;
    if rate.is_finite() && (0.0..=1.0).contains(&rate) {
        Ok(rate)
    } else {
        Err("verify rate must be a number between 0 and 1".to_string())
    }
}

fn current_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn request_of(args: &CommandArgs) -> Request {
    Request {
        argv: args.cmd.clone(),
        cwd: current_dir(),
        mode: args.mode.map(Mode::from).or_else(|| {
            let cwd = current_dir();
            let (root, _) = kioku::key::repository(&cwd).ok()?;
            kioku::config::mode(&root, &args.cmd).ok().flatten()
        }),
        inputs: args.inputs.clone(),
        env: args.env.clone(),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one CLI event includes the full execution outcome"
)]
fn run_or_verify(json: bool, quiet: bool, args: &CommandArgs, verify: bool) -> ExitCode {
    let out = Out {
        json,
        quiet,
        to_stderr: true,
    };
    let request = request_of(args);
    let outcome = runner::run_with_options(
        &request,
        runner::TraceOptions {
            trust_trace: args.trust_trace,
            require_output: args.require_output,
            verify_rate: args.verify_rate,
        },
        verify,
    );
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            print_command_error(json, &request, &error);
            return ExitCode::from(error_code(&error));
        }
    };

    if verify && let Err(error) = runner::log_verification(&request, &outcome) {
        print_command_error(json, &request, &error);
        return ExitCode::from(error_code(&error));
    }
    if !json {
        print_warnings(&outcome.warnings);
    }
    if args.trust_trace {
        out.human("kioku: trace-v2 assumes deterministic behavior and stable inputs; exit status only, output not replayed; effective mode: bytes");
    } else {
        out.human(&format!(
            "kioku: execute-only ({}); effective mode: bytes",
            outcome.reason
        ));
    }
    if let Some(reason) = &outcome.denial_reason {
        out.human(&format!("  not cached: {}", human_reason(reason)));
    }
    let event = if verify { "verify" } else { "run" };
    let decision = match outcome.decision {
        Decision::Cached => "cached",
        Decision::Executed => "executed",
        Decision::Bypassed => "bypassed",
    };
    out.event(&json!({
        "schema": SCHEMA,
        "event": event,
        "decision": decision,
        "policy": if args.trust_trace { "trace-v2" } else { "execute-only" },
        "contract": args.trust_trace.then_some("caller-asserted-stable-inputs"),
        "mode_requested": if request.mode == Some(Mode::Ast) { "ast" } else { "bytes" },
        "mode_effective": "bytes",
        "reason": outcome.reason,
        "cacheable": outcome.cacheable,
        "trace_state": outcome.trace_state,
        "tracer_sha256": outcome.tracer_sha256,
        "tracer_flags": outcome.tracer_flags,
        "tracer_schema": outcome.tracer_schema,
        "denial_reason": outcome.denial_reason,
        "manifest_digest": outcome.manifest_digest,
        "result": "exit-status",
        "output_replayed": false,
        "key": outcome.key,
        "cwd": outcome.cwd,
        "mode": outcome.mode,
        "ast_contract": (outcome.mode == "ast").then_some("caller-asserted-trivia-insensitive"),
        "exit_code": outcome.exit_code,
        "actual_exit": if verify { Some(outcome.exit_code) } else { None },
        "duration_ms": outcome.duration_ms,
        "saved_ms": outcome.saved_ms,
        "stored": outcome.stored,
        "invalidated": outcome.invalidated,
        "would_skip": outcome.would_skip,
        "false_green": outcome.false_green,
        "warnings": outcome.warnings,
    }));

    if verify {
        if outcome.false_green {
            out.human(&format!(
                "false green: kioku would have skipped this command, it exits {}",
                outcome.exit_code
            ));
            out.human(&format!("the cached pass {} was invalidated", outcome.key));
        }
        if outcome.would_skip && outcome.exit_code == 0 {
            out.human("shadow: kioku would have skipped this command, it passes");
        } else if !outcome.would_skip {
            out.human("shadow: kioku would have run this command");
        }
    } else {
        match outcome.decision {
            Decision::Cached => out.human(&format!(
                "cached (pass, {} saved)",
                human_duration(outcome.saved_ms)
            )),
            Decision::Executed => out.human(&format!(
                "executed (exit {}, {})",
                outcome.exit_code,
                human_duration(outcome.duration_ms)
            )),
            Decision::Bypassed => out.human("cache not used: the command was run"),
        }
    }

    ExitCode::from(u8::try_from(outcome.exit_code).unwrap_or(1))
}

#[allow(clippy::too_many_lines)]
fn explain(json: bool, quiet: bool, args: &CommandArgs) -> ExitCode {
    let out = Out {
        json,
        quiet,
        to_stderr: false,
    };
    let request = request_of(args);
    let explanation = match runner::explain_with_options(
        &request,
        runner::TraceOptions {
            trust_trace: args.trust_trace,
            require_output: args.require_output,
            verify_rate: args.verify_rate,
        },
    ) {
        Ok(explanation) => explanation,
        Err(error) => {
            print_command_error(json, &request, &error);
            return ExitCode::from(error_code(&error));
        }
    };

    print_explanation_json(&out, &explanation, args, &request);
    let diff = explanation.diff.as_ref();
    out.human(&format!(
        "kioku: {}",
        if explanation.hit {
            if explanation.traced {
                "hit, every recorded component matches"
            } else {
                "trace reuse would match (execute-only; add --trust-trace to enable)"
            }
        } else {
            "miss"
        }
    ));
    out.human(&format!(
        "  key: {}",
        if explanation.key.is_empty() {
            "(unavailable)"
        } else {
            &explanation.key
        }
    ));
    out.human(&format!(
        "  mode: {}; tracing: {}",
        explanation.mode,
        if explanation.traced {
            "enabled"
        } else {
            "execute-only"
        }
    ));
    out.human(&format!("  reason: {}", explanation.reason));
    out.human(&format!("  dependencies: {}", explanation.dependencies));
    if let Some(reason) = &explanation.denial_reason {
        out.human(&format!("  denied: {}", human_reason(reason)));
    }
    if let Some(changes) = &explanation.changed_dependencies {
        for change in changes {
            out.human(&format!(
                "  changed dependency: {} ({:?})",
                change.id, change.kind
            ));
        }
    }
    if explanation.environment_changed == Some(true) {
        out.human("  environment changed");
    }
    if explanation.tracer_changed == Some(true) {
        out.human("  tracer changed");
    }
    if let Some(would_skip) = explanation.ast_would_skip {
        out.human(&format!(
            "  partial repository AST diagnostic would match: {would_skip} (not a reuse decision)"
        ));
    }
    if let (Some(entry), Some(diff)) = (explanation.closest.as_ref(), diff) {
        out.human(&format!(
            "  closest entry: {} ({})",
            entry.key,
            kioku::store::format_iso8601(entry.created_unix_ms)
        ));
        for path in &diff.added {
            out.human(&format!("  + {path}"));
        }
        for path in &diff.removed {
            out.human(&format!("  - {path}"));
        }
        for modified in &diff.modified {
            out.human(&format!(
                "  ~ {} ({} -> {})",
                modified.path, modified.old_digest, modified.new_digest
            ));
            if modified.old_mode != modified.new_mode {
                out.human(&format!(
                    "    {} permissions {} -> {}",
                    modified.path,
                    format_permissions(modified.old_mode),
                    format_permissions(modified.new_mode)
                ));
            }
        }
        for changed in diff.env.iter().chain(&diff.lockfiles).chain(&diff.tools) {
            out.human(&format!(
                "  ~ {} ({} -> {})",
                changed.name,
                changed.old.as_deref().unwrap_or("-"),
                changed.new.as_deref().unwrap_or("-")
            ));
        }
    }
    ExitCode::SUCCESS
}

fn print_explanation_json(
    out: &Out,
    explanation: &runner::Explanation,
    args: &CommandArgs,
    request: &Request,
) {
    let diff = explanation.diff.as_ref();
    if args.trust_trace {
        out.human("kioku: trace-v2 assumes deterministic behavior and stable inputs; exit status only, output not replayed; effective mode: bytes");
    } else {
        out.human("kioku: execute-only; add --trust-trace to evaluate traced reuse");
    }
    out.event(&json!({
        "schema": SCHEMA,
        "event": "explain",
        "policy": if args.trust_trace { "trace-v2" } else { "execute-only" },
        "contract": args.trust_trace.then_some("caller-asserted-stable-inputs"),
        "mode_requested": if request.mode == Some(Mode::Ast) { "ast" } else { "bytes" },
        "mode_effective": "bytes",
        "result": "exit-status",
        "output_replayed": false,
        "key": explanation.key,
        "hit": explanation.hit,
        "mode": explanation.mode,
        "ast_contract": (explanation.mode == "ast").then_some("caller-asserted-trivia-insensitive"),
        "ast_would_skip": explanation.ast_would_skip,
        "ast_diagnostic": "partial-repository-ast-not-reuse",
        "changed_dependencies": explanation.changed_dependencies,
        "environment_changed": explanation.environment_changed,
        "tracer_changed": explanation.tracer_changed,
        "reason": explanation.reason,
        "traced": explanation.traced,
        "denial_reason": explanation.denial_reason,
        "dependencies": explanation.dependencies,
        "closest": explanation.closest,
        "diff": diff.map(|diff| json!({
            "added": diff.added,
            "removed": diff.removed,
            "modified": diff.modified.iter().map(|modified| json!({
                "path": modified.path,
                "old_digest": modified.old_digest,
                "new_digest": modified.new_digest,
                "old_mode": modified.old_mode,
                "new_mode": modified.new_mode,
            })).collect::<Vec<_>>(),
            "env": diff.env.iter().map(|changed| json!({
                "name": changed.name,
                "old": changed.old,
                "new": changed.new,
            })).collect::<Vec<_>>(),
            "lockfiles": diff.lockfiles.iter().map(|changed| json!({
                "name": changed.name,
                "old": changed.old,
                "new": changed.new,
            })).collect::<Vec<_>>(),
            "tools": diff.tools.iter().map(|changed| json!({
                "name": changed.name,
                "old": changed.old,
                "new": changed.new,
            })).collect::<Vec<_>>(),
        })),
    }));
}

fn status(json: bool, quiet: bool) -> ExitCode {
    let out = Out {
        json,
        quiet,
        to_stderr: false,
    };
    let status = match runner::status(&current_dir()) {
        Ok(status) => status,
        Err(error) if outside_repository(&error) => {
            out.event(&json!({
                "schema": SCHEMA,
                "event": "status",
                "repository": false,
                "reason": "not-a-git-repository",
            }));
            out.human("kioku: not a Git repository; nothing to report");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            print_error(json, "runtime", &error.to_string());
            return ExitCode::from(error_code(&error));
        }
    };

    out.event(&json!({
        "schema": SCHEMA,
        "event": "status",
        "store": status.store_root.to_string_lossy(),
        "entries": status.entries,
        "trace_states": status.trace_states,
        "quarantines": status.quarantines,
        "invalid_entries": status.invalid,
        "bytes": status.bytes,
        "cached_ms": status.cached_ms,
        "counters": status.counters,
    }));

    out.human(&format!(
        "kioku: {} trace states, {} quarantines, {} legacy entries",
        status.trace_states, status.quarantines, status.entries
    ));
    out.human(&format!(
        "  legacy: {}, {} legacy saved",
        human_bytes(status.bytes),
        human_duration(status.counters.saved_ms)
    ));
    out.human(&format!("  store: {}", status.store_root.display()));
    out.human(&format!(
        "  legacy hits: {}, misses: {}, executions: {}, invalidated: {}",
        status.counters.hits,
        status.counters.misses,
        status.counters.executions,
        status.counters.invalidated
    ));
    if status.invalid > 0 {
        out.human(&format!(
            "  {} unreadable files were ignored",
            status.invalid
        ));
    }
    ExitCode::SUCCESS
}

fn clean(json: bool, quiet: bool, args: &CleanArgs) -> ExitCode {
    let out = Out {
        json,
        quiet,
        to_stderr: false,
    };
    let older_than = match args.older_than.as_deref() {
        Some(value) => match parse_duration(value) {
            Ok(duration) => Some(duration),
            Err(message) => {
                print_error(json, "usage", &message);
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    if older_than.is_none() && !args.all {
        print_error(
            json,
            "usage",
            "clean needs --older-than <duration> or --all",
        );
        return ExitCode::from(2);
    }

    let result = match runner::clean(&current_dir(), older_than) {
        Ok(result) => result,
        Err(error) if outside_repository(&error) => {
            out.event(&json!({
                "schema": SCHEMA,
                "event": "clean",
                "repository": false,
                "reason": "not-a-git-repository",
                "removed": 0,
                "kept": 0,
                "bytes_freed": 0,
            }));
            out.human("kioku: not a Git repository; nothing to clean");
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            print_error(json, "runtime", &error.to_string());
            return ExitCode::from(error_code(&error));
        }
    };

    out.event(&json!({
        "schema": SCHEMA,
        "event": "clean",
        "removed": result.removed,
        "kept": result.kept,
        "bytes_freed": result.bytes_freed,
    }));
    out.human(&format!(
        "kioku: removed {} entries ({}), kept {}",
        result.removed,
        human_bytes(result.bytes_freed),
        result.kept
    ));
    ExitCode::SUCCESS
}

fn outside_repository(error: &runner::Error) -> bool {
    matches!(
        error,
        runner::Error::Key(
            kioku::Error::NotARepository { .. } | kioku::Error::GitUnavailable { .. }
        )
    )
}

fn human_reason(code: &str) -> &'static str {
    match code {
        "executable-format" => {
            "unsupported executable format (only static ELF binaries are traced)"
        }
        "repository-unavailable" => "not a Git repository",
        "program-unresolved" => "program not found or not executable",
        "cwd-unavailable" => "working directory unavailable",
        "trace-unsupported-path" => "repository path is not valid UTF-8",
        "tracer-unavailable" => {
            "tracer unavailable; install the pinned strace 6.1 and set KIOKU_STRACE (README.md, \
section Pinned tracer)"
        }
        "tracer-not-absolute" => "KIOKU_STRACE must be an absolute path",
        "tracer-untrusted" => {
            "KIOKU_STRACE is not the pinned strace 6.1 binary for this architecture (README.md, \
section Pinned tracer)"
        }
        "platform-unsupported" => "tracing is supported only on Linux aarch64 and x86_64",
        "write" => "the command writes files",
        "network" => "the command uses the network",
        "clock" => "the command reads the clock",
        "random" => "the command uses randomness",
        "stdin" => "the command reads standard input",
        "system-path" => "the command reads a system path",
        "unknown-syscall" => "the command uses a syscall the trace grammar does not model",
        "parse-error" => "the trace could not be parsed unambiguously",
        "unsupported" => "the command is outside the trace contract",
        _ => "the command cannot be traced",
    }
}

fn error_code(error: &runner::Error) -> u8 {
    match error {
        runner::Error::Launch { source, .. } => {
            if source.kind() == std::io::ErrorKind::NotFound {
                127
            } else {
                126
            }
        }
        runner::Error::Key(_) | runner::Error::Config(_) | runner::Error::EmptyCommand => 2,
        // A store failure and any failure that is not known yet are internal.
        _ => 3,
    }
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let (number, multiplier) = if let Some(rest) = value.strip_suffix('d') {
        (rest, 86_400_u64)
    } else if let Some(rest) = value.strip_suffix('h') {
        (rest, 3_600)
    } else if let Some(rest) = value.strip_suffix('m') {
        (rest, 60)
    } else if let Some(rest) = value.strip_suffix('s') {
        (rest, 1)
    } else {
        (value, 1)
    };
    let number: u64 = number.trim().parse().map_err(|_| {
        format!("invalid duration {value:?}, expected a number followed by d, h, m or s")
    })?;
    Ok(Duration::from_secs(number.saturating_mul(multiplier)))
}

/// Unix permissions, written the way `ls` writes them.
fn format_permissions(mode: u32) -> String {
    let bits = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    let mut text: String = bits
        .iter()
        .map(|(bit, letter)| if mode & bit != 0 { *letter } else { '-' })
        .collect();
    if mode & 0o4000 != 0 {
        text.insert(0, 's');
    }
    if mode & 0o2000 != 0 {
        text.insert(0, 'w');
    }
    if mode & 0o1000 != 0 {
        text.insert(0, 'x');
    }
    text
}

fn human_duration(milliseconds: u64) -> String {
    if milliseconds < 1000 {
        format!("{milliseconds} ms")
    } else {
        format!("{}.{} s", milliseconds / 1000, (milliseconds % 1000) / 100)
    }
}

fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{}.{} KiB", bytes / KIB, (bytes % KIB) * 10 / KIB)
    } else {
        format!("{}.{} MiB", bytes / MIB, (bytes % MIB) * 10 / MIB)
    }
}

#[cfg(test)]
mod tests {
    use super::{human_bytes, human_duration, parse_duration};
    use std::time::Duration;

    #[test]
    fn native_mode_changes_only_full_success() {
        for code in [0, 1, 2, 3, 5, 126, 127] {
            assert_eq!(super::compatible_exit_code(code, false), code);
            assert_eq!(
                super::compatible_exit_code(code, true),
                if code == 5 { 0 } else { code }
            );
        }
    }

    #[test]
    fn parses_the_documented_durations() {
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(604_800));
        assert_eq!(parse_duration("12h").unwrap(), Duration::from_secs(43_200));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1_800));
        assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
    }

    #[test]
    fn rejects_a_broken_duration() {
        assert!(parse_duration("seven days").is_err());
        assert!(parse_duration("7y").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn formats_sizes_and_durations() {
        assert_eq!(human_duration(340), "340 ms");
        assert_eq!(human_duration(1_500), "1.5 s");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
    }
}
