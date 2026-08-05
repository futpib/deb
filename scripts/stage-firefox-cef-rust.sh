#!/usr/bin/env bash

set -euo pipefail

script_directory=$(dirname "${BASH_SOURCE[0]}")
pushd "$script_directory/.." >/dev/null
project_root=$PWD
popd >/dev/null
runtime_directory="$project_root/target/debug/firefox-cef-runtime"
build_rust=1

replace_file() {
  local source=$1
  local destination=$2
  local temporary="${destination}.new-$$"
  cp "$source" "$temporary"
  mv -f "$temporary" "$destination"
}

case "${1:-}" in
  "") ;;
  --no-build) build_rust=0 ;;
  *)
    echo "Usage: $0 [--no-build]" >&2
    exit 2
    ;;
esac

if ((build_rust)); then
  pushd "$project_root" >/dev/null
  cargo build --workspace
  popd >/dev/null
fi

if [[ ! -f "$runtime_directory/libxul.so" ]]; then
  echo "Firefox runtime is not staged; run scripts/build-firefox-cef.sh" >&2
  exit 1
fi

mkdir -p "$runtime_directory/browser/defaults/preferences"
replace_file "$project_root/firefox-runtime/firefox-cef.ini" \
  "$runtime_directory/browser/firefox-cef.ini"
replace_file "$project_root/firefox-runtime/firefox-cef.js" \
  "$runtime_directory/browser/defaults/preferences/zz-firefox-cef.js"
replace_file "$project_root/target/debug/cef-renderer" "$runtime_directory/cef-renderer"
replace_file "$project_root/target/debug/libfirefox_cef.so" "$runtime_directory/libcef.so"

echo "Staged current Rust helper and Firefox CEF adapter"
