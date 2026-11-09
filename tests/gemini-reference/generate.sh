#!/usr/bin/env sh
# Regenerate the CLIProxyAPI golden files read by tests/gemini_differential.rs.
# Needs git and Go. REF pins the commit the committed goldens came from.
set -eu
REF="${REF:-681ce41b6e44c45b15b79378b003909d6ba29e68}"
here="$(cd "$(dirname "$0")" && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
git clone --quiet https://github.com/router-for-me/CLIProxyAPI "$work/ref"
git -C "$work/ref" checkout --quiet "$REF"
mkdir -p "$work/ref/cmd/lpref"
cp "$here/main.go" "$work/ref/cmd/lpref/main.go"
(cd "$work/ref" && go run ./cmd/lpref "$here/../fixtures/gemini")
