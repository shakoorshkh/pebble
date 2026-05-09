$ErrorActionPreference = "Stop"

$ScriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$HarnessRoot = (Resolve-Path (Join-Path $ScriptRoot "..")).Path
$RunId = Get-Date -Format "yyyyMMdd-HHmmss"
$DemoRoot = Join-Path $HarnessRoot ".demo"
$DemoRepo = Join-Path $DemoRoot "pebble-format-demo-$RunId"

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)]
        [ScriptBlock] $Command,

        [Parameter(Mandatory = $true)]
        [string] $Label
    )

    Write-Host ""
    Write-Host "==> $Label"
    & $Command

    if ($LASTEXITCODE -ne 0) {
        throw "Step failed: $Label"
    }
}

function Set-BadFormat {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Message
    )

    $MainPath = Join-Path $DemoRepo "src\main.rs"
    Set-Content -Path $MainPath -Value "fn main(){println!(""$Message"");}" -Encoding ASCII
}

Write-Host "Pebble demo"
Write-Host "Harness: $HarnessRoot"
Write-Host "Demo repo: $DemoRepo"

New-Item -ItemType Directory -Force -Path $DemoRoot | Out-Null

Invoke-Checked -Label "Create temporary Rust demo project" -Command {
    cargo new $DemoRepo --bin --name pebble_format_demo
}

$Messages = @(
    "first format recovery",
    "second format recovery",
    "third format recovery",
    "pebble trail decision"
)

for ($Index = 0; $Index -lt $Messages.Count; $Index++) {
    $RunNumber = $Index + 1
    Set-BadFormat -Message $Messages[$Index]

    Invoke-Checked -Label "Run harness recovery sample $RunNumber" -Command {
        cargo run --manifest-path (Join-Path $HarnessRoot "Cargo.toml") -- $DemoRepo cargo fmt --check
    }
}

Invoke-Checked -Label "Show Pebble trail stats" -Command {
    cargo run --manifest-path (Join-Path $HarnessRoot "Cargo.toml") -- stats $DemoRepo
}

Write-Host ""
Write-Host "Demo complete."
Write-Host "Look for: Decision source: pebble-trail"
