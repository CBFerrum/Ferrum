//! Where the server keeps files (docs/plan/stateless.md, D6; docs/operations.md, "Object storage").
//!
//! * **The object store** the settings name (crate::objects::s3), and nothing else outside development and tests, which
//!   keep files in a directory ([`decide`]). The server never makes, keys or fills a store: it is given one, and the doctor
//!   says what the bucket lacks. `ferrum task bucket_set_up` (crate::tasks) is the one thing that changes a bucket.
//! * **Deletions that wait** ([`Releases`], `files_delete_after_days`): names are never used again for other bytes, so a
//!   database restored to a moment within the backups' reach needs only that the objects it names still exist.
//! * **The bucket** asked what the doctor checks, and set up on request ([`Bucket`]).

use crate::objects::Objects;
use crate::state::Settings;
use ferrum_kernel::doctor::Finding;
use ferrum_kernel::{Db, Error, Kernel, Result};
use futures_util::StreamExt;
use opendal::Operator;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

/// Keys a restore never needs (an upload not yet recorded, a probe): deleted at once, and never moved.
fn transient(key: &str) -> bool {
    key.starts_with("uploads/") || key.starts_with(".ferrum-probe/")
}

/// Keys written again after a deletion: everything but a workspace's files (`tenants/<workspace>/<id>`, a new id each
/// time). An interface's or an app's files are named by their content's digest.
fn reused(key: &str) -> bool {
    !key.starts_with("tenants/")
}

/// Deletions of stored objects that wait `files_delete_after_days` (0: at once, as before), kept in `object_release`.
/// Without a database (a task's source store, tests) nothing waits.
#[derive(Default)]
pub struct Releases {
    db: Option<Db>,
    days: AtomicU64,
}

impl Releases {
    pub fn new(db: Db, days: u64) -> Releases {
        Releases { db: Some(db), days: AtomicU64::new(days) }
    }

    /// The setting's value now (it applies at once).
    pub fn set_days(&self, days: u64) {
        self.days.store(days, Ordering::Relaxed);
    }

