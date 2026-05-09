use std::collections::{HashMap, VecDeque};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

mod classifiers;
mod util;

const PRODUCT_NAME: &str = "Pebble";
const PRODUCT_VERSION: &str = "4.2.0";

const MAX_TOTAL_RETRIES: u32 = 3;
const MAX_SAME_FAILURE: u32 = 3;
const MAX_OSCILLATION_BREAKERS: u32 = 1;
const COMMAND_TIMEOUT_SECS: u64 = 120;
const BACKOFF_SECS: u64 = 5;

const TRAIL_MIN_SAMPLES: u32 = 3;
const TRAIL_MIN_SUCCESS_RATE: f64 = 0.70;

fn main() {
    let mut raw_args = env::args();
    let program_name = raw_args
        .next()
        .unwrap_or_else(|| "pebble".to_string());
    let mut args: Vec<String> = raw_args.collect();

    if args.is_empty() {
        print_usage(&program_name);
        return;
    }

    let dry_run = matches!(
        args.first().map(String::as_str),
        Some("--dry-run") | Some("dry-run")
    );

    if dry_run {
        args.remove(0);

        if args.is_empty() {
            print_usage(&program_name);
            std::process::exit(2);
        }
    }

    if !dry_run && args[0] == "stats" {
        if args.len() != 2 {
            print_usage(&program_name);
            std::process::exit(2);
        }

        let repo_path = match canonical_repo_path(&args[1]) {
            Ok(path) => path,
            Err(message) => {
                eprintln!("ERROR: {}", message);
                std::process::exit(2);
            }
        };

        let store = PebbleStore::new(repo_path);
        let events = store.load_events();
        print_stats(&store, &events);
        return;
    }

    if args.len() < 2 {
        print_usage(&program_name);
        std::process::exit(2);
    }

    let repo_path = match canonical_repo_path(&args[0]) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("ERROR: {}", message);
            std::process::exit(2);
        }
    };

    let command = match build_command_spec(&args[1..]) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("ERROR: {}", message);
            print_usage(&program_name);
            std::process::exit(2);
        }
    };

    let store = PebbleStore::new(repo_path.clone());
    let prior_events = store.load_events();
    let trail_index = TrailIndex::from_events(&prior_events);

    let config = ExecutionConfig {
        repo_path,
        command,
        dry_run,
        timeout: Duration::from_secs(COMMAND_TIMEOUT_SECS),
        max_total_retries: MAX_TOTAL_RETRIES,
        max_same_failure: MAX_SAME_FAILURE,
    };

    print_header(&config, &store, prior_events.len());

    let mut result = run_executor(&config, &trail_index);

    if config.dry_run {
        result
            .warnings
            .push("Dry-run mode: Pebble memory and reports were not written".to_string());
    } else if let Err(error) = store.append_result_events(&result) {
        result
            .warnings
            .push(format!("Could not append Pebble memory events: {}", error));
    }

    print_report(&result, &trail_index);

    if config.dry_run {
        println!("\nDry-run mode: report file was not written.");
    } else {
        match save_report(&result, &trail_index) {
            Ok(path) => println!("\nReport saved to: {}", path.display()),
            Err(error) => eprintln!("\nWARNING: Could not save report: {}", error),
        }
    }

    if !result.completed {
        std::process::exit(1);
    }
}

fn print_usage(program_name: &str) {
    println!("{} v{}", PRODUCT_NAME, PRODUCT_VERSION);
    println!();
    println!("Usage:");
    println!("  {} <repo-path> <command> [args...]", program_name);
    println!(
        "  {} --dry-run <repo-path> <command> [args...]",
        program_name
    );
    println!("  {} stats <repo-path>", program_name);
    println!();
    println!("Examples:");
    println!("  cargo run -- . cargo test");
    println!("  cargo run -- --dry-run . cargo fmt --check");
    println!("  cargo run -- C:\\Users\\Shakoor\\my-rust-project cargo test");
    println!("  cargo run -- stats .");
    println!();
    println!("What this does:");
    println!("  Pebble runs real commands and safe recoveries.");
    println!("  Pebble records failures, fingerprints, recoveries, and outcomes.");
    println!("  Future runs use local Pebble trails to choose the safest recovery.");
    println!("  Dry-run observes one command run, plans recovery, and writes nothing.");
}

fn canonical_repo_path(raw_path: &str) -> Result<PathBuf, String> {
    let repo_path = PathBuf::from(raw_path);

    if !repo_path.exists() || !repo_path.is_dir() {
        return Err(format!(
            "Repo path does not exist or is not a directory: {}",
            repo_path.display()
        ));
    }

    Ok(fs::canonicalize(&repo_path).unwrap_or(repo_path))
}

fn print_header(config: &ExecutionConfig, store: &PebbleStore, event_count: usize) {
    println!("================================================");
    println!("  {} v{}", PRODUCT_NAME, PRODUCT_VERSION);
    println!("  Deterministic failure memory + recovery engine");
    println!("================================================");
    println!();
    println!("Repo:        {}", config.repo_path.display());
    println!("Command:     {}", config.command.display);
    println!(
        "Mode:        {}",
        if config.dry_run {
            "dry-run observe and plan"
        } else {
            "execute"
        }
    );
    println!("Timeout:     {} seconds", config.timeout.as_secs());
    println!("Retries:     {}", config.max_total_retries);
    println!("Pebble path: {}", store.pebble_dir.display());
    println!("Trail events loaded: {}", event_count);
    println!();
}

#[derive(Debug, Clone)]
struct ExecutionConfig {
    repo_path: PathBuf,
    command: CommandSpec,
    dry_run: bool,
    timeout: Duration,
    max_total_retries: u32,
    max_same_failure: u32,
}

#[derive(Debug, Clone)]
struct CommandSpec {
    program: String,
    args: Vec<String>,
    display: String,
}

fn build_command_spec(parts: &[String]) -> Result<CommandSpec, String> {
    let tokens = if parts.len() == 1 {
        parse_command_line(&parts[0])
    } else {
        parts.to_vec()
    };

    if tokens.is_empty() {
        return Err("No command was provided.".to_string());
    }

    let program = tokens[0].clone();
    let args = tokens[1..].to_vec();
    let display = tokens.join(" ");

    Ok(CommandSpec {
        program,
        args,
        display,
    })
}

fn parse_command_line(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    for ch in input.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            ' ' | '\t' if !in_quotes => {
                if !current.is_empty() {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

// ============================================================
// Process execution
// ============================================================

#[derive(Debug, Clone)]
struct ProcessOutput {
    success: bool,
    exit_code: Option<i32>,
    timed_out: bool,
    duration_ms: u128,
    stdout: String,
    stderr: String,
}

fn run_process<S>(
    repo_path: &Path,
    program: &str,
    args: &[S],
    timeout: Duration,
) -> Result<ProcessOutput, String>
where
    S: AsRef<OsStr>,
{
    let nonce = now_millis();

    let stdout_path = env::temp_dir().join(format!(
        "pebble-stdout-{}-{}.log",
        std::process::id(),
        nonce
    ));

    let stderr_path = env::temp_dir().join(format!(
        "pebble-stderr-{}-{}.log",
        std::process::id(),
        nonce
    ));

    let stdout_file = File::create(&stdout_path)
        .map_err(|e| format!("Could not create stdout log file: {}", e))?;

    let stderr_file = File::create(&stderr_path)
        .map_err(|e| format!("Could not create stderr log file: {}", e))?;

    let start = Instant::now();

    let mut child = match Command::new(program)
        .args(args)
        .current_dir(repo_path)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = fs::remove_file(&stdout_path);
            let _ = fs::remove_file(&stderr_path);

            return Ok(ProcessOutput {
                success: false,
                exit_code: None,
                timed_out: false,
                duration_ms: start.elapsed().as_millis(),
                stdout: String::new(),
                stderr: format!("Failed to start command '{}': {}", program, error),
            });
        }
    };

    let mut timed_out = false;

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    timed_out = true;
                    let _ = child.kill();
                    let status = child
                        .wait()
                        .map_err(|e| format!("Failed to wait after timeout: {}", e))?;
                    break status;
                }

                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(format!("Failed while waiting for command: {}", error)),
        }
    };

    let stdout = fs::read_to_string(&stdout_path).unwrap_or_default();
    let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();

    let _ = fs::remove_file(&stdout_path);
    let _ = fs::remove_file(&stderr_path);

    Ok(ProcessOutput {
        success: status.success() && !timed_out,
        exit_code: status.code(),
        timed_out,
        duration_ms: start.elapsed().as_millis(),
        stdout: clamp_log(&redact_secrets(&stdout), 20_000),
        stderr: clamp_log(&redact_secrets(&stderr), 20_000),
    })
}

