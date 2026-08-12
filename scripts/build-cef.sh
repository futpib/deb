#!/usr/bin/env bash

set -euo pipefail

script_directory=$(dirname "${BASH_SOURCE[0]}")
pushd "$script_directory/.." >/dev/null
project_root=$PWD
popd >/dev/null

cef_source="$project_root/cef"
build_root="$project_root/target/cef-build"
automation_checkout="$build_root/cef"
chromium_source="$build_root/chromium/src"
chromium_cef_source="$build_root/chromium/src/cef"
patch_file="$project_root/cef-patches/0001-partitioned-cookie-observer.patch"
extension_patch_file="$project_root/cef-patches/0002-windowless-extension-tabs.patch"
chromium_extension_patch_file="$project_root/cef-patches/chromium-windowless-extension-tabs.patch"
applied_patch="$build_root/applied-cookie-observer.patch"
applied_extension_patch="$build_root/applied-windowless-extension-tabs.patch"
applied_chromium_extension_patch="$build_root/applied-chromium-windowless-extension-tabs.patch"
runtime_directory="$project_root/cef-runtime"
force_build=0
run_smoke=1

for argument in "$@"; do
  case "$argument" in
    --force) force_build=1 ;;
    --no-smoke) run_smoke=0 ;;
    *)
      echo "Usage: $0 [--force] [--no-smoke]" >&2
      exit 2
      ;;
  esac
done

pushd "$cef_source" >/dev/null
pinned_commit=$(git rev-parse HEAD)
popd >/dev/null
gn_defines="is_official_build=true use_sysroot=true symbol_level=1 is_cfi=false use_thin_lto=false ozone_platform_x11=true ozone_platform_wayland=false"
patch_hash=$(git hash-object "$patch_file")
extension_patch_hash=$(git hash-object "$extension_patch_file")
chromium_extension_patch_hash=$(git hash-object "$chromium_extension_patch_file")
script_hash=$(git hash-object "$project_root/scripts/build-cef.sh")
input_manifest="cef=$pinned_commit"$'\n'"patch=$patch_hash"$'\n'"extension_patch=$extension_patch_hash"$'\n'"chromium_extension_patch=$chromium_extension_patch_hash"$'\n'"script=$script_hash"$'\n'"gn=$gn_defines"
input_stamp="$build_root/inputs"

needs_build=$force_build
if [[ ! -f "$input_stamp" || ! -f "$runtime_directory/libcef.so" ]]; then
  needs_build=1
elif [[ "$(<"$input_stamp")" != "$input_manifest" ]]; then
  needs_build=1
fi

mkdir -p "$build_root"
if [[ ! -e "$automation_checkout/.git" ]]; then
  git clone --local "$cef_source" "$automation_checkout"
fi

pushd "$automation_checkout" >/dev/null
checkout_commit=$(git rev-parse HEAD)
if [[ "$checkout_commit" != "$pinned_commit" ]]; then
  echo "CEF build checkout is at $checkout_commit, expected $pinned_commit" >&2
  exit 1
fi
if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "CEF automation checkout has local changes: $automation_checkout" >&2
  exit 1
fi
popd >/dev/null

automation=(
  python3 "$automation_checkout/tools/automate/automate-git.py"
  --download-dir="$build_root"
  --branch=7871
  --checkout="$pinned_commit"
  --x64-build
  --force-config
  --with-pgo-profiles
)

if [[ ! -e "$chromium_cef_source/.git" ]]; then
  env CEF_USE_GN=1 GN_DEFINES="$gn_defines" \
    "${automation[@]}" \
    --no-cef-update \
    --no-chromium-history \
    --no-build \
    --no-distrib
fi

if ((needs_build)); then
  previous_chromium_extension_patch="$applied_chromium_extension_patch"
  if [[ ! -f "$previous_chromium_extension_patch" ]]; then
    previous_chromium_extension_patch="$chromium_extension_patch_file"
  fi
  pushd "$chromium_source" >/dev/null
  if git apply -p0 --reverse --check "$previous_chromium_extension_patch"; then
    git apply -p0 --reverse "$previous_chromium_extension_patch"
  elif ! git apply -p0 --check "$previous_chromium_extension_patch"; then
    echo "Cannot determine the previously applied Chromium extension patch state" >&2
    exit 1
  fi
  popd >/dev/null

  pushd "$chromium_cef_source" >/dev/null
  source_commit=$(git rev-parse HEAD)
  if [[ "$source_commit" != "$pinned_commit" ]]; then
    echo "Chromium CEF source is at $source_commit, expected $pinned_commit" >&2
    exit 1
  fi

  if [[ -f "$applied_extension_patch" ]]; then
    if ! git apply --reverse --check "$applied_extension_patch"; then
      echo "Cannot reverse the previously applied CEF extension patch" >&2
      exit 1
    fi
    git apply --reverse "$applied_extension_patch"
  fi
  if [[ -f "$applied_patch" ]]; then
    if ! git apply --reverse --check "$applied_patch"; then
      echo "Cannot reverse the previously applied CEF patch" >&2
      exit 1
    fi
    git apply --reverse "$applied_patch"
  fi
  if ! git diff --quiet; then
    echo "Chromium CEF source has unexpected tracked changes" >&2
    exit 1
  fi
  git apply --check "$patch_file"
  git apply "$patch_file"
  git apply --check "$extension_patch_file"
  git apply "$extension_patch_file"
  cp "$patch_file" "$applied_patch"
  cp "$extension_patch_file" "$applied_extension_patch"
  cp "$chromium_extension_patch_file" "$applied_chromium_extension_patch"
  cp "$chromium_extension_patch_file" \
    "$chromium_cef_source/patch/patches/deb_windowless_extension_tabs.patch"
  python3 tools/translator.py --root-dir .
  popd >/dev/null

  env CEF_USE_GN=1 GN_DEFINES="$gn_defines" \
    "${automation[@]}" \
    --no-update \
    --force-build \
    --force-distrib \
    --build-target=libcef \
    --no-debug-build \
    --minimal-distrib-only \
    --no-distrib-symbols \
    --no-distrib-docs \
    --no-distrib-archive

  shopt -s nullglob
  distributions=("$chromium_cef_source"/binary_distrib/*_linux64_minimal)
  if ((${#distributions[@]} != 1)); then
    echo "Expected one Linux minimal CEF distribution, found ${#distributions[@]}" >&2
    exit 1
  fi
  distribution=${distributions[0]}

  if [[ "$runtime_directory" != "$project_root/cef-runtime" ]]; then
    echo "Refusing to replace unexpected runtime path: $runtime_directory" >&2
    exit 1
  fi
  rm -rf "$runtime_directory"
  mkdir -p "$runtime_directory"
  cp -a "$distribution/." "$runtime_directory/"
  cp -a "$distribution/Release/." "$runtime_directory/"
  cp -a "$distribution/Resources/." "$runtime_directory/"
  cp "$project_root/support/cef-archive.json" "$runtime_directory/archive.json"
  printf '%s\n' "$input_manifest" >"$input_stamp"
else
  echo "CEF source inputs are unchanged; skipping the Chromium build"
fi

python3 "$project_root/scripts/check-cef-api-hash.py" \
  "$chromium_cef_source" "$project_root/cef-cookie/src/lib.rs"

pushd "$project_root" >/dev/null
if ((needs_build)); then
  cargo clean -p cef-dll-sys
fi
cargo build --workspace
popd >/dev/null

echo "Staged patched Chromium CEF in $runtime_directory"

if ((run_smoke)); then
  "$script_directory/smoke-test.sh" --no-build
fi