    /// Records that `key` goes once the days have passed, and whether it did; false: delete it now.
    pub async fn defer(&self, key: &str) -> Result<bool> {
        let days = self.days.load(Ordering::Relaxed);
        let Some(db) = self.db.as_ref().filter(|_| days > 0 && !transient(key)) else { return Ok(false) };
        let mut tx = db.system_tx().await?;
        sqlx::query("insert into object_release (key, due_at) values ($1, now() + make_interval(days => $2)) on conflict (key) do nothing")
            .bind(key)
            .bind(i32::try_from(days).unwrap_or(i32::MAX))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// `key` is being written again: its waiting deletion, if any, is called off. Waits while a sweep deletes it, so the
    /// write lands after the deletion ([`sweep`]).
    pub async fn take_back(&self, key: &str) -> Result<()> {
        let Some(db) = self.db.as_ref().filter(|db| reused(key) && !transient(key) && !db.fenced()) else { return Ok(()) };
        let mut tx = db.system_tx().await?;
        sqlx::query("delete from object_release where key = $1").bind(key).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Whether `key`'s deletion waits (only a key that may be written again is asked).
    pub async fn pending(&self, key: &str) -> Result<bool> {
        let Some(db) = self.db.as_ref().filter(|_| reused(key) && !transient(key)) else { return Ok(false) };
        let mut tx = db.system_tx().await?;
        let waiting: bool = sqlx::query_scalar("select exists (select 1 from object_release where key = $1)").bind(key).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(waiting)
    }
}

/// Deletes the objects whose time has come, a hundred at a time, each while its row is held: a write of the same key
/// waits for the deletion and then stays ([`Releases::take_back`]). Every replica may sweep at once (each takes rows
/// nobody holds; a deletion done twice is harmless). Nothing while a restore or a planned move holds outward work: a
/// restored copy started beside the live installation deletes nothing of a store they share.
pub async fn sweep(k: &Kernel, objects: &Objects) -> Result<usize> {
    if k.holding().await? {
        return Ok(0);
    }
    let mut tx = k.db.system_tx().await?;
    let keys: Vec<String> =
        sqlx::query_scalar("select key from object_release where due_at <= now() order by due_at limit 100 for update skip locked").fetch_all(&mut *tx).await?;
    for key in &keys {
        objects.remove_now(key).await?;
    }
    sqlx::query("delete from object_release where key = any($1)").bind(&keys).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(keys.len())
}

/// Whether this replica's object store has answered (rule 8). A replica is not ready until a write-then-read probe has
/// worked since it started, so that a rollout with a wrong bucket or wrong keys never takes traffic and the replicas
/// before it keep serving. After that, a failure no longer takes it out of service (that would turn a fault of the store
/// into an outage of every replica at once): the doctor and the metrics say so instead.
#[derive(Default)]
pub struct Health {
    answered: std::sync::atomic::AtomicBool,
    up: std::sync::atomic::AtomicBool,
    failures: AtomicU64,
    cause: std::sync::Mutex<Option<String>>,
}

impl Health {
    /// Whether a probe has worked since this replica started.
    pub fn answered(&self) -> bool {
        self.answered.load(Ordering::SeqCst)
    }

    /// `Ok` once the store has answered since this replica started; else why not, in plain words.
    pub fn ready(&self) -> std::result::Result<(), String> {
        if self.answered() {
            return Ok(());
        }
        let cause = self.cause.lock().ok().and_then(|c| c.clone()).unwrap_or_else(|| "it has not been asked yet".into());
        Err(format!("object storage has not taken a write and a read since this replica started: {cause}"))
    }

    /// Records one probe's outcome; what changed, for the log to say once.
    pub fn record(&self, outcome: &std::result::Result<(), String>) -> Change {
        let (answered, was_up) = (self.answered(), self.up.swap(outcome.is_ok(), Ordering::SeqCst));
        if let Ok(mut cause) = self.cause.lock() {
            *cause = outcome.as_ref().err().cloned();
        }
        match outcome {
            Ok(()) => {
                self.answered.store(true, Ordering::SeqCst);
                match (answered, was_up) {
                    (false, _) => Change::Ready,
                    (true, false) => Change::Back,
                    (true, true) => Change::None,
                }
            }
            Err(_) => {
                let first = self.failures.fetch_add(1, Ordering::SeqCst) == 0;
                match (answered, was_up) {
                    (false, _) if first => Change::NotReady,
                    (true, true) => Change::Down,
                    _ => Change::None,
                }
            }
        }
    }

    /// `ferrum_object_storage_up` (this replica's last probe) and `ferrum_object_storage_probe_failures_total`.
    pub fn metrics_text(&self) -> String {
        format!(
            "# HELP ferrum_object_storage_up 1 when this replica's last write-then-read probe of object storage worked.\n\
             # TYPE ferrum_object_storage_up gauge\nferrum_object_storage_up {}\n\
             # HELP ferrum_object_storage_probe_failures_total Probes of object storage that failed since this replica started.\n\
             # TYPE ferrum_object_storage_probe_failures_total counter\nferrum_object_storage_probe_failures_total {}\n",
            u8::from(self.up.load(Ordering::SeqCst)),
            self.failures.load(Ordering::SeqCst)
        )
    }
}

/// What one probe changed of what is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    None,
    /// The first failure before any success: the replica is not ready.
    NotReady,
    /// The first success: the replica is ready.
    Ready,
    /// A failure after it was up.
    Down,
    /// Up again after a failure.
    Back,
}

/// One write-then-read probe of `objects`, recorded, and said in the log when it changes what is known (once each).
pub async fn check(objects: &Objects) {
    let outcome = objects.probe().await.map_err(|e| e.to_string());
    match (objects.health.record(&outcome), &outcome) {
        (Change::NotReady, Err(e)) => tracing::error!(
            "object storage did not take a write and a read ({e}): this replica is not ready until it does, and asks it again every \
             {} seconds",
            NOT_READY_EVERY.as_secs()
        ),
        (Change::Ready, _) if objects.health.failures.load(Ordering::SeqCst) > 0 => {
            tracing::warn!("object storage takes a write and a read now: this replica is ready")
        }
        (Change::Ready, _) => tracing::info!("object storage reachable"),
        (Change::Down, Err(e)) => {
            tracing::error!("object storage stopped taking a write and a read ({e}): this replica stays ready; the doctor and ferrum_object_storage_up say so")
        }
        (Change::Back, _) => tracing::warn!("object storage takes a write and a read again"),
        _ => {}
    }
}

/// Where this server keeps files, decided as it starts; refused, saying what to set instead (rule 8): the store the
/// settings name, or, only in development and tests (`development`), the local directory `files_dir`. A production start
/// without a bucket is refused: only this machine sees a directory, and a new container starts without it.
pub fn decide(settings: &Settings, development: bool) -> std::result::Result<Objects, String> {
    if !settings.s3_bucket.trim().is_empty() {
        return Objects::new(settings);
    }
    let dir = settings.files_dir.as_str();
    if !development {
        return Err(format!(
            "no object storage is set, and only development and tests keep files in a local directory ({dir}): only this machine sees it, and a \
             new container starts without it. Set FERRUM_S3_BUCKET, with FERRUM_S3_ENDPOINT, FERRUM_S3_REGION and the keys for an S3-compatible \
             store (docs/operations.md, \"Object storage\")"
        ));
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("files_dir: {e}"))?;
    Objects::local(dir)
}

/// How often a replica asks its store until it has answered, and after.
const NOT_READY_EVERY: std::time::Duration = std::time::Duration::from_secs(5);
const READY_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// Asks the store again on every replica that serves: until it has answered, every five seconds; after, every thirty, for
/// the metric and the log.
pub fn spawn_health(objects: std::sync::Arc<Objects>, mut stop: tokio::sync::watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let wait = if objects.health.answered() { READY_EVERY } else { NOT_READY_EVERY };
            tokio::select! {
                _ = stop.changed() => return,
                _ = tokio::time::sleep(wait) => {}
            }
            check(&objects).await;
        }
    })
}

