#!/usr/bin/env bash
# Download the visual-test baselines recorded by the "Update visual baselines" workflow into
# crates/rsdirstat/tests/snapshots/{linux,macos,windows}/, replacing the committed ones, so they can
# be reviewed and committed.
#
#   scripts/download-visual-baselines.sh           # the latest successful run on the current branch
#   scripts/download-visual-baselines.sh <run-id>  # a specific run
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

run=${1:-}
if [[ -z $run ]]; then
    branch=$(git branch --show-current)
    run=$(gh run list --workflow visual-baselines.yml --branch "$branch" --status success --limit 1 \
        --json databaseId --jq '.[0].databaseId // empty')
    if [[ -z $run ]]; then
        echo "no successful \"Update visual baselines\" run on $branch;" \
            "start one with: gh workflow run visual-baselines.yml --ref $branch" >&2
        exit 1
    fi
fi

# All three first, so a download that fails leaves the committed baselines as they were.
downloads=$(mktemp -d)
trap 'rm -rf "$downloads"' EXIT
for os in linux macos windows; do
    gh run download "$run" --name "visual-baselines-$os" --dir "$downloads/$os"
done
for os in linux macos windows; do
    dir=crates/rsdirstat/tests/snapshots/$os
    rm -f "$dir"/*.png
    cp "$downloads/$os"/*.png "$dir"/
done

echo "Downloaded the baselines from run $run. Review them before committing:"
git status --short crates/rsdirstat/tests/snapshots
