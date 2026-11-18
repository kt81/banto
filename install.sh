#!/bin/sh
# Install or upgrade banto from a GitHub release (Linux and macOS).
#
#   curl -fsSL https://github.com/kt81/banto/releases/latest/download/install.sh | sh
#
# One banto per machine. Codex trusts the hook banto writes (`<exe> _hook`) per full
# command string and keeps a single trust record, so a second banto at another path
# silently takes the trust from the first, and a path with a space cannot brief Codex
# at all. The target is therefore, in this order:
#   1. the banto already on PATH, replaced where it is;
#   2. $BANTO_INSTALL_DIR/banto;
#   3. ~/.local/bin/banto.
# This script never edits PATH or any shell profile: when the directory is not on PATH it
# prints the line to add.
#
# Environment:
#   BANTO_VERSION         a release tag such as v0.2.0 (default: the latest release)
#   BANTO_INSTALL_DIR     directory used when no banto is on PATH yet
#   BANTO_INSTALL_SOURCE  test hook: a directory, or a URL prefix, holding the release files
#                         (banto-<tag>-<target>.tar.gz and SHA256SUMS) instead of GitHub
#
# Re-running is an upgrade. It refuses while a banto started from the target file is running,
# and refuses to overwrite a `banto` on PATH that is not a banto (a wrapper script, a package
# manager's shim: it must run and answer `--version` with "banto ..."). The new binary is
# staged beside the target and run once before it replaces anything. Either run is cut off
# after 10 seconds.
#
# What the checksum is worth: SHA256SUMS comes from the same release as the archive, so
# the check proves the download arrived intact, not who built it. Provenance is a separate
# check: gh attestation verify <archive> --repo kt81/banto
set -eu

REPO="kt81/banto"