/// The length and SHA-256 of an object, read once as a stream; `None` when it is not there.
pub async fn digest_of(op: &Operator, key: &str) -> std::result::Result<Option<(u64, String)>, opendal::Error> {
    // Asked first: a reader is lazy, and an object that is not there shows only once read, as another kind of error.
    match op.stat(key).await {
        Ok(_) => {}
        Err(e) if e.kind() == opendal::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut stream = op.reader(key).await?.into_bytes_stream(..).await?;
    let (mut hash, mut size) = (Sha256::new(), 0u64);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| opendal::Error::new(opendal::ErrorKind::Unexpected, e.to_string()))?;
        size += chunk.len() as u64;
        hash.update(&chunk);
    }
    Ok(Some((size, hex::encode(hash.finalize()))))
}

/// Every workspace file the database names (one workspace's with `tenant`), read through where it is kept: its length
/// and SHA-256 against its row's (the restore drill). Each that differs goes to `differs` as a line naming the workspace,
/// the file and how (never its contents); one that is not there is `check_files`'s to name. How many were read.
pub async fn verify_files(db: &Db, objects: &Objects, tenant: Option<uuid::Uuid>, mut differs: impl FnMut(String)) -> Result<u64> {
    type Row = (uuid::Uuid, uuid::Uuid, String, i64, String);
    let mut after: Option<(uuid::Uuid, uuid::Uuid)> = None;
    let mut read = 0u64;
    loop {
        let mut tx = db.system_tx().await?;
        let page: Vec<Row> = sqlx::query_as(
            "select tenant_id, id, storage_key, size_bytes, sha256 from file_object where ($1::uuid is null or tenant_id = $1) \
             and ($2::uuid is null or (tenant_id, id) > ($2, $3)) order by tenant_id, id limit 500",
        )
        .bind(tenant)
        .bind(after.map(|a| a.0))
        .bind(after.map(|a| a.1))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        after = page.last().map(|r| (r.0, r.1));
        let len = page.len();
        let mut found = futures_util::stream::iter(page).map(|row| async move { (objects.digest_found(&row.2).await, row) }).buffered(16);
        while let Some((got, (t, id, _, size, sha))) = found.next().await {
            let Some((s, h)) = got.map_err(|e| Error::Unavailable(format!("storage: {e}")))? else { continue };
            read += 1;
            if s != size as u64 || h != sha {
                differs(format!("workspace {t}, file {id}: {s} bytes whose SHA-256 is not its row's ({size} bytes)"));
            }
        }
        if len < 500 {
            return Ok(read);
        }
    }
}

/// What the doctor says of where files are, when no bucket is set: nothing to report with one. In production that is a
/// blocker (the server does not start so; this is what a task sees); elsewhere a directory is the development default.
pub fn place_findings(settings: &Settings, production: bool) -> Vec<Finding> {
    let dir = &settings.files_dir;
    if !settings.s3_bucket.trim().is_empty() {
        return vec![];
    }
    vec![if production {
        Finding::blocker(
            "storage.directory",
            format!("no object storage is set: files would be kept in a local directory ({dir}), which only this machine sees"),
            "Set the store (FERRUM_S3_BUCKET, FERRUM_S3_ENDPOINT, FERRUM_S3_REGION and its keys): Ferrum takes a bucket and operates none \
             (docs/operations.md, Object storage).",
        )
    } else {
        Finding::off("storage.directory", format!("files are kept in a local directory ({dir}): development and tests only"))
    }]
}

