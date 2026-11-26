# Installs Unharnessed for the terminal on Windows, in PowerShell:
#   irm https://unharnessed.dev/install.ps1 | iex
# Run again, it updates. It downloads the release's archive for Windows
# (scripts/unharnessed-package.sh builds it), checks it against its SHA-256,
# and puts it in a folder of its own, named by its version:
#   %LOCALAPPDATA%\unharnessed\<version>\   the agent, its kernel's runtime, the built-in skills
#   %LOCALAPPDATA%\unharnessed\current      a junction to that folder, put on your user PATH
# The version a running session uses is kept until the next update. Then it
# builds the agent's Python kernel, which needs uv (https://docs.astral.sh/uv/):
# without uv it says how to get it, and the kernel is built at the first run.
#
# Its options, with the script saved (.\install.ps1 -Version 0.0.8) or as a
# script block (& ([scriptblock]::Create((irm <url>))) -Uninstall):
#   -Version <v>   the release to install (default: the latest)
#   -NoKernel      do not build the Python kernel now
#   -Uninstall     remove the agent and its PATH entry; ~\.unharnessed (your
#                  conversations, keys and settings) is kept
# UNHARNESSED_VERSION, UNHARNESSED_INSTALL_DIR and UNHARNESSED_INSTALL_FROM
# (where the archives are, a URL or a folder) work as for install.sh.
# It runs on Windows PowerShell 5.1 and on PowerShell 7.
param(
  [string]$Version = $env:UNHARNESSED_VERSION,
  [switch]$NoKernel,
  [switch]$Uninstall
)

