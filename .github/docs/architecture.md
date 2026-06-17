# Hellion Architecture

Component overview and operational flows for the default docker-compose stack.

## Overview

```mermaid
flowchart TB
    subgraph clients [Humans and questionable automation]
        CLI[curl / scripts]
    end

    subgraph control [Control Plane]
        API[Go API]
    end

    subgraph messaging [Job Queue]
        NATS[NATS]
    end

    subgraph storage [State]
        PG[(Postgres)]
    end

    subgraph workers [Rust Workers]
        W1[worker]
        W2[worker]
        W3[worker]
    end

    subgraph targets [Things You Are Allowed To Test]
        JS[Juice Shop]
    end

    CLI --> API
    API --> PG
    API --> NATS
    NATS --> W1
    NATS --> W2
    NATS --> W3
    W1 --> JS
    W2 --> JS
    W3 --> JS
    W1 --> PG
    W2 --> PG
    W3 --> PG
```

| Component | Role |
|-----------|------|
| **control-api** | REST API and embedded web UI |
| **worker-rust** | Executes test packs; enforces scope; batches state writes to Postgres |
| **NATS** | Distributes jobs to workers (`hellion.jobs.http.{scope_id}`) |
| **Postgres** | Run metadata, event history, aggregated stats |

## Run creation

```mermaid
sequenceDiagram
    participant C as Client
    participant API as control-api
    participant PG as Postgres
    participant N as NATS

    C->>API: POST /runs
    API->>API: validate test pack
    API->>PG: INSERT run (status=queued)
    loop each target
        API->>N: publish hellion.jobs.http.{scope_id}
    end
    API-->>C: run_id, status=queued
```

Run IDs are UUID v4 values prefixed with `run_`, e.g. `run_550e8400-e29b-41d4-a716-446655440000`.

## Worker execution

```mermaid
sequenceDiagram
    participant N as NATS
    participant W as worker-rust
    participant PG as Postgres
    participant T as Target

    N->>W: job message (run_id, target, test_pack)
    W->>W: verify scope_id matches
    W->>PG: batch flush (claimed, status=running)
    loop each test-pack step
        alt HTTP step
            W->>W: check allowed_methods / origins
            W->>T: HTTP request
            T-->>W: response
            W->>PG: batch events + status patches
        else Assert step
            W->>W: evaluate status, headers, body
            W->>PG: assert.passed / assert.failed
        else Extract step
            W->>W: regex capture into variable
            W->>PG: extract.completed / extract.failed
        else Finding step
            W->>PG: finding.created
        end
    end
    W->>PG: target.completed, status=completed
    Note over W,PG: critical/high findings set outcome=potentially_exploitable
```

Workers batch Postgres writes: status updates flush immediately; events are bulk-inserted when the buffer fills or the job completes.

## Run lifecycle

```mermaid
stateDiagram-v2
    [*] --> queued: POST /runs
    queued --> running: target.started
    running --> completed: target.completed
    running --> cancelled: POST /runs/{id}/cancel
    completed --> [*]
    cancelled --> [*]
```

## Related docs

| Guide | Description |
|-------|-------------|
| [API guide](./api.md) | Endpoints, request/response shapes |
| [Performance guide](./performance.md) | Benchmarks, bottlenecks, tuning |
| [Test packs guide](./test-packs.md) | Writing HTTP check workflows |