fn clamp_log(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        input.to_string()
    } else {
        format!("... [log truncated]\n{}", tail_text(input, max_chars))
    }
}

fn tail_text(input: &str, max_chars: usize) -> String {
    let count = input.chars().count();

    if count <= max_chars {
        input.to_string()
    } else {
        input.chars().skip(count - max_chars).collect()
    }
}

fn redact_secrets(input: &str) -> String {
    let mut output = String::new();

    for line in input.lines() {
        if looks_like_secret_line(line) {
            output.push_str("[redacted potential secret]\n");
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }

    output
}

fn looks_like_secret_line(line: &str) -> bool {
    let lower = line.to_lowercase();

    util::contains_any(
        &lower,
        &[
            "authorization:",
            "bearer ",
            "password=",
            "password:",
            "secret=",
            "secret:",
            "api_key=",
            "api_key:",
            "apikey=",
            "apikey:",
            "token=",
            "access_token=",
            "refresh_token=",
            "auth_token=",
        ],
    )
}

// ============================================================
// Failure signals and fingerprints
// ============================================================

#[derive(Debug, Clone)]
enum Signal {
    TaskCompleted,
    StaleBranch,
    InfraFlake,
    Timeout,
    FormatFailure,
    CompilationError { retryable: bool, auto_fixable: bool },
    TestFailure { retryable: bool },
    UnknownFailure,
}

impl Signal {
    fn label(&self) -> &'static str {
        match self {
            Self::TaskCompleted => "TaskCompleted",
            Self::StaleBranch => "StaleBranch",
            Self::InfraFlake => "InfraFlake",
            Self::Timeout => "Timeout",
            Self::FormatFailure => "FormatFailure",
            Self::CompilationError {
                auto_fixable: true, ..
            } => "CompilationErrorAutoFixable",
            Self::CompilationError {
                auto_fixable: false,
                ..
            } => "CompilationError",
            Self::TestFailure { retryable: true } => "TestFailureRetryable",
            Self::TestFailure { retryable: false } => "TestFailure",
            Self::UnknownFailure => "UnknownFailure",
        }
    }

    fn retryable(&self) -> bool {
        match self {
            Self::TaskCompleted => false,
            Self::StaleBranch => true,
            Self::InfraFlake => true,
            Self::Timeout => true,
            Self::FormatFailure => true,
            Self::CompilationError { retryable, .. } => *retryable,
            Self::TestFailure { retryable } => *retryable,
            Self::UnknownFailure => false,
        }
    }

    fn auto_fixable(&self) -> bool {
        match self {
            Self::CompilationError { auto_fixable, .. } => *auto_fixable,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FailureClass {
    StaleBranch,
    Compilation,
    Format,
    Infra,
    Timeout,
    Test,
    Unknown,
}

impl FailureClass {
    fn from_signal(signal: &Signal) -> Self {
        match signal {
            Signal::TaskCompleted => Self::Unknown,
            Signal::StaleBranch => Self::StaleBranch,
            Signal::InfraFlake => Self::Infra,
            Signal::Timeout => Self::Timeout,
            Signal::FormatFailure => Self::Format,
            Signal::CompilationError { .. } => Self::Compilation,
            Signal::TestFailure { .. } => Self::Test,
            Signal::UnknownFailure => Self::Unknown,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::StaleBranch => "StaleBranch",
            Self::Compilation => "Compilation",
            Self::Format => "Format",
            Self::Infra => "Infra",
            Self::Timeout => "Timeout",
            Self::Test => "Test",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone)]
struct FailureFingerprint {
    key: String,
    summary: String,
}

fn classify_output(output: &ProcessOutput, command: &CommandSpec) -> Signal {
    classifiers::classify_output(output, command)
}

fn fingerprint_failure(output: &ProcessOutput, signal: &Signal) -> FailureFingerprint {
    let combined = util::combined_output(&output.stdout, &output.stderr);

    let (key, summary) = match signal {
        Signal::TaskCompleted => ("task_completed", "Command passed"),
        Signal::Timeout => ("command_timeout", "Command timed out"),
        Signal::FormatFailure => ("rust_format_check_failed", "Rust formatting check failed"),
        Signal::StaleBranch => ("git_branch_state", "Git branch state blocked execution"),
        Signal::InfraFlake => {
            if util::contains_any(&combined, &["failed to download", "spurious network error"]) {
                ("cargo_network_fetch", "Cargo dependency fetch failed")
            } else if util::contains_any(&combined, &["connection refused", "connection reset"]) {
                ("network_connection_failure", "Network connection failed")
            } else {
                ("infra_transient_failure", "Transient infrastructure failure")
            }
        }
        Signal::CompilationError { .. } => {
            if util::contains_any(&combined, &["error[e0432]", "unresolved import"]) {
                ("rust_unresolved_import", "Rust unresolved import")
            } else if util::contains_any(&combined, &["error[e0308]", "mismatched types"]) {
                ("rust_type_mismatch", "Rust type mismatch")
            } else if util::contains_any(&combined, &["cannot find value", "cannot find type"]) {
                ("rust_cannot_find_item", "Rust missing value or type")
            } else if util::contains_any(
                &combined,
                &[
                    "borrowed value does not live long enough",
                    "does not live long enough",
                    "cannot borrow",
                ],
            ) {
                ("rust_borrow_lifetime", "Rust borrow checker or lifetime failure")
            } else if util::contains_any(&combined, &["unused import", "unused variable"]) {
                ("rust_unused_item", "Rust unused item cleanup")
            } else {
                ("rust_compile_error", "Rust compilation failure")
            }
        }
        Signal::TestFailure { retryable: true } => {
            ("rust_retryable_test_failure", "Retryable Rust test failure")
        }
        Signal::TestFailure { retryable: false } => {
            if util::contains_any(&combined, &["assertion failed", "assertion `"]) {
                ("rust_test_assertion_failed", "Rust test assertion failed")
            } else if combined.contains("panicked at") {
                ("rust_test_panic", "Rust test panic")
            } else {
                ("rust_test_failure", "Rust test failure")
            }
        }
        Signal::UnknownFailure => {
            if combined.contains("failed to start command") {
                ("process_start_failed", "Command could not start")
            } else {
                ("unknown_failure", "Unknown failure")
            }
        }
    };

    FailureFingerprint {
        key: key.to_string(),
        summary: summary.to_string(),
    }
}

// ============================================================
// Pebble memory store
// ============================================================

#[derive(Debug, Clone)]
struct PebbleEvent {
    timestamp_ms: u128,
    repo_id: String,
    command: String,
    fingerprint: String,
    failure_class: String,
    signal: String,
    recovery_action: String,
    action_source: String,
    outcome_success: bool,
    recovery_command_success: bool,
    duration_ms: u128,
}

#[derive(Debug)]
struct PebbleStore {
    repo_path: PathBuf,
    pebble_dir: PathBuf,
    events_path: PathBuf,
}

impl PebbleStore {
    fn new(repo_path: PathBuf) -> Self {
        let pebble_dir = repo_path.join(".pebble");
        let events_path = pebble_dir.join("events.jsonl");

        Self {
            repo_path,
            pebble_dir,
            events_path,
        }
    }

    fn load_events(&self) -> Vec<PebbleEvent> {
        let content = match fs::read_to_string(&self.events_path) {
            Ok(content) => content,
            Err(_) => return Vec::new(),
        };

        content
            .lines()
            .filter_map(parse_pebble_event)
            .collect::<Vec<_>>()
    }

    fn append_result_events(&self, result: &ExecutorResult) -> io::Result<usize> {
        fs::create_dir_all(&self.pebble_dir)?;
        self.ensure_gitignore_entry()?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.events_path)?;

        let repo_id = stable_repo_id(&self.repo_path);
        let mut written = 0usize;

        for recovery in result
            .recoveries
            .iter()
            .filter(|recovery| recovery.executed)
        {
            let outcome_success = recovery.command_passed_after.unwrap_or(false);

            let event = PebbleEvent {
                timestamp_ms: now_millis(),
                repo_id: repo_id.clone(),
                command: result.command_display.clone(),
                fingerprint: recovery.fingerprint.clone(),
                failure_class: recovery.failure_class.clone(),
                signal: recovery.signal.clone(),
                recovery_action: recovery.action.clone(),
                action_source: recovery.action_source.clone(),
                outcome_success,
                recovery_command_success: recovery.success,
                duration_ms: recovery.duration_ms,
            };

            writeln!(file, "{}", event.to_json_line())?;
            written += 1;
        }

        Ok(written)
    }

    /// Ensure `.pebble/` appears in .gitignore exactly once.
    ///
    /// Because task_report.txt now lives at `.pebble/task_report.txt`, it is
    /// covered by the `.pebble/` entry — no separate exclusion line needed.
    fn ensure_gitignore_entry(&self) -> io::Result<()> {
        let gitignore_path = self.repo_path.join(".gitignore");
        let content = fs::read_to_string(&gitignore_path).unwrap_or_default();

        if content
            .lines()
            .map(str::trim)
            .any(|line| line == ".pebble/" || line == ".pebble")
        {
            return Ok(());
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&gitignore_path)?;

        if !content.is_empty() && !content.ends_with('\n') {
            writeln!(file)?;
        }

        writeln!(file, ".pebble/")?;
        Ok(())
    }
}

impl PebbleEvent {
    fn to_json_line(&self) -> String {
        format!(
            "{{\"version\":\"{}\",\"timestamp_ms\":{},\"repo_id\":\"{}\",\"command\":\"{}\",\"fingerprint\":\"{}\",\"failure_class\":\"{}\",\"signal\":\"{}\",\"recovery_action\":\"{}\",\"action_source\":\"{}\",\"outcome_success\":{},\"recovery_command_success\":{},\"duration_ms\":{}}}",
            PRODUCT_VERSION,
            self.timestamp_ms,
            json_escape(&self.repo_id),
            json_escape(&self.command),
            json_escape(&self.fingerprint),
            json_escape(&self.failure_class),
            json_escape(&self.signal),
            json_escape(&self.recovery_action),
            json_escape(&self.action_source),
            self.outcome_success,
            self.recovery_command_success,
            self.duration_ms
        )
    }
}

fn parse_pebble_event(line: &str) -> Option<PebbleEvent> {
    Some(PebbleEvent {
        timestamp_ms: extract_json_u128(line, "timestamp_ms").unwrap_or(0),
        repo_id: extract_json_string(line, "repo_id")?,
        command: extract_json_string(line, "command")?,
        fingerprint: extract_json_string(line, "fingerprint")?,
        failure_class: extract_json_string(line, "failure_class")?,
        signal: extract_json_string(line, "signal")?,
        recovery_action: extract_json_string(line, "recovery_action")?,
        action_source: extract_json_string(line, "action_source")
            .unwrap_or_else(|| "unknown".to_string()),
        outcome_success: extract_json_bool(line, "outcome_success").unwrap_or(false),
        recovery_command_success: extract_json_bool(line, "recovery_command_success")
            .unwrap_or(false),
        duration_ms: extract_json_u128(line, "duration_ms").unwrap_or(0),
    })
}

fn extract_json_string(line: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\":\"", key);
    let start = line.find(&pattern)? + pattern.len();
    let mut value = String::new();
    let mut escaped = false;

    for ch in line[start..].chars() {
        if escaped {
            value.push(match ch {
                '"' => '"',
                '\\' => '\\',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            });
            escaped = false;
            continue;
        }

        if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(value);
        } else {
            value.push(ch);
        }
    }

    None
}

fn extract_json_bool(line: &str, key: &str) -> Option<bool> {
    let pattern = format!("\"{}\":", key);
    let start = line.find(&pattern)? + pattern.len();
    let rest = &line[start..];

    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn extract_json_u128(line: &str, key: &str) -> Option<u128> {
    let pattern = format!("\"{}\":", key);
    let start = line.find(&pattern)? + pattern.len();
    let digits: String = line[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect();

    digits.parse().ok()
}

fn json_escape(input: &str) -> String {
    let mut escaped = String::new();

    for ch in input.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => escaped.push(' '),
            ch => escaped.push(ch),
        }
    }

    escaped
}

fn stable_repo_id(repo_path: &Path) -> String {
    let display = repo_path.display().to_string().to_lowercase();
    format!("{:016x}", stable_hash(&display))
}

fn stable_hash(input: &str) -> u64 {
    let mut hash = 14_695_981_039_346_656_037u64;

    for byte in input.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(1_099_511_628_211u64);
    }

    hash
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ============================================================
// Trail analysis and policy
// ============================================================

#[derive(Debug, Clone)]
struct TrailActionStats {
    action: String,
    samples: u32,
    successes: u32,
    recovery_command_successes: u32,
    total_duration_ms: u128,
}

impl TrailActionStats {
    fn success_rate(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            self.successes as f64 / self.samples as f64
        }
    }

    fn average_duration_ms(&self) -> u128 {
        if self.samples == 0 {
            0
        } else {
            self.total_duration_ms / self.samples as u128
        }
    }
}

