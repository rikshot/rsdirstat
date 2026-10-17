#!/usr/bin/env bash
# Time rsdirstat-cli scanning directories at several thread counts, with hyperfine. Results go to
# target/bench/<label>/: hyperfine's JSON per directory (mean, user and system time of every run),
# the binary that was measured, and a note of the machine and settings, so runs from different
# commits or machines can be compared afterwards.
#
#   scripts/bench.sh [options] DIR...
#
#   --threads LIST  thread counts to try, comma-separated (default: 1, 2, 4, ... and the CPU count)
#   --cold          empty the OS file cache before every run: `sudo sysctl vm.drop_caches=3` on
#                   Linux, `sudo purge` on macOS, each of which must be allowed without a password
#   --runs N        runs per thread count (default: hyperfine's, at least 10)
#   --label NAME    results directory (default: the commit, with -dirty if there are changes)
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

cpus=$(getconf _NPROCESSORS_ONLN)
threads=
cold=
runs=()
label=$(git describe --always --dirty --exclude '*')
while [[ $# -gt 0 ]]; do
    case $1 in
        --threads) threads=$2; shift 2 ;;
        --cold) cold=1; shift ;;
        --runs) runs=(--runs "$2"); shift 2 ;;
        --label) label=$2; shift 2 ;;
        -*) echo "unknown option $1" >&2; exit 1 ;;
        *) break ;;
    esac
done
if [[ $# -eq 0 ]]; then
    echo "usage: scripts/bench.sh [--threads LIST] [--cold] [--runs N] [--label NAME] DIR..." >&2
    exit 1
fi
if [[ -z $threads ]]; then
    threads=1
    for ((n = 2; n < cpus; n *= 2)); do threads+=",$n"; done
    ((cpus > 1)) && threads+=",$cpus"
fi

if [[ -n $cold ]]; then
    case $(uname -s) in
        Linux) drop_cache='sudo -n sysctl -q vm.drop_caches=3' ;;
        Darwin) drop_cache='sudo -n purge' ;;
        *) echo "--cold isn't supported on $(uname -s)" >&2; exit 1 ;;
    esac
    # Fail now rather than in the middle of a run.
    $drop_cache
    warm=(--prepare "$drop_cache")
else
    # One untimed run first, so every timed one finds the same things cached.
    warm=(--warmup 1)
fi

out=target/bench/$label
mkdir -p "$out"
cargo build --release --bin rsdirstat-cli
cp target/release/rsdirstat-cli "$out/"
{
    echo "commit: $(git describe --always --dirty --exclude '*')"
    echo "date: $(date -u +%FT%TZ)"
    echo "system: $(uname -srm)"
    echo "cpus: $cpus"
    echo "cache: $([[ -n $cold ]] && echo cold || echo warm)"
} >"$out/env.txt"

# Without a shell (-N), hyperfine splits commands into words itself, like a POSIX shell would.
# With sed rather than ${1//...}, whose quote handling differs in macOS's bash 3.2.
quote() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
for dir in "$@"; do
    name=$(printf '%s' "$dir" | tr -c 'A-Za-z0-9._-' _)
    # `${runs[@]+...}`: bash before 4.4 calls an empty array unbound under `set -u`.
    hyperfine -N "${warm[@]}" ${runs[@]+"${runs[@]}"} \
        --parameter-list threads "$threads" \
        --command-name "{threads} threads" \
        --export-json "$out/$name.json" \
        "$(quote "$out/rsdirstat-cli") --top 1 --threads {threads} $(quote "$dir")"
done

echo "Results in $out/"
