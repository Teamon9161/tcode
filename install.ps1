param(
    [string]$Repo = $env:TCODE_INSTALL_REPO,
    [string]$Version = $env:TCODE_VERSION,
    [string]$InstallDir = $env:TCODE_INSTALL_DIR
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($Repo)) { $Repo = "Teamon9161/tcode" }
if ([string]::IsNullOrWhiteSpace($Version)) { $Version = "latest" }
if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $InstallDir = Join-Path $env:LOCALAPPDATA "Programs\tcode\bin"
}

$processor = if ($env:PROCESSOR_ARCHITEW6432) {
    $env:PROCESSOR_ARCHITEW6432
} else {
    $env:PROCESSOR_ARCHITECTURE
}
switch -Regex ($processor) {
    "ARM64" { $Arch = "aarch64"; break }
    "AMD64|x86_64" { $Arch = "x86_64"; break }
    default { throw "unsupported architecture: $processor" }
}

$Asset = "tcode-$Arch-windows.exe"
if ($Version -eq "latest") {
    $BaseUrl = "https://github.com/$Repo/releases/latest/download"
} else {
    $Tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
    $BaseUrl = "https://github.com/$Repo/releases/download/$Tag"
}

$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("tcode-install-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $TempDir | Out-Null

try {
    $AssetPath = Join-Path $TempDir $Asset
    $ChecksumsPath = Join-Path $TempDir "checksums.txt"
    Write-Host "Downloading $Asset..."
    Invoke-WebRequest -Uri "$BaseUrl/$Asset" -OutFile $AssetPath
    Invoke-WebRequest -Uri "$BaseUrl/checksums.txt" -OutFile $ChecksumsPath

    $ChecksumLine = Get-Content $ChecksumsPath | Where-Object {
        ($_ -split "\s+")[-1] -eq $Asset
    } | Select-Object -First 1
    if (-not $ChecksumLine) { throw "checksum not found for $Asset" }

    $Expected = ($ChecksumLine -split "\s+")[0].ToLowerInvariant()
    $Actual = (Get-FileHash -Algorithm SHA256 $AssetPath).Hash.ToLowerInvariant()
    if ($Actual -ne $Expected) { throw "checksum mismatch for $Asset" }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $Target = Join-Path $InstallDir "tcode.exe"
    Copy-Item -Path $AssetPath -Destination $Target -Force

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $PathParts = if ([string]::IsNullOrWhiteSpace($UserPath)) { @() } else { $UserPath -split ";" }
    if ($PathParts -notcontains $InstallDir) {
        $NewUserPath = if ([string]::IsNullOrWhiteSpace($UserPath)) {
            $InstallDir
        } else {
            "$UserPath;$InstallDir"
        }
        [Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")
    }
    if (($env:Path -split ";") -notcontains $InstallDir) { $env:Path = "$env:Path;$InstallDir" }

    Write-Host "tcode installed to $Target"
    Write-Host "Restart your terminal if tcode is not found in PATH."
} finally {
    Remove-Item -LiteralPath $TempDir -Recurse -Force
}
