# Releasing banto

For the maintainer. A release is a tag; the workflow
([.github/workflows/release.yml](../.github/workflows/release.yml)) does the building
and leaves a **draft** release. Publishing is a deliberate step by hand, after the
checks below, and it publishes the exact files the workflow produced.

## 1. Version and tag

1. Bump `version` under `[workspace.package]` in the root `Cargo.toml` (and let
   `Cargo.lock` follow: the workflow builds with `--locked`).
2. Move the five `version` requirements that sit beside the `path` dependencies in
   the crate manifests to the new version as well. A
   `path` dependency with a `version` is checked against the version it finds, so they
   do not follow the workspace version by themselves.
3. Commit, then tag `v<version>` and push the tag. The tag **must equal Cargo's version
   exactly**, prerelease suffix included: tag `v0.2.0-rc.1` needs version `0.2.0-rc.1`.
   The first job fails otherwise, because `banto --version` prints the Cargo version and
   a binary must not name a different release than the page it is downloaded from. A tag
   with a `-` in it becomes a prerelease. Push the tag with git, not from GitHub's "Draft
   a new release" page: v0.1.0 was first tagged there, which also published an empty
   release for the tag, and the workflow's own draft step would have failed on it.

You can rehearse everything except the draft without a tag: run the workflow by hand
(`workflow_dispatch`). It uses a throwaway tag name and stops before attesting and
drafting.

## 2. What the workflow does

1. Checks the tag against the `banto` crate's version.
2. Builds `--release --locked` for `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`
   (on Ubuntu 22.04, so the glibc floor is 2.35; the step fails if the binary needs a
   newer one and writes the real floor to the job summary) and `aarch64-apple-darwin`,
   with `BANTO_BUILD_COMMIT` set to the commit (`crates/banto/build.rs` uses a value
   already in the environment in preference to asking git, cut to 12 characters; the
   `dirty=` field of the pane log's start line is always git's, so a release from the
   workflow's clean checkout says `no`). On Windows it also runs an
   informational Microsoft Defender scan of the built `banto.exe` and writes the result
   and the definitions version to the job summary.
3. Packs `banto-<tag>-<target>.zip` / `.tar.gz` (the single binary at the root), fails if
   an archive holds anything else, and writes `SHA256SUMS`.
4. Runs `install.ps1` (Windows PowerShell 5.1 and 7) and `install.sh` (Linux, macOS)
   from the checkout against those archives in an empty home: install, upgrade in
   place, refusal while a banto runs, refusal to overwrite a file that is not a banto
   (one that answers `--version` with something else), replacing an old banto, refusal
   on a wrong checksum. The upgrade runs once with the new banto on `PATH` (replaced where
   it is) and the refusals run against `BANTO_INSTALL_DIR` and `PATH`. A small stand-in
   program plays the old banto. Not in CI, only tested by hand: a new binary that
   does not run or hangs past 10 seconds (the run-before-replace refusal), links and
   junctions, the warning about more than one banto on `PATH`, the `PATH` hint and its
   quoting, and the cleanup of stale staging files. The running-banto refusal is allowed
   to fail on macOS (`continue-on-error`), so it is not a gate there.
5. Attests the three archives (build provenance, `actions/attest`).
6. Creates a **draft** release with the archives, `SHA256SUMS`, `install.sh` and
   `install.ps1`.

## 3. The gate before publishing

Do these on the draft's own files. **Never rebuild between the gate and publishing**: what
you checked must be the bytes users get. If a check fails, fix the cause and start again
with a new tag; do not edit the draft's assets. A run that fails before it drafts has put
nothing where anyone can download it, so there the tag can move instead: fix the cause on
`main`, rehearse with `workflow_dispatch`, then `git tag -f v<version> <fixed commit>` and
`git push -f origin v<version>` (v0.1.0 went out this way after its first run failed in a
test step).

### a. Defender scan of the Windows archive

Download `banto-<tag>-x86_64-pc-windows-msvc.zip` from the draft, extract it into a
scratch directory, and scan the exe (this ignores exclusions and takes no action):

```powershell
& "$env:ProgramFiles\Windows Defender\MpCmdRun.exe" -Scan -ScanType 3 -File <path-to-banto.exe> -DisableRemediation
```

It reports whether it found a threat. On a clean file it printed "found no threats" and
exited 0 (observed on a scratch file); the exit code for a detection was not observed, so
read the output rather than trust the code alone. Write down the definitions version
(`(Get-MpComputerStatus).AntivirusSignatureVersion`).

### b. Windows, as a user would get it

Once per release, on a machine whose Defender has no exclusion covering the directory
(check with `& "$env:ProgramFiles\Windows Defender\MpCmdRun.exe" -CheckExclusion -Path <dir>`,
which fails with `0x80070005` unless the shell is elevated, so run that one step as
administrator):

1. Download the Windows archive **with a browser** into that directory, so the file
   carries the web's mark (SmartScreen).
2. Extract it, run `banto.exe --version`, then start banto, once from a terminal and once
   by double-clicking it in Explorer: for v0.1.0, SmartScreen spoke only when it was
   started from Explorer. On a machine where a banto is already running on the same
   database the start is refused (one banto per database), which still shows whether
   Windows let it run.
3. Run the documented install line in a plain PowerShell window. A draft's files are not
   public, so the line as printed (`irm .../releases/latest/download/install.ps1 | iex`)
   cannot reach them yet: download the draft's `install.ps1`, `SHA256SUMS` and the Windows
   archive into one directory (with the browser, as above), set
   `$env:BANTO_INSTALL_SOURCE` to that directory and `$env:BANTO_VERSION` to the tag, and
   run `Get-Content -Raw .\install.ps1 | iex`, which is what `irm <url> | iex` does with
   the same text. After publishing, run the line exactly as printed (section 4). On a
   machine that already has banto installed this upgrades that install in place:
   `BANTO_INSTALL_DIR` is ignored then (one banto per machine), so it cannot be pointed
   at a scratch directory (v0.3.0's check upgraded the maintainer's install that way).
4. Write down what Defender and SmartScreen said at each step, even if it was nothing.

### c. macOS, on a real Mac

CI only builds and runs the installer there. On a real Mac, install the draft the way the
one-liner would (the one-liner itself cannot reach a draft's files): download the
draft's `install.sh`, `SHA256SUMS` and the macOS archive into one directory with
`gh release download v<version> --repo kt81/banto -p install.sh -p SHA256SUMS -p '*darwin*'`
(the one-liner fetches with curl, not a browser, so a browser download is not the same
test), then run `BANTO_INSTALL_SOURCE="$PWD" BANTO_VERSION=v<version> sh install.sh`. Note
any Gatekeeper message, then check that a session starts in a pane and resumes
(PTY launch), that the list updates when session files change (file watch), and that
drawing and input behave. Record the results, and update the macOS line in the README's
Install section.

### d. If Defender flags anything

Do not publish. Submit the file at
<https://www.microsoft.com/en-us/wdsi/filesubmission> as a software developer, and
record the incident in [notes/windows-defender.md](notes/windows-defender.md) using its
template. Publish once it is cleared, or decide knowingly to publish with the risk
written on the release page; never tell users to add an exclusion.

## 4. Publish

Publish the draft in the GitHub UI. The one-line installs use
`releases/latest/download/...`, which only resolves once a release is published (and is
not a prerelease), so check both lines against the published release straight away, and
check one archive with `gh attestation verify <archive> --repo kt81/banto`.
