use deadpool_postgres::{Pool, Runtime};
use reqwest::Method;
use tokio_postgres::NoTls;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, env, fs, sync::Arc, time::Duration};
use url::Url;
use regex::Regex;
use std::process;
use futures::StreamExt;
use tokio::sync::Semaphore;

#[derive(Debug, Deserialize)]
struct Scope {
    scope_id: String,
    allowed_origins: Vec<String>,
    allowed_methods: Vec<String>,
    max_rps: Option<u64>,
    worker_concurrency: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct Job {
    run_id: String,
    scope_id: String,
    target: String,
    test_pack: String,
}

#[derive(Debug, Deserialize)]
struct TestPack {
    id: String,
    name: String,
    steps: Vec<Step>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum Step {
    Http { http: HttpStep },
    Assert { assert: AssertStep },
    Extract { extract: ExtractStep },
    Finding { finding: FindingStep },
}

#[derive(Debug, Deserialize, Clone)]
struct HttpStep {
    id: String,
    method: String,
    path: String,

    headers: Option<HashMap<String, String>>,
    query: Option<HashMap<String, String>>,
    form: Option<HashMap<String, String>>,
    json: Option<serde_json::Value>,
    body: Option<String>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Clone)]
struct AssertStep {
    response: String,

    status: Option<u16>,
    status_lt: Option<u16>,
    status_gte: Option<u16>,
    status_not: Option<u16>,

    header_absent: Option<String>,
    header_present: Option<String>,
    header_contains: Option<HeaderContains>,

    body_contains: Option<String>,
    body_not_contains: Option<String>,