#[derive(Debug, Clone)]
struct TrailRecommendation {
    action: Recovery,
    samples: u32,
    success_rate: f64,
    reason: String,
}

#[derive(Debug)]
struct TrailIndex {
    by_fingerprint: HashMap<String, HashMap<String, TrailActionStats>>,
}

impl TrailIndex {
    fn from_events(events: &[PebbleEvent]) -> Self {
        let mut by_fingerprint: HashMap<String, HashMap<String, TrailActionStats>> = HashMap::new();

        for event in events {
            if event.recovery_action == "Escalate" || event.fingerprint.is_empty() {
                continue;
            }

            let action_map = by_fingerprint.entry(event.fingerprint.clone()).or_default();

            let stats = action_map
                .entry(event.recovery_action.clone())
                .or_insert_with(|| TrailActionStats {
                    action: event.recovery_action.clone(),
                    samples: 0,
                    successes: 0,
                    recovery_command_successes: 0,
                    total_duration_ms: 0,
                });

            stats.samples += 1;
            stats.total_duration_ms += event.duration_ms;

            if event.outcome_success {
                stats.successes += 1;
            }

            if event.recovery_command_success {
                stats.recovery_command_successes += 1;
            }
        }

        Self { by_fingerprint }
    }

    fn best_allowed_action(
        &self,
        fingerprint: &str,
        signal: &Signal,
    ) -> Option<TrailRecommendation> {
        let action_map = self.by_fingerprint.get(fingerprint)?;

        let mut candidates = Vec::new();

        for stats in action_map.values() {
            let recovery = Recovery::from_label(&stats.action)?;

            if !recovery_allowed_for_signal(recovery, signal) {
                continue;
            }

            candidates.push((recovery, stats));
        }

        candidates.sort_by(|(_, a), (_, b)| {
            b.success_rate()
                .partial_cmp(&a.success_rate())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.samples.cmp(&a.samples))
        });

        let (action, stats) = candidates.into_iter().next()?;

        if stats.samples >= TRAIL_MIN_SAMPLES && stats.success_rate() >= TRAIL_MIN_SUCCESS_RATE {
            Some(TrailRecommendation {
                action,
                samples: stats.samples,
                success_rate: stats.success_rate(),
                reason: format!(
                    "Pebble trail selected {}: {}/{} prior successes for this fingerprint",
                    action.label(),
                    stats.successes,
                    stats.samples
                ),
            })
        } else {
            None
        }
    }

    fn stats_for_fingerprint(&self, fingerprint: &str) -> Vec<TrailActionStats> {
        let mut stats = self
            .by_fingerprint
            .get(fingerprint)
            .map(|actions| actions.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();

        stats.sort_by(|a, b| {
            b.success_rate()
                .partial_cmp(&a.success_rate())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.samples.cmp(&a.samples))
        });

        stats
    }

    fn all_stats(&self) -> Vec<(String, TrailActionStats)> {
        let mut rows = Vec::new();

        for (fingerprint, actions) in &self.by_fingerprint {
            for stats in actions.values() {
                rows.push((fingerprint.clone(), stats.clone()));
            }
        }

        rows.sort_by(|a, b| {
            b.1.samples
                .cmp(&a.1.samples)
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| a.1.action.cmp(&b.1.action))
        });

        rows
    }
}

