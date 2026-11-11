#!/bin/sh
# Regenerates the TypeScript FlatBuffers bindings for FCast protocol v4.
#
# Usage (from anywhere):
#   FLATC=/path/to/flatc sh receivers/webos/scripts/gen-v4-flatbuffers.sh
#
# FLATC defaults to `flatc` on PATH and must be flatc 25.9.23, the version
# the committed bindings were generated with.
set -eu

FLATC="${FLATC:-flatc}"
EXPECTED_VERSION="flatc version 25.9.23"

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"

version="$("$FLATC" --version | tr -d '\r')"
if [ "$version" != "$EXPECTED_VERSION" ]; then
    echo "error: expected '$EXPECTED_VERSION' from '$FLATC', got '$version'" >&2
    exit 1
fi

"$FLATC" --ts -o receivers/common/web/v4/generated crates/fcast-protocol/flatbuffers/fcast.fbs

# flatc on Windows writes CRLF line endings; the committed bindings use LF.
find receivers/common/web/v4/generated -name '*.ts' -exec sed -i 's/\r$//' {} +