/// Days a deletion waits against how far back the database's backups reach (`files_delete_after_days`,
/// `backup_reach_days`): a database restored to a moment the files no longer reach names files that are gone, which in
/// production blocks.
pub fn lag_finding(lag_days: u64, reach_days: u64, production: bool) -> Finding {
    match (lag_days, reach_days) {
        (0, 0) => Finding::warning(
            "storage.deletions",
            "deleted files are removed from object storage at once: a database restored to an earlier moment names files that are gone",
            "Set backup_reach_days to how many days back the database's backups reach, and files_delete_after_days to at least that \
             (ferrum task settings set; docs/operations.md, Backups of files).",
        ),
        (lag, 0) => Finding::ok(
            "storage.deletions",
            format!("deleted files are kept {lag} days before they go (backup_reach_days is not set: keep it no longer than this)"),
        ),
        (lag, reach) if lag >= reach => {
            Finding::ok("storage.deletions", format!("deleted files are kept {lag} days, as far as the database's backups reach ({reach} days)"))
        }
        (lag, reach) => {
            let detail = format!(
                "deleted files are kept {lag} days and the database's backups reach {reach} days: a database restored to more than {lag} days back names files that are gone"
            );
            let fix = "Set files_delete_after_days to at least backup_reach_days (ferrum task settings set).";
            if production { Finding::blocker("storage.deletions", detail, fix) } else { Finding::warning("storage.deletions", detail, fix) }
        }
    }
}

/// How long the lifecycle rule `ferrum task bucket_set_up` adds keeps old versions: as far as the database's backups reach, or as
/// long as deletions wait, whichever is longer; 30 days when neither is said.
pub fn keep_versions_days(settings: &Settings) -> u64 {
    match settings.backup_reach_days.max(settings.files_delete_after_days) {
        0 => 30,
        days => days,
    }
}

/// The bucket's own configuration, asked over S3's API with the same keys or identity as the files: what the doctor
/// checks, and what `ferrum task bucket_set_up` sets up.
pub struct Bucket {
    /// `<endpoint>/<bucket>`, path-style as the files are reached.
    url: String,
    region: String,
    signer: reqsign_core::Signer<reqsign_aws_v4::Credential>,
    http: reqwest::Client,
}

/// One answer of the store: its status, its body, and the time it says it is.
struct Answer {
    status: u16,
    body: String,
    date: Option<chrono::DateTime<chrono::Utc>>,
}

impl Answer {
    fn code(&self) -> Option<&str> {
        between(&self.body, "<Code>", "</Code>")
    }
}

/// The text between the first `open` and the `close` after it.
fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    Some(&text[start..start + text[start..].find(close)?])
}