// ============================================================
// Recovery engine
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recovery {
    GitSyncAndRetry,
    CargoFixAndRetry,
    CargoFmtAndRetry,
    WaitAndRetry,
    CargoCleanAndRetry,
}

impl Recovery {
    fn label(self) -> &'static str {
        match self {
            Self::GitSyncAndRetry => "GitSyncAndRetry",
            Self::CargoFixAndRetry => "CargoFixAndRetry",
            Self::CargoFmtAndRetry => "CargoFmtAndRetry",
            Self::WaitAndRetry => "WaitAndRetry",
            Self::CargoCleanAndRetry => "CargoCleanAndRetry",
        }
    }

    fn from_label(label: &str) -> Option<Self> {
        match label {
            "GitSyncAndRetry" => Some(Self::GitSyncAndRetry),
            "CargoFixAndRetry" => Some(Self::CargoFixAndRetry),
            "CargoFmtAndRetry" => Some(Self::CargoFmtAndRetry),
            "WaitAndRetry" => Some(Self::WaitAndRetry),
            "CargoCleanAndRetry" => Some(Self::CargoCleanAndRetry),
            _ => None,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::GitSyncAndRetry => {
                "Recovery: stale branch detected; checking Git status, fetching, and rebasing."
            }
            Self::CargoFixAndRetry => {
                "Recovery: running cargo fix for an auto-fixable compile issue."
            }
            Self::CargoFmtAndRetry => "Recovery: running cargo fmt before retry.",
            Self::WaitAndRetry => "Recovery: waiting before retry for transient failure.",
            Self::CargoCleanAndRetry => "Recovery: repeated pattern detected; running cargo clean.",
        }
    }
}

#[derive(Debug)]
enum Decision {
    Recover(DecisionPlan),
    Escalate(String),
}

#[derive(Debug, Clone)]
struct DecisionPlan {
    recovery: Recovery,
    source: String,
    reason: String,
    confidence: f64,
    trail_samples: u32,
    trail_success_rate: f64,
}

struct RecoveryEngine<'a> {
    max_total_retries: u32,
    max_same_failure: u32,
    max_oscillation_breakers: u32,
    oscillation_breakers_used: u32,
    failure_history: VecDeque<FailureClass>,
    oscillation_limit: usize,
    trail_index: &'a TrailIndex,
}

impl<'a> RecoveryEngine<'a> {
    fn new(max_total_retries: u32, max_same_failure: u32, trail_index: &'a TrailIndex) -> Self {
        Self {
            max_total_retries,
            max_same_failure,
            max_oscillation_breakers: MAX_OSCILLATION_BREAKERS,
            oscillation_breakers_used: 0,
            failure_history: VecDeque::new(),
            oscillation_limit: 6,
            trail_index,
        }
    }

    fn decide(
        &mut self,
        signal: &Signal,
        fingerprint: &FailureFingerprint,
        retries_so_far: u32,
        same_count: u32,
    ) -> Decision {
        let class = FailureClass::from_signal(signal);

        if !signal.retryable() && !signal.auto_fixable() {
            return Decision::Escalate(format!(
                "{} is not safely recoverable automatically",
                signal.label()
            ));
        }

        self.remember_failure(class);

        if self.detect_oscillation() {
            if self.oscillation_breakers_used < self.max_oscillation_breakers {
                self.oscillation_breakers_used += 1;
                self.failure_history.clear();
                return Decision::Recover(DecisionPlan {
                    recovery: Recovery::CargoCleanAndRetry,
                    source: "safety-policy".to_string(),
                    reason: "Detected alternating failure classes; using one cleanup attempt"
                        .to_string(),
                    confidence: 0.50,
                    trail_samples: 0,
                    trail_success_rate: 0.0,
                });
            }

            return Decision::Escalate("Repeated failure oscillation persisted".to_string());
        }

        if retries_so_far >= self.max_total_retries {
            return Decision::Escalate(format!(
                "Max retries exhausted ({})",
                self.max_total_retries
            ));
        }

        if same_count >= self.max_same_failure {
            return Decision::Escalate(format!(
                "Too many repeated {} failures ({})",
                class.label(),
                same_count
            ));
        }

        if let Some(trail) = self
            .trail_index
            .best_allowed_action(&fingerprint.key, signal)
        {
            return Decision::Recover(DecisionPlan {
                recovery: trail.action,
                source: "pebble-trail".to_string(),
                reason: trail.reason,
                confidence: trail.success_rate,
                trail_samples: trail.samples,
                trail_success_rate: trail.success_rate,
            });
        }

        match default_recovery_for_signal(signal) {
            Some(recovery) => Decision::Recover(DecisionPlan {
                recovery,
                source: "default-policy".to_string(),
                reason: format!(
                    "No proven Pebble trail yet; using deterministic default for {}",
                    signal.label()
                ),
                confidence: 0.40,
                trail_samples: 0,
                trail_success_rate: 0.0,
            }),
            None => Decision::Escalate(format!(
                "No safe recovery policy exists for {}",
                signal.label()
            )),
        }
    }

    fn remember_failure(&mut self, class: FailureClass) {
        self.failure_history.push_back(class);

        if self.failure_history.len() > self.oscillation_limit {
            self.failure_history.pop_front();
        }
    }

    fn detect_oscillation(&self) -> bool {
        if self.failure_history.len() < 4 {
            return false;
        }

        let v: Vec<_> = self.failure_history.iter().copied().collect();
        let n = v.len();

        let abab = v[n - 4] == v[n - 2] && v[n - 3] == v[n - 1] && v[n - 4] != v[n - 3];
        let abcabc = n >= 6 && v[n - 6..n - 3] == v[n - 3..n];

        abab || abcabc
    }
}

fn default_recovery_for_signal(signal: &Signal) -> Option<Recovery> {
    match signal {
        Signal::StaleBranch => Some(Recovery::GitSyncAndRetry),
        Signal::InfraFlake | Signal::Timeout => Some(Recovery::WaitAndRetry),
        Signal::FormatFailure => Some(Recovery::CargoFmtAndRetry),
        Signal::CompilationError {
            auto_fixable: true, ..
        } => Some(Recovery::CargoFixAndRetry),
        Signal::CompilationError {
            auto_fixable: false,
            ..
        } => None,
        Signal::TestFailure { retryable: true } => Some(Recovery::WaitAndRetry),
        Signal::TestFailure { retryable: false } => None,
        Signal::UnknownFailure | Signal::TaskCompleted => None,
    }
}

fn recovery_allowed_for_signal(recovery: Recovery, signal: &Signal) -> bool {
    match signal {
        Signal::StaleBranch => matches!(recovery, Recovery::GitSyncAndRetry),
        Signal::InfraFlake | Signal::Timeout => matches!(recovery, Recovery::WaitAndRetry),
        Signal::FormatFailure => matches!(recovery, Recovery::CargoFmtAndRetry),
        Signal::CompilationError {
            auto_fixable: true, ..
        } => matches!(
            recovery,
            Recovery::CargoFixAndRetry | Recovery::CargoFmtAndRetry | Recovery::CargoCleanAndRetry
        ),
        Signal::CompilationError {
            auto_fixable: false,
            ..
        } => false,
        Signal::TestFailure { retryable: true } => matches!(recovery, Recovery::WaitAndRetry),
        Signal::TestFailure { retryable: false } => false,
        Signal::UnknownFailure | Signal::TaskCompleted => false,
    }
}

// ============================================================
// Executor state
// ============================================================

#[derive(Debug, Clone)]
struct CommandRun {
    attempt: u32,
    program: String,
    args: Vec<String>,
    exit_code: Option<i32>,
    timed_out: bool,
    duration_ms: u128,
    stdout: String,
    stderr: String,
    signal: Signal,
    fingerprint: FailureFingerprint,
}

#[derive(Debug, Clone)]
struct RecoveryRecord {
    action: String,
    executed: bool,
    success: bool,
    details: String,
    duration_ms: u128,
    signal: String,
    failure_class: String,
    fingerprint: String,
    fingerprint_summary: String,
    action_source: String,
    decision_reason: String,
    confidence: f64,
    trail_samples: u32,
    trail_success_rate: f64,
    command_passed_after: Option<bool>,
}

