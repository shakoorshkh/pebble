# Pebble + Claw Harness

Pebble + Claw Harness is a local-first MVP for deterministic software failure recovery.

Claw runs real commands. Pebble remembers failures, fingerprints them, records which recoveries worked, and then uses that history to choose the safest recovery on future runs.

## What Is Real Today

- Runs real commands in a real repo.
- Captures stdout, stderr, exit code, timeout status, and duration.
- Classifies common Rust, Git, formatting, test, timeout, and infrastructure failures.
- Fingerprints recurring failures such as `rust_format_check_failed`.
- Runs allowlisted recovery actions such as `cargo fmt`, `cargo fix`, `cargo clean`, wait-and-retry, and guarded Git sync.
- Stores local memory in `.pebble/events.jsonl`.
- Uses Pebble trail history only after enough evidence exists.
- Writes a human-readable `task_report.txt`.

## Deterministic Policy

Pebble will only choose a recovery from history when both are true:

- The failure fingerprint has at least `3` prior samples.
- The recovery has at least `70%` prior success.

Otherwise Claw uses a deterministic default policy or escalates to human review.

This is intentionally conservative. The product should earn autonomy with evidence, not vibes.

## Quick Start

Run the harness against the current repo:

```powershell
cargo run -- . cargo test
```

Preview a recovery plan without running recovery actions or writing Pebble memory:

```powershell
cargo run -- --dry-run . cargo fmt --check
```

Run the stats view:

```powershell
cargo run -- stats .
```

## Dry-Run Mode

Dry-run mode is observe-and-plan mode.

It does run the target command once so Pebble can classify real output. It does not run recovery commands, write `.pebble/events.jsonl`, or write `task_report.txt`.

Dry-run still exits with the observed command result. If the observed command fails, dry-run exits `1`.

Use it when adding or testing classifier rules:

```powershell
cargo run -- --dry-run C:\Users\Shakoor\pebble-demo cargo fmt --check
```

Expected output on a formatting failure:

```text
Signal:      FormatFailure
Fingerprint: rust_format_check_failed
DRY RUN: would execute recovery action CargoFmtAndRetry
```

## Classifier Modules

Classifier rules live behind a registry in `src/classifiers.rs`.

Current classifiers:

- `CargoFmtCheckClassifier`
- `GitStateClassifier`
- `InfraClassifier`
- `RustToolchainClassifier`
- `RustCompileClassifier`
- `RustTestClassifier`

New tools should be added as new classifiers rather than expanding the executor core.

## Full Demo

Run the included demo script:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\run-demo.ps1
```

The script creates a temporary Rust demo project under `.demo`, introduces formatting failures, runs Claw, and shows Pebble learning the recovery trail.

Expected milestone:

```text
Decision source: pebble-trail
Pebble trail selected CargoFmtAndRetry: 3/3 prior successes
```

## Manual Demo

Create or open a Rust project with a badly formatted `src/main.rs`:

```rust
fn main(){println!("hello");}
```

Run:

```powershell
cargo run -- C:\Users\Shakoor\pebble-demo cargo fmt --check
```

Claw should detect `FormatFailure`, run `cargo fmt`, rerun `cargo fmt --check`, and pass.

Then inspect memory:

```powershell
cargo run -- stats C:\Users\Shakoor\pebble-demo
```

## Safety Rules

- Commands are run directly, not through a shell.
- Unknown failures escalate.
- Retry and same-failure limits are enforced.
- Git rebase only runs when the working tree is clean.
- Logs are redacted for common secret patterns before storage.
- The harness never claims recovery unless the command passes after recovery.
- `.pebble/` is added to the target repo `.gitignore` before memory events are written.

## Product Positioning

Sell this first as:

> Pebble for CI Failure Memory: a deterministic system that shows which failures repeat, which recoveries work, and which recoveries are safe to automate.

Do not sell it yet as a full autonomous coding platform. The next commercial step is CI integration, especially GitHub Actions, plus a small dashboard.