    severity: Option<String>,
    message: String,
}

#[derive(Debug, Deserialize, Clone)]
struct HeaderContains {
    name: String,
    value: String,
}

#[derive(Debug, Deserialize, Clone)]
struct FindingStep {
    severity: String,
    message: String,
}

#[derive(Debug)]
struct StoredResponse {
    status: u16,
    headers: HashMap<String, String>,
    body: String,
}

#[derive(Debug, Serialize)]
struct Event {
    event_type: String,
    run_id: String,
    target: Option<String>,
    message: String,
    severity: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct ExtractStep {
    response: String,
    from: ExtractFrom,
    regex: String,
    into: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
enum ExtractFrom {
    Body,
    Header,
}

#[derive(Debug)]
struct ExecutionContext {
    responses: HashMap<String, StoredResponse>,
    variables: HashMap<String, String>,
    failed: bool,
}

type TestPackCache = HashMap<String, Arc<TestPack>>;

#[tokio::main]
async fn main() {
    let worker_id = env::var("HOSTNAME")
    .unwrap_or_else(|_| format!("worker-{}", process::id()));

    let nats_url =
    env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".into());

    let nats = async_nats::connect(&nats_url)
        .await
        .expect("nats connection");

    let database_url = env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://surface:surface@postgres:5432/surface_tester?sslmode=disable".into());
    let scope_path =
        env::var("SCOPE_PATH").unwrap_or_else(|_| "/app/scopes/local-juice-shop.yaml".into());
    let batch_size = env::var("STATE_BATCH_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);

    let scope_raw = fs::read_to_string(scope_path).expect("failed to read scope file");
    let scope: Scope = serde_yaml::from_str(&scope_raw).expect("failed to parse scope");

    let subject = format!(
        "hellion.jobs.http.{}",
        scope.scope_id
    );
    
    let test_packs = Arc::new(load_test_packs("/app/test-packs"));

    let pg_pool = create_pool(&database_url);
    migrate_schema(&pg_pool).await;

    let concurrency = scope.worker_concurrency.unwrap_or(25).clamp(1, 100);

    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(concurrency)
        .build()
        .expect("http client");

    let benchmark_mode = env::var("BENCHMARK_MODE").unwrap_or_else(|_| "false".into()) == "true";
    println!(
        "worker {} concurrency {} benchmark_mode={}",
        worker_id, concurrency, benchmark_mode
    );

    let scope = Arc::new(scope);
    let pg_pool = Arc::new(pg_pool);
    let http = Arc::new(http);
    let nats = Arc::new(nats);
    
    worker_loop(
        nats,
        pg_pool,
        http,
        scope,
        subject,
        worker_id,
        test_packs,
        batch_size,
    )
    .await;

}

async fn worker_loop(
    nats: Arc<async_nats::Client>,
    pg_pool: Arc<Pool>,
    http: Arc<reqwest::Client>,
    scope: Arc<Scope>,
    subject: String,
    worker_id: String,
    test_packs: Arc<TestPackCache>,
    batch_size: usize,
) {
    let mut subscription = nats
    .queue_subscribe(subject.clone(), "hellion-http-workers".to_string())
    .await
    .expect("queue subscription");

    let semaphore = Arc::new(Semaphore::new(scope.worker_concurrency.unwrap_or(25).clamp(1, 100)));

    while let Some(message) = subscription.next().await {
        let permit = semaphore.clone().acquire_owned().await.unwrap();

        let pg_pool = pg_pool.clone();
        let http = http.clone();
        let scope = scope.clone();
        let test_packs = test_packs.clone();
        let worker_id = worker_id.clone();

        tokio::spawn(async move {
            let _permit = permit;

            let payload = match std::str::from_utf8(&message.payload) {
                Ok(payload) => payload,
                Err(e) => {
                    eprintln!("invalid utf8 payload: {}", e);
                    return;
                }
            };

            let job: Job = match serde_json::from_str(payload) {
                Ok(job) => job,
                Err(e) => {
                    eprintln!("bad job payload: {}", e);
                    return;
                }
            };

            let mut state = StateBatcher::new(pg_pool.as_ref().clone(), batch_size);

            if job.scope_id != scope.scope_id {
                emit(
                    &mut state,
                    Event {
                        event_type: "scope.blocked".into(),
                        run_id: job.run_id.clone(),
                        target: Some(job.target.clone()),
                        message: "job scope_id does not match worker scope".into(),
                        severity: Some("high".into()),
                    },
                )
                .await;
                complete_run(&mut state, &job).await;
                return;
            }

            run_job(
                http.as_ref(),
                &mut state,
                scope.as_ref(),
                job,
                &worker_id,
                test_packs.as_ref(),
            )
            .await;
        });
    }
}

async fn run_job(
    http: &reqwest::Client,
    state: &mut StateBatcher,
    scope: &Scope,
    job: Job,
    worker_id: &str,
    test_packs: &TestPackCache,
) {
    emit(
        state,
        Event {
            event_type: "worker.job.claimed".into(),
            run_id: job.run_id.clone(),
            target: Some(job.target.clone()),
            message: format!("claimed by {}", worker_id),
            severity: None,
        },
    )
    .await;

    let Some(test_pack) = test_packs.get(&job.test_pack) else {
        emit(
            state,
            Event {
                event_type: "test_pack.error".into(),
                run_id: job.run_id.clone(),
                target: Some(job.target.clone()),
                message: format!("unknown test pack {}", job.test_pack),
                severity: Some("high".into()),
            },
        )
        .await;
        complete_run(state, &job).await;
        return;
    };

    emit(
        state,
        Event {
            event_type: "target.started".into(),
            run_id: job.run_id.clone(),
            target: Some(job.target.clone()),
            message: format!("running test pack {} - {}", test_pack.id, test_pack.name),
            severity: None,
        },
    )
    .await;

    let mut exec = ExecutionContext {
        responses: HashMap::new(),
        variables: HashMap::new(),
        failed: false,
    };

    for step in test_pack.steps.clone() {
        if exec.failed {
            emit(
                state,
                Event {
                    event_type: "step.skipped".into(),
                    run_id: job.run_id.clone(),
                    target: Some(job.target.clone()),
                    message: "previous assertion failed".into(),
                    severity: None,
                },
            )
            .await;
            continue;
        }
    
        match step {
            Step::Http { http: step } => {
                execute_http_step(http, state, scope, &job, step, &mut exec).await;
            }
            Step::Assert { assert: step } => {
                execute_assert_step(state, &job, step.clone(), &mut exec).await;
            }
            Step::Extract { extract: step } => {
                execute_extract_step(state, &job, step.clone(), &mut exec).await;
            }
            Step::Finding { finding: step } => {
                emit(
                    state,
                    Event {
                        event_type: "finding.created".into(),
                        run_id: job.run_id.clone(),
                        target: Some(job.target.clone()),
                        message: step.message.clone(),
                        severity: Some(step.severity.clone()),
                    },
                )
                .await;
            }
        }
    }

    complete_run(state, &job).await;
}

async fn complete_run(state: &mut StateBatcher, job: &Job) {
    emit(
        state,
        Event {
            event_type: "target.completed".into(),
            run_id: job.run_id.clone(),
            target: Some(job.target.clone()),
            message: "target completed".into(),
            severity: None,
        },
    )
    .await;

    if let Err(e) = state.flush_with_retry().await {
        eprintln!("final state flush error: {}", e);
    }
}

async fn execute_http_step(
    client: &reqwest::Client,
    state: &mut StateBatcher,
    scope: &Scope,
    job: &Job,
    step: HttpStep,
    exec: &mut ExecutionContext,
) {
    let method_raw = interpolate(&step.method, &exec.variables);
    let path_raw = interpolate(&step.path, &exec.variables);

    if !scope
        .allowed_methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case(&method_raw))
    {
        emit(
            state,
            Event {
                event_type: "scope.blocked".into(),
                run_id: job.run_id.clone(),
                target: Some(job.target.clone()),
                message: format!("method {} is not allowed by scope", method_raw),
                severity: Some("high".into()),
            },
        )
        .await;
        return;
    }

    let url = match build_url(&job.target, &path_raw) {
        Ok(url) => url,
        Err(e) => {
            emit(
                state,
                Event {
                    event_type: "request.error".into(),
                    run_id: job.run_id.clone(),
                    target: Some(job.target.clone()),
                    message: e,
                    severity: Some("medium".into()),
                },
            )
            .await;
            return;
        }
    };

    if !in_scope(scope, url.as_str()) {
        emit(
            state,
            Event {
                event_type: "scope.blocked".into(),
                run_id: job.run_id.clone(),
                target: Some(url.to_string()),
                message: "request URL is outside allowed origins".into(),
                severity: Some("high".into()),
            },
        )
        .await;
        return;
    }

    emit(
        state,
        Event {
            event_type: "request.sent".into(),
            run_id: job.run_id.clone(),
            target: Some(url.to_string()),
            message: format!("{} {}", method_raw, url),
            severity: None,
        },
    )
    .await;

    let method = match Method::from_bytes(method_raw.as_bytes()) {
        Ok(method) => method,
        Err(_) => {
            emit(
                state,
                Event {
                    event_type: "request.error".into(),
                    run_id: job.run_id.clone(),
                    target: Some(url.to_string()),
                    message: format!("invalid method {}", method_raw),
                    severity: Some("medium".into()),
                },
            )
            .await;
            return;
        }
    };

    let mut request = client.request(method, url.clone());

    if let Some(timeout_ms) = step.timeout_ms {
        request = request.timeout(Duration::from_millis(timeout_ms));
    }
    
    if let Some(headers) = &step.headers {
        let headers = interpolate_map(headers, &exec.variables);
        for (key, value) in headers {
            request = request.header(key, value);
        }
    }
    
    if let Some(query) = &step.query {
        let query = interpolate_map(query, &exec.variables);
        request = request.query(&query);
    }
    
    if let Some(form) = &step.form {
        let form = interpolate_map(form, &exec.variables);
        request = request.form(&form);
    }
    
    if let Some(json) = &step.json {
        let json = interpolate_json(json, &exec.variables);
        request = request.json(&json);
    }
    
    if let Some(body) = &step.body {
        request = request.body(interpolate(body, &exec.variables));
    }
    
    match request.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();

            let mut headers = HashMap::new();
            for (key, value) in resp.headers() {
                headers.insert(
                    key.as_str().to_ascii_lowercase(),
                    value.to_str().unwrap_or("").to_string(),
                );
            }

            let body = resp.text().await.unwrap_or_default();

            exec.responses.insert(
                step.id.clone(),
                StoredResponse {
                    status,
                    headers,
                    body,
                },
            );

            emit(
                state,
                Event {
                    event_type: "request.completed".into(),
                    run_id: job.run_id.clone(),
                    target: Some(url.to_string()),
                    message: format!("{} completed with status {}", step.id, status),
                    severity: None,
                },
            )
            .await;
        }
        Err(e) => {
            exec.failed = true;
            emit(
                state,
                Event {
                    event_type: "request.error".into(),
                    run_id: job.run_id.clone(),
                    target: Some(url.to_string()),
                    message: e.to_string(),
                    severity: Some("medium".into()),
                },
            )
            .await;
        }
    }
}

async fn execute_assert_step(
    state: &mut StateBatcher,
    job: &Job,
    step: AssertStep,
    exec: &mut ExecutionContext,
) {
    let Some(response) = exec.responses.get(&step.response) else {
        exec.failed = true;

        emit(
            state,
            Event {
                event_type: "assert.failed".into(),
                run_id: job.run_id.clone(),
                target: Some(job.target.clone()),
                message: format!("unknown response id {}", step.response),
                severity: Some("medium".into()),
            },
        )
        .await;

        return;
    };

    let severity = step.severity.unwrap_or_else(|| "info".into());

    if let Some(expected) = step.status {
        if response.status != expected {
            fail_assert(
                state,
                job,
                &mut exec.failed,
                severity,
                format!("{}: expected status {}, got {}", step.message, expected, response.status),
            )
            .await;
            return;
        }
    }

    if let Some(limit) = step.status_lt {
        if response.status >= limit {
            fail_assert(
                state,
                job,
                &mut exec.failed,
                severity,
                format!("{}: expected status < {}, got {}", step.message, limit, response.status),
            )
            .await;
            return;
        }
    }

    if let Some(limit) = step.status_gte {
        if response.status < limit {
            fail_assert(
                state,
                job,
                &mut exec.failed,
                severity,
                format!("{}: expected status >= {}, got {}", step.message, limit, response.status),
            )
            .await;
            return;
        }
    }

    if let Some(header) = step.header_absent {
        let header = header.to_ascii_lowercase();

        if response.headers.contains_key(&header) {
            fail_assert(
                state,
                job,
                &mut exec.failed,
                severity,
                format!("{}: header {} was present", step.message, header),
            )
            .await;
            return;
        }
    }

    if let Some(header) = step.header_present {
        let header = header.to_ascii_lowercase();

        if !response.headers.contains_key(&header) {
            fail_assert(
                state,
                job,
                &mut exec.failed,
                severity,
                format!("{}: header {} was absent", step.message, header),
            )
            .await;
            return;
        }
    }

    if let Some(check) = step.header_contains {
        let header = check.name.to_ascii_lowercase();
    
        match response.headers.get(&header) {
            Some(value) if value.to_ascii_lowercase().contains(&check.value.to_ascii_lowercase()) => {}
            Some(value) => {
                fail_assert(
                    state,
                    job,
                    &mut exec.failed,
                    severity,
                    format!(
                        "{}: header {} did not contain {}, got {}",
                        step.message, header, check.value, value
                    ),
                )
                .await;
                return;
            }
            None => {
                fail_assert(
                    state,
                    job,
                    &mut exec.failed,
                    severity,
                    format!("{}: header {} was absent", step.message, header),
                )
                .await;
                return;
            }
        }
    }

    if let Some(needle) = step.body_contains {
        if !response.body.contains(&needle) {
            fail_assert(
                state,
                job,
                &mut exec.failed,
                severity,
                format!("{}: body did not contain {}", step.message, needle),
            )
            .await;
            return;
        }
    }

    if let Some(needle) = step.body_not_contains {
        if response.body.contains(&needle) {
            fail_assert(
                state,
                job,
                &mut exec.failed,
                severity,
                format!("{}: body contained forbidden text {}", step.message, needle),
            )
            .await;
            return;
        }
    }

    if let Some(disallowed) = step.status_not {
        if response.status == disallowed {
            fail_assert(
                state,
                job,
                &mut exec.failed,
                severity,
                format!(
                    "{}: status must not be {}, got {}",
                    step.message, disallowed, response.status
                ),
            )
            .await;
            return;
        }
    }

    emit(
        state,
        Event {
            event_type: "assert.passed".into(),
            run_id: job.run_id.clone(),
            target: Some(job.target.clone()),
            message: step.message,
            severity: None,
        },
    )
    .await;
}

async fn finding(state: &mut StateBatcher, job: &Job, severity: String, message: String) {
    emit(
        state,
        Event {
            event_type: "finding.created".into(),
            run_id: job.run_id.clone(),
            target: Some(job.target.clone()),
            message,
            severity: Some(severity),
        },
    )
    .await;
}

fn build_url(base: &str, path: &str) -> Result<Url, String> {
    let base = Url::parse(base).map_err(|e| format!("invalid base target: {}", e))?;
    base.join(path)
        .map_err(|e| format!("invalid step path: {}", e))
}

fn in_scope(scope: &Scope, target: &str) -> bool {
    let target_url = match Url::parse(target) {
        Ok(url) => url,
        Err(_) => return false,
    };

    scope.allowed_origins.iter().any(|origin| {
        Url::parse(origin)
            .map(|allowed| {
                allowed.scheme() == target_url.scheme()
                    && allowed.host_str() == target_url.host_str()
                    && allowed.port_or_known_default() == target_url.port_or_known_default()
            })
            .unwrap_or(false)
    })
}

async fn execute_extract_step(
    state: &mut StateBatcher,
    job: &Job,
    step: ExtractStep,
    exec: &mut ExecutionContext,
) {
    let Some(response) = exec.responses.get(&step.response) else {
        exec.failed = true;

        emit(
            state,
            Event {
                event_type: "extract.failed".into(),
                run_id: job.run_id.clone(),
                target: Some(job.target.clone()),
                message: format!("unknown response id {}", step.response),
                severity: Some("medium".into()),
            },
        )
        .await;

        return;
    };

    let source = match step.from {
        ExtractFrom::Body => response.body.clone(),
        ExtractFrom::Header => response
            .headers
            .iter()
            .map(|(k, v)| format!("{}: {}", k, v))
            .collect::<Vec<_>>()
            .join("\n"),
    };

    let regex = match Regex::new(&step.regex) {
        Ok(regex) => regex,
        Err(e) => {
            exec.failed = true;

            emit(
                state,
                Event {
                    event_type: "extract.failed".into(),
                    run_id: job.run_id.clone(),
                    target: Some(job.target.clone()),
                    message: format!("invalid regex: {}", e),
                    severity: Some("medium".into()),
                },
            )
            .await;

            return;
        }
    };

    let Some(captures) = regex.captures(&source) else {
        exec.failed = true;

        emit(
            state,
            Event {
                event_type: "extract.failed".into(),
                run_id: job.run_id.clone(),
                target: Some(job.target.clone()),
                message: format!("regex did not match for {}", step.into),
                severity: Some("info".into()),
            },
        )
        .await;

        return;
    };

    let Some(value) = captures.get(1) else {
        exec.failed = true;

        emit(
            state,
            Event {
                event_type: "extract.failed".into(),
                run_id: job.run_id.clone(),
                target: Some(job.target.clone()),
                message: "regex matched but had no capture group".into(),
                severity: Some("medium".into()),
            },
        )
        .await;

        return;
    };

    exec.variables
        .insert(step.into.clone(), value.as_str().to_string());

    emit(
        state,
        Event {
            event_type: "extract.completed".into(),
            run_id: job.run_id.clone(),
            target: Some(job.target.clone()),
            message: format!("extracted variable {}", step.into),
            severity: None,
        },
    )
    .await;
}

struct PendingEvent {
    run_id: String,
    event_type: String,
    target: Option<String>,
    message: String,
    severity: Option<String>,
}

#[derive(Default)]
struct RunPatch {
    status: Option<String>,
    outcome: Option<String>,
    outcome_only_if_unknown: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FlushAction {
    None,
    PatchesOnly,
    Full,
}

struct StateBatcher {
    pool: Pool,
    events: Vec<PendingEvent>,
    run_patches: HashMap<String, RunPatch>,
    batch_size: usize,
}

impl StateBatcher {
    fn new(pool: Pool, batch_size: usize) -> Self {
        Self {
            pool,
            events: Vec::new(),
            run_patches: HashMap::new(),
            batch_size,
        }
    }