#[derive(Debug)]
struct ExecutorResult {
    repo_path: PathBuf,
    command_display: String,
    dry_run: bool,
    completed: bool,
    attempts: Vec<CommandRun>,
    recoveries: Vec<RecoveryRecord>,
    failures: Vec<String>,
    warnings: Vec<String>,
    final_reason: String,
}

fn run_executor(config: &ExecutionConfig, trail_index: &TrailIndex) -> ExecutorResult {
    let mut result = ExecutorResult {
        repo_path: config.repo_path.clone(),
        command_display: config.command.display.clone(),
        dry_run: config.dry_run,
        completed: false,
        attempts: Vec::new(),
        recoveries: Vec::new(),
        failures: Vec::new(),
        warnings: Vec::new(),
        final_reason: "Not started".to_string(),
    };

    let mut engine = RecoveryEngine::new(
        config.max_total_retries,
        config.max_same_failure,
        trail_index,
    );
    let mut retries = 0u32;
    let mut same_failure_count: HashMap<FailureClass, u32> = HashMap::new();

    loop {
        let attempt_number = result.attempts.len() as u32 + 1;

        println!("------------------------------------------------");
        println!("Attempt {}", attempt_number);
        println!("------------------------------------------------");

        let run = run_main_command(config, attempt_number);
        print_attempt_summary(&run);

        let signal = run.signal.clone();

        if let Some(last_recovery) = result.recoveries.last_mut() {
            if last_recovery.command_passed_after.is_none() {
                last_recovery.command_passed_after = Some(matches!(signal, Signal::TaskCompleted));
            }
        }

        if matches!(signal, Signal::TaskCompleted) {
            result.completed = true;
            result.final_reason = "Command completed successfully".to_string();
            result.attempts.push(run);
            break;
        }

        let class = FailureClass::from_signal(&signal);
        let fingerprint = run.fingerprint.clone();

        let same_count = {
            let counter = same_failure_count.entry(class).or_insert(0);
            *counter += 1;
            *counter
        };

        result.failures.push(format!(
            "Attempt {}: {} / {}",
            attempt_number,
            signal.label(),
            fingerprint.key
        ));

        result.attempts.push(run);

        match engine.decide(&signal, &fingerprint, retries, same_count) {
            Decision::Recover(plan) => {
                println!("{}", plan.recovery.message());
                println!("Decision source: {}", plan.source);
                println!("Decision reason: {}", plan.reason);

                if config.dry_run {
                    println!(
                        "DRY RUN: would execute recovery action {}. No recovery command will run.",
                        plan.recovery.label()
                    );

                    result
                        .recoveries
                        .push(planned_recovery_record(plan, &signal, &fingerprint));
                    result.completed = false;
                    result.final_reason = "Dry-run stopped before recovery execution".to_string();
                    break;
                }

                let record = perform_recovery(plan, &signal, &fingerprint, config);

                println!(
                    "Recovery result: {} - {}",
                    if record.success { "OK" } else { "FAILED" },
                    record.details
                );

                let recovery_succeeded = record.success;
                result.recoveries.push(record);

                if !recovery_succeeded {
                    result.completed = false;
                    result.final_reason = "Recovery command failed".to_string();
                    break;
                }

                retries += 1;
                println!();
            }
            Decision::Escalate(reason) => {
                println!("ESCALATED: {}. Human review required.", reason);
                result.completed = false;
                result.final_reason = reason;
                break;
            }
        }
    }

    result
}

fn run_main_command(config: &ExecutionConfig, attempt: u32) -> CommandRun {
    let process = match run_process(
        &config.repo_path,
        &config.command.program,
        &config.command.args,
        config.timeout,
    ) {
        Ok(output) => output,
        Err(error) => ProcessOutput {
            success: false,
            exit_code: None,
            timed_out: false,
            duration_ms: 0,
            stdout: String::new(),
            stderr: error,
        },
    };

    let signal = classify_output(&process, &config.command);
    let fingerprint = fingerprint_failure(&process, &signal);

    CommandRun {
        attempt,
        program: config.command.program.clone(),
        args: config.command.args.clone(),
        exit_code: process.exit_code,
        timed_out: process.timed_out,
        duration_ms: process.duration_ms,
        stdout: process.stdout,
        stderr: process.stderr,
        signal,
        fingerprint,
    }
}

fn print_attempt_summary(run: &CommandRun) {
    println!("Running: {} {}", run.program, run.args.join(" "));

    let exit_text = match run.exit_code {
        Some(code) => code.to_string(),
        None => "none".to_string(),
    };

    println!("Exit code:   {}", exit_text);
    println!("Duration:    {} ms", run.duration_ms);
    println!("Signal:      {}", run.signal.label());
    println!(
        "Fingerprint: {} ({})",
        run.fingerprint.key, run.fingerprint.summary
    );

    if run.timed_out {
        println!("Warning: command timed out");
    }

    if !matches!(run.signal, Signal::TaskCompleted) {
        let stderr_tail = tail_text(&run.stderr, 1_000);
        let stdout_tail = tail_text(&run.stdout, 1_000);

        if !stderr_tail.trim().is_empty() {
            println!();
            println!("Last stderr lines:");
            for line in stderr_tail.lines().take(12) {
                println!("  {}", line);
            }
        } else if !stdout_tail.trim().is_empty() {
            println!();
            println!("Last stdout lines:");
            for line in stdout_tail.lines().take(12) {
                println!("  {}", line);
            }
        }
    }

    if matches!(run.signal, Signal::TaskCompleted) {
        println!("Attempt result: OK");
    } else {
        println!("Attempt result: FAILED");
    }
}

// ============================================================
// Recovery actions
// ============================================================

fn planned_recovery_record(
    plan: DecisionPlan,
    signal: &Signal,
    fingerprint: &FailureFingerprint,
) -> RecoveryRecord {
    let recovery = plan.recovery;

    RecoveryRecord {
        action: recovery.label().to_string(),
        executed: false,
        success: false,
        details: format!("Dry-run plan only; would execute {}", recovery.label()),
        duration_ms: 0,
        signal: signal.label().to_string(),
        failure_class: FailureClass::from_signal(signal).label().to_string(),
        fingerprint: fingerprint.key.clone(),
        fingerprint_summary: fingerprint.summary.clone(),
        action_source: plan.source,
        decision_reason: plan.reason,
        confidence: plan.confidence,
        trail_samples: plan.trail_samples,
        trail_success_rate: plan.trail_success_rate,
        command_passed_after: None,
    }
}

