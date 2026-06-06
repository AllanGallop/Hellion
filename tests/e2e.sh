#!/usr/bin/env bash
set -euo pipefail

API="${API:-http://localhost:8080}"

echo "== Hellion E2E test =="

echo "Clearing old state..."
curl -fsS -X POST "$API/admin/queue/clear" >/dev/null
curl -fsS -X POST "$API/admin/runs/clear" >/dev/null

echo "Checking health..."
curl -fsS "$API/health" | grep -q "ok"

echo "Creating Juice Shop detection run..."
CREATE_RESPONSE="$(curl -fsS -X POST "$API/runs" \
  -H "Content-Type: application/json" \
  -d '{
    "scope_id": "local-juice-shop",
    "targets": ["http://juice-shop:3000"],
    "test_pack": "juice-shop-detect"
  }')"

echo "$CREATE_RESPONSE"

RUN_ID="$(echo "$CREATE_RESPONSE" | sed -n 's/.*"run_id":"\([^"]*\)".*/\1/p')"

if [ -z "$RUN_ID" ]; then
  echo "Failed to extract run_id"
  exit 1
fi

echo "Run ID: $RUN_ID"

echo "Waiting for run completion..."
for i in $(seq 1 30); do
  RUN_JSON="$(curl -fsS "$API/runs/$RUN_ID")"
  STATUS="$(echo "$RUN_JSON" | sed -n 's/.*"status":"\([^"]*\)".*/\1/p')"
  OUTCOME="$(echo "$RUN_JSON" | sed -n 's/.*"outcome":"\([^"]*\)".*/\1/p')"

  echo "status=$STATUS outcome=$OUTCOME"

  if [ "$STATUS" = "completed" ]; then
    break
  fi

  sleep 1
done

if [ "$STATUS" != "completed" ]; then
  echo "Run did not complete"
  curl -fsS "$API/runs/$RUN_ID/events" || true
  exit 1
fi

echo "Checking outcome..."
if [ "$OUTCOME" != "potentially_exploitable" ]; then
  echo "Expected outcome potentially_exploitable, got $OUTCOME"
  echo "Events:"
  curl -fsS "$API/runs/$RUN_ID/events" || true
  exit 1
fi

echo "Checking events..."
EVENTS="$(curl -fsS "$API/runs/$RUN_ID/events")"

echo "$EVENTS" | grep -q "target.started"
echo "$EVENTS" | grep -q "request.sent"
echo "$EVENTS" | grep -q "request.completed"
echo "$EVENTS" | grep -q "assert.passed"
echo "$EVENTS" | grep -q "finding.created"
echo "$EVENTS" | grep -q "target.completed"

echo "Checking run list..."
curl -fsS "$API/runs" | grep -q "$RUN_ID"

echo "PASS"