    fn record(&mut self, event: Event) -> FlushAction {
        if should_emit(&event.event_type) {
            self.events.push(PendingEvent {
                run_id: event.run_id.clone(),
                event_type: event.event_type.clone(),
                target: event.target.clone(),
                message: event.message.clone(),
                severity: event.severity.clone(),
            });
        }

        match event.event_type.as_str() {
            "target.started" => {
                self.patch_run(
                    &event.run_id,
                    RunPatch {
                        status: Some("running".into()),
                        ..Default::default()
                    },
                );
            }
            "finding.created" => {
                let severity = event.severity.as_deref().unwrap_or("info");
                if severity == "critical" || severity == "high" {
                    self.patch_run(
                        &event.run_id,
                        RunPatch {
                            outcome: Some("potentially_exploitable".into()),
                            ..Default::default()
                        },
                    );
                }
            }
            "target.completed" => {
                self.patch_run(
                    &event.run_id,
                    RunPatch {
                        status: Some("completed".into()),
                        outcome: Some("not_exploitable".into()),
                        outcome_only_if_unknown: true,
                    },
                );
            }
            _ => {}
        }

        if self.events.len() >= self.batch_size || event.event_type == "target.completed" {
            return FlushAction::Full;
        }

        if matches!(
            event.event_type.as_str(),
            "target.started" | "finding.created"
        ) {
            return FlushAction::PatchesOnly;
        }

        FlushAction::None
    }

