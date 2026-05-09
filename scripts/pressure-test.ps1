# ============================================================
# Pebble v4.2.0 — Full Pressure Test
# ============================================================

$ErrorActionPreference = "Continue"

# ---- config ----
$PEBBLE_ROOT = Resolve-Path "$PSScriptRoot\.."
$DEMO_ROOT   = "$env:TEMP\pebble-pressure-test-$(Get-Random)"
$PASS        = 0
$FAIL        = 0
$FAILURES    = @()

# ---- helpers ----
function Banner($text) {
    Write-Host ""
    Write-Host "================================================" -ForegroundColor Cyan
    Write-Host "  $text" -ForegroundColor Cyan
    Write-Host "================================================" -ForegroundColor Cyan
}

function Ok($label) {
    Write-Host "  [PASS] $label" -ForegroundColor Green
    $script:PASS++
}

function Fail($label, $detail) {
    Write-Host "  [FAIL] $label" -ForegroundColor Red
    if ($detail) {
        Write-Host "         $detail" -ForegroundColor DarkRed
    }
    $script:FAIL++
    $script:FAILURES += "$label - $detail"
}

function Assert-Contains($label, $text, $needle) {
    if ($text -match [regex]::Escape($needle)) {
        Ok $label
    } else {
        Fail $label "Expected to find: $needle"
    }
}

function Assert-NotContains($label, $text, $needle) {
    if ($text -notmatch [regex]::Escape($needle)) {
        Ok $label
    } else {
        Fail $label "Did NOT expect to find: $needle"
    }
}

function Assert-FileExists($label, $path) {
    if (Test-Path $path) {
        Ok $label
    } else {
        Fail $label "File not found: $path"
    }
}

function Assert-FileNotExists($label, $path) {
    if (-not (Test-Path $path)) {
        Ok $label
    } else {
        Fail $label "File should not exist: $path"
    }
}

function Assert-ExitCode($label, $expected, $actual) {
    if ($actual -eq $expected) {
        Ok $label
    } else {
        Fail $label "Expected exit $expected, got $actual"
    }
}