impl Bucket {
    /// The bucket `settings` name, `None` without one.
    pub fn new(settings: &Settings) -> std::result::Result<Option<Bucket>, String> {
        if settings.s3_bucket.trim().is_empty() {
            return Ok(None);
        }
        opendal::install_default();
        let region = if settings.s3_region.is_empty() { "auto" } else { settings.s3_region.as_str() };
        let endpoint = match settings.s3_endpoint.trim().trim_end_matches('/') {
            "" if region == "us-east-1" || region == "auto" => "https://s3.amazonaws.com".to_string(),
            "" => format!("https://s3.{region}.amazonaws.com"),
            e => e.to_string(),
        };
        let given = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty()).or_else(|| settings.revealed.get(&k.to_ascii_lowercase()).map(str::to_string));
        let ctx = reqsign_core::Context::new()
            .with_file_read(reqsign_file_read_tokio::TokioFileRead)
            .with_http_send(opendal::HttpTransporter::default())
            .with_env(reqsign_core::OsEnv);
        let mut chain = reqsign_core::ProvideCredentialChain::new().push(reqsign_aws_v4::DefaultCredentialProvider::new());
        if let (Some(id), Some(secret)) = (given("AWS_ACCESS_KEY_ID"), given("AWS_SECRET_ACCESS_KEY")) {
            let fixed = reqsign_aws_v4::StaticCredentialProvider::new(&id, &secret);
            chain = chain.push_front(match given("AWS_SESSION_TOKEN") {
                Some(token) => fixed.with_session_token(&token),
                None => fixed,
            });
        }
        let signer = reqsign_core::Signer::new(ctx, chain, reqsign_aws_v4::RequestSigner::new("s3", region));
        let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build().map_err(|e| e.to_string())?;
        Ok(Some(Bucket { url: format!("{endpoint}/{}", settings.s3_bucket.trim()), region: region.to_string(), signer, http }))
    }

    /// One request about the bucket (`at`: `?versioning`, `?lifecycle`, …, or `/<key>` for an object), signed unless
    /// `anonymous`.
    async fn call(&self, method: reqwest::Method, at: &str, body: Option<String>, anonymous: bool) -> std::result::Result<Answer, String> {
        let body = body.unwrap_or_default();
        let digest = Sha256::digest(body.as_bytes());
        let url = format!("{}{at}", self.url);
        let mut req = axum::http::Request::builder().method(method.clone()).uri(&url).header("x-amz-content-sha256", hex::encode(digest));
        if !body.is_empty() {
            use base64::Engine as _;
            req = req
                .header("x-amz-checksum-sha256", base64::engine::general_purpose::STANDARD.encode(digest))
                .header("x-amz-sdk-checksum-algorithm", "SHA256")
                .header("content-type", "application/xml");
        }
        let (mut parts, ()) = req.body(()).map_err(|e| e.to_string())?.into_parts();
        if !anonymous {
            self.signer.sign(&mut parts, None).await.map_err(|e| format!("signing the request: {e}"))?;
        }
        let sent = self.http.request(method, &url).headers(parts.headers).body(body).send().await.map_err(|e| format!("the store did not answer: {e}"))?;
        let status = sent.status().as_u16();
        let date = sent
            .headers()
            .get("date")
            .and_then(|d| d.to_str().ok())
            .and_then(|d| chrono::DateTime::parse_from_rfc2822(d).ok())
            .map(|d| d.with_timezone(&chrono::Utc));
        Ok(Answer { status, body: sent.text().await.unwrap_or_default(), date })
    }

    async fn get(&self, query: &str) -> std::result::Result<Answer, String> {
        self.call(reqwest::Method::GET, &format!("?{query}"), None, false).await
    }

    /// What the doctor says of the bucket: versioning, a rule ending old versions, encryption at rest, that it is not
    /// public, and the clock. `reach`: how far back the database's backups reach, in days (0: not said).
    pub async fn findings(&self, reach: u64) -> Vec<Finding> {
        let refused = |check: &'static str, e: String| {
            Finding::warning(check, format!("the store could not be asked: {e}"), "Check the store's address, keys and network.")
        };
        let mut out = Vec::new();
        out.push(match self.get("versioning").await {
            Ok(a) if a.status == 200 && between(&a.body, "<Status>", "</Status>") == Some("Enabled") => {
                Finding::ok("storage.versioning", "the bucket keeps versions")
            }
            Ok(a) if a.status == 200 => Finding::warning(
                "storage.versioning",
                "the bucket keeps no versions: a file deleted or written over outside Ferrum cannot be taken back",
                "Turn versioning on for the bucket (aws s3api put-bucket-versioning --versioning-configuration Status=Enabled), or run \
                 ferrum task bucket_set_up.",
            ),
            Ok(a) => not_asked("storage.versioning", &a, "versioning"),
            Err(e) => refused("storage.versioning", e),
        });
        out.push(match self.get("lifecycle").await {
            Ok(a) if a.status == 200 => lifecycle_finding(&a.body, reach),
            Ok(a) if a.code() == Some("NoSuchLifecycleConfiguration") => Finding::warning(
                "storage.lifecycle",
                "no lifecycle rule ends old versions: with versioning on, every deleted file is kept for ever",
                "Add a rule that expires noncurrent versions after as many days as the database's backups reach (and aborts unfinished \
                 uploads after a day), or run ferrum task bucket_set_up.",
            ),
            Ok(a) => not_asked("storage.lifecycle", &a, "lifecycle rules"),
            Err(e) => refused("storage.lifecycle", e),
        });
        out.push(match self.get("encryption").await {
            Ok(a) if a.status == 200 => Finding::ok(
                "storage.encryption",
                format!("objects are encrypted at rest ({})", between(&a.body, "<SSEAlgorithm>", "</SSEAlgorithm>").unwrap_or("by the store's default")),
            ),
            Ok(a) if a.code() == Some("ServerSideEncryptionConfigurationNotFoundError") => Finding::warning(
                "storage.encryption",
                "the bucket does not encrypt objects at rest",
                "Turn default encryption on for the bucket (SSE-S3 or a KMS key), or run ferrum task bucket_set_up.",
            ),
            Ok(a) => not_asked("storage.encryption", &a, "encryption at rest"),
            Err(e) => refused("storage.encryption", e),
        });
        out.push(self.public_finding().await);
        out.push(match self.get("location").await {
            Ok(a) => clock_finding(a.date, chrono::Utc::now()),
            Err(e) => refused("storage.clock", e),
        });
        out
    }

    /// Whether anyone may list or read the bucket: asked without keys (a listing, and a probe object written for it and
    /// deleted after), and by its policy's status where the store has one.
    async fn public_finding(&self) -> Finding {
        let fix = "Remove the bucket's public access (its policy, its ACL, and the account's public access block on AWS): files are served through \
                   Ferrum, or by short-lived links it signs.";
        let down = |e: String| Finding::warning("storage.public", format!("the store could not be asked: {e}"), "Check the store's address and network.");
        match self.call(reqwest::Method::GET, "?list-type=2&max-keys=1", None, true).await {
            Ok(a) if a.status == 200 => return Finding::blocker("storage.public", "anyone may list the bucket's files without keys", fix),
            Ok(_) => {}
            Err(e) => return down(e),
        }
        let probe = format!("/.ferrum-probe/public-{}", uuid::Uuid::now_v7());
        let read = match self.call(reqwest::Method::PUT, &probe, Some("public?".into()), false).await {
            Ok(a) if (200..300).contains(&a.status) => {
                let read = self.call(reqwest::Method::GET, &probe, None, true).await;
                let _ = self.call(reqwest::Method::DELETE, &probe, None, false).await;
                read.map(|a| a.status)
            }
            Ok(a) => Err(format!("a probe object was refused ({} {})", a.status, a.code().unwrap_or(""))),
            Err(e) => Err(e),
        };
        match read {
            Ok(200) => return Finding::blocker("storage.public", "anyone may read the bucket's files without keys", fix),
            Ok(_) => {}
            Err(e) => return down(e),
        }
        match self.get("policyStatus").await {
            Ok(a) if a.status == 200 && between(&a.body, "<IsPublic>", "</IsPublic>") == Some("true") => {
                Finding::blocker("storage.public", "the bucket's policy makes it public", fix)
            }
            _ => Finding::ok("storage.public", "the bucket is not public: requests without keys to list it or read a file are refused"),
        }
    }

    /// Makes the bucket when it is missing, and turns on what the doctor looks for when the bucket has none of it:
    /// versioning, lifecycle rules (old versions end after `keep_days`, the restore marker's after a day, unfinished
    /// uploads after a day) and default encryption (SSE-S3). A bucket's own rules or encryption are never replaced.
    pub async fn set_up(&self, keep_days: u64) -> std::result::Result<Vec<&'static str>, String> {
        let mut done = Vec::new();
        let put = |query: &'static str, body: Option<String>| self.call(reqwest::Method::PUT, query, body, false);
        let ok = |a: Answer, what: &str| {
            if (200..300).contains(&a.status) { Ok(()) } else { Err(format!("{what}: the store answered {} {}", a.status, a.code().unwrap_or(""))) }
        };
        let head = self.call(reqwest::Method::HEAD, "", None, false).await?;
        if head.status == 404 {
            // AWS makes a bucket outside us-east-1 only where its configuration names the region.
            let place = Some(&self.region).filter(|r| !["us-east-1", "auto", ""].contains(&r.as_str())).map(|r| {
                format!(r#"<CreateBucketConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><LocationConstraint>{r}</LocationConstraint></CreateBucketConfiguration>"#)
            });
            ok(put("", place).await?, "making the bucket")?;
            done.push("made the bucket");
        } else if !(200..300).contains(&head.status) {
            return Err(format!("the bucket answered {}", head.status));
        }
        let versioning = self.get("versioning").await?;
        if between(&versioning.body, "<Status>", "</Status>") != Some("Enabled") {
            let body = r#"<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Status>Enabled</Status></VersioningConfiguration>"#;
            ok(put("?versioning", Some(body.into())).await?, "turning versioning on")?;
            done.push("turned versioning on");
        }
        if self.get("lifecycle").await?.code() == Some("NoSuchLifecycleConfiguration") {
            let body = format!(
                r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><ID>ferrum-old-versions</ID><Filter><Prefix></Prefix></Filter><Status>Enabled</Status><NoncurrentVersionExpiration><NoncurrentDays>{}</NoncurrentDays></NoncurrentVersionExpiration><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule><Rule><ID>ferrum-restore-marker</ID><Filter><Prefix>{}</Prefix></Filter><Status>Enabled</Status><NoncurrentVersionExpiration><NoncurrentDays>1</NoncurrentDays></NoncurrentVersionExpiration></Rule></LifecycleConfiguration>"#,
                keep_days.max(1),
                ferrum_kernel::recovery::MARKER_PREFIX
            );
            ok(put("?lifecycle", Some(body)).await?, "adding the lifecycle rules")?;
            done.push("added lifecycle rules");
        }
        if self.get("encryption").await?.code() == Some("ServerSideEncryptionConfigurationNotFoundError") {
            let body = r#"<ServerSideEncryptionConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><ApplyServerSideEncryptionByDefault><SSEAlgorithm>AES256</SSEAlgorithm></ApplyServerSideEncryptionByDefault></Rule></ServerSideEncryptionConfiguration>"#;
            ok(put("?encryption", Some(body.into())).await?, "turning encryption on")?;
            done.push("turned default encryption on");
        }
        Ok(done)
    }
}

