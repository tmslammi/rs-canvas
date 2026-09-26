#!/usr/bin/env bash
# Stage the static app for deployment. A file copy, not a build step.
set -euo pipefail
cd "$(dirname "$0")/.."

rm -rf dist
mkdir -p dist
cp index.html dist/
cp -R src fonts dist/
rm -f dist/src/*.md
