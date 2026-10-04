#!/usr/bin/env bash
set -euo pipefail

cd "${GITHUB_WORKSPACE:?}"
case "${VOICE_TARGET:?}" in
  x86_64-unknown-linux-gnu) prefix=linux_x86_64; expected_host=Linux-x86_64 ;;
  x86_64-apple-darwin) prefix=macos_x86_64; expected_host=Darwin-x86_64 ;;
  *) echo "Unsupported manual voice target: $VOICE_TARGET" >&2; exit 1 ;;
esac
if [[ "$(uname -s)-$(uname -m)" != "$expected_host" ]]; then
  echo "Manual voice inputs must be built on their native target" >&2
  exit 1
fi
targets=(//third_party/voice:native_runtime)
case "${VOICE_PURPOSE:?}" in
  cargo) targets+=(//third_party/voice:native_sdk //third_party/voice:native_link) ;;
  package) targets+=(//codex-rs/voice-host:codex-voice-host) ;;
  *) echo "Unsupported manual voice purpose: $VOICE_PURPOSE" >&2; exit 1 ;;
esac
# All sources, native tools, and inspection rules come from the pinned target.
bazel build -c opt "${targets[@]}"
root="${CARGO_TARGET_DIR:?}/voice-native"
mkdir "$root"
cp -RL "bazel-bin/third_party/voice/native_runtime_${prefix}" "$root/runtime"
PYTHONPATH=third_party/voice python3 - "$root/runtime" "$VOICE_TARGET" "$(git rev-parse HEAD)" <<'PY'
import json
from pathlib import Path
import sys
from package_runtime import runtime_files

runtime = Path(sys.argv[1]).resolve(strict=True)
runtime_files(runtime, sys.argv[2])
if json.loads((runtime / "runtime.json").read_text())["sourceCommit"] != sys.argv[3]:
    raise ValueError("runtime and application build commits differ")
PY
echo "root=$root" >> "${GITHUB_OUTPUT:?}"
if [[ "$VOICE_PURPOSE" == package ]]; then
  if [[ "$(bazel-bin/codex-rs/voice-host/codex-voice-host --build-commit)" != "$(git rev-parse HEAD)" ]]; then
    echo "Voice helper does not match the application build commit" >&2
    exit 1
  fi
  install -m 0755 bazel-bin/codex-rs/voice-host/codex-voice-host "$root/codex-voice-host"
  exit 0
fi

cp -RL "bazel-bin/third_party/voice/native_runtime_${prefix}_sdk" "$root/sdk"
cp -RL "bazel-bin/third_party/voice/native_link_${prefix}" "$root/link"
install -m 0755 .github/scripts/manual-voice-pkg-config.sh "$root/pkg-config"
{
  echo "CODEX_VOICE_SDK_ROOT=$root/sdk"
  echo "CODEX_VOICE_PKG_CONFIG_REAL=$(command -v pkg-config)"
  echo "PKG_CONFIG=$root/pkg-config"
  echo "CODEX_TEST_VOICE_RUNTIME=$root/runtime"
  echo "STABLE_GIT_COMMIT=$(git rev-parse HEAD)"
  for key in GLIB_2_0 GOBJECT_2_0 GIO_2_0 GSTREAMER_1_0 GSTREAMER_BASE_1_0 GSTREAMER_APP_1_0 GSTREAMER_AUDIO_1_0; do
    echo "SYSTEM_DEPS_${key}_SEARCH_NATIVE=$root/link/lib"
    if [[ "$key" == GSTREAMER_* ]]; then
      echo "SYSTEM_DEPS_${key}_LDFLAGS="
    fi
  done
  echo "LD_LIBRARY_PATH=$root/link/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
} >> "${GITHUB_ENV:?}"
