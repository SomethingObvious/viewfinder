param(
    # Skips the tests, which need the GPU and the NVIDIA DLLs.
    [switch]$NoTests
)

# Every check this repository has, in one place. Each one is fatal, and a missing
# tool fails its step rather than being skipped.
#
#   scripts\lint.ps1             everything
#   scripts\lint.ps1 -NoTests    everything but cargo test
#
# Written for Windows PowerShell 5.1, since that's what install.ps1 needs too.

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$Failed = @()

function Invoke-Step([string]$Name, [scriptblock]$Body) {
    Write-Host ''
    Write-Host "> $Name" -ForegroundColor White
    $ok = $false
    try { $ok = [bool](& $Body) } catch {
        Write-Host "  x $($_.Exception.Message)" -ForegroundColor Red
    }
    if ($ok) { Write-Host '  + ok' -ForegroundColor Green }
    else { $script:Failed += $Name }
}

function Test-Tool([string]$Name) {
    if (Get-Command $Name -ErrorAction SilentlyContinue) { return $true }
    Write-Host "  x $Name isn't installed" -ForegroundColor Red
    return $false
}

function Invoke-Native([string]$Exe, [string[]]$Arguments) {
    if (-not (Test-Tool $Exe)) { return $false }
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    Push-Location $Root
    # Each stderr line as plain text, since 5.1 wraps them in error records.
    try { & $Exe @Arguments 2>&1 | ForEach-Object { "$_" } | Out-Host; return ($LASTEXITCODE -eq 0) }
    finally { Pop-Location; $ErrorActionPreference = $prev }
}

$Files = @(& git -C $Root ls-files --cached --others --exclude-standard | Where-Object { $_ })

