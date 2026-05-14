# Find existing codex binaries on a Windows remote and emit "codex:<path>" for
# the newest parseable version, or the first executable fallback if none report
# a version.
$ErrorActionPreference = 'SilentlyContinue'

$firstPath = $null
$bestPath = $null
$bestVersion = $null
$seen = @{}

function Consider-CodexPath {
    param([string]$Path)
    if ([string]::IsNullOrWhiteSpace($Path)) {
        return
    }
    try {
        $resolved = [System.IO.Path]::GetFullPath([Environment]::ExpandEnvironmentVariables($Path))
    } catch {
        return
    }
    if ($seen.ContainsKey($resolved)) {
        return
    }
    $seen[$resolved] = $true
    if (-not (Test-Path -LiteralPath $resolved -PathType Leaf)) {
        return
    }
    if ($null -eq $firstPath) {
        $firstPath = $resolved
    }

    $versionText = (& $resolved --version 2>$null) -join "`n"
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($versionText)) {
        return
    }
    $match = [regex]::Match($versionText, '(\d+)\.(\d+)\.(\d+)')
    if (-not $match.Success) {
        return
    }
    try {
        $version = [version]::new(
            [int]$match.Groups[1].Value,
            [int]$match.Groups[2].Value,
            [int]$match.Groups[3].Value
        )
    } catch {
        return
    }
    if ($null -eq $bestVersion -or $version.CompareTo($bestVersion) -gt 0) {
        $bestVersion = $version
        $bestPath = $resolved
    }
}

Get-Command codex -All -ErrorAction SilentlyContinue | ForEach-Object {
    Consider-CodexPath $_.Source
}

$codexHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $HOME '.codex' }
$commonCandidates = @(
    (Join-Path $codexHome 'packages\standalone\current\codex.exe'),
    (Join-Path $codexHome 'packages\standalone\current\codex.cmd'),
    (Join-Path $HOME 'AppData\Roaming\npm\codex.cmd'),
    (Join-Path $HOME '.cargo\bin\codex.exe'),
    (Join-Path $HOME '.bun\bin\codex.exe'),
    (Join-Path $HOME '.bun\bin\codex.cmd'),
    (Join-Path $HOME '.volta\bin\codex.exe'),
    (Join-Path $HOME '.volta\bin\codex.cmd'),
    (Join-Path $HOME '.local\bin\codex.exe')
)
foreach ($candidate in $commonCandidates) {
    Consider-CodexPath $candidate
}

if ($null -ne $bestPath) {
    Write-Output "codex:$bestPath"
    exit 0
}
if ($null -ne $firstPath) {
    Write-Output "codex:$firstPath"
    exit 0
}
