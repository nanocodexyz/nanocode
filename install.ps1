& {
  $ErrorActionPreference = 'Stop'
  $ProgressPreference = 'SilentlyContinue'
  $release = if ($env:NANOCODE_RELEASE) { $env:NANOCODE_RELEASE } else { 'https://github.com/nanocodexyz/nanocode/releases/download/nanocode-cli' }
  $bin = if ($env:NANOCODE_BIN) { $env:NANOCODE_BIN } else { Join-Path $HOME '.nanocode\bin' }
  $arch = try { [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() } catch { $env:PROCESSOR_ARCHITECTURE }
  if ($env:PROCESSOR_ARCHITEW6432) { $arch = $env:PROCESSOR_ARCHITEW6432 }
  $target = switch -Regex ($arch) {
    '^(X64|AMD64)$' { 'x86_64-pc-windows-msvc' }
    '^(Arm64|ARM64)$' { 'aarch64-pc-windows-msvc' }
    default { throw "nanocode has no build for Windows $arch" }
  }
  $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ('nanocode-' + [System.Guid]::NewGuid())
  New-Item -ItemType Directory -Force -Path $tmp | Out-Null
  try {
    Invoke-WebRequest -UseBasicParsing -Uri "$release/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS')
    $line = Get-Content (Join-Path $tmp 'SHA256SUMS') | Where-Object { $_ -match " nanocode-$target\.zip$" } | Select-Object -First 1
    if (-not $line) { throw "nanocode: the release has no build for $target" }
    $expected = ($line -split ' ')[0].ToLowerInvariant()
    $stamp = Join-Path $bin '.nanocode.sha256'
    $installed = if (Test-Path $stamp) { (Get-Content $stamp -Raw).Trim() } else { '' }
    $exe = Join-Path $bin 'nanocode.exe'
    if (-not (Test-Path $exe) -or $installed -ne $expected) {
      Write-Host "nanocode: downloading $target"
      $zip = Join-Path $tmp "nanocode-$target.zip"
      Invoke-WebRequest -UseBasicParsing -Uri "$release/nanocode-$target.zip" -OutFile $zip
      $found = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLowerInvariant()
      if ($found -ne $expected) { throw "nanocode: checksum mismatch for nanocode-$target.zip" }
      $unpacked = Join-Path $tmp 'unpacked'
      Expand-Archive -Path $zip -DestinationPath $unpacked
      New-Item -ItemType Directory -Force -Path $bin | Out-Null
      foreach ($name in 'nanocode.exe', 'nc.exe', 'nanocode-python.exe') {
        $path = Join-Path $bin $name
        $aside = Join-Path $bin ".$name.old"
        if (Test-Path $aside) { Remove-Item -Force $aside -ErrorAction SilentlyContinue }
        if (Test-Path $path) { Move-Item -Force $path $aside }
      }
      Move-Item (Join-Path $unpacked 'nanocode.exe') $exe
      Move-Item (Join-Path $unpacked 'nanocode-python.exe') (Join-Path $bin 'nanocode-python.exe')
      try {
        New-Item -ItemType HardLink -Path (Join-Path $bin 'nc.exe') -Target $exe | Out-Null
      } catch {
        Copy-Item $exe (Join-Path $bin 'nc.exe')
      }
      Set-Content -Path $stamp -Value $expected -NoNewline
      Write-Host "nanocode: installed $(Join-Path $bin 'nc.exe')"
    }
  } finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
  }
  $user = [Environment]::GetEnvironmentVariable('Path', 'User')
  $entries = @($user -split ';' | Where-Object { $_ })
  if ($entries -contains $bin) {
    if (-not (($env:Path -split ';') -contains $bin)) { Write-Host "nanocode: your user PATH has $bin; open a new terminal to run nc" }
  } elseif ($env:NANOCODE_NO_MODIFY_PATH) {
    Write-Host "nanocode: add $bin to your PATH to run nc from anywhere"
  } else {
    [Environment]::SetEnvironmentVariable('Path', (($entries + $bin) -join ';'), 'User')
    Write-Host "nanocode: added $bin to your user PATH; open a new terminal to run nc"
  }
  Write-Host "nanocode: run $(Join-Path $bin 'nc.exe') to start"
}
