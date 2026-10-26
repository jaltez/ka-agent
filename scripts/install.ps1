# Ka Agent installer for Windows: downloads the release build, verifies its
# checksum, installs it, and starts `ka-agent setup`.
#
#   irm https://raw.githubusercontent.com/jaltez/ka-agent/main/scripts/install.ps1 | iex
#
# Environment (set before running):
#   KA_AGENT_INSTALL_DIR   where ka-agent.exe goes (default: %LOCALAPPDATA%\ka-agent\bin)
#   KA_AGENT_NO_SETUP=1    install only; do not start the setup wizard
#   KA_AGENT_INSTALL_FROM  install from this local zip (no download)
#
# The release asset is ka-agent-x86_64-pc-windows-msvc.zip with a matching
# .sha256 next to it (the same format the Unix installers verify).

$ErrorActionPreference = "Stop"
$Repo = "jaltez/ka-agent"
$Dir = if ($env:KA_AGENT_INSTALL_DIR) { $env:KA_AGENT_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "ka-agent\bin" }

function Say($msg) { Write-Host $msg }
function Die($msg) { Write-Error "ka-agent install: $msg"; exit 1 }

$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { "x86_64" }
    "ARM64" { "aarch64" }
    default { $null }
}
if (-not $arch) {
    Die "unsupported architecture $env:PROCESSOR_ARCHITECTURE; build from source: cargo install --git https://github.com/$Repo ka-agent"
}
$Target = "$arch-pc-windows-msvc"
$Name = "ka-agent-$Target.zip"

$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("ka-agent-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
    $Zip = Join-Path $Tmp $Name
    if ($env:KA_AGENT_INSTALL_FROM) {
        if (-not (Test-Path $env:KA_AGENT_INSTALL_FROM)) { Die "no such file: $($env:KA_AGENT_INSTALL_FROM)" }
        Copy-Item $env:KA_AGENT_INSTALL_FROM $Zip
    } else {
        $Url = "https://github.com/$Repo/releases/latest/download/$Name"
        Say "downloading $Name..."
        Invoke-WebRequest -Uri $Url -OutFile $Zip -UseBasicParsing
        $SumFile = Join-Path $Tmp "$Name.sha256"
        Invoke-WebRequest -Uri "$Url.sha256" -OutFile $SumFile -UseBasicParsing
        # The checksum file is `HASH  NAME` (sha256sum format).
        $expected = ((Get-Content $SumFile -Raw).Trim() -split '\s+')[0].ToLowerInvariant()
        $actual = (Get-FileHash $Zip -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($expected -ne $actual) { Die "checksum mismatch for $Name" }
        Say "checksum ok"
    }

    $Exe = Join-Path $Tmp "ka-agent.exe"
    Expand-Archive -Path $Zip -DestinationPath $Tmp -Force
    if (-not (Test-Path $Exe)) { Die "the archive has no ka-agent.exe" }

    New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    $Dest = Join-Path $Dir "ka-agent.exe"
    # Replace atomically: a running daemon keeps its old image.
    $New = Join-Path $Dir ".ka-agent.new.exe"
    Copy-Item $Exe $New -Force
    Move-Item $New $Dest -Force
    $version = & $Dest --version
    Say "installed $version to $Dest"

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($userPath -notlike "*$Dir*") {
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$Dir", "User")
        $env:Path = "$env:Path;$Dir"
        Say "added $Dir to your user PATH (open a new terminal to pick it up)"
    }

    if ($env:KA_AGENT_NO_SETUP -ne "1") {
        & $Dest setup
    } else {
        Say "next: run ``ka-agent setup``"
    }
} finally {
    Remove-Item $Tmp -Recurse -Force -ErrorAction SilentlyContinue
}
