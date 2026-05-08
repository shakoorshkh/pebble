use crate::util::{combined_output, contains_any};
use crate::{CommandSpec, ProcessOutput, Signal};

// ---------------------------------------------------------------------------
// Public classifier trait and dispatch
// ---------------------------------------------------------------------------

pub(crate) trait SignalClassifier {
    fn classify(&self, output: &ProcessOutput, command: &CommandSpec) -> Option<Signal>;
}

/// Classify a process output into a `Signal`.
///
/// Classifiers are tried in priority order.  The first one to return `Some`
/// wins.  If none match, `UnknownFailure` is returned.
///
/// Priority rationale:
///   1. CargoFmtCheck   — exact command+exit-code match; highest specificity
///   2. GitState        — git keyword match; unambiguous
///   3. Infra           — network / connectivity keywords only (no timeout text
///                        — process timeout is handled by the caller via
///                        `output.timed_out` before this function is reached)
///   4. RustToolchain   — std/core/alloc crate-missing errors that look like
///                        compile errors but are not fixable via cargo fix/fmt
///   5. RustCompile     — general rustc error patterns
///   6. RustTest        — test harness output patterns (includes retryable
///                        timeout detection for test-level timeouts that are
///                        reported in stdout, not via process kill)
pub(crate) fn classify_output(output: &ProcessOutput, command: &CommandSpec) -> Signal {
    if output.success {
        return Signal::TaskCompleted;
    }

    if output.timed_out {
        return Signal::Timeout;
    }

    let classifiers: [&dyn SignalClassifier; 6] = [
        &CargoFmtCheckClassifier,
        &GitStateClassifier,
        &InfraClassifier,
        &RustToolchainClassifier,
        &RustCompileClassifier,
        &RustTestClassifier,
    ];

    for classifier in classifiers {
        if let Some(signal) = classifier.classify(output, command) {
            return signal;
        }
    }

    Signal::UnknownFailure
}

// ---------------------------------------------------------------------------
// Fix 1: CargoFmtCheck — content-independent on exit 1, manifest errors excepted
// ---------------------------------------------------------------------------

struct CargoFmtCheckClassifier;

impl SignalClassifier for CargoFmtCheckClassifier {
    fn classify(&self, output: &ProcessOutput, command: &CommandSpec) -> Option<Signal> {
        if !is_cargo_fmt_check(command) || output.exit_code != Some(1) {
            return None;
        }

        let combined = combined_output(&output.stdout, &output.stderr);

        // Manifest / toolchain errors produce exit 1 too but are not formatting
        // issues — let them fall through to UnknownFailure.
        let is_manifest_error = contains_any(
            &combined,
            &[
                "`cargo metadata` exited with an error",
                "failed to parse manifest",
                "failed to load manifest",
                "could not find `cargo.toml`",
            ],
        );

        if is_manifest_error {
            None
        } else {
            Some(Signal::FormatFailure)
        }
    }
}

// ---------------------------------------------------------------------------
// Git state
// ---------------------------------------------------------------------------

struct GitStateClassifier;

