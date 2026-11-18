# Windows Defender and banto.exe (2026-11-06, 2026-11-18)

A record of what Microsoft Defender has done to banto, kept so the next incident can
be compared with this one. It states what happened and nothing about why Defender
decided it.

## 2026-11-06: a flagged release build

All times are JST (UTC+9), as the maintainer recorded them.

| Time | What happened |
|---|---|
| 16:40:19 | Microsoft Defender real-time protection detected `Trojan:Win32/Bearfoos.A!ml` — an ML detection — on the operator's locally built **release** `banto.exe` in the operator's dogfood directory, and on its five running processes. |
| 16:40:40 | Remediated. banto and every session it was hosting ended with it. |
| 16:43:03 | Detected again when the file was copied back into place. |
| 16:53:44 | An exclusion for the dogfood directory was added. |

Also:

- The **debug** build was not flagged.
- The detection came with a Defender definitions update the same day.
- The file was reported to Microsoft.

Defender acting on banto is therefore not a warning but the end of the process: banto
hosts its sessions itself, so they end with it.

## 2026-11-18: a release build scanned clean

The release build of that day scanned clean with definitions **1.463.347.0**:

```powershell
& "$env:ProgramFiles\Windows Defender\MpCmdRun.exe" -Scan -ScanType 3 -File <path-to-banto.exe> -DisableRemediation
```

`-ScanType 3` is a custom scan of one file, `-DisableRemediation` makes it report and
take no action, and it needs no administrator rights. It **ignores exclusions**, which
is what makes it usable on a machine like the operator's where the dogfood directory is
excluded. Exit code 0 means nothing was found, 2 means a threat was found. To see
whether a directory is excluded: `MpCmdRun.exe -CheckExclusion -Path <directory>`. The
definitions version is `(Get-MpComputerStatus).AntivirusSignatureVersion`.

The release workflow runs the same scan on the freshly built exe and writes the result
to the job summary; it is informational and never blocks (see
[RELEASING.md](../RELEASING.md)).

## How to record the next one

Add a section like the ones above, with a heading of the date. Copy this table and fill
it in; leave a field as `unknown` rather than guessing.

| Field | Value |
|---|---|
| Date and time (with zone) | |
| Detection name (exactly as Defender prints it) | |
| Where it fired (real-time protection, scheduled scan, SmartScreen, on download, on copy) | |
| Definitions version (`AntivirusSignatureVersion`) | |
| File and its sha256 (`Get-FileHash`) | |
| Build (release tag or commit, who built it, release or debug, signed or not) | |
| What Defender did (quarantined, removed, blocked, only warned) | |
| What it took with it (processes ended, sessions lost) | |
| Does `MpCmdRun -Scan -ScanType 3 -File <exe> -DisableRemediation` flag the same file now | |
| Reported to Microsoft (date, submission id), outcome | |
| Outcome (cleared by a definitions update, exclusion added, build changed, ...) | |
