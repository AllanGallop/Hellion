#!/usr/bin/env bash
set -euo pipefail

API="${API:-http://localhost:8080}"
RUNS="${RUNS:-50}"

echo "Clearing old state..."

curl -fsS -X POST "$API/admin/queue/clear" >/dev/null
curl -fsS -X POST "$API/admin/runs/clear" >/dev/null

echo "Generating $RUNS runs"

START="$(date +%s)"

curl -fsS -X POST "$API/runs/bulk" \
  -H "Content-Type: application/json" \
  -d "{
    \"scope_id\":\"local-juice-shop\",
    \"target\":\"http://juice-shop:3000\",
    \"test_pack\":\"juice-shop-detect\",
    \"count\":$RUNS
  }" >/dev/null

echo "Queued $RUNS runs"