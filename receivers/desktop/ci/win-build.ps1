$ErrorActionPreference = "Stop"

function Assert-Native([string]$what) {
  if ($LASTEXITCODE -ne 0) { throw "$what failed (exit $LASTEXITCODE)" }
}

function Step([string]$msg) { Write-Host "==> $msg" }

# Repo root is three levels up from receivers/desktop/ci; workspace (its parent)
# holds sibling checkouts. cargo xtask runs from the repo root.
$RepoRoot  = (Resolve-Path (Join-Path $PSScriptRoot "..\..\..")).Path
$Workspace = (Split-Path $RepoRoot -Parent)
Write-Host "RepoRoot=$RepoRoot"
Write-Host "Workspace=$Workspace"

# DNS / default route can lag behind the guest agent reporting an IP; winget
# then fails silently. Block until the internet is reachable.
Step "Waiting for network connectivity"
$net = $false
foreach ($i in 1..30) {
  try {
    Invoke-WebRequest -Uri "https://gitlab.futo.org" -UseBasicParsing -TimeoutSec 10 | Out-Null
    $net = $true; break
  } catch { Start-Sleep -Seconds 10 }
}
if (-not $net) { throw "VM has no internet connectivity after ~5 minutes" }

Step "Installing Rust nightly"
rustup set auto-self-update disable
rustup install nightly
rustup default nightly

Step "Installing build tools via winget"
$wingetIds = @(
  "NASM.NASM", "Kitware.CMake", "LLVM.LLVM",
  "Python.Python.3.14", "Ninja-build.Ninja", "Google.flatbuffers"
)
foreach ($id in $wingetIds) {
  Write-Host "--- winget install $id"
  winget install --id=$id -e --accept-package-agreements --accept-source-agreements
}

Step "Cloning fcast-receiver-windows-build-deps"
Set-Location $Workspace
if (-not (Test-Path "fcast-receiver-windows-build-deps")) {
  git clone --recursive --depth=1 "https://gitlab.futo.org/videostreaming/fcast-receiver-windows-build-deps.git"
  Assert-Native "clone fcast-receiver-windows-build-deps"
}

Step "Installing WiX CLI"
Set-Location (Join-Path $Workspace "fcast-receiver-windows-build-deps")
Start-Process "msiexec.exe" -ArgumentList '/i "wix-cli-x64.msi" /qn' -Wait
Set-Location $Workspace

# Find the real Python: winget installs it per-user OR (elevated) all-users,
# and either way the Windows Store python.exe alias already on PATH shadows it,
# so resolve the real root and put it FIRST.
$pyRoot = @(
  "C:\Program Files\Python314",
  "C:\Users\$Env:UserName\AppData\Local\Programs\Python\Python314"
) | Where-Object { Test-Path (Join-Path $_ "python.exe") } | Select-Object -First 1
if (-not $pyRoot) { throw "python.exe not found after winget install" }

# winget updates the persisted PATH, not this session. Put Python + the known
# install dirs first, then the existing and persisted machine/user PATH as a
# catch-all (NASM's dir varies by version).
$env:PATH = @(
  $pyRoot,
  "$pyRoot\Scripts",
  "C:\Program Files\Git\cmd",
  "C:\Program Files\NASM",
  "C:\Program Files\CMake\bin",
  "C:\Program Files\LLVM\bin",
  "C:\Users\$Env:UserName\AppData\Roaming\Python\Python314\Scripts",
  "C:\Users\$Env:UserName\AppData\Local\Microsoft\WinGet\Links",
  "C:\Program Files\WiX Toolset v6.0\bin",
  $env:PATH,
  [System.Environment]::GetEnvironmentVariable("Path", "Machine"),
  [System.Environment]::GetEnvironmentVariable("Path", "User")
) -join ";"

$env:CC  = "clang-cl"
$env:CXX = "clang-cl"

# The real check that provisioning worked. --version too: Get-Command also
# matches the Windows Store python.exe alias stub.
Step "Verifying toolchain"
foreach ($tool in @("git","rustup","cargo","python","ninja","cmake","clang-cl","nasm","flatc","wix")) {
  if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
    throw "provisioning failed: '$tool' is not on PATH"
  }
}
python --version; Assert-Native "python (Store alias stub instead of a real install?)"
cargo --version;  Assert-Native "cargo (rust toolchain not resolved?)"

# pkgconf pinned <3: the 3.0 Windows port returns empty --modversion and panics
# the pkg-config crate in glib-sys. `python -m pip` (not pip3) so the install
# lands in the same interpreter that resolves get_executable() below.
Step "Installing meson and pinned pkgconf"
python -m pip install --force-reinstall meson "pkgconf==2.5.1.post2"
Assert-Native "pip install of meson + pinned pkgconf"

$env:PKG_CONFIG = (python -c "import pkgconf; print(pkgconf.get_executable())")
if ([string]::IsNullOrWhiteSpace($env:PKG_CONFIG)) {
  throw "python failed to resolve the pinned pkgconf executable"
}
Write-Host "PKG_CONFIG=$env:PKG_CONFIG"
$pcver = & $env:PKG_CONFIG --version
Assert-Native "pkgconf --version"
Write-Host "pkgconf --version: $pcver"
if ($pcver -notlike "2.*") { throw "pkgconf pin not in effect: reports '$pcver'" }
if (-not (Get-Command meson -ErrorAction SilentlyContinue)) { throw "meson missing after pip install" }

Step "Building the Windows installer"
Set-Location $RepoRoot
cargo xtask receiver build-windows-installer --gst-ref 1.29.2
Assert-Native "cargo xtask receiver build-windows-installer"

Step "Done. Installer(s):"
Get-ChildItem (Join-Path $RepoRoot "target\win-build\*.msi") | ForEach-Object { Write-Host "  $($_.FullName)" }
