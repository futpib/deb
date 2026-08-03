#!/usr/bin/env bash

set -euo pipefail

mkdir -p cef-runtime
cp -asn /usr/lib/cef/. cef-runtime
cp support/cef-archive.json cef-runtime/archive.json

echo "Staged Arch's CEF runtime in cef-runtime/"
