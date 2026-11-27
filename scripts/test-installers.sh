#!/bin/sh
# Tests install.sh (fjo) and gea/install.sh against real release downloads.
#
#   mise run installer-check
#
# The one thing faked is GitHub's release *listing*, through a `curl` shim that answers
# api.github.com/repos/perfectra1n/fjo/releases* from a fixture and passes everything else
# to the real curl. That is what lets a test put a gea release at the top of the list,
# which is the situation this file exists for: fjo and gea publish from one repository,
# so "the repository's latest release" is not "the latest release of this tool". Archives
# and checksums are downloaded for real, so this needs the network.
set -eu

ROOT=$(cd "$(dirname "$0")/.." && pwd)
REAL_CURL=$(command -v curl) || { echo "need curl" >&2; exit 1; }
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT INT TERM
mkdir -p "$T/bin"
export REAL_CURL

cat > "$T/bin/curl" <<'SHIM'
#!/bin/sh
for a in "$@"; do
    case "$a" in https://*) printf '%s\n' "$a" >> "$CURL_LOG" ;; esac
done
for a in "$@"; do
    case "$a" in
        https://api.github.com/repos/perfectra1n/fjo/releases*)
            # A missing fixture stands for the API refusing the request (403, rate limit).
            [ -f "$FIXTURE" ] || exit 22
            cat "$FIXTURE"; exit 0 ;;
    esac
done
exec "$REAL_CURL" "$@"
SHIM
chmod +x "$T/bin/curl"

# One release, in the field order api.github.com uses (tag_name, draft, prerelease).
release() {
    printf '  {\n    "url": "https://api.github.com/repos/perfectra1n/fjo/releases/1",\n'
    printf '    "tag_name": "%s",\n    "target_commitish": "main",\n    "name": "%s",\n' "$1" "$1"
    printf '    "draft": false,\n    "immutable": false,\n    "prerelease": %s,\n' "$2"
    printf '    "created_at": "2026-11-10T01:39:15Z"\n  }'
}
# A gea release is the newest thing in the repository, then an fjo prerelease.
{ printf '[\n'; release gea-v9.9.9 false; printf ',\n'; release v9.9.9-rc.1 true; printf ',\n'
  release v0.3.1 false; printf ',\n'; release v0.3.0 false; printf '\n]\n'; } > "$T/mixed.json"
# The repository before gea has published anything from it.
{ printf '[\n'; release v0.3.1 false; printf ',\n'; release v0.3.0 false; printf '\n]\n'; } > "$T/nogea.json"

fails=0
n=0
# run NAME FIXTURE SCRIPT [VAR=value ...]  -> leaves $out, $log, $dir, $rc
run() {
    name=$1; fixture=$2; script=$3; shift 3
    n=$((n + 1))
    dir="$T/install-$n"; log="$T/curl-$n.log"; out="$T/out-$n.txt"
    : > "$log"
    rc=0
    env PATH="$T/bin:$PATH" CURL_LOG="$log" FIXTURE="$T/$fixture" HOME="$T/home-$n" \
        FJO_INSTALL_DIR="$dir" GEA_INSTALL_DIR="$dir" FJO_NO_COMPLETIONS=1 GEA_NO_COMPLETIONS=1 \
        "$@" "${RUN_SHELL:-sh}" "$ROOT/$script" > "$out" 2>&1 || rc=$?
}
ok()   { printf 'ok    %s\n' "$name"; }
fail() { printf 'FAIL  %s: %s\n' "$name" "$1"; sed 's/^/      | /' "$out"; fails=$((fails + 1)); }
expect_version() {
    got=$("$dir/$1" --version 2>/dev/null || true)
    # `--version` prints e.g. "fjo 0.3.1 (Forgejo API 17.0.1)".
    case "$rc:$got" in
        "0:$1 $2 "*) ok ;;
        *) fail "wanted '$1 $2 (...)', got '$got' (exit $rc)" ;;
    esac
}
expect_requested() {
    if grep -qxF "$1" "$log"; then ok; else fail "never requested $1"; fi
}

run "fjo: latest skips a newer gea release and a prerelease" mixed.json install.sh
expect_version fjo 0.3.1

run "fjo: FJO_VERSION pins a release" mixed.json install.sh FJO_VERSION=v0.3.0
expect_version fjo 0.3.0

run "gea: GEA_VERSION=v0.2.2 installs from the old repository's release" mixed.json gea/install.sh GEA_VERSION=v0.2.2
expect_version gea 0.2.2
expect_requested "https://github.com/perfectra1n/gea/releases/download/v0.2.2/gea-v0.2.2-$(uname -m | sed 's/arm64/aarch64/;s/amd64/x86_64/')-$(grep -o 'unknown-linux-[a-z]*\|apple-darwin' "$out" | head -1).tar.gz"

run "gea: GEA_VERSION accepts the new tag spelling gea-v0.2.2" mixed.json gea/install.sh GEA_VERSION=gea-v0.2.2
expect_version gea 0.2.2

run "gea: with no gea release in this repository yet, latest is the last standalone release" nogea.json gea/install.sh
expect_version gea 0.2.2

# gea-v9.9.9 does not exist, so the install fails; what is asserted is where it looked.
run "gea: latest resolves the newest gea-v* tag and downloads from this repository" mixed.json gea/install.sh
if grep -q '^https://github.com/perfectra1n/fjo/releases/download/gea-v9.9.9/gea-v9.9.9-.*\.tar\.gz$' "$log"; then
    ok
else
    fail "never requested gea-v9.9.9 from perfectra1n/fjo"
fi

# `curl ... | bash` is how many people run an installer, and bash outside POSIX mode does
# not carry `set -e` into a command substitution. A failed listing must stop the install
# there too, and must never be mistaken for "no release yet" and install an old version.
expect_refused() {
    if [ "$rc" -ne 0 ] && [ ! -e "$dir/$1" ] && grep -q 'could not list the releases' "$out"; then
        ok
    else
        fail "wanted a refusal naming the release listing, got exit $rc"
    fi
}
if command -v bash >/dev/null 2>&1; then
    RUN_SHELL=bash
    run "gea: a failed release listing stops the install under bash" api-down.json gea/install.sh
    expect_refused gea
    run "fjo: a failed release listing stops the install under bash" api-down.json install.sh
    expect_refused fjo
    unset RUN_SHELL
fi
run "gea: a failed release listing stops the install under sh" api-down.json gea/install.sh
expect_refused gea
run "fjo: a failed release listing stops the install under sh" api-down.json install.sh
expect_refused fjo

if [ "$fails" -ne 0 ]; then
    printf '%s installer test(s) failed\n' "$fails"
    exit 1
fi
printf 'all installer tests passed\n'
