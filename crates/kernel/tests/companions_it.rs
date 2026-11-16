//! Files for the programs beside the server (crates/kernel/src/companion.rs) against a real PostgreSQL
//! (`FERRUM_COMPANIONS_TEST_DATABASE_URL`; its public schema and the vault's are reset first): services declared by an
//! app and by the platform, each rendered from the settings and its owner's secrets alone; a name with two owners stops
//! the start; a setting a service needs and nobody set, or a closed vault, refuses the render before anything is written.
//!
//! The meeting server as Comms is to declare it (docs/app-contract.md, "Declaring a service"): its secret made by the
//! first render, once though two replicas render at once, and nobody's to change but its app's; every render on the
//! record, naming the service, never the value, under the cap on uses; a secret its rule keeps in the vault, or one a
//! person set under the name, refused; a rotation's overlap given to the next render as a second key. Where
//! `livekit-server` is installed (`FERRUM_TEST_LIVEKIT`, else the PATH's), it is started on the files written and called
//! with tokens signed by the vault: the version before and the current one both let in during the overlap, the version
//! before refused once it is over and LiveKit started again.

use ferrum_kernel::companion::{self, Companion, File, Rendering};
use ferrum_kernel::vault::{self, Kdf, New, Opened, Opener, Rule, Scope, Vault, Zeroizing};
use ferrum_kernel::{Db, DbConfig, Error, HookRegistry, Kernel, KernelSettings, Result};
use ferrum_meta::Catalog;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn url() -> String {
    std::env::var("FERRUM_COMPANIONS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_companions_test".into())
}

async fn replica() -> Db {
    Db::connect(DbConfig { url: url(), max_connections: 4, ..Default::default() }).await.expect("connect")
}

/// What a process with this password and no key file opens the vault with (Argon2id's costs low for the tests).
fn opener(password: Option<&str>) -> Opener {
    Opener {
        database_url: match password {
            Some(p) => format!("postgres://ferrum:{p}@localhost:5432/ferrum"),
            None => "postgres://ferrum@localhost:5432/ferrum".into(),
        },
        file: PathBuf::from("/nonexistent/ferrum-companions-it/root.key"),
        transit: None,
        command: None,
        kdf: Kdf { m_kib: 64, t: 1, p: 1 },
    }
}

fn core() -> Catalog {
    let core = ferrum_meta::catalog::load_package_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core")).expect("core");
    Catalog::from_packages(vec![core]).unwrap()
}

async fn reset(db: &Db) {
    sqlx::raw_sql("drop schema if exists public cascade; drop schema if exists vault cascade; create schema public;").execute(&db.pool).await.expect("reset");
    db.migrate().await.expect("migrate");
}

/// A program of the platform's that takes settings of ours and no secret.
struct Greeter;

#[async_trait::async_trait]
impl Companion for Greeter {
    fn files(&self) -> &'static [&'static str] {
        &["greeter.conf"]
    }
    async fn render(&self, r: &Rendering<'_>) -> Result<Vec<File>> {
        let address = r.needs("FERRUM_GREETER_URL")?;
        let shown = r.setting("FERRUM_GREETER_NAME").unwrap_or_else(|| "Ferrum".into());
        Ok(vec![File::new("greeter.conf", format!("address = {address}\nname = {shown}\nservice = {}\n", r.service()).into_bytes(), false)])
    }
}

/// LiveKit, as Comms is to declare it: `livekit.yaml`, LiveKit's configuration file, with each kept version of the
/// secret `meet.secret` under the API key `ferrum.<version>` (a token's `iss` names the version that signed it), made by
/// the vault and revealed to Comms (which signs with it) and to this file alone. The ports are the test's settings here;
/// in a container they are LiveKit's own (7880, 7881, 7882).
struct Meet;

