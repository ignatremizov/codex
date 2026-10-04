#!/usr/bin/env bash
set -euo pipefail

# Only the native voice modules use the receipt-backed SDK. Other consumers
# (notably ALSA on Linux) retain their normal platform pkg-config environment.
for argument in "$@"; do
  case "$argument" in
    glib-2.0|gobject-2.0|gio-2.0|gmodule-*.0|gstreamer*.0|libffi|libpcre2-8|zlib)
      exec env \
        PKG_CONFIG_PATH= \
        PKG_CONFIG_LIBDIR="${CODEX_VOICE_SDK_ROOT:?}/lib/pkgconfig" \
        "${CODEX_VOICE_PKG_CONFIG_REAL:?}" --define-prefix "$@"
      ;;
  esac
done
exec "${CODEX_VOICE_PKG_CONFIG_REAL:?}" "$@"
