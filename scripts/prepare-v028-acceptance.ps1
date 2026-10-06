param(
    [Parameter(Mandatory = $true)][string]$OutputDir,
    [Parameter(Mandatory = $true)][string]$InstallerPath
)
$ErrorActionPreference = 'Stop'

$releaseTag = 'v0.2.8'
$releaseCommit = '16f27f5878c44cf7aa13ea3719a84dce9b58b0fd'
$assetName = 'dotz_0.2.8_x64-setup.exe'
$assetUrl = 'https://github.com/cayleb-james2008/dotz/releases/download/v0.2.8/dotz_0.2.8_x64-setup.exe'
$expectedSha256 = '64a7e18f6b00d04f7a20533ecd860684bcaf365de11185d032472424cf729642'
$expectedBytes = [long]100052806
$feedUrl = 'https://github.com/cayleb-james2008/dotz/releases/download/v0.2.8/latest.json'
$sourceConfigUrl = "https://raw.githubusercontent.com/cayleb-james2008/dotz/$releaseCommit/src-tauri/tauri.conf.json"
New-Item -ItemType Directory -Path $OutputDir -Force | Out-Null
$InstallerPath = [IO.Path]::GetFullPath($InstallerPath)

function Write-JsonFile([string]$Path, $Value) {
    $json = $Value | ConvertTo-Json -Depth 20
    [IO.File]::WriteAllText($Path, $json + "`n", [Text.UTF8Encoding]::new($false))
}
function Get-Sha256([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

# All inputs are public release objects, pinned to the immutable v0.2.8 release.
$apiHeaders = @{ 'User-Agent' = 'dotz-windows-installer-acceptance' }
$releaseApiUrl = 'https://api.github.com/repos/cayleb-james2008/dotz/releases/tags/v0.2.8'
$releaseRefUrl = 'https://api.github.com/repos/cayleb-james2008/dotz/git/ref/tags/v0.2.8'
Invoke-WebRequest -Uri $releaseApiUrl -Headers $apiHeaders -OutFile (Join-Path $OutputDir 'release-api.json')
Invoke-WebRequest -Uri $releaseRefUrl -Headers $apiHeaders -OutFile (Join-Path $OutputDir 'release-tag-ref.json')
$release = Get-Content -LiteralPath (Join-Path $OutputDir 'release-api.json') -Raw | ConvertFrom-Json
$tagRef = Get-Content -LiteralPath (Join-Path $OutputDir 'release-tag-ref.json') -Raw | ConvertFrom-Json
if ($release.tag_name -ne $releaseTag -or $tagRef.object.sha -ne $releaseCommit) {
    throw "Public release tag/commit changed: tag=$($release.tag_name) sha=$($tagRef.object.sha)"
}
$installerAsset = @($release.assets | Where-Object { $_.name -eq $assetName })
$feedAsset = @($release.assets | Where-Object { $_.name -eq 'latest.json' })
if ($installerAsset.Count -ne 1 -or $feedAsset.Count -ne 1) { throw 'Expected exactly one public setup.exe and one latest.json release asset.' }
$installerAsset = $installerAsset[0]
$feedAsset = $feedAsset[0]
if ($installerAsset.browser_download_url -ne $assetUrl -or $installerAsset.digest -ne "sha256:$expectedSha256" -or [long]$installerAsset.size -ne $expectedBytes) {
    throw 'GitHub release asset name, URL, size, or digest differs from the pinned v0.2.8 Windows installer.'
}
if ($feedAsset.browser_download_url -ne $feedUrl -or [long]$feedAsset.size -ne 971) { throw 'Pinned v0.2.8 updater-feed asset metadata changed.' }

Invoke-WebRequest -Uri $installerAsset.browser_download_url -Headers $apiHeaders -OutFile $InstallerPath
$installerInfo = Get-Item -LiteralPath $InstallerPath
$installerSha = Get-Sha256 $InstallerPath
if ([long]$installerInfo.Length -ne $expectedBytes -or $installerSha -ne $expectedSha256) {
    throw "Public setup.exe bytes failed size/SHA-256 verification: bytes=$($installerInfo.Length) sha256=$installerSha"
}

$feedPath = Join-Path $OutputDir 'latest.json'
Invoke-WebRequest -Uri $feedAsset.browser_download_url -Headers $apiHeaders -OutFile $feedPath
$feedInfo = Get-Item -LiteralPath $feedPath
$feedSha = Get-Sha256 $feedPath
if ([long]$feedInfo.Length -ne 971 -or $feedSha -ne 'e67d7cf6e12bcfd9c57b3d829416c4c2947b1ebacaa598a6f79cfe3cd100a142') {
    throw "Public latest.json bytes failed size/SHA-256 verification: bytes=$($feedInfo.Length) sha256=$feedSha"
}
$feed = Get-Content -LiteralPath $feedPath -Raw | ConvertFrom-Json
$feedPlatform = $feed.platforms.'windows-x86_64'
if ($feed.version -ne '0.2.8' -or $feedPlatform.url -ne $assetUrl) { throw 'Public updater feed version/platform URL does not identify the pinned v0.2.8 Windows asset.' }
$signatureText = [string]$feedPlatform.signature
$signatureBytes = [Convert]::FromBase64String($signatureText)
$signaturePath = Join-Path $OutputDir 'latest.json.sig'
[IO.File]::WriteAllBytes($signaturePath, $signatureBytes)
$signatureSha = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($signatureText))).ToLowerInvariant()
if ($signatureSha -ne '4194a09c14c54cc5008db922cafdd5bb51fda0d79e78ed5c2fff3adeefa6f9c8') { throw 'Public updater signature text differs from the pinned release evidence.' }

