# Hellion Control API

The Control API is the entry point for creating test runs, polling status, and reading event history. It runs on port **8080** in the default docker-compose stack.

- **OpenAPI spec:** [openapi.yaml](./openapi.yaml)
- **Base URL (local):** `http://localhost:8080`

## Quick start

```bash
# Health check
curl http://localhost:8080/health

# Create a run against OWASP Juice Shop
curl -X POST http://localhost:8080/runs \
  -H "Content-Type: application/json" \
  -d '{
    "scope_id": "local-juice-shop",
    "targets": ["http://juice-shop:3000"],
    "test_pack": "juice-shop-detect"
  }'

# Poll run status
curl http://localhost:8080/runs/run_20260606120000_123456

# Read event history (NDJSON)
curl http://localhost:8080/runs/run_20260606120000_123456/events
```

## Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/health` | Liveness check |
| `GET` | `/runs` | List all runs |
| `POST` | `/runs` | Create a run (one job per target) |
| `POST` | `/runs/bulk` | Create many runs (benchmarking) |
| `GET` | `/runs/{run_id}` | Get run metadata |
| `DELETE` | `/runs/{run_id}` | Delete run and its events |
| `GET` | `/runs/{run_id}/events` | Event history as NDJSON |
| `POST` | `/runs/{run_id}/cancel` | Mark run cancelled |
| `POST` | `/admin/queue/clear` | Clear pending Redis job list |
| `POST` | `/admin/runs/clear` | Delete all run records |

## Create run

`POST /runs`

Creates a single run record in Redis and publishes one NATS job per target.

**Request body**

```json
{
  "scope_id": "local-juice-shop",
  "targets": ["http://juice-shop:3000"],
  "test_pack": "juice-shop-detect"
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `scope_id` | yes | Must match a worker scope and NATS subject suffix |
| `targets` | yes | One or more base URLs to test |
| `test_pack` | yes | Test pack name (filename without `.yaml`) |

**Response**

```json
{
  "run_id": "run_20260606120000_123456",
  "status": "queued",
  "outcome": "unknown"
}
```

The `test_pack` value is validated at request time. The file must exist at `/app/test-packs/{test_pack}.yaml` inside the control-api container.

## Bulk create

`POST /runs/bulk`

Creates a separate run for each target. When `targets` is omitted, the single `target` value is repeated `count` times.

**Request body**

```json
{
  "scope_id": "local-juice-shop",
  "target": "http://juice-shop:3000",
  "test_pack": "juice-shop-detect",
  "count": 100
}
```

**Response**

```json
{
  "status": "queued",
  "created": 100
}
```

## Run lifecycle

| Status | Meaning |
|--------|---------|
| `queued` | Run created; job published to NATS |
| `running` | Worker claimed job and started target |
| `completed` | Worker finished all test-pack steps |
| `cancelled` | Cancelled via API |
| `failed` | Reserved for future use |

| Outcome | Meaning |
|---------|---------|
| `unknown` | Run not finished |
| `not_exploitable` | Completed with no critical/high findings |
| `potentially_exploitable` | A critical or high severity finding was emitted |
| `exploitable` | Reserved for future use |
| `error` | Reserved for future use |

Workers update `status` and `outcome` in Redis as events are emitted. See [Run events](#run-events) below.

## Run events

`GET /runs/{run_id}/events`

Returns newline-delimited JSON. Each line is one event object:

```json
{"event_type":"target.started","run_id":"run_...","target":"http://juice-shop:3000","message":"running test pack juice-shop-detect - Detect OWASP Juice Shop","severity":null}
{"event_type":"request.sent","run_id":"run_...","target":"http://juice-shop:3000","message":"root GET /","severity":null}
{"event_type":"finding.created","run_id":"run_...","target":"http://juice-shop:3000","message":"OWASP Juice Shop detected","severity":"critical"}
{"event_type":"target.completed","run_id":"run_...","target":"http://juice-shop:3000","message":"target completed","severity":null}
```

Common event types:

| Event | Description |
|-------|-------------|
| `worker.job.claimed` | Worker picked up the job |
| `target.started` | Test pack execution began |
| `request.sent` | HTTP request dispatched |
| `request.completed` | HTTP response received |
| `request.error` | Request failed (timeout, DNS, etc.) |
| `assert.passed` | Assertion succeeded |
| `assert.failed` | Assertion failed; later steps skipped |
| `extract.completed` | Variable extracted from response |
| `extract.failed` | Regex extraction failed |
| `finding.created` | Security finding recorded |
| `scope.blocked` | Request blocked by scope rules |
| `step.skipped` | Step skipped after prior failure |
| `target.completed` | All steps finished for target |
| `test_pack.error` | Test pack missing or invalid |

Set `HELLION_VERBOSE_EVENTS=false` on workers to suppress low-signal events from storage while still updating run status.

## Admin endpoints

Use these before benchmarks or e2e tests to reset state:

```bash
curl -X POST http://localhost:8080/admin/queue/clear
curl -X POST http://localhost:8080/admin/runs/clear
```

## NATS subjects

Jobs are published to:

```
hellion.jobs.http.{scope_id}
```

Workers subscribe with queue group `hellion-http-workers`, so jobs are load-balanced across worker instances.

## Error responses

| Code | Cause |
|------|-------|
| `400` | Missing fields, invalid JSON, or unknown test pack |
| `404` | Run not found |
| `405` | Wrong HTTP method |
| `500` | Redis or NATS failure |
