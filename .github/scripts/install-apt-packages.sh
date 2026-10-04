#!/usr/bin/env bash

set -euo pipefail

if [[ $# -eq 0 ]]; then
  echo "usage: $0 PACKAGE..." >&2
  exit 2
fi

readonly max_attempts=3
readonly command_timeout=5m
readonly -a apt_options=(
  -o Acquire::Retries=3
  -o Acquire::http::Timeout=30
  -o Acquire::https::Timeout=30
)

run_apt() {
  local attempt
  local status

  for ((attempt = 1; attempt <= max_attempts; attempt++)); do
    if sudo timeout --signal=TERM --kill-after=30s "$command_timeout" \
      env DEBIAN_FRONTEND=noninteractive \
      apt-get "${apt_options[@]}" "$@"; then
      return 0
    else
      status=$?
    fi
    if ((attempt == max_attempts)); then
      echo "apt-get $1 failed after $attempt bounded attempts" >&2
      return "$status"
    fi
    echo "apt-get $1 failed with status $status; retrying" >&2
    sleep "$((attempt * 10))"
  done
}

run_apt update -y
run_apt install -y --no-install-recommends "$@"