#[async_trait::async_trait]
impl Companion for Meet {
    fn files(&self) -> &'static [&'static str] {
        &["livekit.yaml"]
    }
    fn made(&self) -> &'static [&'static str] {
        &["meet.secret"]
    }
    async fn render(&self, r: &Rendering<'_>) -> Result<Vec<File>> {
        r.made("meet.secret", Rule { reveal: true, overlap_secs: 86_400, ..Default::default() }).await?;
        let port = |name: &str, default: &str| r.setting(name).unwrap_or_else(|| default.to_string());
        let mut yaml = Zeroizing::new(String::with_capacity(1024));
        let _ = write!(
            yaml,
            "port: {}\nbind_addresses: [\"127.0.0.1\"]\nrtc:\n  tcp_port: {}\n  udp_port: {}\n  use_external_ip: false\n  node_ip: 127.0.0.1\nkeys:\n",
            port("FERRUM_MEET_TEST_PORT", "7880"),
            port("FERRUM_MEET_TEST_TCP_PORT", "7881"),
            port("FERRUM_MEET_TEST_UDP_PORT", "7882"),
        );
        for v in r.versions("meet.secret").await? {
            let secret = std::str::from_utf8(&v.value).map_err(|_| Error::invalid("meet.secret is text"))?;
            let _ = writeln!(yaml, "  \"ferrum.{}\": \"{secret}\"", v.version);
        }
        Ok(vec![File { name: "livekit.yaml".into(), bytes: Zeroizing::new(yaml.as_bytes().to_vec()), secret: true }])
    }
}

/// A service that writes a secret its owner keeps for signing only, or one that a person set: each refused.
struct Peeking(&'static str);

#[async_trait::async_trait]
impl Companion for Peeking {
    fn files(&self) -> &'static [&'static str] {
        &["peek.txt"]
    }
    fn made(&self) -> &'static [&'static str] {
        &["typed.token"]
    }
    async fn render(&self, r: &Rendering<'_>) -> Result<Vec<File>> {
        if self.0 == "typed.token" {
            r.made("typed.token", Rule { reveal: true, ..Default::default() }).await?;
        }
        let value = r.secret(self.0).await?;
        Ok(vec![File { name: "peek.txt".into(), bytes: value, secret: true }])
    }
}

/// A program whose key it makes itself and hands over, as the store Compose bundles does: the service writes that key
/// back for it, and refuses any other installation, where the key was not handed over by it.
struct Handed;

#[async_trait::async_trait]
impl Companion for Handed {
    fn files(&self) -> &'static [&'static str] {
        &["handed.key"]
    }
    fn made(&self) -> &'static [&'static str] {
        &["handed.key"]
    }
    async fn refusal(&self, r: &Rendering<'_>) -> Result<Option<String>> {
        let origin = r.vault().origin(r.kernel(), "handed.key").await?;
        let ours =
            origin.as_ref().is_some_and(|o| (o.verb == "adopt" && o.by.as_deref() == Some("the program")) || o.service.as_deref() == Some("peek-handed"));
        Ok((!ours).then(|| format!("handed.key was not handed over by the program ({origin:?})")))
    }
    async fn render(&self, r: &Rendering<'_>) -> Result<Vec<File>> {
        r.keep("handed.key", Rule { reveal: true, overlap_secs: 3600, ..Default::default() }).await?;
        let mut bytes = Zeroizing::new(Vec::new());
        for v in r.versions("handed.key").await? {
            bytes.extend_from_slice(&v.value);
            bytes.push(b'\n');
        }
        Ok(vec![File { name: "handed.key".into(), bytes, secret: true }])
    }
}

/// The settings the server resolved, as a render is given them.
fn settings(pairs: Vec<(&'static str, String)>) -> impl Fn(&str) -> Option<String> + Sync {
    move |name: &str| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| v.clone())
}