fn perform_recovery(
    plan: DecisionPlan,
    signal: &Signal,
    fingerprint: &FailureFingerprint,
    config: &ExecutionConfig,
) -> RecoveryRecord {
    let start = Instant::now();
    let recovery = plan.recovery;

    let (success, details) = match recovery {
        Recovery::WaitAndRetry => {
            thread::sleep(Duration::from_secs(BACKOFF_SECS));
            (true, format!("Waited {} seconds", BACKOFF_SECS))
        }

        Recovery::CargoFmtAndRetry => {
            if !repo_has_cargo(&config.repo_path) {
                (
                    false,
                    "Cargo.toml not found; cannot run cargo fmt".to_string(),
                )
            } else {
                let check = run_process(
                    &config.repo_path,
                    "cargo",
                    &["check"],
                    Duration::from_secs(60),
                );

                if let Ok(output) = check {
                    if !output.success {
                        let failed_plan = plan.clone();
                        let useful_tail = if !output.stderr.trim().is_empty() {
                            tail_text(&output.stderr, 500)
                        } else {
                            tail_text(&output.stdout, 500)
                        };

                        return RecoveryRecord {
                            action: recovery.label().to_string(),
                            executed: true,
                            success: false,
                            details: format!(
                                "cargo check failed before fmt; compilation error present. {}",
                                useful_tail.trim()
                            ),
                            duration_ms: start.elapsed().as_millis(),
                            signal: signal.label().to_string(),
                            failure_class: FailureClass::from_signal(signal).label().to_string(),
                            fingerprint: fingerprint.key.clone(),
                            fingerprint_summary: fingerprint.summary.clone(),
                            action_source: failed_plan.source,
                            decision_reason: failed_plan.reason,
                            confidence: failed_plan.confidence,
                            trail_samples: failed_plan.trail_samples,
                            trail_success_rate: failed_plan.trail_success_rate,
                            command_passed_after: None,
                        };
                    }
                }

                recovery_from_output(
                    "cargo fmt",
                    run_process(
                        &config.repo_path,
                        "cargo",
                        &["fmt"],
                        Duration::from_secs(60),
                    ),
                )
            }
        }

        Recovery::CargoFixAndRetry => {
            if !repo_has_cargo(&config.repo_path) {
                (
                    false,
                    "Cargo.toml not found; cannot run cargo fix".to_string(),
                )
            } else {
                recovery_from_output(
                    "cargo fix --allow-dirty --allow-staged",
                    run_process(
                        &config.repo_path,
                        "cargo",
                        &["fix", "--allow-dirty", "--allow-staged"],
                        Duration::from_secs(180),
                    ),
                )
            }
        }

        Recovery::CargoCleanAndRetry => {
            if !repo_has_cargo(&config.repo_path) {
                (
                    false,
                    "Cargo.toml not found; cannot run cargo clean".to_string(),
                )
            } else {
                recovery_from_output(
                    "cargo clean",
                    run_process(
                        &config.repo_path,
                        "cargo",
                        &["clean"],
                        Duration::from_secs(120),
                    ),
                )
            }
        }

        Recovery::GitSyncAndRetry => run_git_sync(&config.repo_path),
    };

    RecoveryRecord {
        action: recovery.label().to_string(),
        executed: true,
        success,
        details,
        duration_ms: start.elapsed().as_millis(),
        signal: signal.label().to_string(),
        failure_class: FailureClass::from_signal(signal).label().to_string(),
        fingerprint: fingerprint.key.clone(),
        fingerprint_summary: fingerprint.summary.clone(),
        action_source: plan.source,
        decision_reason: plan.reason,
        confidence: plan.confidence,
        trail_samples: plan.trail_samples,
        trail_success_rate: plan.trail_success_rate,
        command_passed_after: None,
    }
}

fn recovery_from_output(
    command_text: &str,
    output: Result<ProcessOutput, String>,
) -> (bool, String) {
    match output {
        Ok(process) if process.success => {
            (true, format!("{} completed successfully", command_text))
        }
        Ok(process) => {
            let stderr_tail = tail_text(&process.stderr, 500);
            let stdout_tail = tail_text(&process.stdout, 500);
            let useful_tail = if !stderr_tail.trim().is_empty() {
                stderr_tail
            } else {
                stdout_tail
            };

            (
                false,
                format!(
                    "{} failed with exit code {:?}. {}",
                    command_text,
                    process.exit_code,
                    useful_tail.trim()
                ),
            )
        }
        Err(error) => (false, format!("{} failed: {}", command_text, error)),
    }
}

fn run_git_sync(repo_path: &Path) -> (bool, String) {
    if !is_git_repo(repo_path) {
        return (false, "Not a Git repo; cannot sync branch".to_string());
    }

    match git_working_tree_is_clean(repo_path) {
        Ok(true) => {}
        Ok(false) => {
            return (
                false,
                "Git working tree has local changes; refusing automatic rebase".to_string(),
            );
        }
        Err(error) => return (false, error),
    }

    let fetch = recovery_from_output(
        "git fetch --all --prune",
        run_process(
            repo_path,
            "git",
            &["fetch", "--all", "--prune"],
            Duration::from_secs(120),
        ),
    );

    if !fetch.0 {
        return fetch;
    }

    recovery_from_output(
        "git pull --rebase",
        run_process(
            repo_path,
            "git",
            &["pull", "--rebase"],
            Duration::from_secs(120),
        ),
    )
}

fn repo_has_cargo(repo_path: &Path) -> bool {
    repo_path.join("Cargo.toml").is_file()
}

fn is_git_repo(repo_path: &Path) -> bool {
    match run_process(
        repo_path,
        "git",
        &["rev-parse", "--is-inside-work-tree"],
        Duration::from_secs(30),
    ) {
        Ok(output) => output.success && output.stdout.split_whitespace().next() == Some("true"),
        Err(_) => false,
    }
}

fn git_working_tree_is_clean(repo_path: &Path) -> Result<bool, String> {
    let output = run_process(
        repo_path,
        "git",
        &["status", "--porcelain"],
        Duration::from_secs(30),
    )?;

    if !output.success {
        return Err(format!(
            "Could not inspect Git status: {}",
            tail_text(&output.stderr, 500)
        ));
    }

    Ok(output.stdout.trim().is_empty())
}

// ============================================================
// Reporting
// ============================================================

fn print_report(result: &ExecutorResult, trail_index: &TrailIndex) {
    println!();
    println!("================================================");
    println!("  Real execution report");
    println!("================================================");
    println!();
    println!("Repo:       {}", result.repo_path.display());
    println!("Command:    {}", result.command_display);
    println!(
        "Mode:       {}",
        if result.dry_run {
            "dry-run observe and plan"
        } else {
            "execute"
        }
    );
    println!(
        "Result:     {}",
        if result.completed {
            "SUCCESS"
        } else {
            "FAILED"
        }
    );
    println!("Reason:     {}", result.final_reason);
    println!("Attempts:   {}", result.attempts.len());
    println!("Recoveries: {}", result.recoveries.len());

    if !result.warnings.is_empty() {
        println!();
        println!("Warnings:");
        for warning in &result.warnings {
            println!("  - {}", warning);
        }
    }

    println!();
    println!("Attempts:");
    println!(
        "{:<8} {:<8} {:<10} {:<28} {}",
        "Attempt", "Exit", "Duration", "Signal", "Fingerprint"
    );
    println!("{}", "-".repeat(88));

    for attempt in &result.attempts {
        let exit = match attempt.exit_code {
            Some(code) => code.to_string(),
            None => "none".to_string(),
        };

        println!(
            "{:<8} {:<8} {:<10} {:<28} {}",
            attempt.attempt,
            exit,
            format!("{}ms", attempt.duration_ms),
            attempt.signal.label(),
            attempt.fingerprint.key
        );
    }

    println!();
    println!("Recoveries:");
    println!("{}", "-".repeat(88));

    if result.recoveries.is_empty() {
        println!("  No recovery actions were needed.");
    } else {
        for recovery in &result.recoveries {
            println!(
                "  {} | command={} | next_run={}",
                recovery.action,
                if !recovery.executed {
                    "planned"
                } else if recovery.success {
                    "ok"
                } else {
                    "failed"
                },
                match recovery.command_passed_after {
                    Some(true) => "passed",
                    Some(false) => "failed",
                    None => "not-run",
                }
            );
            println!("    fingerprint: {}", recovery.fingerprint);
            println!("    source:      {}", recovery.action_source);
            println!("    reason:      {}", recovery.decision_reason);
            if recovery.trail_samples > 0 {
                println!(
                    "    trail:       {} samples, {:.0}% success",
                    recovery.trail_samples,
                    recovery.trail_success_rate * 100.0
                );
            }
            println!("    details:     {}", recovery.details);
        }
    }

    println!();
    println!("Pebble trail context:");
    println!("{}", "-".repeat(88));

    let mut printed = false;
    for attempt in result
        .attempts
        .iter()
        .filter(|attempt| !matches!(attempt.signal, Signal::TaskCompleted))
    {
        let rows = trail_index.stats_for_fingerprint(&attempt.fingerprint.key);

        if rows.is_empty() {
            println!("  {}: no prior trail data", attempt.fingerprint.key);
        } else {
            printed = true;
            println!("  {}", attempt.fingerprint.key);
            for row in rows {
                println!(
                    "    {}: {}/{} success ({:.0}%), avg {}ms",
                    row.action,
                    row.successes,
                    row.samples,
                    row.success_rate() * 100.0,
                    row.average_duration_ms()
                );
            }
        }
    }

    if !printed
        && result
            .attempts
            .iter()
            .all(|attempt| matches!(attempt.signal, Signal::TaskCompleted))
    {
        println!("  No failure fingerprint was produced in this run.");
    }

    println!();
    println!("Safety rules:");
    println!("{}", "-".repeat(88));
    println!("  - Commands are run directly, not through a shell.");
    println!("  - Retry limit enforced: {}.", MAX_TOTAL_RETRIES);
    println!("  - Same-failure limit enforced: {}.", MAX_SAME_FAILURE);
    println!(
        "  - Pebble trails require at least {} samples.",
        TRAIL_MIN_SAMPLES
    );
    println!(
        "  - Pebble trails require at least {:.0}% success.",
        TRAIL_MIN_SUCCESS_RATE * 100.0
    );
    println!("  - Git rebase only runs when the working tree is clean.");
    println!("  - Unknown failures escalate to human review.");
    println!("  - Logs are redacted for common secret patterns before storage.");

    println!();
    if result.completed {
        println!("Verdict: real command completed successfully.");
    } else {
        println!("Verdict: human review required.");
    }
}

