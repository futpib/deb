#!/usr/bin/env bash

set -euo pipefail

script_directory=$(dirname "${BASH_SOURCE[0]}")
pushd "$script_directory/.." >/dev/null
project_root=$PWD
popd >/dev/null
build_rust=1
require_touch=0

for argument in "$@"; do
  case "$argument" in
  --no-build) build_rust=0 ;;
  --require-touch) require_touch=1 ;;
  *)
    echo "Usage: $0 [--no-build] [--require-touch]" >&2
    exit 2
    ;;
  esac
done

for command in glxinfo python3 timeout rg xdotool xdpyinfo; do
  if ! command -v "$command" >/dev/null; then
    echo "Required smoke-test command is unavailable: $command" >&2
    exit 1
  fi
done

if [[ -z "${DISPLAY:-}" ]]; then
  echo "The DMA-BUF smoke test requires a real X11 DISPLAY" >&2
  exit 1
fi

display_info=$(xdpyinfo)
if rg -q '^    XWAYLAND$' <<<"$display_info"; then
  echo "The E2E smoke test requires native Xorg; DISPLAY=$DISPLAY is XWayland" >&2
  exit 1
fi
if ! rg -q '^    XTEST$' <<<"$display_info"; then
  echo "The E2E smoke test requires the XTEST extension" >&2
  exit 1
fi
if ((require_touch)) && [[ ! -w /dev/uinput ]]; then
  echo "Raw touch E2E requires write access to /dev/uinput" >&2
  exit 1
fi
gl_info=$(glxinfo -B)
if rg -qi 'Accelerated: no|llvmpipe|softpipe|Software Rasterizer' <<<"$gl_info"; then
  echo "The DMA-BUF smoke test requires hardware-accelerated OpenGL" >&2
  exit 1
fi

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
test_passed=0
cleanup() {
  if ((test_passed)) && [[ -n "$test_root" && -d "$test_root" && "$test_root" == */deb-smoke.* ]]; then
    rm -rf -- "$test_root"
  fi
}
trap cleanup EXIT

app_log="$test_root/deb.log"
driver_log="$test_root/e2e.log"
artifacts="$test_root/artifacts"
touch_arguments=()
if ((require_touch)); then
  touch_arguments+=(--require-touch)
fi
SECONDS=0
set +e
timeout --signal=TERM --kill-after=20s 180s \
  env \
  XDG_CONFIG_HOME="$test_root/config" \
  XDG_DATA_HOME="$test_root/data" \
  XDG_CACHE_HOME="$test_root/cache" \
  QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 \
  DEB_URL=deb://new-tab/#deb-smoke \
  python3 "$script_directory/e2e-smoke.py" \
    --binary "$project_root/target/debug/deb" \
    --log "$app_log" \
    --artifacts "$artifacts" \
    "${touch_arguments[@]}" >"$driver_log" 2>&1
status=$?

if ((status != 0)); then
  echo "Browser smoke test failed with status $status" >&2
  rg -n "." "$driver_log" >&2
  rg -n "." "$app_log" >&2
  echo "Failure artifacts retained at $test_root" >&2
  exit "$status"
fi
set -e

rg '^deb-smoke:' "$driver_log"

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

test_passed=1
echo "Browser smoke test passed in ${SECONDS}s"