    fn patch_run(&mut self, run_id: &str, patch: RunPatch) {
        let entry = self.run_patches.entry(run_id.to_string()).or_default();

        if patch.status.is_some() {
            entry.status = patch.status;
        }

        if patch.outcome.is_some() {
            entry.outcome = patch.outcome;
            entry.outcome_only_if_unknown = patch.outcome_only_if_unknown;
        }
    }

    async fn flush_patches(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.run_patches.is_empty() {
            return Ok(());
        }

        let patches = std::mem::take(&mut self.run_patches);
        let mut client = self.pool.get().await?;

        if let Err(e) = Self::apply_patches(&mut client, &patches).await {
            for (run_id, patch) in patches {
                self.patch_run(&run_id, patch);
            }
            return Err(e);
        }

        Ok(())
    }

    async fn flush(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.events.is_empty() && self.run_patches.is_empty() {
            return Ok(());
        }

        let events = std::mem::take(&mut self.events);
        let patches = std::mem::take(&mut self.run_patches);
        let mut client = self.pool.get().await?;
        let tx = client.transaction().await?;

        let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
            if !events.is_empty() {
                let run_ids: Vec<&str> = events.iter().map(|e| e.run_id.as_str()).collect();
                let event_types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
                let targets: Vec<Option<&str>> =
                    events.iter().map(|e| e.target.as_deref()).collect();
                let messages: Vec<&str> = events.iter().map(|e| e.message.as_str()).collect();
                let severities: Vec<Option<&str>> =
                    events.iter().map(|e| e.severity.as_deref()).collect();

                tx.execute(
                    "INSERT INTO events (run_id, event_type, target, message, severity)
                     SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::text[])",
                    &[&run_ids, &event_types, &targets, &messages, &severities],
                )
                .await?;
            }

