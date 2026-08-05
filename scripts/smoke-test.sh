#!/usr/bin/env bash

set -euo pipefail

script_directory=$(dirname "${BASH_SOURCE[0]}")
pushd "$script_directory/.." >/dev/null
project_root=$PWD
popd >/dev/null
build_rust=1

case "${1:-}" in
  "") ;;
  --no-build) build_rust=0 ;;
  *)
    echo "Usage: $0 [--no-build]" >&2
    exit 2
    ;;
esac

for command in timeout xvfb-run rg; do
  if ! command -v "$command" >/dev/null; then
    echo "Required smoke-test command is unavailable: $command" >&2
    exit 1
  fi
done

if ((build_rust)); then
  "$script_directory/stage-firefox-cef-rust.sh"
fi

if [[ ! -x "$project_root/target/debug/deb" ]]; then
  echo "deb is not built; omit --no-build or run cargo build --workspace" >&2
  exit 1
fi
if [[ ! -f "$project_root/target/debug/firefox-cef-runtime/libcef.so" ]]; then
  echo "Firefox CEF adapter is not staged" >&2
  exit 1
fi

test_root=$(mktemp -d "${TMPDIR:-/tmp}/deb-smoke.XXXXXX")
cleanup() {
  if [[ -n "$test_root" && -d "$test_root" && "$test_root" == */deb-smoke.* ]]; then
    rm -rf -- "$test_root"
  fi
}
trap cleanup EXIT

log_file="$test_root/smoke.log"
SECONDS=0
set +e
timeout --signal=TERM 30s \
  xvfb-run -a -s "-screen 0 1440x900x24" \
  env \
  XDG_CONFIG_HOME="$test_root/config" \
  XDG_DATA_HOME="$test_root/data" \
  XDG_CACHE_HOME="$test_root/cache" \
  LIBGL_ALWAYS_SOFTWARE=1 \
  DEB_URL=about:blank \
  DEB_SMOKE_NAVIGATE_URL=deb://new-tab/ \
  DEB_AUTOMATED_SMOKE_TEST=1 \
  "$project_root/target/debug/deb" >"$log_file" 2>&1
status=$?
set -e

if ((status != 0)); then
  echo "Browser smoke test failed with status $status" >&2
  rg -n "." "$log_file" >&2
  exit "$status"
fi

rg '^deb-smoke:' "$log_file"

required_profile_files=(
  "$test_root/config/deb/profiles.json"
  "$test_root/data/deb/profiles/default/cookies.sqlite3"
  "$test_root/data/deb/profiles/default/chromium/Default/Preferences"
  "$test_root/data/deb/profiles/default/firefox/prefs.js"
  "$test_root/cache/deb/profiles/default/chromium/Default/Cache/Cache_Data/index"
  "$test_root/cache/deb/profiles/default/firefox/cache2"
)
for required_path in "${required_profile_files[@]}"; do
  if [[ ! -e "$required_path" ]]; then
    echo "Browser smoke test did not create $required_path" >&2
    exit 1
  fi
done

echo "Browser smoke test passed in ${SECONDS}s"
