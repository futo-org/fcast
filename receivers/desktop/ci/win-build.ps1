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

# Do NOT export CC/CXX here. gstreamer-src pins meson to MSVC cl (wrap meson
# checks gate on cc.get_id() == 'msvc'), but its vcvars env capture re-exports
# whatever CC this shell set, silently overriding the pin. clang-cl here is
# what broke flex --nounistd and the openssl wrap's find_library workaround.
# LLVM stays installed above for bindgen's libclang and for libplacebo, whose
# build script pins clang-cl itself (GNU C extensions, cl cannot build it).
#
# Instead enter a VS dev shell so cl/INCLUDE/LIB exist for every build script
# that spawns meson itself (libplacebo-sys inherits this env directly).
Step "Entering VS developer environment"
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vsPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vsPath) { throw "vswhere found no VS installation with C++ tools" }
Import-Module (Join-Path $vsPath "Common7\Tools\Microsoft.VisualStudio.DevShell.dll")
Enter-VsDevShell -VsInstallPath $vsPath -SkipAutomaticLocation -DevCmdArguments '-arch=x64'
if (-not (Get-Command cl -ErrorAction SilentlyContinue)) { throw "cl not on PATH after Enter-VsDevShell" }

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
# The GStreamer repo/ref come from .cargo/config.toml (GSTREAMER_SRC_REPO/REF);
# the old --gst-ref flag no longer exists and would fail argument parsing.
cargo xtask receiver build-windows-installer
if ($LASTEXITCODE -ne 0) {
  # The VM is deleted after the job, so dump the meson log while we can. The
  # console error is often a late echo of a subproject that failed pages
  # earlier and only the meson log holds the first failure.
  Step "Build failed, dumping meson logs"
  Get-ChildItem (Join-Path $RepoRoot "target\gst-static\build-*\meson-logs\meson-log.txt") -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host "===== $($_.FullName): libnice/openssl mentions ====="
    Select-String -Path $_.FullName -Pattern 'libnice|openssl' -SimpleMatch:$false -Context 2,10 | ForEach-Object { $_.ToString() }
    Write-Host "===== $($_.FullName) (last 400 lines) ====="
    Get-Content $_.FullName -Tail 400
  }
  # ninja interleaves job output, so the failing job is usually NOT at the
  # tail. Pull every FAILED: block with enough context to see the error.
  $ninjaLog = Join-Path $RepoRoot "target\gst-static\ninja.log"
  if (Test-Path $ninjaLog) {
    Write-Host "===== ${ninjaLog}: FAILED jobs ====="
    Select-String -Path $ninjaLog -Pattern '^FAILED:' -Context 1,60 | ForEach-Object { $_.ToString() }
    Write-Host "===== ${ninjaLog}: error lines ====="
    Select-String -Path $ninjaLog -Pattern 'error:|error C\d|LNK\d{4}|fatal' -Context 2,6 | Select-Object -First 40 | ForEach-Object { $_.ToString() }
  }
}
Assert-Native "cargo xtask receiver build-windows-installer"

Step "Done. Installer(s):"
Get-ChildItem (Join-Path $RepoRoot "target\win-build\*.msi") | ForEach-Object { Write-Host "  $($_.FullName)" }