$configPath = Join-Path $OutputDir 'release-source-tauri.conf.json'
Invoke-WebRequest -Uri $sourceConfigUrl -Headers $apiHeaders -OutFile $configPath
$config = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
if ($config.version -ne '0.2.8' -or $config.bundle.targets -notcontains 'nsis') { throw 'Tag source does not configure the expected v0.2.8 NSIS bundle.' }
$publicKeyBytes = [Convert]::FromBase64String([string]$config.plugins.updater.pubkey)
$publicKeyPath = Join-Path $OutputDir 'updater-public-key.pub'
[IO.File]::WriteAllBytes($publicKeyPath, $publicKeyBytes)
$publicKeyText = [Text.Encoding]::UTF8.GetString($publicKeyBytes)
$keyMatch = [regex]::Match($publicKeyText, '(?im)^untrusted comment: minisign public key:\s*([a-f0-9]+)')
if (-not $keyMatch.Success -or $keyMatch.Groups[1].Value.ToUpperInvariant() -ne 'A55EE27DA4AEE7BB') { throw 'Tag-configured updater key is not the pinned public Minisign key.' }
$keySha = Get-Sha256 $publicKeyPath

# Use the official Minisign CLI when available. The signature is made with the
# public key from the v0.2.8 tag config; no private key or credentials are used.
$minisign = Get-Command minisign.exe -ErrorAction SilentlyContinue
$minisignInstallLog = Join-Path $OutputDir 'minisign-install.log'
if (-not $minisign) {
    $choco = Get-Command choco.exe -ErrorAction SilentlyContinue
    if ($choco) {
        $installLines = & $choco.Source install minisign --version=0.8.0 --yes --no-progress 2>&1
        $installExit = $LASTEXITCODE
        $installLines | Out-File -LiteralPath $minisignInstallLog -Encoding utf8
        "choco_exit_code=$installExit" | Add-Content -LiteralPath $minisignInstallLog -Encoding utf8
        if ($installExit -eq 0) { $minisign = Get-Command minisign.exe -ErrorAction SilentlyContinue }
        else { $minisign = $null }
    } else {
        'Chocolatey unavailable; Minisign CLI was not installed.' | Set-Content -LiteralPath $minisignInstallLog -Encoding utf8
    }
}

