# Install or upgrade banto from a GitHub release (Windows; PowerShell 5.1 and 7).
#
#   irm https://github.com/kt81/banto/releases/latest/download/install.ps1 | iex
#
# One banto per machine. Codex trusts the hook banto writes (`<exe> _hook`) per full
# command string and keeps a single trust record, so a second banto at another path
# silently takes the trust from the first, and a path with a space cannot brief Codex
# at all. The target is therefore, in this order:
#   1. the banto.exe already on PATH, replaced where it is;
#   2. $env:BANTO_INSTALL_DIR\banto.exe;
#   3. %USERPROFILE%\.local\bin\banto.exe.
# This script never edits PATH: when the directory is not on PATH it prints the command
# that adds it.
#
# Environment:
#   BANTO_VERSION         a release tag such as v0.2.0 (default: the latest release)
#   BANTO_INSTALL_DIR     directory used when no banto is on PATH yet
#   BANTO_INSTALL_SOURCE  test hook: a directory, or a URL prefix, holding the release files
#                         (banto-<tag>-<target>.zip and SHA256SUMS) instead of GitHub
#
# Re-running is an upgrade. It refuses while a banto started from the target file is
# running, because Windows will not replace a running executable, and refuses to overwrite a
# `banto` on PATH that is not a banto (a PE file that runs and answers `--version` with
# "banto ..."; a shim or wrapper is not). The new exe is staged beside the target and run
# once before it replaces anything.
#
# What the checksum is worth: SHA256SUMS comes from the same release as the archive, so the
# check proves the download arrived intact, not who built it. Provenance is a separate
# check: gh attestation verify <archive> --repo kt81/banto
#
# Everything is inside a function so that `| iex` leaves no variables behind and a failure
# does not close the caller's session; when run as a file, a failure exits with code 1.