/// Fix 4: report now lives inside `.pebble/` so it is covered by the existing
/// `.pebble/` gitignore entry and never appears in the repo root.
fn save_report(result: &ExecutorResult, trail_index: &TrailIndex) -> io::Result<PathBuf> {
    let pebble_dir = result.repo_path.join(".pebble");
    fs::create_dir_all(&pebble_dir)?;

    let report_path = pebble_dir.join("task_report.txt");
    let mut file = File::create(&report_path)?;

    writeln!(file, "{} v{}", PRODUCT_NAME, PRODUCT_VERSION)?;
    writeln!(file, "Real execution report")?;
    writeln!(file, "================================================")?;
    writeln!(file)?;
    writeln!(file, "Repo: {}", result.repo_path.display())?;
    writeln!(file, "Command: {}", result.command_display)?;
    writeln!(
        file,
        "Mode: {}",
        if result.dry_run {
            "dry-run observe and plan"
        } else {
            "execute"
        }
    )?;
    writeln!(
        file,
        "Result: {}",
        if result.completed {
            "SUCCESS"
        } else {
            "FAILED"
        }
    )?;
    writeln!(file, "Final Reason: {}", result.final_reason)?;
    writeln!(file, "Attempts: {}", result.attempts.len())?;
    writeln!(file, "Recoveries: {}", result.recoveries.len())?;
    writeln!(file)?;

    if !result.warnings.is_empty() {
        writeln!(file, "WARNINGS")?;
        writeln!(file, "--------")?;
        for warning in &result.warnings {
            writeln!(file, "- {}", warning)?;
        }
        writeln!(file)?;
    }

    writeln!(file, "ATTEMPTS")?;
    writeln!(file, "--------")?;

    for attempt in &result.attempts {
        writeln!(file, "Attempt {}", attempt.attempt)?;
        writeln!(
            file,
            "  Command: {} {}",
            attempt.program,
            attempt.args.join(" ")
        )?;
        writeln!(file, "  Exit Code: {:?}", attempt.exit_code)?;
        writeln!(file, "  Timed Out: {}", attempt.timed_out)?;
        writeln!(file, "  Duration: {} ms", attempt.duration_ms)?;
        writeln!(file, "  Signal: {}", attempt.signal.label())?;
        writeln!(
            file,
            "  Fingerprint: {} ({})",
            attempt.fingerprint.key, attempt.fingerprint.summary
        )?;

        if !attempt.stdout.trim().is_empty() {
            writeln!(file, "  Stdout Tail:")?;
            writeln!(file, "{}", tail_text(&attempt.stdout, 2_000))?;
        }

        if !attempt.stderr.trim().is_empty() {
            writeln!(file, "  Stderr Tail:")?;
            writeln!(file, "{}", tail_text(&attempt.stderr, 2_000))?;
        }

        writeln!(file)?;
    }

    writeln!(file, "RECOVERIES")?;
    writeln!(file, "----------")?;

    if result.recoveries.is_empty() {
        writeln!(file, "No recovery actions were needed.")?;
    } else {
        for recovery in &result.recoveries {
            writeln!(file, "Action: {}", recovery.action)?;
            writeln!(file, "  Executed: {}", recovery.executed)?;
            writeln!(file, "  Recovery Command Success: {}", recovery.success)?;
            writeln!(
                file,
                "  Next Command Passed: {:?}",
                recovery.command_passed_after
            )?;
            writeln!(file, "  Signal: {}", recovery.signal)?;
            writeln!(file, "  Failure Class: {}", recovery.failure_class)?;
            writeln!(
                file,
                "  Fingerprint: {} ({})",
                recovery.fingerprint, recovery.fingerprint_summary
            )?;
            writeln!(file, "  Source: {}", recovery.action_source)?;
            writeln!(file, "  Reason: {}", recovery.decision_reason)?;
            writeln!(file, "  Confidence: {:.2}", recovery.confidence)?;
            writeln!(file, "  Trail Samples: {}", recovery.trail_samples)?;
            writeln!(
                file,
                "  Trail Success Rate: {:.2}",
                recovery.trail_success_rate
            )?;
            writeln!(file, "  Details: {}", recovery.details)?;
            writeln!(file)?;
        }
    }

    writeln!(file)?;
    writeln!(file, "PEBBLE TRAIL CONTEXT")?;
    writeln!(file, "--------------------")?;

    for attempt in result
        .attempts
        .iter()
        .filter(|attempt| !matches!(attempt.signal, Signal::TaskCompleted))
    {
        let rows = trail_index.stats_for_fingerprint(&attempt.fingerprint.key);

        if rows.is_empty() {
            writeln!(file, "{}: no prior trail data", attempt.fingerprint.key)?;
        } else {
            writeln!(file, "{}", attempt.fingerprint.key)?;
            for row in rows {
                writeln!(
                    file,
                    "  {}: {}/{} success ({:.0}%), avg {}ms",
                    row.action,
                    row.successes,
                    row.samples,
                    row.success_rate() * 100.0,
                    row.average_duration_ms()
                )?;
            }
        }
    }

    writeln!(file)?;
    writeln!(file, "FAILURE TRAIL")?;
    writeln!(file, "-------------")?;

    if result.failures.is_empty() {
        writeln!(file, "No failures detected.")?;
    } else {
        for failure in &result.failures {
            writeln!(file, "- {}", failure)?;
        }
    }

    writeln!(file)?;
    writeln!(file, "Generated by {} v{}", PRODUCT_NAME, PRODUCT_VERSION)?;

    Ok(report_path)
}

fn print_stats(store: &PebbleStore, events: &[PebbleEvent]) {
    let trail_index = TrailIndex::from_events(events);
    let rows = trail_index.all_stats();

    println!("{} v{}", PRODUCT_NAME, PRODUCT_VERSION);
    println!("Pebble trail stats");
    println!("================================================");
    println!("Repo:        {}", store.repo_path.display());
    println!("Events file: {}", store.events_path.display());
    println!("Events:      {}", events.len());
    println!();

    if rows.is_empty() {
        println!("No Pebble recovery events have been recorded yet.");
        println!("Run a command that fails and recovers to create trail data.");
        return;
    }

    println!(
        "{:<30} {:<22} {:<8} {:<8} {:<10}",
        "Fingerprint", "Action", "Samples", "Success", "Avg ms"
    );
    println!("{}", "-".repeat(86));

    for (fingerprint, stats) in rows {
        println!(
            "{:<30} {:<22} {:<8} {:<8} {:<10}",
            truncate(&fingerprint, 30),
            truncate(&stats.action, 22),
            stats.samples,
            format!("{:.0}%", stats.success_rate() * 100.0),
            stats.average_duration_ms()
        );
    }
}

