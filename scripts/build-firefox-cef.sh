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

pushd "$firefox_source" >/dev/null
pinned_commit=$(git rev-parse HEAD)
popd >/dev/null

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
  git apply "$patch_file"
fi
popd >/dev/null

cp -a "$overlay_directory/." "$build_source/"

pushd "$build_source" >/dev/null
env MOZCONFIG="$project_root/firefox.mozconfig" ./mach build
popd >/dev/null

pushd "$project_root" >/dev/null
cargo build --workspace
popd >/dev/null

if [[ "$runtime_directory" != "$project_root/target/debug/firefox-cef-runtime" ]]; then
  echo "Refusing to replace unexpected runtime path: $runtime_directory" >&2
  exit 1
fi
rm -rf "$runtime_directory"
mkdir -p "$runtime_directory"
cp -a "$object_directory/dist/bin/." "$runtime_directory/"
mkdir -p "$runtime_directory/browser/defaults/preferences"
cp "$project_root/firefox-runtime/firefox-cef.ini" \
  "$runtime_directory/browser/firefox-cef.ini"
cp "$project_root/firefox-runtime/firefox-cef.js" \
  "$runtime_directory/browser/defaults/preferences/zz-firefox-cef.js"
cp "$project_root/target/debug/cef-renderer" "$runtime_directory/cef-renderer"
cp "$project_root/target/debug/libfirefox_cef.so" "$runtime_directory/libcef.so"

echo "Staged FirefoxCEF in $runtime_directory"