impl SignalClassifier for GitStateClassifier {
    fn classify(&self, output: &ProcessOutput, _command: &CommandSpec) -> Option<Signal> {
        let combined = combined_output(&output.stdout, &output.stderr);

        if contains_any(
            &combined,
            &[
                "non-fast-forward",
                "your branch is behind",
                "needs merge",
                "merge conflict",
                "automatic merge failed",
                "cannot rebase",
                "rebase in progress",
            ],
        ) {
            Some(Signal::StaleBranch)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Fix 2: Infra — network / connectivity only; NO timeout text
//
// Rationale: "timeout" / "timed out" appear in test panic messages
// ("operation timed out after 30s") and would misclassify retryable test
// failures as InfraFlake, polluting trail memory with the wrong fingerprint.
// Process-level timeouts are already caught by `output.timed_out` above.
// ---------------------------------------------------------------------------

struct InfraClassifier;

impl SignalClassifier for InfraClassifier {
    fn classify(&self, output: &ProcessOutput, _command: &CommandSpec) -> Option<Signal> {
        let combined = combined_output(&output.stdout, &output.stderr);

        if contains_any(
            &combined,
            &[
                "connection reset",
                "connection refused",
                "temporary failure",
                "failed to download",
                "failed to fetch",
                "spurious network error",
                "network failure",
                "dns error",
                "http 502",
                "http 503",
                "http 504",
            ],
        ) {
            Some(Signal::InfraFlake)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Fix 3: RustToolchain — std / core / alloc missing-crate errors
//
// These look like compilation errors in output but are toolchain / target
// misconfiguration issues.  They are not fixable by cargo fix or cargo fmt,
// and classifying them as CompilationError would cause misleading recovery
// attempts.  We emit UnknownFailure so the engine escalates to human review.
//
// Matches: "cannot find crate for `std`"
//          "cannot find crate for `core`"
//          "cannot find crate for `alloc`"
//          "can't find crate for `std`"   (older rustc wording)
//          "can't find crate for `core`"
//          "can't find crate for `alloc`"
//
// Uses prefix matching on "cannot find crate for" to be forward-compatible
// with any future std-family crate names rustc may emit.
// ---------------------------------------------------------------------------

struct RustToolchainClassifier;

impl SignalClassifier for RustToolchainClassifier {
    fn classify(&self, output: &ProcessOutput, _command: &CommandSpec) -> Option<Signal> {
        let combined = combined_output(&output.stdout, &output.stderr);

        // Prefix patterns cover std, core, alloc, and any future additions.
        if contains_any(
            &combined,
            &[
                "cannot find crate for `std`",
                "cannot find crate for `core`",
                "cannot find crate for `alloc`",
                "can't find crate for `std`",
                "can't find crate for `core`",
                "can't find crate for `alloc`",
                // Broader fallback: any "cannot find crate for" not already
                // matched above will hit this; keeps forward-compatibility.
                "cannot find crate for",
                "can't find crate for",
            ],
        ) {
            Some(Signal::UnknownFailure)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Rust compile errors
// ---------------------------------------------------------------------------

struct RustCompileClassifier;

impl SignalClassifier for RustCompileClassifier {
    fn classify(&self, output: &ProcessOutput, _command: &CommandSpec) -> Option<Signal> {
        let combined = combined_output(&output.stdout, &output.stderr);

        if contains_any(
            &combined,
            &[
                "could not compile",
                "failed to compile",
                "compilation failed",
                "error[e",
                "unresolved import",
                "cannot find value",
                "cannot find type",
                "mismatched types",
                "borrowed value does not live long enough",
            ],
        ) {
            let auto_fixable = contains_any(
                &combined,
                &[
                    "cargo fix",
                    "machine-applicable",
                    "unused import",
                    "unused variable",
                    "remove this",
                ],
            );

            Some(Signal::CompilationError {
                retryable: true,
                auto_fixable,
            })
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Fix 2 (continued): RustTest — retains timeout detection for test-level
// timeouts that appear in stdout (not via process kill).
// ---------------------------------------------------------------------------

struct RustTestClassifier;

impl SignalClassifier for RustTestClassifier {
    fn classify(&self, output: &ProcessOutput, _command: &CommandSpec) -> Option<Signal> {
        let combined = combined_output(&output.stdout, &output.stderr);

        if contains_any(
            &combined,
            &[
                "test result: failed",
                "failures:",
                "panicked at",
                "assertion failed",
                "assertion `",
            ],
        ) {
            // Timeout text here means the test itself reported a timeout
            // in its output (e.g. a tokio / async test) — correctly retryable.
            let retryable = contains_any(
                &combined,
                &[
                    "timeout",
                    "timed out",
                    "connection reset",
                    "connection refused",
                    "temporary failure",
                    "flaky",
                ],
            );

            Some(Signal::TestFailure { retryable })
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers (private to this module — callers use util::*)
// ---------------------------------------------------------------------------

fn is_cargo_fmt_check(command: &CommandSpec) -> bool {
    command.program.eq_ignore_ascii_case("cargo")
        && command
            .args
            .first()
            .map(|arg| arg.eq_ignore_ascii_case("fmt"))
            .unwrap_or(false)
        && command.args.iter().any(|arg| arg == "--check")
}
