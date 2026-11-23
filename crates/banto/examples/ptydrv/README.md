# ptydrv

Run one program in a pseudo-terminal under banto's own PTY host and drive it
through files in a control directory. A diagnostic tool: it exists to watch a real
interactive agent TUI under ConPTY without banto's UI in between, and to time what
banto cannot see from outside, such as how long after a write the child answers.

**Never stop a Claude younger than 10 seconds (no hangup, no kill): force-killing a
young Claude twice turns full-screen rendering off machine-wide.** The driver only
logs `YOUNG` in `driver.log`; it does not refuse.

Verified running on Windows. Builds on Unix and inherits banto-io's shutdown
handling, but not run there. The control convention (files in `<ctl>/in/`) is not
fixed: it is whatever the measurements needed, and may change.

It starts the child with `banto_io::pty::PortablePtyHost` (the same PTY fork and
spawn as banto's panes) and keeps the screen with `banto_core::screen::Screen`.

## Run

```
cargo run -p banto --example ptydrv -- run --cwd <dir> --ctl <dir> [--cols 120 --rows 40] \
    [--gap-ms 300] [--hangup-grace-ms <ms>] [--env KEY=VALUE]... -- <program> <args...>
```

or build once and run the binary, `target/debug/examples/ptydrv.exe` (no `.exe`
off Windows). It blocks until the child exits, so start it detached
(`Start-Process -WindowStyle Hidden`) when something else has to type into it. Use
one `--ctl` per child. `<program>` is resolved as `CreateProcess` does: a `.cmd`
shim needs `cmd /c <shim> ...` or its full path.

`--ctl` is refused inside this workspace and inside an agent's home (`~/.claude`,
`~/.codex`, `$CODEX_HOME`, `$CLAUDE_CONFIG_DIR`): `raw.log` and `screen.txt` are a
real session's output, which must not land in the repository or in an agent's home.
`--hangup-grace-ms` defaults to banto's `PANE_END_GRACE`.

## Files in `<ctl>`

| file | what |
|---|---|
| `raw.log` | the child's output bytes, appended as they arrive, untouched |
| `screen.txt` | the current screen as text (`--cols` by `--rows`), rewritten every 500 ms |
| `in/` | input drop box, looked at every 200 ms (below) |
| `exit.txt` | written when the child exits: `exit_code`, `signal`, `exited_at` (UTC), `exited_at_unix_ms`, `killed_by_driver` |
| `driver.log` | what the driver did, each line stamped with UTC and milliseconds since start: the spawn, every output chunk (`out offset=<byte in raw.log> len=<bytes>`), every write taken from `in/`, hangup, kill |

`raw.log` has no timestamps and `screen.txt` is 500 ms granular; the `out` lines
in `driver.log` are what give a wake-up latency. The driver's own exit code is the
child's, clamped to 0-255; the full code is in `exit.txt`. A hangup on Windows ends
the child with `0xC000013A` (3221225786), which is not an error.

## Typing: `in/`

Put a file in `<ctl>/in/`; its content is written to the PTY and the file is
deleted. Files go in ascending file-name order, at least `--gap-ms` apart (300 by
default, banto's `RELAY_SUBMIT_DELAY`), so a body and its Enter as two files are
two PTY writes 300 ms apart:

```powershell
$in = "$ctl\in"
[IO.File]::WriteAllText("$in\001-body.tmp", 'hello'); Move-Item "$in\001-body.tmp" "$in\001-body"
[IO.File]::WriteAllText("$in\002-enter.tmp", '\r');   Move-Item "$in\002-enter.tmp" "$in\002-enter"
```

* Write to `<name>.tmp` (or `.<name>`) and rename: such names are ignored until
  renamed, so a half-written file is never sent.
* Escapes: `\r` `\n` `\t` `\e` `\x1b` `\xNN` `\\`. Any other backslash stays as it
  is. UTF-8 passes through.
* A literal CR or LF in the file is dropped, so the line ending that `Set-Content`
  or `echo >` adds does not become an Enter, and so is a leading UTF-8 BOM. Enter is
  `\r`. `\n` is LF, which a TUI may take as a newline (PSReadLine continued the line
  instead of submitting it).
* A file named exactly `hangup` closes the console the way banto closes a pane:
  hang up, drop the input and the PTY master off-thread, and kill the child if it is
  still there after `--hangup-grace-ms`. Files after it are not written. On Windows
  the console-close gives a child about 5 s of its own before it is terminated, so at
  the default grace the driver's kill and Windows's termination race; a shorter
  `--hangup-grace-ms` is what reaches the kill.
* Leave `in/` empty when you start: anything the driver finds there is sent.

## Environment

Inherited from the parent, except what banto's own spawn strips
(`CLAUDE_CODE_CHILD_SESSION`, which makes a Claude write no `sessions/<pid>.json`)
and every variable starting `BANTO_`. The second is this tool's own need and not
banto's: a child started from a Worker's pane would otherwise carry that Worker's
brigade, member and role. `CLAUDE_CODE_NO_FLICKER` is never added. banto sets
`CLAUDE_CODE_NATIVE_CURSOR=1` on a Claude pane; pass `--env
CLAUDE_CODE_NATIVE_CURSOR=1` to match it.

## Limits

* The Unix hangup (SIGHUP) banto sends is in `banto-io`'s `Hangup` and is what this
  calls, but it has not been run here.
* The driver answers no terminal queries (DSR/DA), as banto does not.
* `screen.txt` is text only: no colours, attributes or cursor position.

## Tests

`cargo test -p banto --example ptydrv` runs without a real PTY: the escapes, which
file goes next and when (a pure function, so the gap is tested without sleeping), the
`--ctl` guard, and the loop against `MockPtyHost` in a temporary directory.
