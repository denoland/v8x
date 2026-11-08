#!/usr/bin/env bash
# NOTE: build.rs carries a pure-Rust port of this script (fn setup_vendor and
# friends) so `cargo build` works without bash, notably on Windows. This script
# stays for manual/CI use — keep the two in sync when changing patch semantics.
#
# Init the pinned vendor submodules and apply our patches on top. Every vendored
# dependency lives in vendor/<name> at an exact upstream commit (see .gitmodules /
# the gitlinks); our edits ship as patches/<name>-NN-*.patch applied to the
# submodule working tree — never committed into the vendored source.
#
#   vendor/rusty_v8     denoland/rusty_v8 @ v158.0.0 — the Rust API surface this
#                       crate IS. src/lib.rs #[path]-includes its modules; our
#                       shims (src/jsc/, src/quickjs/) replace its C++ binding.cc
#                       by defining the same ~570 v8__* C-ABI over JSC / QuickJS.
#                       2 patches: EscapeSlot (Local==JSValueRef), serializer cap.
#   vendor/quickjs-ng   quickjs-ng @ 034f2ab (master, post-v0.15.1)  (QuickJS backend)
#   vendor/wamr         wasm-micro-runtime @ 26756a5c   (WebAssembly for QuickJS)
#
# Bumping a pin: move the submodule to the new commit, re-run this; if a patch
# rejects, upstream touched a patched file — reconcile by hand. New v8__* that
# upstream declares surface as undefined-symbol link errors on the next build.
#
# Usage: setup_vendor.sh [rusty_v8|quickjs]
#   rusty_v8  (default) only the Rust API surface — enough for the JSC backend
#   quickjs             also the QuickJS + WAMR engine submodules
set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-rusty_v8}"

# Apply every patches/<prefix>-NN-*.patch onto a submodule, in numeric order.
# Idempotent: a patch that already reverse-applies is skipped.
patch_already_applied() {
  local sub="$1" patch="$2"
  local out
  out=$(patch --batch --dry-run --forward -p1 -d "$sub" < "$patch" 2>&1 || true)
  [[ "$out" == *"previously applied"* ]]
}

apply_series() {
  local sub="$1" prefix="$2"
  local stamp_dir="$sub/.v8x-patches"
  if [ ! -e "$sub/.git" ]; then
    if ! git -c "submodule.$sub.update=checkout" submodule update --init "$sub"; then
      # Cargo git checkouts may be restored with the source tree present but
      # detached from stale submodule metadata. Never clean a normal checkout.
      [ -f .cargo-ok ] || return 1
      echo "repairing stale Cargo submodule checkout: $sub"
      local cleared=false
      for _ in {1..10}; do
        rm -rf -- "$sub" ".git/modules/$sub"
        if [ ! -e "$sub" ] && [ ! -e ".git/modules/$sub" ]; then
          cleared=true
          break
        fi
        sleep 1
      done
      [ "$cleared" = true ] || return 1
      local initialized=false
      for _ in 1 2 3; do
        if git -c "submodule.$sub.update=checkout" submodule update --init "$sub"; then
          initialized=true
          break
        fi
        rm -f -- ".git/modules/$sub/index.lock"
        sleep 1
      done
      [ "$initialized" = true ] || return 1
      git -c core.autocrlf=false -C "$sub" reset --hard HEAD
      git -C "$sub" clean -ffdx
    fi
  fi
  mkdir -p "$stamp_dir"
  while IFS= read -r p; do
    local stamp="$stamp_dir/$(basename "$p")"
    local checksum
    [ -e "$p" ] || continue
    checksum=$(cksum < "$p")
    if [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$checksum" ]; then
      continue
    fi
    if ! git -C "$sub" apply --reverse --check "../../$p" 2>/dev/null; then
      if ! git -C "$sub" apply "../../$p"; then
        # patch(1) silently SKIPS git binary deltas while exiting 0 on the
        # text hunks — never fall back to it for those (half-applied patch).
        if grep -q "GIT binary patch" "$p"; then
          return 1
        fi
        if patch --batch --dry-run --forward -p1 -d "$sub" < "$p" \
             >/dev/null 2>&1; then
          patch --batch --forward -p1 -d "$sub" < "$p"
        elif patch_already_applied "$sub" "$p"; then
          echo "warn: $p may already be applied"
        else
          return 1
        fi
      fi
    fi
    printf '%s\n' "$checksum" > "$stamp"
  done < <(printf '%s\n' patches/"$prefix"-[0-9]*.patch | sort -V)
}

# rusty_v8 is always needed — it's the crate's own source, used by both backends.
apply_series vendor/rusty_v8 rusty_v8

# rusty_v8's tests embed third_party/icu/common/icudtl.dat at compile time. Keep
# the real pinned Chromium ICU data available, but do not commit the 10 MiB blob
# in this repo (the path is ignored at the top level).
if [ ! -s vendor/rusty_v8/third_party/icu/common/icudtl.dat ] || \
   [ "$(wc -c < vendor/rusty_v8/third_party/icu/common/icudtl.dat 2>/dev/null || echo 0)" -lt 1048576 ]; then
  rm -rf vendor/rusty_v8/third_party/icu
  git -C vendor/rusty_v8 submodule update --init third_party/icu
fi

if [ "$MODE" = quickjs ]; then
  apply_series vendor/quickjs-ng quickjs
  apply_series vendor/wamr       wamr
  # WAMR's CMake driver has no upstream counterpart; copy it in.
  mkdir -p vendor/wamr/v82jsc
  cp patches/wamr-v82jsc-CMakeLists.txt vendor/wamr/v82jsc/CMakeLists.txt
fi

echo "vendor setup done ($MODE)"