fn truncate(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        input.to_string()
    } else {
        let mut value: String = input.chars().take(max_chars.saturating_sub(1)).collect();
        value.push('~');
        value
    }
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- classifier tests ----

    #[test]
    fn fmt_check_exit_one_is_a_format_failure() {
        let command = CommandSpec {
            program: "cargo".to_string(),
            args: vec!["fmt".to_string(), "--check".to_string()],
            display: "cargo fmt --check".to_string(),
        };
        let output = ProcessOutput {
            success: false,
            exit_code: Some(1),
            timed_out: false,
            duration_ms: 10,
            stdout: String::new(),
            stderr: String::new(),
        };

        assert!(matches!(
            classify_output(&output, &command),
            Signal::FormatFailure
        ));
    }

    #[test]
    fn fmt_check_manifest_errors_are_not_format_failures() {
        let command = CommandSpec {
            program: "cargo".to_string(),
            args: vec!["fmt".to_string(), "--check".to_string()],
            display: "cargo fmt --check".to_string(),
        };
        let output = ProcessOutput {
            success: false,
            exit_code: Some(1),
            timed_out: false,
            duration_ms: 10,
            stdout: String::new(),
            stderr: "`cargo metadata` exited with an error: failed to parse manifest".to_string(),
        };

        assert!(matches!(
            classify_output(&output, &command),
            Signal::UnknownFailure
        ));
    }

    #[test]
    fn cannot_find_crate_does_not_match_compile_rule() {
        let command = CommandSpec {
            program: "cargo".to_string(),
            args: vec!["check".to_string()],
            display: "cargo check".to_string(),
        };
        let output = ProcessOutput {
            success: false,
            exit_code: Some(1),
            timed_out: false,
            duration_ms: 10,
            stdout: String::new(),
            stderr: "error[E0463]: cannot find crate for `std`".to_string(),
        };

        assert!(matches!(
            classify_output(&output, &command),
            Signal::UnknownFailure
        ));
    }

    #[test]
    fn cannot_find_crate_for_core_is_unknown() {
        let command = CommandSpec {
            program: "cargo".to_string(),
            args: vec!["check".to_string()],
            display: "cargo check".to_string(),
        };
        let output = ProcessOutput {
            success: false,
            exit_code: Some(1),
            timed_out: false,
            duration_ms: 10,
            stdout: String::new(),
            stderr: "error[E0463]: cannot find crate for `core`".to_string(),
        };

        assert!(matches!(
            classify_output(&output, &command),
            Signal::UnknownFailure
        ));
    }

    #[test]
    fn cannot_find_crate_for_alloc_is_unknown() {
        let command = CommandSpec {
            program: "cargo".to_string(),
            args: vec!["check".to_string()],
            display: "cargo check".to_string(),
        };
        let output = ProcessOutput {
            success: false,
            exit_code: Some(1),
            timed_out: false,
            duration_ms: 10,
            stdout: String::new(),
            stderr: "error[E0463]: cannot find crate for `alloc`".to_string(),
        };

        assert!(matches!(
            classify_output(&output, &command),
            Signal::UnknownFailure
        ));
    }

    /// Fix 2: a test that panics with "timed out" in the message must be a
    /// TestFailure (retryable), NOT InfraFlake.
    #[test]
    fn test_timeout_text_is_retryable_test_failure_not_infra() {
        let command = CommandSpec {
            program: "cargo".to_string(),
            args: vec!["test".to_string()],
            display: "cargo test".to_string(),
        };
        let output = ProcessOutput {
            success: false,
            exit_code: Some(1),
            timed_out: false, // process was NOT killed — test itself reported timeout
            duration_ms: 10,
            stdout: "test my_test ... FAILED\nfailures:\n  my_test\nthread 'my_test' panicked at 'operation timed out after 30s'".to_string(),
            stderr: String::new(),
        };

        let signal = classify_output(&output, &command);
        assert!(
            matches!(signal, Signal::TestFailure { retryable: true }),
            "expected TestFailure retryable=true, got {:?}",
            signal.label()
        );
    }

    /// Confirm process-level timeout still maps to Timeout signal (via timed_out flag).
    #[test]
    fn process_level_timeout_is_timeout_signal() {
        let command = CommandSpec {
            program: "cargo".to_string(),
            args: vec!["test".to_string()],
            display: "cargo test".to_string(),
        };
        let output = ProcessOutput {
            success: false,
            exit_code: Some(1),
            timed_out: true, // process was killed by harness
            duration_ms: 120_001,
            stdout: String::new(),
            stderr: String::new(),
        };

        assert!(matches!(classify_output(&output, &command), Signal::Timeout));
    }

    // ---- recovery policy tests ----

    #[test]
    fn non_auto_fix_compile_errors_do_not_get_format_recovery() {
        let signal = Signal::CompilationError {
            retryable: true,
            auto_fixable: false,
        };

        assert!(default_recovery_for_signal(&signal).is_none());
        assert!(!recovery_allowed_for_signal(
            Recovery::CargoFmtAndRetry,
            &signal
        ));
        assert!(!recovery_allowed_for_signal(
            Recovery::CargoCleanAndRetry,
            &signal
        ));
    }

    // ---- redaction tests ----

    #[test]
    fn redaction_preserves_compiler_token_notes() {
        let input = "= note: expected type `u32` (integer token: 42)";
        assert_eq!(redact_secrets(input).trim(), input);
    }

    #[test]
    fn redaction_removes_token_equals_values() {
        let input = "TOKEN=super-secret-value\ntoken=lowercase-secret";
        assert_eq!(
            redact_secrets(input).trim(),
            "[redacted potential secret]\n[redacted potential secret]"
        );
    }

    // ---- parser tests ----

    #[test]
    fn parse_command_line_keeps_quoted_arguments_together() {
        assert_eq!(
            parse_command_line("cargo test \"integration smoke\""),
            vec!["cargo", "test", "integration smoke"]
        );
    }

    // ---- oscillation tests ----

    #[test]
    fn oscillation_detection_catches_abab() {
        let trail = TrailIndex {
            by_fingerprint: HashMap::new(),
        };
        let mut engine = RecoveryEngine::new(3, 3, &trail);

        engine.remember_failure(FailureClass::Compilation);
        engine.remember_failure(FailureClass::StaleBranch);
        engine.remember_failure(FailureClass::Compilation);
        engine.remember_failure(FailureClass::StaleBranch);

        assert!(engine.detect_oscillation());
    }

    #[test]
    fn oscillation_detection_catches_abcabc() {
        let trail = TrailIndex {
            by_fingerprint: HashMap::new(),
        };
        let mut engine = RecoveryEngine::new(3, 3, &trail);

        for class in [
            FailureClass::Compilation,
            FailureClass::StaleBranch,
            FailureClass::Infra,
            FailureClass::Compilation,
            FailureClass::StaleBranch,
            FailureClass::Infra,
        ] {
            engine.remember_failure(class);
        }

        assert!(engine.detect_oscillation());
    }

    // ---- trail tests ----

    #[test]
    fn trail_index_ignores_empty_fingerprints_and_escalations() {
        let events = vec![
            test_event("", "CargoFmtAndRetry", true),
            test_event("rust_format_check_failed", "Escalate", false),
            test_event("rust_format_check_failed", "CargoFmtAndRetry", true),
        ];
        let index = TrailIndex::from_events(&events);
        let rows = index.all_stats();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "rust_format_check_failed");
        assert_eq!(rows[0].1.action, "CargoFmtAndRetry");
        assert_eq!(rows[0].1.samples, 1);
    }

    // ---- gitignore tests ----

    #[test]
    fn ensure_gitignore_entry_does_not_duplicate_pebble() {
        let repo_path = env::temp_dir().join(format!("pebble-gitignore-test-{}", now_millis()));
        fs::create_dir_all(&repo_path).unwrap();
        fs::write(repo_path.join(".gitignore"), ".pebble/\n/target\n").unwrap();

        let store = PebbleStore::new(repo_path.clone());
        store.ensure_gitignore_entry().unwrap();
        store.ensure_gitignore_entry().unwrap();

        let content = fs::read_to_string(repo_path.join(".gitignore")).unwrap();
        let pebble_lines = content
            .lines()
            .filter(|line| line.trim() == ".pebble/")
            .count();

        let _ = fs::remove_dir_all(repo_path);

        assert_eq!(pebble_lines, 1);
    }

    // ---- util tests ----

    #[test]
    fn util_combined_output_is_lowercase() {
        let combined = util::combined_output("STDOUT", "STDERR");
        assert_eq!(combined, "stdout\nstderr");
    }

    #[test]
    fn util_contains_any_returns_false_on_no_match() {
        assert!(!util::contains_any("hello world", &["foo", "bar"]));
    }

    // ---- helpers ----

    fn test_event(fingerprint: &str, action: &str, outcome_success: bool) -> PebbleEvent {
        PebbleEvent {
            timestamp_ms: 1,
            repo_id: "repo".to_string(),
            command: "cargo fmt --check".to_string(),
            fingerprint: fingerprint.to_string(),
            failure_class: "Format".to_string(),
            signal: "FormatFailure".to_string(),
            recovery_action: action.to_string(),
            action_source: "default-policy".to_string(),
            outcome_success,
            recovery_command_success: outcome_success,
            duration_ms: 10,
        }
    }
}