function Invoke-Pebble($args_str) {
    $cmd = "cargo run --manifest-path `"$PEBBLE_ROOT\Cargo.toml`" -- $args_str"
    $output = Invoke-Expression $cmd 2>&1 | Out-String

    return @{
        Output = $output
        ExitCode = $LASTEXITCODE
    }
}

function New-DemoRepo($name) {
    $path = "$DEMO_ROOT\$name"

    New-Item -ItemType Directory -Path $path -Force | Out-Null

    Set-Content "$path\Cargo.toml" @"
[package]
name = "$name"
version = "0.1.0"
edition = "2021"
"@

    New-Item -ItemType Directory -Path "$path\src" -Force | Out-Null

    return $path
}

function Write-BadFmt($repo) {
    Set-Content "$repo\src\main.rs" 'fn main(){println!("hello");}'
}

function Write-GoodFmt($repo) {
    Set-Content "$repo\src\main.rs" @"
fn main() {
    println!("hello");
}
"@
}

function Write-CompileError($repo) {
    Set-Content "$repo\src\main.rs" @"
fn main() {
    let x: u32 = "not a number";
    println!("{}", x);
}
"@
}

# ============================================================
Banner "0. Cleanup old binary artifacts"
# ============================================================

$oldFiles = @(
    "$PEBBLE_ROOT\target\debug\claw-harness.exe",
    "$PEBBLE_ROOT\target\debug\claw_harness.pdb",
    "$PEBBLE_ROOT\target\debug\claw-harness.d"
)

foreach ($f in $oldFiles) {
    if (Test-Path $f) {
        Remove-Item $f -Force
    }
}

Ok "Old Claw Harness artifacts removed"

# ============================================================
Banner "1. Binary name + branding"
# ============================================================

$buildOut = (cargo build --manifest-path "$PEBBLE_ROOT\Cargo.toml" 2>&1 | Out-String)

$binaryPath = "$PEBBLE_ROOT\target\debug\pebble-ci.exe"

Assert-FileExists "Binary is pebble-ci.exe" $binaryPath
Assert-FileNotExists "Old claw-harness.exe removed" "$PEBBLE_ROOT\target\debug\claw-harness.exe"

$versionOut = (& "$binaryPath" 2>&1 | Out-String)

Assert-Contains "Header says Pebble" $versionOut "Pebble v4.2.0"
Assert-NotContains "No Claw references remain" $versionOut "Claw"
Assert-Contains "Usage shows pebble-ci.exe" $versionOut "pebble-ci.exe"

# ============================================================
Banner "2. Format failure → recovery → success"
# ============================================================

$repo1 = New-DemoRepo "fmt-recovery"
Write-BadFmt $repo1

$r = Invoke-Pebble "$repo1 cargo fmt --check"

Assert-ExitCode "Exits 0 after recovery" 0 $r.ExitCode
Assert-Contains "Detects FormatFailure" $r.Output "FormatFailure"
Assert-Contains "Runs CargoFmtAndRetry" $r.Output "CargoFmtAndRetry"
Assert-Contains "Attempt 2 succeeds" $r.Output "TaskCompleted"
Assert-Contains "Verdict successful" $r.Output "real command completed successfully"

Assert-FileExists ".pebble directory exists" "$repo1\.pebble"
Assert-FileExists "events.jsonl written" "$repo1\.pebble\events.jsonl"
Assert-FileExists "task_report.txt in .pebble" "$repo1\.pebble\task_report.txt"
Assert-FileNotExists "task_report.txt not in root" "$repo1\task_report.txt"

# ============================================================
Banner "3. Dry-run mode"
# ============================================================

$repo2 = New-DemoRepo "dry-run"
Write-BadFmt $repo2

$r = Invoke-Pebble "--dry-run $repo2 cargo fmt --check"

Assert-ExitCode "Dry-run exits 1" 1 $r.ExitCode
Assert-Contains "Dry-run shows FormatFailure" $r.Output "FormatFailure"
Assert-Contains "Dry-run shows planned recovery" $r.Output "CargoFmtAndRetry"
Assert-FileNotExists "Dry-run writes no .pebble" "$repo2\.pebble"

# ============================================================
Banner "4. Clean pass"
# ============================================================

$repo3 = New-DemoRepo "clean-pass"
Write-GoodFmt $repo3

$r = Invoke-Pebble "$repo3 cargo fmt --check"

Assert-ExitCode "Clean pass exits 0" 0 $r.ExitCode
Assert-Contains "TaskCompleted detected" $r.Output "TaskCompleted"
Assert-NotContains "No recovery executed" $r.Output "CargoFmtAndRetry"

# ============================================================
Banner "5. Compilation error escalation"
# ============================================================

$repo4 = New-DemoRepo "compile-error"
Write-CompileError $repo4

$r = Invoke-Pebble "$repo4 cargo check"

Assert-ExitCode "Compile error exits 1" 1 $r.ExitCode
Assert-Contains "CompilationError detected" $r.Output "CompilationError"
Assert-Contains "Escalates safely" $r.Output "ESCALATED"
Assert-Contains "Human review required" $r.Output "human review required"
Assert-NotContains "No cargo fmt attempted" $r.Output "cargo fmt completed"

# ============================================================
Banner "6. Stats command"
# ============================================================

$r = Invoke-Pebble "stats $repo1"

Assert-ExitCode "Stats exits 0" 0 $r.ExitCode
Assert-Contains "Shows fingerprint" $r.Output "rust_format_check_failed"
Assert-Contains "Shows recovery action" $r.Output "CargoFmtAndRetry"
Assert-NotContains "No Claw branding" $r.Output "Claw"

# ============================================================
Banner "7. Trail learning activation"
# ============================================================

$repo7 = New-DemoRepo "trail-learning"

# Run 1
Write-BadFmt $repo7
$r1 = Invoke-Pebble "$repo7 cargo fmt --check"
Assert-ExitCode "Run 1 exits 0" 0 $r1.ExitCode

# Run 2
Write-BadFmt $repo7
$r2 = Invoke-Pebble "$repo7 cargo fmt --check"
Assert-ExitCode "Run 2 exits 0" 0 $r2.ExitCode

# Run 3
Write-BadFmt $repo7
$r3 = Invoke-Pebble "$repo7 cargo fmt --check"
Assert-ExitCode "Run 3 exits 0" 0 $r3.ExitCode

# Run 4 should activate pebble-trail
Write-BadFmt $repo7
$r4 = Invoke-Pebble "$repo7 cargo fmt --check"

Assert-ExitCode "Run 4 exits 0" 0 $r4.ExitCode
Assert-Contains "Uses pebble-trail" $r4.Output "pebble-trail"
Assert-Contains "Shows prior successes" $r4.Output "prior successes"
Assert-Contains "Shows 3/3 success" $r4.Output "3/3"

# ============================================================
Banner "8. .gitignore idempotency"
# ============================================================

$repo8 = New-DemoRepo "gitignore-idempotent"
Write-BadFmt $repo8

Invoke-Pebble "$repo8 cargo fmt --check" | Out-Null
Invoke-Pebble "$repo8 cargo fmt --check" | Out-Null

$gi = Get-Content "$repo8\.gitignore"
$pebbleLines = ($gi | Where-Object { $_.Trim() -eq ".pebble/" }).Count

if ($pebbleLines -eq 1) {
    Ok ".pebble/ appears exactly once"
} else {
    Fail ".pebble/ duplication" "Found $pebbleLines entries"
}

# ============================================================
Banner "9. Branding verification"
# ============================================================

$repo9 = New-DemoRepo "branding"
Write-BadFmt $repo9

$r = Invoke-Pebble "$repo9 cargo fmt --check"

Assert-NotContains "No Claw Harness in output" $r.Output "Claw Harness"
Assert-NotContains "No claw-harness in output" $r.Output "claw-harness"
Assert-Contains "Pebble branding present" $r.Output "Pebble v4.2.0"

$report = Get-Content "$repo9\.pebble\task_report.txt" -Raw

Assert-NotContains "No Claw branding in report" $report "Claw"
Assert-Contains "Pebble branding in report" $report "Pebble v4.2.0"

# ============================================================
Banner "10. Event version field"
# ============================================================

$events = Get-Content "$repo1\.pebble\events.jsonl" -Raw

Assert-Contains "Version is 4.2.0" $events '"version":"4.2.0"'
Assert-NotContains "No old version remains" $events '"version":"4.1'

# ============================================================
Banner "11. cargo test"
# ============================================================

$testOut = (cargo test --manifest-path "$PEBBLE_ROOT\Cargo.toml" 2>&1 | Out-String)

Assert-Contains "All tests pass" $testOut "test result: ok"
Assert-Contains "17 tests run" $testOut "17 passed"
Assert-NotContains "No failed tests" $testOut "test result: FAILED"

# ============================================================
Banner "SUMMARY"
# ============================================================

Write-Host ""
Write-Host "Passed: $PASS" -ForegroundColor Green
Write-Host "Failed: $FAIL" -ForegroundColor $(if ($FAIL -eq 0) { "Green" } else { "Red" })

if ($FAILURES.Count -gt 0) {
    Write-Host ""
    Write-Host "Failures:" -ForegroundColor Red

    foreach ($f in $FAILURES) {
        Write-Host "  - $f" -ForegroundColor Red
    }
}

Write-Host ""
Write-Host "Cleaning temp repos: $DEMO_ROOT"
Remove-Item -Recurse -Force $DEMO_ROOT -ErrorAction SilentlyContinue

if ($FAIL -eq 0) {
    Write-Host ""
    Write-Host "All tests passed. Pebble is ready to ship." -ForegroundColor Green
    exit 0
} else {
    Write-Host ""
    Write-Host "Fix failures before shipping." -ForegroundColor Red
    exit 1
}