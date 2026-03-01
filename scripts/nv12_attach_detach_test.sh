#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export GST_PLUGIN_PATH="${ROOT_DIR}/target/debug"

SHM_PATH="${1:-/dev/shm/gst-shm2-nv12-attach-detach-$$}"
TOTAL_SECONDS="${2:-24}"
ATTACH_SECONDS="${3:-4}"
DETACH_SECONDS="${4:-2}"
SHM_SIZE_BYTES="${5:-67108864}" # 64 MiB

SINK_LOG="$(mktemp -t shm2sink-log-XXXXXX.txt)"
SRC_LOG="$(mktemp -t shm2src-log-XXXXXX.txt)"

sink_pid=""
src_pid=""

cleanup() {
  set +e
  if [[ -n "${src_pid}" ]] && kill -0 "${src_pid}" 2>/dev/null; then
    kill "${src_pid}" >/dev/null 2>&1
    wait "${src_pid}" 2>/dev/null
  fi
  if [[ -n "${sink_pid}" ]] && kill -0 "${sink_pid}" 2>/dev/null; then
    kill "${sink_pid}" >/dev/null 2>&1
    wait "${sink_pid}" 2>/dev/null
  fi
  rm -f "${SHM_PATH}" >/dev/null 2>&1
  echo "sink-log: ${SINK_LOG}"
  echo "src-log:  ${SRC_LOG}"
}
trap cleanup EXIT

assert_pid_alive() {
  local pid="$1"
  local name="$2"
  if ! kill -0 "${pid}" 2>/dev/null; then
    echo "FAIL: ${name} process exited unexpectedly"
    exit 1
  fi
}

echo "Building plugin..."
(cargo build --lib >/dev/null)

echo "Starting sink pipeline on ${SHM_PATH}"
gst-launch-1.0 -q \
  videotestsrc is-live=true pattern=ball ! \
  video/x-raw,format=NV12,width=1920,height=1080,framerate=30/1 ! \
  queue ! \
  shm2sink shm-path="${SHM_PATH}" shm-size="${SHM_SIZE_BYTES}" \
  >"${SINK_LOG}" 2>&1 &
sink_pid=$!

sleep 2
assert_pid_alive "${sink_pid}" "sink"

start_ts=$(date +%s)
cycle=0

while true; do
  now_ts=$(date +%s)
  elapsed=$((now_ts - start_ts))
  if (( elapsed >= TOTAL_SECONDS )); then
    break
  fi

  cycle=$((cycle + 1))
  echo "Cycle ${cycle}: attach src for ${ATTACH_SECONDS}s"
  gst-launch-1.0 -q \
    shm2src shm-path="${SHM_PATH}" is-live=true ! \
    queue ! fakesink sync=false \
    >>"${SRC_LOG}" 2>&1 &
  src_pid=$!

  sleep "${ATTACH_SECONDS}"
  assert_pid_alive "${sink_pid}" "sink"

  if kill -0 "${src_pid}" 2>/dev/null; then
    kill "${src_pid}" >/dev/null 2>&1
    wait "${src_pid}" 2>/dev/null || true
  fi
  src_pid=""

  now_ts=$(date +%s)
  elapsed=$((now_ts - start_ts))
  if (( elapsed >= TOTAL_SECONDS )); then
    break
  fi

  echo "Cycle ${cycle}: detach src for ${DETACH_SECONDS}s"
  sleep "${DETACH_SECONDS}"
  assert_pid_alive "${sink_pid}" "sink"
done

assert_pid_alive "${sink_pid}" "sink"

if grep -E "(ERROR|CRITICAL|Another shm2src is already connected)" "${SINK_LOG}" >/dev/null 2>&1; then
  echo "FAIL: sink log contains error indicators"
  exit 1
fi

echo "PASS: sink survived attach/detach cycles for ${TOTAL_SECONDS}s at NV12 1920x1080@30"
