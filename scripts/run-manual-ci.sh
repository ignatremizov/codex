#!/usr/bin/env bash

set -euo pipefail

poll_seconds="${MANUAL_CI_POLL_SECONDS:-300}"
infra_retry_seconds="${MANUAL_CI_INFRA_RETRY_SECONDS:-600}"
repo=""
ref=""
verify_run=""
build_run=""

usage() {
  cat <<'EOF'
Usage: scripts/run-manual-ci.sh [options]

Dispatch Manual Verify, wait for workspace Clippy, then dispatch Manual Release
Build while the rest of Verify continues. The script monitors Verify plus the
Linux and macOS release jobs independently until the requested work succeeds,
and retries Verify when GitHub Actions fails before running repository steps.

Options:
  --repo OWNER/REPO   GitHub repository (default: repository for this checkout)
  --ref REF           Branch or ref to test (default: current branch)
  --verify-run ID     Monitor an existing Manual Verify run instead of dispatching
  --build-run ID      Monitor an existing Manual Release Build run after Clippy
  -h, --help          Show this help

Environment:
  MANUAL_CI_POLL_SECONDS          Status poll interval (default: 300)
  MANUAL_CI_INFRA_RETRY_SECONDS   Infrastructure retry delay (default: 600)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo)
      repo="$2"
      shift 2
      ;;
    --ref)
      ref="$2"
      shift 2
      ;;
    --verify-run)
      verify_run="$2"
      shift 2
      ;;
    --build-run)
      build_run="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

for command in gh git jq rg; do
  if ! command -v "$command" >/dev/null; then
    echo "Required command not found: $command" >&2
    exit 2
  fi
done

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

if [[ -z "$repo" ]]; then
  repo="$(gh repo view --json nameWithOwner --jq .nameWithOwner)"
fi
if [[ -z "$ref" ]]; then
  ref="$(git branch --show-current)"
fi
if [[ -z "$ref" ]]; then
  echo "Unable to infer a branch; pass --ref." >&2
  exit 2
fi

head_sha="$(gh api "repos/$repo/commits/$ref" --jq .sha)"
current_branch="$(git branch --show-current)"
if [[ "$ref" == "$current_branch" ]]; then
  local_head="$(git rev-parse HEAD)"
  if [[ "$local_head" != "$head_sha" ]]; then
    echo "Remote $repo:$ref is $head_sha, but local HEAD is $local_head; push first." >&2
    exit 2
  fi
fi
if [[ -n "$(git status --short)" ]]; then
  echo "Warning: the worktree is dirty; CI will test committed ref $ref at $head_sha." >&2
fi

run_url() {
  local run_id="$1"
  printf 'https://github.com/%s/actions/runs/%s' "$repo" "$run_id"
}

build_job_state() {
  local build_json="$1"
  local job_name="$2"
  local workflow_status="$3"
  local state

  state="$(
    jq -r \
      --arg job_name "$job_name" \
      '[.jobs[] | select(.name == $job_name)][0]
        | if . == null then "" else "\(.status)/\(.conclusion // "")" end' \
      <<<"$build_json"
  )"
  if [[ -n "$state" ]]; then
    printf '%s' "$state"
  elif [[ "$workflow_status" == "completed" ]]; then
    printf 'not-run'
  else
    printf 'waiting'
  fi
}

assert_run_head() {
  local run_id="$1"
  local kind="$2"
  local run_head
  run_head="$(gh api "repos/$repo/actions/runs/$run_id" --jq .head_sha)"
  if [[ "$run_head" != "$head_sha" ]]; then
    echo "$kind run $run_id targets $run_head, expected $head_sha." >&2
    return 1
  fi
}