            Self::apply_patches_tx(&tx, &patches).await?;
            tx.commit().await?;
            Ok(())
        }
        .await;

        if result.is_err() {
            self.events = events;
            for (run_id, patch) in patches {
                self.patch_run(&run_id, patch);
            }
        }

        result
    }

    async fn flush_with_retry(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut last_err: Option<Box<dyn std::error::Error + Send + Sync>> = None;

        for attempt in 0..5 {
            match self.flush().await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last_err = Some(e);
                    if attempt < 4 {
                        tokio::time::sleep(Duration::from_millis(50 * (attempt as u64 + 1))).await;
                    }
                }
            }
        }

        Err(last_err.unwrap())
    }

    async fn apply_patches(
        client: &mut deadpool_postgres::Object,
        patches: &HashMap<String, RunPatch>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for (run_id, patch) in patches {
            if let Some(status) = &patch.status {
                client
                    .execute(
                        "UPDATE runs SET status = $2 WHERE run_id = $1",
                        &[run_id, status],
                    )
                    .await?;
            }

            if let Some(outcome) = &patch.outcome {
                if patch.outcome_only_if_unknown {
                    client
                        .execute(
                            "UPDATE runs SET outcome = $2 WHERE run_id = $1 AND outcome = 'unknown'",
                            &[run_id, outcome],
                        )
                        .await?;
                } else {
                    client
                        .execute(
                            "UPDATE runs SET outcome = $2 WHERE run_id = $1",
                            &[run_id, outcome],
                        )
                        .await?;
                }
            }
        }

        Ok(())
    }

    async fn apply_patches_tx(
        tx: &tokio_postgres::Transaction<'_>,
        patches: &HashMap<String, RunPatch>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for (run_id, patch) in patches {
            if let Some(status) = &patch.status {
                tx.execute(
                    "UPDATE runs SET status = $2 WHERE run_id = $1",
                    &[run_id, status],
                )
                .await?;
            }

            if let Some(outcome) = &patch.outcome {
                if patch.outcome_only_if_unknown {
                    tx.execute(
                        "UPDATE runs SET outcome = $2 WHERE run_id = $1 AND outcome = 'unknown'",
                        &[run_id, outcome],
                    )
                    .await?;
                } else {
                    tx.execute(
                        "UPDATE runs SET outcome = $2 WHERE run_id = $1",
                        &[run_id, outcome],
                    )
                    .await?;
                }
            }
        }

        Ok(())
    }
}