/// A store that does not answer what was asked (`NotImplemented`: some S3-compatible stores have no such thing).
fn not_asked(check: &'static str, a: &Answer, what: &str) -> Finding {
    match a.code() {
        Some("NotImplemented") => Finding::warning(
            check,
            format!("this store has no {what}"),
            "Use a store that has it (docs/references/object-stores.md), or protect the files another way (docs/operations.md, Backups of files).",
        ),
        code => Finding::warning(
            check,
            format!("the store answered {} {} when asked about {what}", a.status, code.unwrap_or("")),
            "Check that the keys may read the bucket's configuration.",
        ),
    }
}

/// A rule that is on and ends old versions, and after how many days, against the backups' reach.
fn lifecycle_finding(xml: &str, reach: u64) -> Finding {
    let days = xml
        .split("<Rule>")
        .skip(1)
        .filter(|rule| between(rule, "<Status>", "</Status>") == Some("Enabled"))
        .filter(|rule| between(rule, "<Prefix>", "</Prefix>").is_none_or(str::is_empty))
        .filter_map(|rule| between(rule, "<NoncurrentVersionExpiration>", "</NoncurrentVersionExpiration>"))
        .filter_map(|e| between(e, "<NoncurrentDays>", "</NoncurrentDays>")?.trim().parse::<u64>().ok())
        .max();
    match days {
        None => Finding::warning(
            "storage.lifecycle",
            "no lifecycle rule ends old versions of every file: with versioning on, every deleted file is kept for ever",
            "Add a rule over the whole bucket that expires noncurrent versions after as many days as the database's backups reach.",
        ),
        Some(d) if reach > 0 && d < reach => Finding::warning(
            "storage.lifecycle",
            format!("old versions end after {d} days, sooner than the database's backups reach ({reach} days)"),
            "Keep noncurrent versions at least as many days as the database's backups reach (backup_reach_days).",
        ),
        Some(d) => Finding::ok("storage.lifecycle", format!("old versions end after {d} days")),
    }
}