/// A replica's kernel with the platform's greeter, the app `talk` with its meeting server and the app `peek` with its
/// two peeking services, and their handles; its vault opened with `password` (none: the vault stays closed, as a server
/// with nothing to make its first slot with starts), and `more` registered.
async fn kernel(password: Option<&str>, more: impl FnOnce(&mut HookRegistry)) -> std::result::Result<(Arc<Kernel>, Vault, Vault), Error> {
    let db = replica().await;
    let opened = vault::open(&db, opener(password), false).await.unwrap();
    assert_eq!(opened == Opened::Closed, password.is_none(), "{opened:?}");
    let mut hooks = HookRegistry::default();
    hooks.add_companion("greeter", Arc::new(Greeter));
    let (mut talk, mut peek) = (None, None);
    hooks.app("talk", &[], |h| {
        talk = Some(h.vault());
        h.add_companion("meet", Arc::new(Meet));
    });
    hooks.app("peek", &[], |h| {
        peek = Some(h.vault());
        h.add_companion("peek-signing", Arc::new(Peeking("signing.key")));
        h.add_companion("peek-typed", Arc::new(Peeking("typed.token")));
        h.add_companion("peek-handed", Arc::new(Handed));
    });
    more(&mut hooks);
    let k = Kernel::boot(db, core(), hooks, KernelSettings::default(), None).await?;
    Ok((k, talk.unwrap(), peek.unwrap()))
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferrum-companions-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The installation's vault events of `name` with `action`, as `ferrum task vault_events` reads them.
async fn events(k: &Kernel, action: &str, name: &str) -> Vec<serde_json::Value> {
    vault::events(&k.db, None, 1000).await.unwrap().into_iter().filter(|e| e.action == action && e.name == name).map(|e| e.details).collect()
}

#[test]
fn services_are_declared_by_their_owners_and_rendered_from_the_database_and_the_vault() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let db = replica().await;
    reset(&db).await;

    // ── services declared by the platform and by apps: listed with their owners, files and what the vault makes ──
    let (k, talk, peek) = kernel(Some("ferrum"), |_| {}).await.expect("boot");
    let none = settings(vec![]);
    let listed: Vec<(String, String, Vec<String>, Vec<String>, bool)> =
        k.companions(&none).await.into_iter().map(|d| (d.name, d.owner, d.files, d.made, d.refused.is_some())).collect();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert_eq!(
        listed,
        [
            ("greeter".to_string(), "platform".to_string(), s(&["greeter.conf"]), s(&[]), false),
            ("meet".into(), "talk".into(), s(&["livekit.yaml"]), s(&["meet.secret"]), false),
            ("peek-handed".into(), "peek".into(), s(&["handed.key"]), s(&["handed.key"]), true),
            ("peek-signing".into(), "peek".into(), s(&["peek.txt"]), s(&["typed.token"]), false),
            ("peek-typed".into(), "peek".into(), s(&["peek.txt"]), s(&["typed.token"]), false),
        ],
        "the service whose key nobody handed over here says it cannot be rendered"
    );

    // ── a service of settings alone: rendered from them, as the server resolved them ──
    let given = settings(vec![("FERRUM_GREETER_URL", "http://greeter:8080".into()), ("FERRUM_GREETER_NAME", " Acme ".into())]);
    let rendered = k.render("greeter", &given).await.expect("render");
    assert_eq!((rendered.service.as_str(), rendered.owner.as_str(), rendered.group_reads), ("greeter", "platform", false));
    assert_eq!(std::str::from_utf8(&rendered.files[0].bytes).unwrap(), "address = http://greeter:8080\nname = Acme\nservice = greeter\n");
    let dir = scratch("greeter");
    let written = companion::write(&dir, &rendered).expect("write");
    assert_eq!(written.files, [("greeter.conf".to_string(), false)]);
    assert_eq!(std::fs::read(dir.join("greeter.conf")).unwrap(), rendered.files[0].bytes.as_slice());
    std::fs::remove_dir_all(&dir).unwrap();
    // A setting it needs and nobody set: refused, naming it, with nothing to write.
    let e = k.render("greeter", &settings(vec![("FERRUM_GREETER_URL", "  ".into())])).await.unwrap_err().to_string();
    assert!(e.contains("the service greeter needs the setting FERRUM_GREETER_URL, which is not set"), "{e}");
    let e = k.render("nobody", &given).await.unwrap_err().to_string();
    assert!(e.contains("service nobody") && e.contains("ferrum task services"), "{e}");

    // ── the meeting server: its secret made by the first render, once, though two replicas render at once ──
    let (k2, _, _) = kernel(Some("ferrum"), |_| {}).await.expect("a second replica");
    let (a, b) = tokio::join!(k.render("meet", &none), k2.render("meet", &none));
    let (a, b) = (a.expect("one replica"), b.expect("the other"));
    assert_eq!(a.files[0].bytes.as_slice(), b.files[0].bytes.as_slice(), "the same secret in both");
    let made = events(&k, "make", "meet.secret").await;
    assert_eq!(made.len(), 1, "made once: {made:?}");
    assert_eq!((made[0]["owner"].as_str(), made[0]["service"].as_str(), made[0]["version"].as_i64()), (Some("talk"), Some("meet"), Some(1)));
    let yaml = std::str::from_utf8(&a.files[0].bytes).unwrap().to_string();
    let secret = yaml.lines().find_map(|l| l.strip_prefix("  \"ferrum.1\": \"")).and_then(|l| l.strip_suffix('"')).expect("its key").to_string();
    assert_eq!(secret.len(), 43, "32 random bytes as base64url");
    // Every render on the record, naming the service; never the value, anywhere in the record.
    let rendered = events(&k, "render", "meet.secret").await;
    assert_eq!(rendered.len(), 2);
    assert!(rendered.iter().all(|d| d["service"] == "meet" && d["owner"] == "talk" && d["version"] == 1), "{rendered:?}");
    let all = serde_json::to_string(&vault::events(&k.db, None, 1000).await.unwrap()).unwrap();
    assert!(!all.contains(&secret), "no value in the vault's record");
    // The app's own handle uses it: what LiveKit is given is what Comms signs with.
    let d = talk.describe(&k, Scope::Installation, "meet.secret").await.unwrap().expect("there");
    assert_eq!((d.managed_by.as_deref(), d.rule.reveal, d.version), (Some("talk"), true, 1), "its app's to manage");
    let signed = talk.sign(&k, Scope::Installation, "meet.secret", b"header.payload").await.unwrap();
    assert_eq!(signed, ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes()), b"header.payload").as_ref());
    // People's and the operator's routes leave it to its app.
    let e = k.vault_regenerate(Scope::Installation, "talk", "meet.secret").await.unwrap_err();
    assert!(matches!(e, Error::Conflict(_)) && e.to_string().contains("managed by its app, talk"), "{e}");

    // ── what is refused: a secret kept for signing, a person's under a name the vault makes, a service's own records ──
    peek.set(&k, Scope::Installation, "signing.key", New::Random(32), Rule::default()).await.unwrap();
    let e = k.render("peek-signing", &none).await.unwrap_err().to_string();
    assert!(e.contains("signing.key's rule keeps its value in the vault: it is used, never written to a file"), "{e}");
    peek.set(
        &k,
        Scope::Installation,
        "typed.token",
        New::Value(Zeroizing::new(b"typed-by-a-person-0123456789".to_vec())),
        Rule { reveal: true, ..Default::default() },
    )
    .await
    .unwrap();
    let e = k.render("peek-typed", &none).await.unwrap_err();
    assert!(matches!(e, Error::Conflict(_)) && e.to_string().contains("typed.token was set by a person"), "{e}");
    let e = k.rotate_companion("peek-typed", &none).await.unwrap_err().to_string();
    assert!(e.contains("not a secret the vault keeps for the service peek-typed"), "{e}");
    assert!(events(&k, "render", "signing.key").await.is_empty() && events(&k, "render", "typed.token").await.is_empty(), "neither given");

    // ── under the cap on uses: the first hundred renders an hour recorded one by one, the rest counted ──
    for _ in 0..105 {
        k.render("meet", &none).await.unwrap();
    }
    assert_eq!(events(&k, "render", "meet.secret").await.len(), vault::USES_PER_HOUR as usize, "the rest wait for the hour's summary");

    // ── a rotation, for the operator: the version before given to the next render too, while its overlap lasts ──
    let rotated = k.rotate_companion("meet", &none).await.expect("rotate");
    assert_eq!((rotated.len(), rotated[0].version, rotated[0].previous_until.is_some()), (1, 2, true));
    let rot = events(&k, "rotate", "meet.secret").await;
    assert_eq!((rot.len(), rot[0]["by"].as_str(), rot[0]["service"].as_str()), (1, Some("operator"), Some("meet")));
    let both = k.render("meet", &none).await.unwrap();
    let yaml = std::str::from_utf8(&both.files[0].bytes).unwrap();
    let keys: Vec<&str> = yaml.lines().filter_map(|l| l.strip_prefix("  \"")).filter_map(|l| l.split('"').next()).collect();
    assert_eq!(keys, ["ferrum.2", "ferrum.1"], "the current, then the one before: {yaml}");
    assert!(yaml.contains(&secret), "the one before is the first one");

    // ── a key the program hands over itself: refused, making and writing nothing, where it did not hand it over ──
    let refused = |e: Error| match e {
        Error::Conflict(why) => why,
        other => panic!("expected a refusal, got {other:?}"),
    };
    let why = refused(k.render("peek-handed", &none).await.unwrap_err());
    assert!(why.starts_with("the service peek-handed is not rendered here: handed.key was not handed over by the program"), "{why}");
    assert!(refused(k.rotate_companion("peek-handed", &none).await.unwrap_err()).contains("not handed over"), "a rotation too");
    assert!(peek.describe(&k, Scope::Installation, "handed.key").await.unwrap().is_none(), "nothing made");
    peek.set(
        &k,
        Scope::Installation,
        "handed.key",
        New::Value(Zeroizing::new(b"a-persons-key-0123456789".to_vec())),
        Rule { reveal: true, ..Default::default() },
    )
    .await
    .unwrap();
    assert!(refused(k.render("peek-handed", &none).await.unwrap_err()).contains("not handed over"), "a person's key is not the program's");
    let d = peek.describe(&k, Scope::Installation, "handed.key").await.unwrap().unwrap();
    assert_eq!((d.version, d.managed_by.as_deref(), d.rule.overlap_secs), (1, None, 0), "left as the person set it");
    assert_eq!(peek.origin(&k, "handed.key").await.unwrap().map(|o| o.verb), Some("set".to_string()));
    assert!(events(&k, "render", "handed.key").await.is_empty() && events(&k, "manage", "handed.key").await.is_empty());
    // Handed over by the program: kept once though two replicas render at once, given the service's rule, and written.
    peek.remove(&k, Scope::Installation, "handed.key").await.unwrap();
    let handed = b"the-programs-own-key-0123456789".to_vec();
    peek.adopt(&k, Scope::Installation, "handed.key", New::Value(Zeroizing::new(handed.clone())), Rule { reveal: true, ..Default::default() }, "the program")
        .await
        .unwrap();
    let o = peek.origin(&k, "handed.key").await.unwrap().unwrap();
    assert_eq!((o.verb.as_str(), o.by.as_deref(), o.service.as_deref()), ("adopt", Some("the program"), None));
    assert!(k.companions(&none).await.iter().all(|d| d.refused.is_none()), "renderable here now");
    let (a, b) = tokio::join!(k.render("peek-handed", &none), k2.render("peek-handed", &none));
    let (a, b) = (a.expect("one"), b.expect("the other"));
    assert_eq!(
        (a.files[0].bytes.as_slice(), b.files[0].bytes.as_slice()),
        ([handed.as_slice(), b"\n"].concat().as_slice(), [handed.as_slice(), b"\n"].concat().as_slice())
    );
    let kept = events(&k, "manage", "handed.key").await;
    assert_eq!((kept.len(), kept[0]["service"].as_str()), (1, Some("peek-handed")), "kept once: {kept:?}");
    let d = peek.describe(&k, Scope::Installation, "handed.key").await.unwrap().unwrap();
    assert_eq!((d.version, d.managed_by.as_deref(), d.rule.overlap_secs), (1, Some("peek"), 3600), "its value as it was, under the service's rule");
    // Rotated by the vault for the service from then on: the next render gives both, and the record says so.
    let rotated = k.rotate_companion("peek-handed", &none).await.expect("rotate the kept key");
    assert_eq!((rotated[0].version, rotated[0].previous_until.is_some()), (2, true));
    let o = peek.origin(&k, "handed.key").await.unwrap().unwrap();
    assert_eq!((o.verb.as_str(), o.by.as_deref(), o.service.as_deref()), ("rotate", Some("operator"), Some("peek-handed")));
    let both = k.render("peek-handed", &none).await.unwrap();
    assert_eq!(both.files[0].bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()).count(), 2, "the new key and the handed one");

    // ── the meeting server itself, started on the file written, called with tokens the vault signed ──
    livekit(&k, &talk, &secret).await;

    // ── a closed vault (a fresh database, and nothing here to make its first slot): refused whatever the service needs ──
    reset(&db).await;
    let (closed, _, _) = kernel(None, |_| {}).await.expect("a server with nothing to make the vault's first slot starts with it closed");
    for service in ["greeter", "meet"] {
        let e = closed.render(service, &given).await.unwrap_err().to_string();
        assert!(e.starts_with("the vault is closed"), "{service}: {e}");
    }
    assert!(closed.rotate_companion("meet", &given).await.unwrap_err().to_string().starts_with("the vault is closed"));
    let listed = closed.companions(&given).await;
    assert!(listed.iter().all(|d| d.refused.as_deref().is_some_and(|r| r.starts_with("the vault is closed"))), "{listed:?}");

    // ── one name, two owners: the start stops, naming both ──
    let e = kernel(Some("ferrum"), |h| {
        h.app("other", &[], |h| {
            h.add_companion("meet", Arc::new(Greeter));
        });
    })
    .await
    .map(|_| ())
    .unwrap_err()
    .to_string();
    assert!(e.contains("the service meet is declared twice: by talk and by other"), "{e}");
}