$minisignResult = [ordered]@{
    status = 'UNAVAILABLE'
    reason = 'Minisign CLI unavailable after the pinned Chocolatey install attempt; refusing to run setup.exe.'
    version = $null
    public_key_id = $keyMatch.Groups[1].Value.ToUpperInvariant()
    public_key_sha256 = $keySha
    positive_exit_code = $null
    tamper_negative_exit_code = $null
    tampered_sha256 = $null
}
if ($minisign) {
    $versionOutput = & $minisign.Source -v 2>&1
    $versionExit = $LASTEXITCODE
    $versionOutput | Out-File -LiteralPath (Join-Path $OutputDir 'minisign-version.log') -Encoding utf8
    $minisignResult.version = (($versionOutput | Out-String).Trim())

    $positiveOutput = & $minisign.Source -V -m $InstallerPath -x $signaturePath -p $publicKeyPath 2>&1
    $positiveExit = $LASTEXITCODE
    $positiveOutput | Out-File -LiteralPath (Join-Path $OutputDir 'minisign-positive.log') -Encoding utf8

    $tamperedPath = Join-Path $OutputDir 'tampered-installer.exe'
    Copy-Item -LiteralPath $InstallerPath -Destination $tamperedPath -Force
    $stream = [IO.File]::Open($tamperedPath, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    try {
        $offset = [long][Math]::Floor($stream.Length / 2)
        [void]$stream.Seek($offset, [IO.SeekOrigin]::Begin)
        $originalByte = $stream.ReadByte()
        if ($originalByte -lt 0) { throw 'Could not read the tamper-test byte.' }
        [void]$stream.Seek($offset, [IO.SeekOrigin]::Begin)
        $stream.WriteByte([byte]($originalByte -bxor 1))
        $stream.Flush()
    } finally { $stream.Dispose() }
    $tamperedSha = Get-Sha256 $tamperedPath
    $negativeOutput = & $minisign.Source -V -m $tamperedPath -x $signaturePath -p $publicKeyPath 2>&1
    $negativeExit = $LASTEXITCODE
    $negativeOutput | Out-File -LiteralPath (Join-Path $OutputDir 'minisign-tamper-negative.log') -Encoding utf8
    Remove-Item -LiteralPath $tamperedPath -Force
    $minisignResult.positive_exit_code = $positiveExit
    $minisignResult.tamper_negative_exit_code = $negativeExit
    $minisignResult.tampered_sha256 = $tamperedSha
    if ($positiveExit -eq 0 -and $negativeExit -ne 0) {
        $minisignResult.status = 'PASS'
        $minisignResult.reason = 'Verified the public v0.2.8 feed signature and confirmed a byte-flipped installer is rejected.'
    } else {
        $minisignResult.status = 'FAIL'
        $minisignResult.reason = "Verification failed: positive_exit=$positiveExit tamper_negative_exit=$negativeExit."
    }
} else {
    $reason = 'Minisign CLI was unavailable; public tag/feed/hash checks still ran.'
    if (Test-Path -LiteralPath $minisignInstallLog) { $reason = (Get-Content -LiteralPath $minisignInstallLog -Raw).Trim() }
    $minisignResult.reason = $reason
}

$metadata = [ordered]@{
    schema_version = 1
    mode = 'released'
    tag = $releaseTag
    tag_commit = $tagRef.object.sha
    release_url = $release.html_url
    published_at = $release.published_at
    asset_name = $installerAsset.name
    asset_url = $installerAsset.browser_download_url
    asset_content_type = $installerAsset.content_type
    asset_bytes = [long]$installerInfo.Length
    sha256 = $installerSha
    feed_url = $feedAsset.browser_download_url
    feed_bytes = [long]$feedInfo.Length
    feed_sha256 = $feedSha
    signature_sha256 = $signatureSha
    source_config_url = $sourceConfigUrl
    source_config_sha256 = Get-Sha256 $configPath
    source_bundle_targets = @($config.bundle.targets)
    target_platform = 'windows-x86_64'
    target_arch = 'x64'
    source_nsis_install_mode = if ($config.bundle.windows.nsis.installMode) { $config.bundle.windows.nsis.installMode } else { 'currentUser (Tauri default; not overridden at tag)' }
    source_webview2_install_mode = if ($config.bundle.windows.webviewInstallMode.type) { $config.bundle.windows.webviewInstallMode.type } else { 'downloadBootstrapper (Tauri default)' }
    public_key_id = $keyMatch.Groups[1].Value.ToUpperInvariant()
    public_key_sha256 = $keySha
    minisign = $minisignResult
}
Write-JsonFile (Join-Path $OutputDir 'release-metadata.json') $metadata
Write-Host "RELEASE_TAG=$releaseTag"
Write-Host "RELEASE_TAG_COMMIT=$($tagRef.object.sha)"
Write-Host "RELEASE_ASSET=$($installerAsset.name) bytes=$($installerInfo.Length) sha256=$installerSha"
Write-Host "UPDATE_FEED=latest.json bytes=$($feedInfo.Length) sha256=$feedSha signature_sha256=$signatureSha"
Write-Host "MINISIGN_STATUS=$($minisignResult.status) public_key_id=$($minisignResult.public_key_id) positive=$($minisignResult.positive_exit_code) negative=$($minisignResult.tamper_negative_exit_code)"
if ($minisignResult.status -ne 'PASS') { throw "Minisign verification is mandatory before setup.exe execution; status=$($minisignResult.status)." }
$global:LASTEXITCODE = 0