/// The store's clock against this machine's: a request signed more than 15 minutes from the store's time is refused.
pub fn clock_finding(store: Option<chrono::DateTime<chrono::Utc>>, here: chrono::DateTime<chrono::Utc>) -> Finding {
    let Some(at) = store else { return Finding::warning("storage.clock", "the store gave no time to compare with", "Check this machine's clock (NTP).") };
    let off = (here - at).num_seconds().abs();
    let fix = "Keep this machine's clock and the store's right (NTP): a request signed more than 15 minutes off is refused.";
    if off > 15 * 60 {
        Finding::blocker("storage.clock", format!("this machine's clock is {off} s off the store's: the store refuses its requests"), fix)
    } else if off > 5 * 60 {
        Finding::warning("storage.clock", format!("this machine's clock is {off} s off the store's"), fix)
    } else {
        Finding::ok("storage.clock", format!("this machine's clock is within {} s of the store's", off.max(1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_waits_and_what_is_written_again() {
        assert!(transient("uploads/t/x.parts/00000000000000000000") && transient(".ferrum-probe/x"));
        assert!(!transient("tenants/t/f") && !transient("ui/shell/d/index.js"));
        assert!(!reused("tenants/t/f"), "a workspace's file is a new id each time");
        assert!(reused("ui/shell/d/index.js") && reused("apps/x/1.0.0-abc.fapp"));
    }

    #[test]
    fn ready_once_the_store_has_answered_and_not_out_of_service_after() {
        let h = Health::default();
        assert!(h.ready().is_err(), "not ready before any probe");
        let down = Err("NoSuchBucket: The specified bucket does not exist".to_string());
        assert_eq!(h.record(&down), Change::NotReady, "said once");
        assert_eq!(h.record(&down), Change::None, "not said again");
        let why = h.ready().unwrap_err();
        assert!(why.contains("not taken a write and a read") && why.contains("NoSuchBucket"), "the cause, in the answer: {why}");
        assert!(h.metrics_text().contains("ferrum_object_storage_up 0\n") && h.metrics_text().contains("probe_failures_total 2\n"));
        assert_eq!(h.record(&Ok(())), Change::Ready, "a store that comes up late: ready, without a restart");
        assert!(h.ready().is_ok() && h.metrics_text().contains("ferrum_object_storage_up 1\n"));
        assert_eq!(h.record(&down), Change::Down);
        assert!(h.ready().is_ok(), "a failure after the first success does not take the replica out of service");
        assert!(h.metrics_text().contains("ferrum_object_storage_up 0\n"), "the metric says so");
        assert_eq!(h.record(&down), Change::None);
        assert_eq!(h.record(&Ok(())), Change::Back);
    }

    #[test]
    fn the_lag_against_the_backups_reach() {
        use ferrum_kernel::doctor::Level;
        for production in [true, false] {
            assert_eq!(lag_finding(0, 0, production).level, Level::Warning, "an explicit opt-out with no reach said");
            assert_eq!(lag_finding(30, 0, production).level, Level::Ok);
            assert_eq!(lag_finding(30, 30, production).level, Level::Ok);
            assert!(lag_finding(7, 30, production).detail.contains("more than 7 days back"));
        }
        // Deleted files kept for less than the backups reach: a blocker in production, a warning elsewhere (D3).
        assert_eq!(lag_finding(7, 30, true).level, Level::Blocker);
        assert_eq!(lag_finding(0, 30, true).level, Level::Blocker, "gone at once, with backups that reach a month");
        assert_eq!(lag_finding(7, 30, false).level, Level::Warning);
        // The defaults keep a deleted file as long as the backups reach.
        let defaults: Settings = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(lag_finding(defaults.files_delete_after_days, defaults.backup_reach_days, true).level, Level::Ok, "{defaults:?}");
    }

    #[test]
    fn lifecycle_rules_read_from_the_stores_answer() {
        use ferrum_kernel::doctor::Level;
        let rule = |status: &str, prefix: &str, days: &str| {
            format!(
                "<Rule><ID>r</ID><Filter><Prefix>{prefix}</Prefix></Filter><Status>{status}</Status><NoncurrentVersionExpiration><NoncurrentDays>{days}</NoncurrentDays></NoncurrentVersionExpiration></Rule>"
            )
        };
        let doc = |rules: &[String]| format!("<LifecycleConfiguration>{}</LifecycleConfiguration>", rules.concat());
        assert_eq!(lifecycle_finding(&doc(&[rule("Enabled", "", "30")]), 30).level, Level::Ok);
        assert_eq!(lifecycle_finding(&doc(&[rule("Enabled", "", "7")]), 30).level, Level::Warning, "sooner than the backups reach");
        assert_eq!(lifecycle_finding(&doc(&[rule("Disabled", "", "30")]), 30).level, Level::Warning, "a rule that is off");
        assert_eq!(lifecycle_finding(&doc(&[rule("Enabled", "restore-marker/", "1")]), 0).level, Level::Warning, "one prefix only");
        assert_eq!(
            lifecycle_finding(
                "<LifecycleConfiguration><Rule><Status>Enabled</Status><Expiration><Days>1</Days></Expiration></Rule></LifecycleConfiguration>",
                0
            )
            .level,
            Level::Warning
        );
    }

    #[test]
    fn the_clock_against_the_stores() {
        use ferrum_kernel::doctor::Level;
        let now = chrono::Utc::now();
        assert_eq!(clock_finding(Some(now - chrono::Duration::seconds(3)), now).level, Level::Ok);
        assert_eq!(clock_finding(Some(now + chrono::Duration::minutes(7)), now).level, Level::Warning);
        assert_eq!(clock_finding(Some(now - chrono::Duration::minutes(16)), now).level, Level::Blocker);
        assert_eq!(clock_finding(None, now).level, Level::Warning);
    }

    #[test]
    fn a_directory_only_in_development_and_a_bucket_anywhere() {
        let dir = std::env::temp_dir().join(format!("ferrum-decide-{}", uuid::Uuid::now_v7()));
        let mut settings: Settings = serde_json::from_value(serde_json::json!({"files_dir": dir.to_str().unwrap()})).unwrap();
        let why = decide(&settings, false).err().expect("production without a bucket is refused");
        assert!(why.contains("FERRUM_S3_BUCKET") && why.contains("only development and tests"), "says what to set: {why}");
        assert!(!dir.exists(), "refused before anything is made");
        assert_eq!(place_findings(&settings, true)[0].level, ferrum_kernel::doctor::Level::Blocker);
        assert_eq!(place_findings(&settings, false)[0].level, ferrum_kernel::doctor::Level::Off);
        assert!(!decide(&settings, true).unwrap().direct_uploads() && dir.is_dir(), "development keeps a directory");
        std::fs::remove_dir_all(&dir).unwrap();
        settings.s3_bucket = "files".into();
        settings.s3_region = "eu-west-1".into();
        assert!(decide(&settings, false).unwrap().direct_uploads(), "a bucket is the store, in production too");
        assert!(place_findings(&settings, true).is_empty() && !dir.exists());
    }
}