/// LiveKit started on what the render of `meet` writes, and called with tokens `talk`'s handle signs (HS256, as Comms
/// signs them): during the overlap both versions are let in; once it is over and LiveKit has started again on a new
/// render, the version before (`first`, as the first render wrote it) is refused. Skipped, saying so, where LiveKit is
/// not installed.
async fn livekit(k: &Arc<Kernel>, talk: &Vault, first: &str) {
    let found = std::env::var("FERRUM_TEST_LIVEKIT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("livekit-server")).find(|p| p.is_file())));
    let Some(bin) = found else {
        println!("livekit-server is not installed (FERRUM_TEST_LIVEKIT): LiveKit was not started");
        return;
    };
    let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (port, tcp) = (free(), free());
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let ports = settings(vec![
        ("FERRUM_MEET_TEST_PORT", port.to_string()),
        ("FERRUM_MEET_TEST_TCP_PORT", tcp.to_string()),
        ("FERRUM_MEET_TEST_UDP_PORT", udp.to_string()),
    ]);
    let dir = scratch("livekit");
    let started: Arc<Mutex<Option<tokio::process::Child>>> = Arc::default();
    let start = |k: Arc<Kernel>| {
        let (bin, dir, started) = (bin.clone(), dir.clone(), started.clone());
        let ports = &ports;
        async move {
            let rendered = k.render("meet", ports).await.expect("render");
            companion::write(&dir, &rendered).expect("write");
            let child = tokio::process::Command::new(&bin)
                .arg("--config")
                .arg(dir.join("livekit.yaml"))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .expect("livekit-server");
            *started.lock().unwrap() = Some(child);
            for _ in 0..100 {
                if reqwest::get(format!("http://127.0.0.1:{port}/")).await.is_ok_and(|r| r.status().is_success()) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            panic!("LiveKit did not answer on {port}");
        }
    };
    let stop = || async {
        let child = started.lock().unwrap().take();
        if let Some(mut child) = child {
            let _ = child.kill().await;
        }
    };
    let b64 = |b: &[u8]| base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, b);
    let unsigned = |version: i32| {
        let now = chrono::Utc::now().timestamp();
        let claims =
            serde_json::json!({"iss": format!("ferrum.{version}"), "sub": "ferrum-test", "nbf": now - 10, "exp": now + 600, "video": {"roomList": true}});
        format!("{}.{}", b64(br#"{"alg":"HS256","typ":"JWT"}"#), b64(claims.to_string().as_bytes()))
    };
    // LiveKit's server API with a token: its answer's status.
    let ask = |token: String| async move {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/twirp/livekit.RoomService/ListRooms"))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("accept-encoding", "gzip")
            .body("{}")
            .send()
            .await
            .expect("LiveKit answers")
            .status()
            .as_u16()
    };
    // A token signed by the vault with the version named, as Comms signs one (it names the version in `iss`).
    let call = |version: i32| async move {
        let unsigned = unsigned(version);
        let (signed_with, signature) = talk.sign_at(k, Scope::Installation, "meet.secret", unsigned.as_bytes(), Some(version)).await.expect("sign");
        assert_eq!(signed_with, version);
        ask(format!("{unsigned}.{}", b64(&signature))).await
    };
    start(k.clone()).await;
    assert_eq!((call(2).await, call(1).await), (200, 200), "during the overlap, a token of either version is let in");
    assert_eq!(std::fs::metadata(dir.join("livekit.yaml")).map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777).unwrap(), 0o600);
    stop().await;
    // The overlap over (as an hour's upkeep finds it): LiveKit's next start is given the current version alone.
    let mut tx = k.db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    sqlx::query("update vault.secret set valid_until = now() - interval '1 second' where owner = 'talk' and name = 'meet.secret' and valid_until is not null")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    start(k.clone()).await;
    assert_eq!(call(2).await, 200, "the current version");
    let e = talk.sign_at(k, Scope::Installation, "meet.secret", b"x", Some(1)).await.unwrap_err();
    assert!(matches!(e, Error::NotFound(_)), "the vault no longer signs with the version before: {e}");
    let old = unsigned(1);
    let by_hand = ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, first.as_bytes()), old.as_bytes());
    assert_eq!(ask(format!("{old}.{}", b64(by_hand.as_ref()))).await, 401, "nor does LiveKit take it any more");
    stop().await;
    std::fs::remove_dir_all(&dir).unwrap();
    println!("LiveKit ({}) started on the rendered file and called with the vault's tokens", bin.display());
}
