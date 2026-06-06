#!/usr/bin/env bash
set -euo pipefail

API="${API:-http://localhost:8080}"
RUNS="${RUNS:-1000}"
TIMEOUT_SEC="${TIMEOUT_SEC:-300}"

now_ms() {
  date +%s%3N
}

read_stat() {
  local json="$1"
  local field="$2"
  printf "%s" "$json" | grep -o "\"$field\":[0-9]*" | head -1 | cut -d: -f2
}

echo "== Hellion benchmark =="
echo "Runs: $RUNS"

echo "Clearing old state..."

curl -fsS -X POST "$API/admin/queue/clear" >/dev/null
curl -fsS -X POST "$API/admin/runs/clear" >/dev/null

TOTAL_START_MS="$(now_ms)"

QUEUE_START_MS="$(now_ms)"

curl -fsS -X POST "$API/runs/bulk" \
  -H "Content-Type: application/json" \
  -d "{
    \"scope_id\":\"local-juice-shop\",
    \"target\":\"http://juice-shop:3000\",
    \"test_pack\":\"juice-shop-detect\",
    \"count\":$RUNS
  }" >/dev/null

echo "Queued $RUNS runs"

QUEUE_END_MS="$(now_ms)"
QUEUE_MS=$((QUEUE_END_MS - QUEUE_START_MS))

echo "Queued $RUNS runs in ${QUEUE_MS}ms"

WORKER_START_MS="$(now_ms)"
DEADLINE=$((SECONDS + TIMEOUT_SEC))

while true; do
  if [ "$SECONDS" -ge "$DEADLINE" ]; then
    echo "Timed out after ${TIMEOUT_SEC}s waiting for completion"
    STATS_JSON="$(curl -fsS "$API/runs/stats" || echo '{}')"
    echo "final stats: $STATS_JSON"
    exit 1
  fi

  if ! STATS_JSON="$(curl -fsS "$API/runs/stats")"; then
    echo "failed to fetch /runs/stats from $API" >&2
    exit 1
  fi

  TOTAL="$(read_stat "$STATS_JSON" "total")"
  QUEUED="$(read_stat "$STATS_JSON" "queued")"
  RUNNING="$(read_stat "$STATS_JSON" "running")"
  COMPLETED="$(read_stat "$STATS_JSON" "completed")"

  TOTAL="${TOTAL:-0}"
  QUEUED="${QUEUED:-0}"
  RUNNING="${RUNNING:-0}"
  COMPLETED="${COMPLETED:-0}"

  echo "queued=$QUEUED running=$RUNNING completed=$COMPLETED total=$TOTAL"

  if [ "$TOTAL" -eq "$RUNS" ] && [ "$COMPLETED" -eq "$RUNS" ]; then
    break
  fi

  sleep 0.25
done

WORKER_END_MS="$(now_ms)"
TOTAL_END_MS="$(now_ms)"

WORKER_MS=$((WORKER_END_MS - WORKER_START_MS))
TOTAL_MS=$((TOTAL_END_MS - TOTAL_START_MS))

echo
echo "== Results =="
echo "Queue time:  ${QUEUE_MS}ms"
echo "Worker time: ${WORKER_MS}ms"
echo "Total time:  ${TOTAL_MS}ms"

awk "BEGIN { printf \"Queue rate:  %.2f runs/sec\n\", $RUNS / ($QUEUE_MS / 1000) }"
awk "BEGIN { printf \"Worker rate: %.2f runs/sec\n\", $RUNS / ($WORKER_MS / 1000) }"
awk "BEGIN { printf \"Total rate:  %.2f runs/sec\n\", $RUNS / ($TOTAL_MS / 1000) }"