# One script block: piped into iex, nothing of it stays in the user's session.
& {
  $ErrorActionPreference = 'Stop'
  # Windows PowerShell draws Invoke-WebRequest's progress so slowly it slows the download.
  $ProgressPreference = 'SilentlyContinue'
  # Run as a file, a failure exits non-zero; piped into iex, `exit` would close the user's window.
  $asFile = [bool]$PSCommandPath
  $repository = 'unharnessed-agent/Unharnessed'
  $uvInstall = 'powershell -ExecutionPolicy ByPass -c "irm https://astral.sh/uv/install.ps1 | iex"'
  $target = 'win32-x64'
  $archive = "unharnessed-$target.tar.gz"

  function Fail([string]$Message) { throw "install.ps1: $Message" }
  function Say([string]$Message = '') { Write-Host $Message }

  # A program run to its end: its exit code and what it wrote. UNHARNESSED_PACKAGE_DIR
  # is left out of its environment: the agent finds what ships beside it.
  function Invoke-Program([string]$File, [string]$Arguments) {
    $info = New-Object System.Diagnostics.ProcessStartInfo
    $info.FileName = $File
    $info.Arguments = $Arguments
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $info.StandardErrorEncoding = [System.Text.Encoding]::UTF8
    $info.EnvironmentVariables.Remove('UNHARNESSED_PACKAGE_DIR')
    $process = [System.Diagnostics.Process]::Start($info)
    $errors = $process.StandardError.ReadToEndAsync()
    $output = $process.StandardOutput.ReadToEnd()
    $process.WaitForExit()
    [pscustomobject]@{ Code = $process.ExitCode; Output = $output; Errors = $errors.Result }
  }

  # A URL downloaded, or a file copied.
  function Get-File([string]$From, [string]$To) {
    if ($From -match '^https?://') {
      Invoke-WebRequest -UseBasicParsing -Uri $From -OutFile $To
    } else {
      Copy-Item -LiteralPath $From -Destination $To
    }
  }

  function Same-Path([string]$A, [string]$B) {
    $A.TrimEnd('\') -ieq $B.TrimEnd('\')
  }

  # The version folder the junction points to, when it is ours (gone or not).
  function Get-Linked([string]$Root, [string]$Current) {
    $item = Get-Item -LiteralPath $Current -Force -ErrorAction SilentlyContinue
    if (-not $item -or $item.LinkType -ne 'Junction') { return $null }
    $to = "$(@($item.Target)[0])" -replace '^\\(\?\?|\\\?)\\', ''
    if ($to -and (Same-Path (Split-Path -Parent $to) $Root)) { return $to.TrimEnd('\') }
    $null
  }

  # Removes the junction, never what it points to.
  function Remove-Junction([string]$Path) {
    [System.IO.Directory]::Delete($Path, $false)
  }

  # The user PATH as written, %VARIABLES% unexpanded, and its folders.
  function Get-UserPath {
    $key = Get-Item -LiteralPath 'HKCU:\Environment'
    [string]$key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
  }

  function Set-UserPath([string]$Value) {
    Set-ItemProperty -LiteralPath 'HKCU:\Environment' -Name 'Path' -Value $Value -Type ExpandString
    # Setting a variable through .NET tells the open programs (Explorer, new terminals) the environment changed.
    [Environment]::SetEnvironmentVariable('UNHARNESSED_INSTALL_PATH_CHANGED', '1', 'User')
    [Environment]::SetEnvironmentVariable('UNHARNESSED_INSTALL_PATH_CHANGED', $null, 'User')
  }

  function Test-OnPath([string]$Paths, [string]$Folder) {
    foreach ($entry in ($Paths -split ';')) {
      if ($entry -and (Same-Path ([Environment]::ExpandEnvironmentVariables($entry)) $Folder)) { return $true }
    }
    $false
  }

  function Uninstall-Agent([string]$Root, [string]$Current) {
    if (Get-Linked $Root $Current) {
      Remove-Junction $Current
      Say "Removed $Current"
    }
    $paths = Get-UserPath
    if (Test-OnPath $paths $Current) {
      $kept = ($paths -split ';') | Where-Object { $_ -and -not (Same-Path ([Environment]::ExpandEnvironmentVariables($_)) $Current) }
      Set-UserPath ($kept -join ';')
      Say "Removed $Current from your PATH"
    }
    if (Test-Path -LiteralPath $Root) {
      Remove-Item -LiteralPath $Root -Recurse -Force
      Say "Removed $Root"
    }
    Say 'Your conversations, keys and settings are in ~\.unharnessed, and the Python kernel too: they are kept.'
  }

  function Build-Kernel([string]$Agent, [string]$Log) {
    $uv = Get-Command uv -ErrorAction SilentlyContinue
    if (-not $uv -and -not (Test-Path -LiteralPath (Join-Path $HOME '.local\bin\uv.exe'))) {
      Say
      Say 'The agent runs its Python in a kernel that uv builds, and uv is not installed. Get it with:'
      Say "  $uvInstall"
      Say 'The kernel is then built the first time the agent runs Python.'
      return
    }
    Say 'Building the Python kernel (once per version of its runtime)...'
    $built = Invoke-Program $Agent '--unharnessed-bootstrap'
    if ($built.Code -ne 0) {
      Set-Content -LiteralPath $Log -Value ($built.Output + $built.Errors) -Encoding UTF8
      ($built.Output + $built.Errors) -split "`n" | ForEach-Object { Write-Host "  $_" }
      Say 'The Python kernel is not built yet: the agent tries again the first time it runs Python.'
    }
  }

  function Install-Agent {
    if (-not $env:LOCALAPPDATA) { Fail 'LOCALAPPDATA is not set' }
    $root = $env:UNHARNESSED_INSTALL_DIR
    if (-not $root) { $root = Join-Path $env:LOCALAPPDATA 'unharnessed' }
    $root = [System.IO.Path]::GetFullPath($root).TrimEnd('\')
    $current = Join-Path $root 'current'
    if ($Uninstall) {
      Uninstall-Agent $root $current
      return
    }

    $machine = $env:PROCESSOR_ARCHITECTURE
    # A 32-bit PowerShell on 64-bit Windows says x86 here.
    if ($env:PROCESSOR_ARCHITEW6432) { $machine = $env:PROCESSOR_ARCHITEW6432 }
    # Windows 11 on Arm runs the x64 build.
    if ($machine -ne 'AMD64' -and $machine -ne 'ARM64') { Fail "there is no build for Windows on $machine" }
    $tar = Join-Path $env:SystemRoot 'System32\tar.exe'
    if (-not (Test-Path -LiteralPath $tar)) { Fail "tar.exe is needed to unpack the archive (Windows 10 1803 and later have it)" }
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $wanted = "$Version" -replace '^v', ''
    if ($env:UNHARNESSED_INSTALL_FROM) {
      $from = $env:UNHARNESSED_INSTALL_FROM.TrimEnd('/', '\')
    } elseif ($wanted) {
      $from = "https://github.com/$repository/releases/download/vscode-v$wanted"
    } else {
      $from = "https://github.com/$repository/releases/latest/download"
    }
    if ((Get-Item -LiteralPath $current -Force -ErrorAction SilentlyContinue) -and -not (Get-Linked $root $current)) {
      Fail "$current is not this installer's: move it away, or set UNHARNESSED_INSTALL_DIR to another folder"
    }

    # Unpacked inside the install's folder: moving it into place is then one rename.
    $created = -not (Test-Path -LiteralPath $root)
    New-Item -ItemType Directory -Force -Path $root | Out-Null
    $incoming = Join-Path $root ".incoming.$PID"
    if (Test-Path -LiteralPath $incoming) { Remove-Item -LiteralPath $incoming -Recurse -Force }
    New-Item -ItemType Directory -Path $incoming | Out-Null
    $installed = $false
    try {
      if ($from -match '^https?://') { $sep = '/' } else { $sep = '\' }
      $release = if ($wanted) { "release $wanted" } else { 'the latest release' }
      Say "Downloading $from$sep$archive"
      $file = Join-Path $incoming $archive
      try { Get-File "$from$sep$archive" $file } catch { Fail "no archive at $from$sep${archive}: $release has no terminal build for $target" }
      try { Get-File "$from$sep$archive.sha256" "$file.sha256" } catch { Fail "no checksum at $from$sep$archive.sha256" }
      $expected = ([string](Get-Content -LiteralPath "$file.sha256" -Raw)).Trim() -split '\s+' | Select-Object -First 1
      $expected = "$expected".TrimStart('*').ToLowerInvariant()
      $got = (Get-FileHash -Algorithm SHA256 -LiteralPath $file).Hash.ToLowerInvariant()
      if (-not $expected -or $expected -ne $got) {
        Fail "the archive is not the one the release published (SHA-256 $got, expected $expected): nothing was installed"
      }

      $unpacked = Join-Path $incoming 'unpacked'
      New-Item -ItemType Directory -Path $unpacked | Out-Null
      $untar = Invoke-Program $tar "-xzf `"$file`" -C `"$unpacked`""
      if ($untar.Code -ne 0) { Fail "the archive does not unpack: $($untar.Errors.Trim())" }
      $app = Join-Path $unpacked 'unharnessed'
      $agent = Join-Path $app 'unharnessed.exe'
      if (-not ((Test-Path -LiteralPath $agent) -and (Test-Path -LiteralPath (Join-Path $app 'unharnessed-runtime\pyproject.toml')) -and (Test-Path -LiteralPath (Join-Path $app 'skills')))) {
        Fail 'the archive is not an Unharnessed build for Windows: nothing was installed'
      }
      # The build says its own version, which also shows it runs on this machine.
      $asked = Invoke-Program $agent '--version'
      if ($asked.Code -ne 0) { Fail "this build does not run on this machine: $($asked.Errors.Trim())" }
      $said = $asked.Output.Trim()
      if ($said -notmatch '^[0-9A-Za-z.+-]+$' -or $said -eq 'current') { Fail "the build says a version that names no folder: '$said'" }
      if ($wanted -and $said -ne $wanted) { Fail "asked for version $wanted, the archive at $from is ${said}: nothing was installed" }

      $before = Get-Linked $root $current
      $dest = Join-Path $root $said
      if (Test-Path -LiteralPath $dest) {
        try { Move-Item -LiteralPath $dest -Destination (Join-Path $incoming 'replaced') }
        catch { Fail "Unharnessed $said is running: close it, then run this again" }
      }
      Move-Item -LiteralPath $app -Destination $dest
      if (Get-Linked $root $current) { Remove-Junction $current }
      New-Item -ItemType Junction -Path $current -Target $dest | Out-Null
      $installed = $true

      # The version the junction named until now may be running: it stays until the next update.
      Get-ChildItem -LiteralPath $root -Directory -Force | Where-Object {
        -not ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -and
        (Test-Path -LiteralPath (Join-Path $_.FullName 'unharnessed.exe')) -and
        -not (Same-Path $_.FullName $dest) -and -not ($before -and (Same-Path $_.FullName $before))
      } | ForEach-Object { Remove-Item -LiteralPath $_.FullName -Recurse -Force -ErrorAction SilentlyContinue }

      if ($before -and (Same-Path $before $dest)) {
        Say "Unharnessed $said is installed again in $dest"
      } elseif ($before) {
        Say "Unharnessed $said is installed in $dest (it was $(Split-Path -Leaf $before))"
      } else {
        Say "Unharnessed $said is installed in $dest"
      }
      if (-not $NoKernel) { Build-Kernel (Join-Path $current 'unharnessed.exe') (Join-Path $root 'kernel.log') }
    } finally {
      Remove-Item -LiteralPath $incoming -Recurse -Force -ErrorAction SilentlyContinue
      # A refused first install leaves no folder of its own.
      if ($created -and -not $installed) { Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue }
    }

    Say
    $paths = Get-UserPath
    if (-not (Test-OnPath $paths $current)) {
      $joined = if ($paths) { "$current;$paths" } else { $current }
      Set-UserPath $joined
      Say "Added $current to your user PATH: a new terminal finds unharnessed."
    }
    if (-not (Test-OnPath $env:Path $current)) { $env:Path = "$current;$env:Path" }
    $seen = Get-Command unharnessed -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($seen -and -not (Same-Path (Split-Path -Parent $seen.Source) $current)) {
      Say "Another unharnessed comes first on your PATH: $($seen.Source)"
      Say "This one is $(Join-Path $current 'unharnessed.exe')"
    } else {
      Say 'Run it in a project''s folder:  unharnessed'
    }
  }

  try {
    Install-Agent
  } catch {
    Write-Host $_.Exception.Message -ForegroundColor Red
    if ($asFile) { exit 1 }
  }
}