say() { printf '%s\n' "$*"; }
warn() { printf 'banto install: warning: %s\n' "$*" >&2; }
die() { printf 'banto install: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

[ -n "${HOME:-}" ] || die "HOME is not set"

tmp=""
staged=""
cleanup() {
    [ -n "$staged" ] && rm -f "$staged"
    [ -n "$tmp" ] && rm -rf "$tmp"
    return 0
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

# fetch URL DEST. An https URL is fetched over https only, redirects included, with no
# fallback to anything older than TLS 1.2; an http one (a local test source) is left alone.
fetch() {
    case "$1" in
        https://*) f_curl="--proto =https --tlsv1.2"; f_wget="--https-only" ;;
        *) f_curl=""; f_wget="" ;;
    esac
    # the flags are word-split on purpose: they hold no spaces inside a word
    if have curl; then
        # shellcheck disable=SC2086
        curl -fsSL $f_curl -o "$2" "$1" || die "could not download $1${fetch_hint:-}"
    elif have wget; then
        # shellcheck disable=SC2086
        wget -q $f_wget -O "$2" "$1" || die "could not download $1${fetch_hint:-}"
    else
        die "curl or wget is required"
    fi
}

# sq STRING: STRING as one single-quoted shell word, whatever it holds
sq() {
    printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

# run_limited OUTFILE CMD [ARGS...]: run CMD with stdin closed and stdout and stderr in
# OUTFILE, and kill it if it is still running after 10 seconds (status 124, as `timeout`
# says it). Done by hand because macOS ships no `timeout`: a banto on PATH that hangs on
# --version must not hang the installer, with or without coreutils. Only the process
# started here is killed; a child it left behind holds a file, not a pipe, so nothing waits
# for it.
run_limited() {
    rl_out=$1
    shift
    if ! have sleep; then
        # no way to count seconds: run it unbounded rather than call a slow start a hang
        rl_rc=0
        "$@" >"$rl_out" 2>&1 </dev/null || rl_rc=$?
        return "$rl_rc"
    fi
    "$@" >"$rl_out" 2>&1 </dev/null &
    rl_pid=$!
    # `sleep 0.1` is not POSIX; fall back to whole seconds where it is refused
    if sleep 0.1 2>/dev/null; then rl_ticks=100; rl_nap=0.1; else rl_ticks=10; rl_nap=1; fi
    rl_i=0
    while kill -0 "$rl_pid" 2>/dev/null; do
        if [ "$rl_i" -ge "$rl_ticks" ]; then
            kill -9 "$rl_pid" 2>/dev/null
            { wait "$rl_pid"; } 2>/dev/null
            return 124
        fi
        sleep "$rl_nap"
        rl_i=$((rl_i + 1))
    done
    rl_rc=0
    { wait "$rl_pid"; } 2>/dev/null || rl_rc=$?
    return "$rl_rc"
}

# banto_version FILE: the first line FILE prints for --version, or nothing if it does not
# run; a hang is reported as such.
banto_version() {
    bv_rc=0
    run_limited "$tmp/version" "$1" --version || bv_rc=$?
    if [ "$bv_rc" = 124 ]; then
        printf '%s\n' "(no answer within 10 seconds)"
    else
        head -n 1 "$tmp/version"
    fi
    return 0
}

# dir_on_path DIR: whether some PATH entry is DIR, by name or as another route to the same
# directory (a symlink to it)
dir_on_path() {
    case ":${PATH:-}:" in *":$1:"*) return 0 ;; esac
    dp_real=$(cd "$1" 2>/dev/null && pwd -P) || return 1
    (
        set -f
        IFS=:
        for e in ${PATH:-}; do
            case "$e" in /*) ;; *) continue ;; esac
            [ "$(cd "$e" 2>/dev/null && pwd -P)" = "$dp_real" ] && exit 0
        done
        exit 1
    )
}

# --- which release file is ours --------------------------------------------------------

os=$(uname -s)
arch=$(uname -m)
if [ "$os" = "Darwin" ] && [ "$arch" = "x86_64" ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = "1" ]; then
    arch=arm64 # a shell running under Rosetta on an Apple silicon Mac
fi
case "$os/$arch" in
    Linux/x86_64 | Linux/amd64)
        target="x86_64-unknown-linux-gnu"
        if have ldd && ldd --version 2>&1 | grep -qi musl; then
            die "the Linux build needs glibc and this system uses musl; build from source: cargo install --git https://github.com/$REPO banto --locked"
        fi
        ;;
    Darwin/arm64 | Darwin/aarch64) target="aarch64-apple-darwin" ;;
    *) die "no prebuilt banto for $os $arch; build from source: cargo install --git https://github.com/$REPO banto --locked" ;;
esac

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t banto-install) || die "cannot create a temporary directory"

source_dir="${BANTO_INSTALL_SOURCE:-}"
case "$source_dir" in
    http://* | https://*) source_kind=url ;;
    "") source_kind=github ;;
    *) source_kind=dir ;;
esac

# get_file NAME DEST: one release file from wherever this run reads them. With no tag yet,
# GitHub's /releases/latest/download/ redirect names the latest release's file, which is how
# the latest tag is learned without the rate-limited API.
get_file() {
    case "$source_kind" in
        github)
            if [ -n "$tag" ]; then
                fetch "https://github.com/$REPO/releases/download/$tag/$1" "$2"
            else
                fetch "https://github.com/$REPO/releases/latest/download/$1" "$2"
            fi
            ;;
        url) fetch "${source_dir%/}/$1" "$2" ;;
        dir) [ -f "$source_dir/$1" ] || die "$source_dir/$1 does not exist"; cp "$source_dir/$1" "$2" ;;
    esac
}

tag="${BANTO_VERSION:-}"
case "$tag" in [0-9]*) tag="v$tag" ;; esac

if [ -z "$tag" ]; then
    # The latest release's SHA256SUMS lists one archive per target, named after its tag. The
    # same file is then the one the download is verified against, so a release published
    # between the two reads cannot split them.
    fetch_hint=" (is a release published? set BANTO_VERSION=<tag> to choose one)"
    get_file SHA256SUMS "$tmp/SHA256SUMS"
    fetch_hint=""
    tag=$(sed -n "s/^[0-9a-fA-F]*[[:space:]]*\\*\\{0,1\\}banto-\\(.*\\)-$target\\.tar\\.gz\$/\\1/p" "$tmp/SHA256SUMS" | head -n 1)
    [ -n "$tag" ] || die "no $target archive in the latest release's SHA256SUMS (set BANTO_VERSION=<tag> to choose a release)"
fi

archive="banto-$tag-$target.tar.gz"

# --- where it goes ---------------------------------------------------------------------

# resolve_link PATH: follow symbolic links to the file itself. banto's own idea of its
# exe path is the resolved one, and replacing the link would orphan the file behind it.
resolve_link() {
    p="$1"
    n=0
    while [ -L "$p" ] && [ "$n" -lt 40 ]; do
        l=$(readlink "$p") || break
        case "$l" in
            /*) p="$l" ;;
            *) p="$(dirname "$p")/$l" ;;
        esac
        n=$((n + 1))
    done
    printf '%s\n' "$p"
}

# assert_is_banto FILE SHOWN HINT: die unless FILE runs and answers --version with
# "banto ...". SHOWN names it the way the user knows it.
assert_is_banto() {
    found=$(banto_version "$1")
    case "$found" in
        "banto "*) ;;
        # banto_version's marker for a run that was cut off: say that, not "not a banto"
        "(no answer within 10 seconds)") die "$2 did not answer \`--version\` within 10 seconds. $3" ;;
        *) die "$2 does not look like banto: \`--version\` printed \"${found:-nothing}\", not \"banto ...\". $3" ;;
    esac
}

# physical PATH: the file PATH names once links and symlinked directories are followed, the
# form dest is kept in, so two spellings of one file compare equal
physical() {
    r=$(resolve_link "$1")
    d=$(dirname "$r")
    if [ -d "$d" ]; then
        printf '%s/%s\n' "$(cd "$d" && pwd -P)" "$(basename "$r")"
    else
        printf '%s\n' "$r"
    fi
}

# all_on_path: every executable file called banto in an absolute PATH entry, in PATH order
all_on_path() {
    (
        set -f
        IFS=:
        for d in ${PATH:-}; do
            case "$d" in /*) ;; *) continue ;; esac
            [ -f "$d/banto" ] && [ -x "$d/banto" ] && printf '%s\n' "$d/banto"
        done
    )
    return 0
}

existing=$(command -v banto 2>/dev/null || true)
case "$existing" in /*) ;; *) existing="" ;; esac

if [ -n "$existing" ]; then
    dest=$(resolve_link "$existing")
    dir=$(dirname "$dest")
    # Whatever answers to `banto` first is about to be overwritten, so it has to be a banto:
    # a wrapper script or a package manager's shim would be destroyed, not upgraded.
    assert_is_banto "$dest" "the banto on your PATH ($existing, file $dest)" "Nothing was changed. Remove it from PATH (or move it away) and run this again; a banto is then installed to BANTO_INSTALL_DIR or ~/.local/bin."
    # Only the first one is replaced. Refusing would stop an upgrade over a leftover, and a
    # second copy cannot be told apart from a deliberate one, so say so and go on.
    first=$(physical "$dest")
    # One path per line, never word-split: a PATH directory may hold spaces.
    nl='
'
    seen="$nl"
    shown=""
    while IFS= read -r c; do
        [ -n "$c" ] || continue
        r=$(physical "$c")
        [ "$r" = "$first" ] && continue
        case "$seen" in *"$nl$r$nl"*) continue ;; esac
        seen="$seen$r$nl"
        shown="$shown${shown:+, }'$r'"
    done <<EOF
$(all_on_path)
EOF
    if [ -n "$shown" ]; then
        warn "more than one banto is on your PATH. Only $first, the first, is replaced; these are left alone and are now stale: $shown. Codex trusts one banto path per machine: remove the others."
    fi
    if [ -n "${BANTO_INSTALL_DIR:-}" ]; then
        say "banto is already installed at $dest; BANTO_INSTALL_DIR is ignored (one banto per machine)."
    fi
else
    dir="${BANTO_INSTALL_DIR:-$HOME/.local/bin}"
    case "$dir" in /*) ;; *) dir="$PWD/$dir" ;; esac
    dir="${dir%/}"
    dest="$dir/banto"
fi
[ -d "$dir" ] && dir=$(cd "$dir" && pwd -P) && dest="$dir/$(basename "$dest")"
if [ -z "$existing" ] && [ -e "$dest" ]; then
    # A file already there that is not a banto belongs to someone else.
    dest=$(physical "$dest")
    dir=$(dirname "$dest")
    assert_is_banto "$dest" "$dest" "Nothing was changed. Move it away, or choose another directory with BANTO_INSTALL_DIR."
fi

case "$dest" in
    *" "*) warn "the path $dest contains a space: Codex members cannot be briefed through such a path (Claude Code members are unaffected). Choose a directory without a space with BANTO_INSTALL_DIR if you use Codex." ;;
esac

# running_from FILE: pids of processes started from exactly this file
running_from() {
    if [ -d /proc/self ]; then
        for p in /proc/[0-9]*; do
            e=$(readlink "$p/exe" 2>/dev/null) || continue
            [ "$e" = "$1" ] && printf '%s\n' "${p#/proc/}"
        done
    elif have lsof; then
        lsof -t -- "$1" 2>/dev/null || true
    else
        ps -axo pid=,comm= 2>/dev/null | while read -r pid comm; do
            [ "$comm" = "$1" ] && printf '%s\n' "$pid"
        done
    fi
    return 0
}

# stale_from FILE: Linux only. A banto started before an earlier upgrade keeps running from
# the replaced file, which the kernel reports as "<path> (deleted)".
stale_from() {
    [ -d /proc/self ] || return 0
    for p in /proc/[0-9]*; do
        e=$(readlink "$p/exe" 2>/dev/null) || continue
        [ "$e" = "$1 (deleted)" ] && printf '%s\n' "${p#/proc/}"
    done
    return 0
}

if [ -e "$dest" ]; then
    pids=$(running_from "$dest" | tr '\n' ' ')
    if [ -n "${pids% }" ]; then
        die "banto is running from $dest (pid ${pids% }). Quit it and run this again."
    fi
    stale=$(stale_from "$dest" | tr '\n' ' ')
    if [ -n "${stale% }" ]; then
        warn "a banto started before an earlier upgrade is still running from a replaced file (pid ${stale% }). Quit it and start banto again to run the new version."
    fi
fi

# --- download and verify ---------------------------------------------------------------

say "Installing banto $tag ($target) to $dest"
[ -f "$tmp/SHA256SUMS" ] || get_file SHA256SUMS "$tmp/SHA256SUMS"
get_file "$archive" "$tmp/$archive"

expected=$(awk -v n="$archive" '($2 == n || $2 == "*" n) { print tolower($1) }' "$tmp/SHA256SUMS" | head -n 1)
[ -n "$expected" ] || die "$archive is not listed in SHA256SUMS"
if have sha256sum; then
    actual=$(sha256sum "$tmp/$archive" | awk '{ print tolower($1) }')
elif have shasum; then
    actual=$(shasum -a 256 "$tmp/$archive" | awk '{ print tolower($1) }')
elif have openssl; then
    actual=$(openssl dgst -sha256 "$tmp/$archive" | awk '{ print tolower($NF) }')
else
    die "sha256sum, shasum or openssl is required to verify the download"
fi
[ "$actual" = "$expected" ] || die "checksum mismatch for $archive (expected $expected, got $actual); nothing was installed"

mkdir "$tmp/x"
tar -xzf "$tmp/$archive" -C "$tmp/x" || die "could not unpack $archive"
[ -f "$tmp/x/banto" ] || die "$archive does not contain a banto binary"

# --- install ---------------------------------------------------------------------------

if [ -x "$dest" ]; then
    say "before: $(banto_version "$dest")"
else
    say "before: (not installed)"
fi

if [ -n "$existing" ]; then
    dir_hint="the banto on your PATH lives in this directory, so BANTO_INSTALL_DIR cannot help: run this as a user who can write there, or remove that banto from PATH first and a new one goes to BANTO_INSTALL_DIR or ~/.local/bin"
else
    dir_hint="this script does not use sudo; run it as a user who can write there, or set BANTO_INSTALL_DIR to a directory you own"
fi
mkdir -p "$dir" || die "cannot create $dir ($dir_hint)"
# An install killed between the copy and the rename leaves its staged file behind. Remove
# those, and only those: files named exactly like our staging names (".banto." plus the six
# characters mktemp adds) that are over an hour old, so an install running right now keeps
# its own. Nothing else in the directory is touched.
find "$dir" -maxdepth 1 -type f -name '.banto.??????' -mmin +60 -exec rm -f {} + 2>/dev/null || true
# Write beside the target and rename over it: overwriting a running executable in place
# fails with "Text file busy", a rename does not, and readers never see a half-written file.
staged=$(mktemp "$dir/.banto.XXXXXX") || die "cannot write to $dir ($dir_hint)"
cp "$tmp/x/banto" "$staged" || die "cannot write to $dir ($dir_hint)"
chmod 755 "$staged"

# Run the new binary once before it replaces anything: a build that does not start here
# (a glibc older than the release's floor, a wrong architecture) must not take the working
# banto with it.
new_failed=0
new_rc=0
run_limited "$tmp/new" "$staged" --version || new_rc=$?
new=$(head -n 1 "$tmp/new")
if [ "$new_rc" = 124 ]; then
    new_failed=1
    new="(no answer within 10 seconds)"
elif [ "$new_rc" != 0 ]; then
    new_failed=1
fi
case "$new" in
    "banto "*) ;;
    *) new_failed=1 ;;
esac
if [ "$new_failed" = 1 ]; then
    # Only a loader complaint about GLIBC says the floor is the cause; a hang, a wrong
    # architecture or a broken download are not helped by it.
    floor=""
    if [ "$os" = "Linux" ] && [ "$new_rc" != 124 ] && grep -qi 'GLIBC' "$tmp/new"; then
        floor=" The Linux build needs glibc 2.35 or newer; check yours with \`ldd --version\`, or build from source: cargo install --git https://github.com/$REPO banto --locked."
    fi
    if [ "$new_rc" = 124 ]; then
        die "the downloaded banto did not answer \`--version\` within 10 seconds. Nothing was changed; the banto you had is untouched."
    fi
    die "the downloaded banto does not run on this machine (\`--version\` printed: ${new:-nothing}).$floor Nothing was changed; the banto you had is untouched."
fi

mv -f "$staged" "$dest" || die "cannot replace $dest"
staged=""

if ! after=$("$dest" --version 2>&1); then
    die "$dest was installed but does not run: $after"
fi
say "after:  $(printf '%s\n' "$after" | head -n 1)"
say "Installed $dest"

# What a new shell will find, which is not always the file just written
now=$(command -v banto 2>/dev/null || true)
reachable=0
case "$now" in
    /*)
        if [ "$(physical "$now")" != "$dest" ]; then
            warn "\`banto\` on your PATH now resolves to $now, not the file just installed ($dest). Remove or reorder the other one."
        else
            reachable=1
        fi
        ;;
esac

# No hint when `banto` already reaches the new file through PATH, even by a link into a
# directory that is itself not on PATH (~/bin/banto -> ~/opt/real/banto).
if [ "$reachable" != 1 ] && ! dir_on_path "$dir"; then
    say ""
    say "$dir is not on your PATH. To add it, put this line in your shell profile:"
    say "  export PATH=$(sq "$dir"):\"\$PATH\""
fi