function Get-FullPath([string]$Rel) { return (Join-Path $Root ($Rel -replace '/', '\')) }

function Test-BinaryFile([string]$Rel) {
    $full = Get-FullPath $Rel
    if (-not (Test-Path -LiteralPath $full)) { return $true }
    $bytes = [System.IO.File]::ReadAllBytes($full)
    $probe = [Math]::Min($bytes.Length, 8192)
    for ($i = 0; $i -lt $probe; $i++) { if ($bytes[$i] -eq 0) { return $true } }
    return $false
}

# Lists each line of the text files that matches $Pattern, skipping files that match $Skip.
function Find-Line([string]$Pattern, [string]$Skip, [string]$Why) {
    $hits = @()
    foreach ($rel in $Files) {
        if ($Skip -and $rel -match $Skip) { continue }
        if (Test-BinaryFile $rel) { continue }
        $n = 0
        foreach ($line in [System.IO.File]::ReadAllLines((Get-FullPath $rel))) {
            $n++
            if ($line -cmatch $Pattern) { $hits += "${rel}:${n}: $($line.Trim())" }
        }
    }
    if ($hits.Count -eq 0) { return $true }
    Write-Host "  x $Why" -ForegroundColor Red
    foreach ($h in $hits) { Write-Host "      $h" -ForegroundColor Red }
    return $false
}

# An #[allow] hides a lint for good. A lint that's wrong for one item gets
# #[expect(lint, reason = "...")] there instead, which fails when the lint stops firing.
$Suppressions = '#!?\[\s*(cfg_attr\(.*)?allow\s*\(' +
'|rustfmt::skip' +
'|markdownlint-(disable|enable|restore|capture|configure)' +
'|PSScriptAnalyzer\s*-\s*disable|SuppressMessageAttribute' +
'|typos:\s*(off|ignore|disable)' +
'|gitleaks:allow'

# A config file below the root would quietly loosen the rules for its folder.
$ConfigNames = @('clippy.toml', '.clippy.toml', 'rustfmt.toml', '.rustfmt.toml', 'deny.toml',
    '.typos.toml', '_typos.toml', 'typos.toml', '.editorconfig', '.gitleaks.toml',
    '.markdownlint-cli2.yaml', '.markdownlint-cli2.jsonc', '.markdownlint.yaml',
    '.markdownlint.json', '.markdownlint.jsonc', 'rust-toolchain.toml', 'rust-toolchain',
    'config.toml', 'PSScriptAnalyzerSettings.psd1', '.ecrc', '.editorconfig-checker.json')
$ConfigHomes = @{
    'config.toml'                   = '.cargo'
    'PSScriptAnalyzerSettings.psd1' = 'scripts'
}

function Test-NoNestedConfig {
    $hits = @()
    foreach ($file in (Get-ChildItem -LiteralPath $Root -Recurse -File -Force)) {
        if ($file.FullName -match '\\(\.git|target|node_modules)\\') { continue }
        if ($ConfigNames -notcontains $file.Name) { continue }
        $dir = $file.DirectoryName.Substring($Root.TrimEnd('\').Length).Trim('\')
        $home_ = ''
        if ($ConfigHomes.ContainsKey($file.Name)) { $home_ = $ConfigHomes[$file.Name] }
        if ($dir -eq $home_) { continue }
        $hits += $file.FullName.Substring($Root.Length).TrimStart('\')
    }
    if ($hits.Count -eq 0) { return $true }
    Write-Host '  x lint config belongs at the root, and these are elsewhere:' -ForegroundColor Red
    foreach ($h in $hits) { Write-Host "      $h" -ForegroundColor Red }
    return $false
}

# rustflags or RUSTFLAGS can turn every lint down at once without touching Cargo.toml.
function Test-LintCap {
    $capping = '--cap-lints|(^|[\s"''])-(A|W)\s*[a-z"'']|--(allow|warn)\b'
    $ok = $true
    $config = Join-Path $Root '.cargo\config.toml'
    if (Test-Path -LiteralPath $config) {
        foreach ($line in [System.IO.File]::ReadAllLines($config)) {
            if ($line -match '^\s*#') { continue }
            if ($line -match $capping) {
                Write-Host "  x .cargo\config.toml turns lints down: $($line.Trim())" -ForegroundColor Red
                $ok = $false
            }
        }
    }
    foreach ($name in @('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTDOCFLAGS', 'CLIPPY_ARGS')) {
        $value = [Environment]::GetEnvironmentVariable($name)
        if ($value -and ($value -replace [char]0x1f, ' ') -match $capping) {
            Write-Host "  x $name turns lints down: $value" -ForegroundColor Red
            $ok = $false
        }
    }
    return $ok
}

function Test-Encoding {
    $hits = @()
    foreach ($rel in $Files) {
        if (Test-BinaryFile $rel) { continue }
        $full = Get-FullPath $rel
        $bytes = [System.IO.File]::ReadAllBytes($full)
        $hasBom = ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF)
        $text = [System.IO.File]::ReadAllText($full)
        $crlf = $text.Contains("`r`n")
        $lfOnly = ($text -replace "`r`n", '').Contains("`n")
        if ($rel -match '\.ps(1|d1|m1)$') {
            # Windows PowerShell reads a script without a BOM as ANSI.
            if (-not $hasBom) { $hits += "$rel isn't UTF-8 with a BOM" }
            if ($lfOnly) { $hits += "$rel has LF line endings" }
        }
        else {
            if ($hasBom) { $hits += "$rel has a byte-order mark" }
            if ($crlf) { $hits += "$rel has CRLF line endings" }
        }
    }
    if ($hits.Count -eq 0) { return $true }
    foreach ($h in $hits) { Write-Host "  x $h" -ForegroundColor Red }
    return $false
}

function Invoke-ScriptAnalyzerCheck {
    if (-not (Get-Module -ListAvailable PSScriptAnalyzer)) {
        Write-Host '  x PSScriptAnalyzer isn''t installed' -ForegroundColor Red
        return $false
    }
    Import-Module PSScriptAnalyzer
    $settings = Join-Path $PSScriptRoot 'PSScriptAnalyzerSettings.psd1'
    $found = @()
    $unformatted = @()
    foreach ($rel in ($Files | Where-Object { $_ -match '\.ps(1|d1|m1)$' })) {
        $full = Get-FullPath $rel
        $found += @(Invoke-ScriptAnalyzer -Path $full -Settings $settings)
        $text = [System.IO.File]::ReadAllText($full)
        $formatted = (Invoke-Formatter -ScriptDefinition $text) -replace "`r?`n", "`r`n"
        if ($formatted -ne $text) { $unformatted += $rel }
    }
    foreach ($f in $found) {
        Write-Host "  x $($f.ScriptName):$($f.Line) $($f.RuleName) $($f.Message)" -ForegroundColor Red
    }
    foreach ($u in $unformatted) { Write-Host "  x $u isn't formatted (Invoke-Formatter)" -ForegroundColor Red }
    return ($found.Count -eq 0 -and $unformatted.Count -eq 0)
}

function Invoke-CargoDoc {
    $prev = $env:RUSTDOCFLAGS
    $env:RUSTDOCFLAGS = '-D warnings'
    try {
        return (Invoke-Native 'cargo' @('doc', '--workspace', '--no-deps', '--all-features',
                '--document-private-items', '--locked', '--quiet'))
    }
    finally { $env:RUSTDOCFLAGS = $prev }
}

$Self = '^scripts/lint\.ps1$'
Invoke-Step 'no inline suppressions' {
    Find-Line -Pattern $Suppressions -Skip $Self -Why 'put the exception in the config, or #[expect] with a reason on the item'
}
Invoke-Step 'no nested lint config' { Test-NoNestedConfig }
Invoke-Step 'lints not capped' { Test-LintCap }
Invoke-Step 'no dashes as punctuation' {
    Find-Line -Pattern "[$([char]0x2013)$([char]0x2014)]" -Skip $Self -Why 'use a comma or a full stop instead'
}
Invoke-Step 'nothing that emails anybody' {
    Find-Line -Pattern 'mailto:|[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}' -Skip '^Cargo\.lock$|^scripts/lint\.ps1$' -Why 'no addresses in the repository'
}
Invoke-Step 'encodings' { Test-Encoding }
Invoke-Step 'editorconfig' { Invoke-Native 'ec' @() }
Invoke-Step 'rustfmt' { Invoke-Native 'cargo' @('fmt', '--all', '--check') }
Invoke-Step 'clippy' {
    Invoke-Native 'cargo' @('clippy', '--workspace', '--all-targets', '--all-features', '--locked',
        '--', '-D', 'warnings')
}
Invoke-Step 'rustdoc' { Invoke-CargoDoc }
Invoke-Step 'cargo deny' { Invoke-Native 'cargo-deny' @('check') }
Invoke-Step 'unused dependencies' { Invoke-Native 'cargo-machete' @() }
Invoke-Step 'PSScriptAnalyzer' { Invoke-ScriptAnalyzerCheck }
Invoke-Step 'markdownlint' { Invoke-Native 'markdownlint-cli2' @() }
Invoke-Step 'typos' { Invoke-Native 'typos' @() }
Invoke-Step 'gitleaks' {
    $dir = Invoke-Native 'gitleaks' @('dir', '--no-banner', '--redact', '--exit-code', '1', '-c', '.gitleaks.toml', '.')
    $git = Invoke-Native 'gitleaks' @('git', '--no-banner', '--redact', '--exit-code', '1', '-c', '.gitleaks.toml', '.')
    $dir -and $git
}
if (-not $NoTests) {
    # One run at a time, since the pipeline tests hold the GPU.
    Invoke-Step 'tests' { Invoke-Native 'cargo' @('test', '--release', '--workspace', '--locked') }
}

Write-Host ''
if ($Failed.Count -gt 0) {
    Write-Host "FAILED: $($Failed -join ', ')" -ForegroundColor Red
    exit 1
}
Write-Host 'all checks passed' -ForegroundColor Green
exit 0