async fn emit(state: &mut StateBatcher, event: Event) {
    match state.record(event) {
        FlushAction::None => {}
        FlushAction::PatchesOnly => {
            if let Err(e) = state.flush_patches().await {
                eprintln!("patch flush error: {}", e);
            }
        }
        FlushAction::Full => {
            if let Err(e) = state.flush_with_retry().await {
                eprintln!("state flush error: {}", e);
            }
        }
    }
}

async fn fail_assert(
    state: &mut StateBatcher,
    job: &Job,
    failed: &mut bool,
    severity: String,
    message: String,
) {
    *failed = true;

    emit(
        state,
        Event {
            event_type: "assert.failed".into(),
            run_id: job.run_id.clone(),
            target: Some(job.target.clone()),
            message,
            severity: Some(severity),
        },
    )
    .await;
}

fn create_pool(database_url: &str) -> Pool {
    let mut cfg = deadpool_postgres::Config::new();
    cfg.url = Some(database_url.to_string());

    let max_size = env::var("PG_POOL_SIZE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12);
    cfg.pool = Some(deadpool_postgres::PoolConfig {
        max_size,
        timeouts: deadpool_postgres::Timeouts {
            wait: Some(Duration::from_secs(30)),
            ..Default::default()
        },
        ..Default::default()
    });

    cfg.create_pool(Some(Runtime::Tokio1), NoTls)
        .expect("postgres pool")
}

async fn migrate_schema(pool: &Pool) {
    let client = match pool.get().await {
        Ok(client) => client,
        Err(e) => {
            eprintln!("postgres connection error during migrate: {}", e);
            return;
        }
    };

    if let Err(e) = client
        .batch_execute(
            "
            CREATE TABLE IF NOT EXISTS runs (
                run_id TEXT PRIMARY KEY,
                status TEXT NOT NULL DEFAULT 'queued',
                outcome TEXT NOT NULL DEFAULT 'unknown',
                scope_id TEXT NOT NULL,
                test_pack TEXT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            );

            CREATE TABLE IF NOT EXISTS events (
                id BIGSERIAL PRIMARY KEY,
                run_id TEXT NOT NULL REFERENCES runs(run_id) ON DELETE CASCADE,
                event_type TEXT NOT NULL,
                target TEXT,
                message TEXT NOT NULL,
                severity TEXT,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            );

            CREATE INDEX IF NOT EXISTS events_run_id_id_idx ON events (run_id, id);
            ",
        )
        .await
    {
        eprintln!("postgres migrate error: {}", e);
    }
}

fn interpolate(input: &str, vars: &HashMap<String, String>) -> String {
    let mut output = input.to_string();

    for (key, value) in vars {
        let token = format!("{{{{ {} }}}}", key);
        let token_no_spaces = format!("{{{{{}}}}}", key);

        output = output.replace(&token, value);
        output = output.replace(&token_no_spaces, value);
    }

    output
}

fn interpolate_map(
    input: &HashMap<String, String>,
    vars: &HashMap<String, String>,
) -> HashMap<String, String> {
    input
        .iter()
        .map(|(k, v)| {
            (
                interpolate(k, vars),
                interpolate(v, vars),
            )
        })
        .collect()
}

fn interpolate_json(value: &serde_json::Value, vars: &HashMap<String, String>) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(interpolate(s, vars)),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(|v| interpolate_json(v, vars)).collect())
        }
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::new();

            for (k, v) in map {
                out.insert(interpolate(k, vars), interpolate_json(v, vars));
            }

            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

fn load_test_packs(dir: &str) -> TestPackCache {
    let mut cache = HashMap::new();

    for entry in fs::read_dir(dir).expect("failed to read test-packs dir") {
        let entry = entry.expect("bad test-pack entry");
        let path = entry.path();

        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }

        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .expect("bad test-pack filename")
            .to_string();

        let raw = fs::read_to_string(&path).expect("failed to read test pack");
        let pack: TestPack = serde_yaml::from_str(&raw).expect("failed to parse test pack");

        cache.insert(name, Arc::new(pack));
    }

    cache
}

fn should_emit(event_type: &str) -> bool {
    if env::var("BENCHMARK_MODE").unwrap_or_else(|_| "false".into()) == "true" {
        return false;
    }

    let verbose = env::var("HELLION_VERBOSE_EVENTS")
        .unwrap_or_else(|_| "true".into());

    if verbose == "true" {
        return true;
    }

    matches!(
        event_type,
        "target.completed"
            | "finding.created"
            | "scope.blocked"
            | "test_pack.error"
            | "request.error"
            | "assert.failed"
            | "extract.failed"
    )
}