dispatch_run() {
  local workflow="$1"
  local known_ids
  local output
  local run_id

  known_ids="$(
    gh run list \
      --repo "$repo" \
      --workflow "$workflow" \
      --branch "$ref" \
      --limit 30 \
      --json databaseId \
      | jq '[.[].databaseId]'
  )"
  if ! output="$(gh workflow run "$workflow" --repo "$repo" --ref "$ref" 2>&1)"; then
    echo "$output" >&2
    return 1
  fi

  run_id="$(sed -nE 's#^.*/actions/runs/([0-9]+).*$#\1#p' <<<"$output" | tail -n 1)"
  if [[ -n "$run_id" ]]; then
    printf '%s' "$run_id"
    return
  fi

  for _ in {1..30}; do
    run_id="$(
      gh run list \
        --repo "$repo" \
        --workflow "$workflow" \
        --branch "$ref" \
        --limit 30 \
        --json databaseId,headSha \
        | jq -r \
          --arg sha "$head_sha" \
          --argjson known "$known_ids" \
          '[.[] | .databaseId as $id | select(.headSha == $sha and ($known | index($id) | not))][0].databaseId // empty'
    )"
    if [[ -n "$run_id" ]]; then
      printf '%s' "$run_id"
      return
    fi
    sleep 2
  done

  echo "Workflow dispatch succeeded but the $workflow run ID could not be resolved." >&2
  return 1
}

latest_active_verify_run() {
  local excluded_run="$1"
  gh run list \
    --repo "$repo" \
    --workflow manual-verify.yml \
    --branch "$ref" \
    --limit 30 \
    --json databaseId,headSha,status \
    | jq -r \
      --arg sha "$head_sha" \
      --argjson excluded "$excluded_run" \
      '[.[]
        | select(
            .headSha == $sha
            and .databaseId != $excluded
            and (.status == "queued" or .status == "pending" or .status == "in_progress")
          )
      ][0].databaseId // empty'
}

dispatch_verify_with_retry() {
  local run_id
  while true; do
    if run_id="$(dispatch_run manual-verify.yml)"; then
      printf '%s' "$run_id"
      return
    fi
    echo "Verify dispatch failed; retrying in ${infra_retry_seconds}s." >&2
    sleep "$infra_retry_seconds"
  done
}

verify_infrastructure_failure() {
  local run_id="$1"
  local verify_json="$2"
  local step_count
  local failed_log

  step_count="$(jq '[.jobs[].steps[]] | length' <<<"$verify_json")"
  if [[ "$step_count" == 0 ]]; then
    return 0
  fi
  failed_log="$(gh run view "$run_id" --repo "$repo" --log-failed 2>&1 || true)"
  rg -q 'Service Unavailable|Failed to resolve action download info' <<<"$failed_log"
}

if [[ -n "$verify_run" ]]; then
  assert_run_head "$verify_run" "Verify"
else
  verify_run="$(dispatch_verify_with_retry)"
fi

while true; do
  echo "Monitoring Verify $verify_run: $(run_url "$verify_run")"
  last_clippy=""
  retry_verify=false

  while true; do
    if ! verify_json="$(
      gh run view "$verify_run" --repo "$repo" --json status,conclusion,jobs,url 2>/dev/null
    )"; then
      echo "Verify status query failed; retrying in ${poll_seconds}s."
      sleep "$poll_seconds"
      continue
    fi
    verify_status="$(jq -r .status <<<"$verify_json")"
    verify_conclusion="$(jq -r .conclusion <<<"$verify_json")"
    clippy_status="$(
      jq -r \
        '[.jobs[].steps[] | select(.name == "cargo clippy --workspace --tests")][0].status // "missing"' \
        <<<"$verify_json"
    )"
    clippy_conclusion="$(
      jq -r \
        '[.jobs[].steps[] | select(.name == "cargo clippy --workspace --tests")][0].conclusion // ""' \
        <<<"$verify_json"
    )"
    clippy_state="$clippy_status/$clippy_conclusion"
    if [[ "$clippy_state" != "$last_clippy" ]]; then
      echo "Clippy: $clippy_state"
      last_clippy="$clippy_state"
    fi

    if [[ "$clippy_status" == "completed" && "$clippy_conclusion" == "success" ]]; then
      break
    fi
    if [[ "$clippy_status" == "completed" ]]; then
      echo "Clippy failed: $(run_url "$verify_run")" >&2
      exit 20
    fi
    if [[ "$verify_status" == "completed" ]]; then
      replacement_run="$(latest_active_verify_run "$verify_run")"
      if [[ -n "$replacement_run" ]]; then
        echo "Verify $verify_run was superseded; adopting active run $replacement_run."
        verify_run="$replacement_run"
        retry_verify=true
      elif verify_infrastructure_failure "$verify_run" "$verify_json"; then
        echo "Verify $verify_run failed before repository checks; retrying in ${infra_retry_seconds}s."
        sleep "$infra_retry_seconds"
        verify_run="$(dispatch_verify_with_retry)"
        retry_verify=true
      else
        echo "Verify failed before Clippy: $(run_url "$verify_run")" >&2
        gh run view "$verify_run" --repo "$repo" --log-failed >&2 || true
        exit 21
      fi
      break
    fi
    sleep "$poll_seconds"
  done

  if [[ "$retry_verify" == false ]]; then
    break
  fi