# Resolve-RealPath PATH: PATH with every symbolic link and junction in it (the file's own or
# any directory above it) followed, the way install.sh does, so two spellings of one file
# compare equal and a link is not replaced where it stands.
function Resolve-RealPath([string]$Path) {
    $p = [IO.Path]::GetFullPath($Path)
    for ($n = 0; $n -lt 40; $n++) {
        $hit = $false
        $cur = $p
        $rest = ''
        while ($cur) {
            $target = $null
            try {
                $item = Get-Item -LiteralPath $cur -Force -ErrorAction Stop
                if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { $target = @($item.Target)[0] }
            } catch { }
            if ($target) {
                if ($target.StartsWith('\??\')) { $target = $target.Substring(4) }
                if (-not [IO.Path]::IsPathRooted($target)) { $target = Join-Path (Split-Path -Parent $cur) $target }
                $p = [IO.Path]::GetFullPath($target) + $rest
                $hit = $true
                break
            }
            $parent = Split-Path -Parent $cur
            if (-not $parent -or $parent -eq $cur) { break }
            $rest = '\' + (Split-Path -Leaf $cur) + $rest
            $cur = $parent
        }
        if (-not $hit) { break }
    }
    $p
}

# Test-PeFile PATH: does the file start like a Windows executable (MZ header, PE signature)?
function Test-PeFile([string]$Path) {
    try {
        $fs = [IO.File]::Open($Path, 'Open', 'Read', 'ReadWrite')
        try {
            $buf = New-Object byte[] 64
            if ($fs.Read($buf, 0, 64) -lt 64) { return $false }
            if ($buf[0] -ne 0x4D -or $buf[1] -ne 0x5A) { return $false }
            $off = [BitConverter]::ToInt32($buf, 60)
            if ($off -lt 64 -or $off -gt $fs.Length - 4) { return $false }
            $null = $fs.Seek($off, 'Begin')
            $sig = New-Object byte[] 4
            if ($fs.Read($sig, 0, 4) -lt 4) { return $false }
            return ($sig[0] -eq 0x50 -and $sig[1] -eq 0x45 -and $sig[2] -eq 0 -and $sig[3] -eq 0)
        } finally { $fs.Dispose() }
    } catch { return $null }
}

# Assert-IsBanto FILE SHOWN HINT: throw unless FILE is a banto (a PE file that runs and
# answers --version with "banto ..."). SHOWN names it the way the user knows it.
function Assert-IsBanto([string]$File, [string]$Shown, [string]$Hint) {
    $pe = Test-PeFile $File
    if ($null -eq $pe) { throw "cannot read $Shown (is another program holding it open?). Nothing was changed." }
    if (-not $pe) { throw "$Shown is not a Windows executable, so it is not a banto. $Hint" }
    $found = Get-BantoVersion $File
    # Get-BantoVersion's marker for a run that was cut off: say that, not "not a banto"
    if ($found -eq '(no answer within 10 seconds)') { throw "$Shown did not answer ``--version`` within 10 seconds. $Hint" }
    if (-not $found -or -not $found.StartsWith('banto ')) {
        $said = if ($null -eq $found) { 'nothing, or it did not run' } else { $found }
        throw "$Shown does not look like banto: ``--version`` gave: $said, not 'banto ...'. $Hint"
    }
}

# Get-BantoVersion EXE: the first line EXE prints for --version, or $null when it does not
# start or exits non-zero. One that takes longer than 10 seconds is killed and reported as
# "(no answer within 10 seconds)", so a hang is refused like any other non-banto.
function Get-BantoVersion([string]$Exe) {
    try {
        $psi = New-Object Diagnostics.ProcessStartInfo
        $psi.FileName = $Exe
        $psi.Arguments = '--version'
        $psi.UseShellExecute = $false
        $psi.CreateNoWindow = $true
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $psi.RedirectStandardInput = $true
        $proc = [Diagnostics.Process]::Start($psi)
        try {
            $proc.StandardInput.Close()
            $out = $proc.StandardOutput.ReadToEndAsync()
            $err = $proc.StandardError.ReadToEndAsync()
            if (-not $proc.WaitForExit(10000)) {
                # waited for, so the file is released before a caller deletes it
                try { $proc.Kill(); $null = $proc.WaitForExit(5000) } catch { }
                return '(no answer within 10 seconds)'
            }
            $text = $out.Result
            if (-not $text) { $text = $err.Result }
            if ($proc.ExitCode -ne 0) { return $null }
            $first = ($text -split "`r?`n" | Select-Object -First 1)
            if ($first) { return "$first".Trim() } else { return '' }
        } finally { $proc.Dispose() }
    } catch { return $null }
}

function Install-Banto {
    $ErrorActionPreference = 'Stop'
    Set-StrictMode -Version 2.0
    $ProgressPreference = 'SilentlyContinue'   # Windows PowerShell 5.1 downloads crawl with the progress bar

    $repo = 'kt81/banto'
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    } catch { }

    $onWindows = $true
    if ($PSVersionTable.PSEdition -eq 'Core') { $onWindows = [bool]$IsWindows }
    if (-not $onWindows) { throw 'install.ps1 is for Windows; on Linux and macOS use install.sh.' }

    # --- which release file is ours ------------------------------------------------------
    $arch = $env:PROCESSOR_ARCHITEW6432
    if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    if ($arch -ne 'AMD64') {
        throw "no prebuilt banto for $arch; build from source: cargo install --git https://github.com/$repo banto --locked"
    }
    $target = 'x86_64-pc-windows-msvc'

    $source = $env:BANTO_INSTALL_SOURCE
    $sourceKind = 'github'
    if ($source) {
        if ($source -match '^https?://') { $sourceKind = 'url' } else { $sourceKind = 'dir' }
    }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('banto-install-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    $staged = $null
    try {
        $tag = $env:BANTO_VERSION
        if ($tag -and $tag -match '^[0-9]') { $tag = "v$tag" }

        # With no tag yet, GitHub's /releases/latest/download/ redirect names the latest
        # release's file, which is how the latest tag is learned without the rate-limited API.
        function Get-ReleaseFile([string]$name, [string]$dest) {
            switch ($sourceKind) {
                'github' {
                    $base = if ($tag) { "https://github.com/$repo/releases/download/$tag" } else { "https://github.com/$repo/releases/latest/download" }
                    Invoke-WebRequest -UseBasicParsing -Uri "$base/$name" -OutFile $dest
                }
                'url'    { Invoke-WebRequest -UseBasicParsing -Uri ($source.TrimEnd('/') + "/$name") -OutFile $dest }
                'dir'    {
                    $from = Join-Path $source $name
                    if (-not (Test-Path -LiteralPath $from)) { throw "$from does not exist" }
                    Copy-Item -LiteralPath $from -Destination $dest
                }
            }
        }

        if (-not $tag) {
            # The latest release's SHA256SUMS lists one archive per target, named after its tag.
            # The same file is then the one the download is verified against, so a release
            # published between the two reads cannot split them.
            try {
                Get-ReleaseFile 'SHA256SUMS' (Join-Path $tmp 'SHA256SUMS')
            } catch {
                throw "could not read the latest release's SHA256SUMS ($($_.Exception.Message)); is a release published? Set `$env:BANTO_VERSION = '<tag>' to choose one."
            }
            foreach ($line in Get-Content -LiteralPath (Join-Path $tmp 'SHA256SUMS')) {
                if ($line -match "^[0-9a-fA-F]{64}\s+\*?banto-(.+)-$([regex]::Escape($target))\.zip\s*$") { $tag = $Matches[1]; break }
            }
            if (-not $tag) { throw "no $target archive in the latest release's SHA256SUMS (set `$env:BANTO_VERSION = '<tag>' to choose a release)" }
        }
        if (-not $tag) { throw 'could not work out which release to install' }
        $archive = "banto-$tag-$target.zip"

        # --- where it goes ---------------------------------------------------------------
        $onPathAll = @(Get-Command banto -CommandType Application -All -ErrorAction SilentlyContinue)
        $existing = $onPathAll | Select-Object -First 1
        if ($existing) {
            $dest = Resolve-RealPath $existing.Source
            $removeHint = 'Nothing was changed. Remove it from PATH (or move it away) and run this again; a banto is then installed to $env:BANTO_INSTALL_DIR or %USERPROFILE%\.local\bin.'
            if ([IO.Path]::GetFileName($existing.Source) -ine 'banto.exe') {
                throw "the banto on your PATH is $($existing.Source), which is not a banto.exe. $removeHint"
            }
            # Whatever answers to `banto` first is about to be overwritten, so it has to be a
            # banto: a shim or wrapper would be destroyed, not upgraded.
            Assert-IsBanto $dest "the banto on your PATH ($($existing.Source), file $dest)" $removeHint
            $dir = Split-Path -Parent $dest
            # Only the first one is replaced. Refusing would stop an upgrade over a leftover,
            # and a second copy cannot be told apart from a deliberate one, so say so and go on.
            $others = @()
            foreach ($c in $onPathAll) {
                $r = Resolve-RealPath $c.Source
                if ($r -ine $dest -and $others -notcontains $r) { $others += $r }
            }
            if ($others.Count -gt 0) {
                Write-Host -ForegroundColor Yellow "banto install: warning: more than one banto is on your PATH. Only $dest, the first, is replaced; these are left alone and are now stale: $($others -join ', '). Codex trusts one banto path per machine: remove the others."
            }
            if ($env:BANTO_INSTALL_DIR) {
                Write-Host "banto is already installed at $dest; BANTO_INSTALL_DIR is ignored (one banto per machine)."
            }
        } else {
            $dir = $env:BANTO_INSTALL_DIR
            if (-not $dir) {
                $home_ = $env:USERPROFILE
                if (-not $home_) { throw 'USERPROFILE is not set' }
                $dir = Join-Path $home_ '.local\bin'
            }
            $dir = [IO.Path]::GetFullPath($dir).TrimEnd('\')
            $dest = Join-Path $dir 'banto.exe'
            if (Test-Path -LiteralPath $dest) {
                $dest = Resolve-RealPath $dest
                $dir = Split-Path -Parent $dest
                # A file already there that is not a banto belongs to someone else.
                Assert-IsBanto $dest $dest 'Nothing was changed. Move it away, or choose another directory with $env:BANTO_INSTALL_DIR.'
            }
        }

        if ($dest -match '\s') {
            Write-Host -ForegroundColor Yellow "banto install: warning: the path $dest contains a space: Codex members cannot be briefed through such a path (Claude Code members are unaffected). Choose a directory without a space with `$env:BANTO_INSTALL_DIR if you use Codex."
        }

        if (Test-Path -LiteralPath $dest) {
            $running = @()
            foreach ($p in @(Get-Process -Name banto -ErrorAction SilentlyContinue)) {
                try { if ($p.Path -and ([IO.Path]::GetFullPath($p.Path) -ieq $dest)) { $running += $p.Id } } catch { }
            }
            if ($running.Count -gt 0) {
                throw "banto is running from $dest (pid $($running -join ', ')). Quit it (including any banto session hosting your agents) and run this again."
            }
        }

        # --- download and verify ---------------------------------------------------------
        Write-Host "Installing banto $tag ($target) to $dest"
        $sums = Join-Path $tmp 'SHA256SUMS'
        $zip = Join-Path $tmp $archive
        if (-not (Test-Path -LiteralPath $sums)) { Get-ReleaseFile 'SHA256SUMS' $sums }
        Get-ReleaseFile $archive $zip

        $expected = $null
        foreach ($line in Get-Content -LiteralPath $sums) {
            if ($line -match '^([0-9a-fA-F]{64})\s+\*?(.+?)\s*$' -and $Matches[2] -eq $archive) { $expected = $Matches[1].ToLowerInvariant(); break }
        }
        if (-not $expected) { throw "$archive is not listed in SHA256SUMS" }
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash.ToLowerInvariant()
        if ($actual -ne $expected) { throw "checksum mismatch for $archive (expected $expected, got $actual); nothing was installed" }

        $unpacked = Join-Path $tmp 'x'
        Expand-Archive -LiteralPath $zip -DestinationPath $unpacked -Force
        $exe = Join-Path $unpacked 'banto.exe'
        if (-not (Test-Path -LiteralPath $exe)) { throw "$archive does not contain banto.exe" }

        # --- install ---------------------------------------------------------------------
        if (Test-Path -LiteralPath $dest) {
            $before = Get-BantoVersion $dest
            Write-Host "before: $(if ($null -eq $before) { '(could not run)' } elseif ($before) { $before } else { '(no output)' })"
        } else {
            Write-Host 'before: (not installed)'
        }

        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        # An install killed between the copy and the move leaves its staged file behind.
        # Remove those, and only those: files named exactly like our staging names
        # (".banto-" + 12 hex digits + ".exe") created over an hour ago, so an install running
        # right now keeps its own. Creation time, not write time: Copy-Item keeps the
        # archive's old write time. Nothing else in the directory is touched.
        $cutoff = (Get-Date).AddHours(-1)
        foreach ($f in @(Get-ChildItem -LiteralPath $dir -Force -File -ErrorAction SilentlyContinue)) {
            if ($f.Name -match '^\.banto-[0-9a-f]{12}\.exe$' -and $f.CreationTime -lt $cutoff) {
                Remove-Item -LiteralPath $f.FullName -Force -ErrorAction SilentlyContinue
            }
        }
        # Copy beside the target under a name nobody else owns (a fixed one would overwrite,
        # then delete, a file of the user's), and move it over the target only after it has
        # run once: a failure leaves the old file intact.
        $staged = Join-Path $dir ('.banto-' + [guid]::NewGuid().ToString('N').Substring(0, 12) + '.exe')
        Copy-Item -LiteralPath $exe -Destination $staged
        $new = Get-BantoVersion $staged
        if ($new -eq '(no answer within 10 seconds)') {
            throw "the downloaded banto did not answer ``--version`` within 10 seconds. Nothing was changed; the banto you had is untouched."
        }
        if (-not $new -or -not $new.StartsWith('banto ')) {
            $shown = if ($null -eq $new) { 'nothing, or it did not run' } else { $new }
            throw "the downloaded banto does not run on this machine (``--version`` gave: $shown). Nothing was changed; the banto you had is untouched."
        }
        try {
            Move-Item -LiteralPath $staged -Destination $dest -Force
            $staged = $null
        } catch {
            throw "could not replace $dest ($($_.Exception.Message)). Windows will not replace a running program: quit every banto and run this again."
        }

        # Collected in full before looking at the exit code: stopping the pipeline early
        # (Select-Object -First 1) leaves $LASTEXITCODE unset, which strict mode turns into an error.
        $out = @(& $dest --version 2>&1)
        $code = $LASTEXITCODE
        $after = if ($out.Count) { "$($out[0])" } else { '(no output)' }
        if ($code -ne 0) { throw "$dest was installed but does not run: $after" }
        Write-Host "after:  $after"
        Write-Host "Installed $dest"

        # What a new shell will find, which is not always the file just written
        $now = Get-Command banto -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        $reachable = $false
        if ($now) {
            if ((Resolve-RealPath $now.Source) -ine $dest) {
                Write-Host -ForegroundColor Yellow "banto install: warning: ``banto`` on your PATH now resolves to $($now.Source), not the file just installed ($dest). Remove or reorder the other one."
            } else {
                $reachable = $true
            }
        }

        # On PATH by name, or by another route to the same directory (a junction or symlink)
        $onPath = $false
        $dirReal = Resolve-RealPath $dir
        foreach ($entry in ($env:Path -split ';')) {
            if (-not $entry) { continue }
            if ($entry.TrimEnd('\') -ieq $dir) { $onPath = $true; break }
            try {
                if ((Test-Path -LiteralPath $entry -PathType Container) -and ((Resolve-RealPath $entry).TrimEnd('\') -ieq $dirReal.TrimEnd('\'))) { $onPath = $true; break }
            } catch { }
        }
        # No hint when `banto` already reaches the new file through PATH, even by a link into a
        # directory that is itself not on PATH.
        if (-not $onPath -and -not $reachable) {
            $q = $dir.Replace("'", "''")   # inside single quotes only ' needs escaping, by doubling
            Write-Host ''
            Write-Host "$dir is not on your PATH. To add it for your user, run:"
            Write-Host "  [Environment]::SetEnvironmentVariable('Path', '$q;' + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')"
            Write-Host 'then open a new terminal. For this terminal only:'
            Write-Host "  `$env:Path = '$q;' + `$env:Path"
        }
    } finally {
        if ($staged -and (Test-Path -LiteralPath $staged)) { Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue }
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

try {
    Install-Banto
} catch {
    Write-Host "banto install: $($_.Exception.Message)" -ForegroundColor Red
    if ($PSCommandPath) { exit 1 }
}
