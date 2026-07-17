# dotz release builder — reproduces the signed NSIS installer + latest.json locally.
#
# Why: release/ held no local copy of the shipped installer and the sole copy lived on a GitHub
# release of a shadowbanned account (audit 2026-07-17). This script rebuilds the FULL release
# artifact set from source + the rotated signing key, and archives it under release-archive\
# (gitignored), so the operator always holds a local copy.
#
# Usage (from the repo root, PowerShell 5.1+):
#   powershell -NoProfile -File scripts\release.ps1                # build + sign + archive
#   powershell -NoProfile -File scripts\release.ps1 -SkipNpm      # skip npm install/fetch-model
#   powershell -NoProfile -File scripts\release.ps1 -Publish      # ALSO gh release create (operator-gated)
#
# Key material: read from ~\.claude\dotz-rust\ into PROCESS env vars only — never echoed, never
# written anywhere under the repo. The password file format is "PASSWORD=<value>".
param(
    [switch]$SkipNpm,
    [switch]$Publish,
    [string]$Notes = ""
)
$ErrorActionPreference = "Stop"

$repo = Split-Path -Parent $PSScriptRoot
Set-Location $repo

# ---- version + pubkey sanity -------------------------------------------------
$conf = Get-Content (Join-Path $repo "src-tauri\tauri.conf.json") -Raw | ConvertFrom-Json
$ver = $conf.version
if (-not $ver) { throw "could not read version from src-tauri\tauri.conf.json" }

$keyDir = Join-Path $env:USERPROFILE ".claude\dotz-rust"
# The rotated (v0.2.8+) key is dotz-updater.key; DEPLOY.md's older name is the fallback.
$keyFile = @("dotz-updater.key", "dotz-updater-v2.key") |
    ForEach-Object { Join-Path $keyDir $_ } | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $keyFile) { throw "no signing key found under $keyDir (want dotz-updater.key)" }
$pwFile = "$keyFile.password"
$pubFile = "$keyFile.pub"
if (-not (Test-Path $pwFile)) { throw "missing key password file $pwFile" }

# The bundled updater pubkey MUST be the signer's .pub content verbatim (see commit 16f27f5) —
# a mismatch would ship an installer whose latest.json no installed client can verify.
if (Test-Path $pubFile) {
    $pub = (Get-Content $pubFile -Raw).Trim()
    if ($pub -ne $conf.plugins.updater.pubkey.Trim()) {
        throw "pubkey mismatch: $pubFile does not equal plugins.updater.pubkey in tauri.conf.json — wrong key, aborting"
    }
}

Write-Host "building dotz v$ver (signing key: $keyFile)"

# ---- sign + build ------------------------------------------------------------
# Process-env only; cleared in finally. NEVER Write-Host these values.
$env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content $keyFile -Raw)
$env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ((Get-Content $pwFile | Select-Object -First 1) -replace '^PASSWORD=', '')
try {
    if (-not $SkipNpm) {
        npm install
        if ($LASTEXITCODE -ne 0) { throw "npm install failed ($LASTEXITCODE)" }
        npm run fetch-model
        if ($LASTEXITCODE -ne 0) { throw "npm run fetch-model failed ($LASTEXITCODE)" }
    }
    cargo tauri build
    if ($LASTEXITCODE -ne 0) { throw "cargo tauri build failed ($LASTEXITCODE)" }

    # createUpdaterArtifacts=true drops the installer + its minisign .sig in the nsis bundle dir.
    $nsis = @("target\release\bundle\nsis", "src-tauri\target\release\bundle\nsis") |
        ForEach-Object { Join-Path $repo $_ } | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $nsis) { throw "no nsis bundle dir found after build" }
    $setup = Join-Path $nsis "dotz_${ver}_x64-setup.exe"
    $sig = "$setup.sig"
    if (-not (Test-Path $setup)) { throw "installer not found: $setup" }
    if (-not (Test-Path $sig)) { throw "signature not found: $sig (TAURI_SIGNING_PRIVATE_KEY not picked up?)" }

    # ---- latest.json (tauri-plugin-updater manifest) -------------------------
    $latest = @{
        version  = "$ver"
        notes    = if ($Notes) { $Notes } else { "dotz v$ver" }
        pub_date = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
        platforms = @{
            "windows-x86_64" = @{
                signature = (Get-Content $sig -Raw).Trim()
                url       = "https://github.com/cayleb-james2008/dotz/releases/download/v$ver/dotz_${ver}_x64-setup.exe"
            }
        }
    }
    $latestPath = Join-Path $nsis "latest.json"
    # PS 5.1 Out-File -Encoding utf8 writes a BOM, which some JSON consumers choke on.
    [IO.File]::WriteAllText($latestPath, ($latest | ConvertTo-Json -Depth 5), (New-Object Text.UTF8Encoding($false)))

    # ---- archive locally (release-archive\ is gitignored) --------------------
    $archive = Join-Path $repo "release-archive\v$ver"
    New-Item -ItemType Directory -Force $archive | Out-Null
    Copy-Item $setup, $sig, $latestPath -Destination $archive -Force
    Write-Host "archived: $archive (installer + .sig + latest.json)"

    # ---- publish (operator-gated: only with -Publish) ------------------------
    $ghArgs = "release create v$ver --repo cayleb-james2008/dotz --target master --title `"dotz v$ver`" --notes `"$($latest.notes)`" `"$setup`" `"$latestPath`""
    if ($Publish) {
        Invoke-Expression "gh $ghArgs"
        if ($LASTEXITCODE -ne 0) { throw "gh release create failed ($LASTEXITCODE)" }
    } else {
        Write-Host "not publishing (pass -Publish to run):"
        Write-Host "  gh $ghArgs"
    }
}
finally {
    $env:TAURI_SIGNING_PRIVATE_KEY = ""
    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = ""
}