done

if [[ -n "$build_run" ]]; then
  assert_run_head "$build_run" "Build"
else
  while ! build_run="$(dispatch_run manual-release-build.yml)"; do
    echo "Build dispatch failed; retrying in ${infra_retry_seconds}s." >&2
    sleep "$infra_retry_seconds"
  done
fi
echo "Clippy is green; monitoring Build $build_run: $(run_url "$build_run")"

last_pair=""
linux_ready_reported=false
macos_ready_reported=false
while true; do
  if ! verify_json="$(
    gh run view "$verify_run" --repo "$repo" --json status,conclusion,url 2>/dev/null
  )" || ! build_json="$(
    gh run view "$build_run" --repo "$repo" --json status,conclusion,jobs,url 2>/dev/null
  )"; then
    echo "Workflow status query failed; retrying in ${poll_seconds}s."
    sleep "$poll_seconds"
    continue
  fi
  verify_status="$(jq -r .status <<<"$verify_json")"
  verify_conclusion="$(jq -r .conclusion <<<"$verify_json")"
  build_status="$(jq -r .status <<<"$build_json")"
  build_conclusion="$(jq -r .conclusion <<<"$build_json")"
  linux_state="$(
    build_job_state \
      "$build_json" \
      "Release build — x86_64-unknown-linux-gnu" \
      "$build_status"
  )"
  macos_primary_state="$(
    build_job_state \
      "$build_json" \
      "Release build — x86_64-apple-darwin (primary)" \
      "$build_status"
  )"
  macos_app_server_state="$(
    build_job_state \
      "$build_json" \
      "Release build — x86_64-apple-darwin (app-server)" \
      "$build_status"
  )"
  macos_package_state="$(
    build_job_state \
      "$build_json" \
      "Package release — x86_64-apple-darwin" \
      "$build_status"
  )"
  pair="Verify=$verify_status/$verify_conclusion Linux=$linux_state macOS(primary=$macos_primary_state app-server=$macos_app_server_state package=$macos_package_state)"
  if [[ "$pair" != "$last_pair" ]]; then
    echo "$pair"
    last_pair="$pair"
  fi
  if [[ "$linux_ready_reported" == false && "$linux_state" == "completed/success" ]]; then
    echo "Linux release artifacts are ready: $(run_url "$build_run")"
    linux_ready_reported=true
  fi
  if [[ "$macos_ready_reported" == false && "$macos_package_state" == "completed/success" ]]; then
    echo "macOS release artifacts are ready: $(run_url "$build_run")"
    macos_ready_reported=true
  fi

  if [[ "$verify_status" == "completed" && "$verify_conclusion" != "success" ]]; then
    echo "Verify failed after Clippy: $(run_url "$verify_run")" >&2
    exit 22
  fi
  if [[ "$build_status" == "completed" && "$build_conclusion" != "success" ]]; then
    echo "Build failed: $(run_url "$build_run")" >&2
    exit 23
  fi
  if [[ "$verify_status" == "completed" && "$build_status" == "completed" ]]; then
    echo "Verify and all requested Build platforms succeeded."
    echo "VERIFY_RUN_ID=$verify_run"
    echo "BUILD_RUN_ID=$build_run"
    exit
  fi
  sleep "$poll_seconds"
done
