#!/usr/bin/env bash

set -euo pipefail

script_directory=$(dirname "${BASH_SOURCE[0]}")
pushd "$script_directory/.." >/dev/null
project_root=$PWD
popd >/dev/null
firefox_source="$project_root/firefox"
build_source="$project_root/target/firefox-source"
object_directory="$project_root/target/firefox-obj"
runtime_directory="$project_root/target/debug/firefox-cef-runtime"
patch_file="$project_root/firefox-patches/0001-firefox-cef-runtime.patch"
overlay_directory="$project_root/firefox-overlay"
input_stamp="$project_root/target/firefox-cef-inputs"
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

pushd "$firefox_source" >/dev/null
pinned_commit=$(git rev-parse HEAD)
popd >/dev/null

shopt -s globstar nullglob
firefox_inputs=(
  "$project_root/firefox.mozconfig"
  "$patch_file"
  "$project_root/internal-pages/new-tab.html"
  "$overlay_directory"/**/*
)
input_manifest="firefox=$pinned_commit"
for input in "${firefox_inputs[@]}"; do
  if [[ -f "$input" ]]; then
    input_hash=$(git hash-object "$input")
    relative_input=${input#"$project_root"/}
    input_manifest+=$'\n'"$relative_input=$input_hash"
  fi
done

needs_firefox_build=$force_build
if [[ ! -f "$runtime_directory/libxul.so" || ! -f "$input_stamp" ]]; then
  needs_firefox_build=1
else
  staged_inputs=$(<"$input_stamp")
  if [[ "$staged_inputs" != "$input_manifest" ]]; then
    needs_firefox_build=1
  fi
fi

if ((needs_firefox_build)); then
  if [[ ! -e "$build_source/.git" ]]; then
    mkdir -p "$project_root/target"
    pushd "$firefox_source" >/dev/null
    git worktree add --detach "$build_source" "$pinned_commit"
    popd >/dev/null
  fi

  pushd "$build_source" >/dev/null
  build_commit=$(git rev-parse HEAD)
  if [[ "$build_commit" != "$pinned_commit" ]]; then
    echo "Firefox build worktree is at $build_commit, expected $pinned_commit" >&2
    exit 1
  fi
  if ! git apply --reverse --check "$patch_file" >/dev/null 2>&1; then
    if ! git apply --check "$patch_file" >/dev/null 2>&1; then
      git restore -- \
        browser/components/moz.build \
        browser/moz.configure \
        gfx/webrender_bindings/RenderCompositor.cpp \
        toolkit/library/libxul.symbols \
        xpfe/appshell/AppWindow.cpp
    fi
    git apply "$patch_file"
  fi
  popd >/dev/null

  cp -a "$overlay_directory/." "$build_source/"
  cp "$project_root/internal-pages/new-tab.html" \
    "$build_source/browser/components/firefoxcef/content/deb-new-tab.html"

  pushd "$build_source" >/dev/null
  env MOZCONFIG="$project_root/firefox.mozconfig" ./mach build
  popd >/dev/null
else
  echo "Firefox source inputs are unchanged; skipping the Gecko build"
fi

pushd "$project_root" >/dev/null
cargo build --workspace
popd >/dev/null

if ((needs_firefox_build)); then
  if [[ "$runtime_directory" != "$project_root/target/debug/firefox-cef-runtime" ]]; then
    echo "Refusing to replace unexpected runtime path: $runtime_directory" >&2
    exit 1
  fi
  rm -rf "$runtime_directory"
  mkdir -p "$runtime_directory"
  cp -aL "$object_directory/dist/bin/." "$runtime_directory/"

  cxx_line=$(rg '^_CXX = ' "$object_directory/config/autoconf.mk")
  cxx=${cxx_line#_CXX = }
  if [[ ! -x "$cxx" ]]; then
    echo "Firefox C++ compiler is unavailable: $cxx" >&2
    exit 1
  fi

  mozglue_objects=()
  collect_mozglue=0
  while IFS= read -r object; do
    if [[ "$object" == "../../mozglue/build/dummy.o" ]]; then
      collect_mozglue=1
    fi
    if ((collect_mozglue)); then
      mozglue_objects+=("$object")
    fi
  done <"$object_directory/browser/app/firefox.list"
  if ((${#mozglue_objects[@]} == 0)); then
    echo "Firefox launcher did not provide its mozglue object list" >&2
    exit 1
  fi

  pushd "$object_directory/browser/app" >/dev/null
  "$cxx" -shared -o "$runtime_directory/libmozglue-cef.so" \
    "${mozglue_objects[@]}" \
    ../../build/pure_virtual/libpure_virtual.a \
    -pthread -ldl -lm
  popd >/dev/null

  printf '%s\n' "$input_manifest" >"$input_stamp"
fi

"$script_directory/stage-firefox-cef-rust.sh" --no-build

echo "Staged FirefoxCEF in $runtime_directory"

if ((run_smoke)); then
  "$script_directory/smoke-test.sh" --no-build
fi
