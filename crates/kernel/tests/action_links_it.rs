//! Signed action links (docs/builder/decisions.md BD-23) against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the
//! database is reset first), set up by configuration alone on a custom entity: a quotation accepted by link with a typed
//! name, a rating from 1 to 5 with a comment, a link used twice refused the second time. And the ways past one: an
//! action it does not name, the issuer's fixed values overridden, another workspace's address, a revoked, expired or
//! unknown link (all answered alike), a code not given or wrong, two uses at once of a single-use link.

use ferrum_kernel::action_link::LinkSpec;
use ferrum_kernel::customize::NewCustomization;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::{Catalog, MessageDef};
use serde_json::{Map, Value, json};
use std::path::Path;
use uuid::Uuid;

fn catalog() -> Catalog {
    let dir = std::env::temp_dir().join(format!("ferrum-links-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn id_of(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn token(url: &str) -> String {
    url.rsplit('/').next().unwrap().to_string()
}

fn given(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

async fn workspace(k: &Kernel, slug: &str) -> Ctx {
    let tenant = k
        .create_tenant(NewTenant {
            slug: slug.into(),
            name: format!("{slug} Ltd"),
            admin_email: format!("admin@{slug}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["core".into()],
            settings: None,
        })
        .await
        .expect("workspace");
    let session = k.login(slug, &format!("admin@{slug}.test"), "Correct-Horse-9").await.unwrap();
    k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap()
}

/// The quotation, by configuration: accepting it takes a typed name, rating it a number from 1 to 5 and a comment, and
/// sending it issues a link and mails it.
fn quotation() -> NewCustomization {
    let entity = json!({
        "name": "XQuote", "label": "Quotation", "kind": "master", "title_field": "title",
        "fields": [
            {"name": "title", "type": "data", "required": true},
            {"name": "total", "type": "data"},
            {"name": "customer_email", "type": "data"},
            {"name": "margin", "type": "data"},
            {"name": "status", "type": "select", "options": ["Open", "Accepted"], "default": "Open"},
            {"name": "accepted_by", "type": "data"},
            {"name": "rating", "type": "int"},
            {"name": "feedback", "type": "data"},
            {"name": "cost", "type": "data", "perm_group": "internal"}
        ],
        "actions": [
            {"name": "accept", "label": "Accept the quotation", "permission": "write", "condition": "doc.status == 'Open'",
             "params": [{"name": "signed_by", "label": "Your full name", "type": "data", "required": true}],
             "effects": [{"type": "set_fields", "values": {"status": "'Accepted'", "accepted_by": "params.signed_by"}}]},
            {"name": "rate", "label": "Rate us", "permission": "write",
             "params": [{"name": "rating", "label": "Rating", "type": "int", "required": true}, {"name": "note", "label": "Comment", "type": "text"}],
             "refuse_if": [{"when": "params.rating < 1 || params.rating > 5", "message": "a rating is 1 to 5"}],
             "effects": [{"type": "set_fields", "values": {"rating": "params.rating", "feedback": "has(params.note) ? params.note : ''"}}]},
            {"name": "send", "label": "Send to the customer", "permission": "write",
             "effects": [
                {"type": "link", "actions": ["accept"], "show": ["title", "total"], "days": 3, "recipient": "doc.customer_email"},
                {"type": "message", "template": "x_quote_link", "to": ["expr:doc.customer_email"], "values": {"url": "results[0].url"}, "secret": ["url"]}
             ]}
        ]
    });
    serde_json::from_value(json!({"entity": "XQuote", "op": {"op": "define_entity", "entity": entity}})).unwrap()
}

#[test]
fn a_link_does_one_thing_to_one_record() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 12, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let settings = KernelSettings {
        secret_key: Some(ferrum_kernel::secret::SecretKey::new([7u8; 32])),
        public_url: Some("https://ferrum.test".into()),
        ..Default::default()
    };
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), settings, None).await.expect("boot");
    let admin = workspace(&k, "acme").await;
    let template: MessageDef = serde_json::from_value(json!({
        "key": "x_quote_link", "values": ["url"],
        "languages": {"en": {"email": {"subject": "Your quotation", "body": "<p><a href=\"{{ values.url }}\">Open it</a></p>"}}}
    }))
    .unwrap();
    k.save_message_template(&admin, &template).await.unwrap();
    k.batch_customizations(&admin, vec![quotation()], false).await.map_err(|(_, e)| e.to_string()).unwrap();
    let admin = k.load_ctx(admin.tenant, admin.user, None, "it".into()).await.unwrap();
    let quote = |title: &str| {
        let (k, admin, title) = (&k, admin.clone(), title.to_string());
        async move {
            id_of(
                &k.insert(&admin, "XQuote", obj(json!({"title": title, "total": "1,200.00", "customer_email": "ann@buyer.test", "margin": "41%"})))
                    .await
                    .unwrap(),
            )
        }
    };
    let spec = |v: Value| -> LinkSpec { serde_json::from_value(v).unwrap() };
    let get = |id: Uuid| {
        let (k, admin) = (&k, admin.clone());
        async move { k.get(&admin, "XQuote", id, false).await.unwrap() }
    };

    // ───────────── issuing: what a link may name ─────────────
    let q1 = quote("Q-1 Printers").await;
    for (bad, why) in [
        (json!({"actions": ["approve_all"]}), "an action the entity lacks"),
        (json!({"actions": []}), "no action"),
        (json!({"actions": ["accept"], "show": ["nothing"]}), "a field the entity lacks"),
        (json!({"actions": ["accept"], "show": ["title", "cost"]}), "a field in a group"),
        (json!({"actions": ["accept"], "params": {"rating": 5}}), "a value no action of it asks for"),
        (json!({"actions": ["accept"], "uses": 0}), "no uses"),
        (json!({"actions": ["accept"], "expires_at": "2099-01-01T00:00:00Z"}), "beyond 90 days"),
        (json!({"actions": ["accept"], "frame_origins": ["https://*.evil.test"]}), "a wildcard frame"),
    ] {
        assert!(matches!(k.issue_link_now(&admin, "XQuote", q1, &spec(bad.clone())).await, Err(Error::Validation(_))), "{why}: {bad}");
    }

    // ───────────── a quotation accepted by link, with a typed name ─────────────
    let accept =
        k.issue_link_now(&admin, "XQuote", q1, &spec(json!({"actions": ["accept"], "show": ["title", "total"], "recipient": "Ann Buyer"}))).await.unwrap();
    let t = token(&accept.url);
    assert!(accept.url.contains("/api/public/acme/links/") && t.len() == 43, "{}", accept.url);
    let page = k.link_page("acme", &t).await.unwrap().expect("the page");
    let labels: Vec<&str> = page.fields.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(labels, ["Title", "Total"], "the allow-list, never the margin");
    assert_eq!(page.actions.len(), 1);
    assert_eq!((page.actions[0].0.as_str(), page.actions[0].2.len()), ("accept", 1), "accept asks for the name");
    // A missing name, and an action the link does not name, use nothing.
    assert!(k.use_link("acme", &t, "accept", &Map::new(), None).await.is_err(), "the name is required");
    let other = k.use_link("acme", &t, "rate", &given(json!({"rating": "5"})), None).await;
    assert!(matches!(other, Err(Error::Invalid(_))), "an action the link does not name: {other:?}");
    assert!(
        k.use_link("acme", &t, "accept", &given(json!({"signed_by": "Ann", "status": "Lost"})), None).await.is_err(),
        "a value the action does not ask for"
    );
    assert_eq!(get(q1).await["status"], json!("Open"), "nothing changed yet");
    k.use_link("acme", &t, "accept", &given(json!({"signed_by": "Ann Buyer"})), None).await.unwrap().expect("accepted");
    let q = get(q1).await;
    assert_eq!((q["status"].clone(), q["accepted_by"].clone()), (json!("Accepted"), json!("Ann Buyer")));
    // Used once, it is gone: the second use and the page answer as an unknown link does.
    assert!(k.use_link("acme", &t, "accept", &given(json!({"signed_by": "Mallory"})), None).await.unwrap().is_none(), "used twice");
    assert!(k.link_page("acme", &t).await.unwrap().is_none());
    assert_eq!(get(q1).await["accepted_by"], json!("Ann Buyer"));
    // The record's history names the link and its recipient.
    let trail = k.audit_trail(&admin, "XQuote", q1).await.unwrap();
    let used = trail["events"].as_array().unwrap().iter().find(|e| e["action"] == json!("link:accept")).cloned().expect("the use in the history");
    assert_eq!((used["diff"]["recipient"].clone(), used["diff"]["link"].clone()), (json!("Ann Buyer"), json!(accept.id)), "{used}");
    assert!(trail["events"].as_array().unwrap().iter().any(|e| e["action"] == json!("issue_link")));

    // ───────────── a rating from 1 to 5 with a comment, twice allowed ─────────────
    let q2 = quote("Q-2 Repair").await;
    let rate = k.issue_link_now(&admin, "XQuote", q2, &spec(json!({"actions": ["rate"], "uses": 2}))).await.unwrap();
    let t = token(&rate.url);
    let refused = k.use_link("acme", &t, "rate", &given(json!({"rating": "9"})), None).await;
    assert!(matches!(refused, Err(Error::Forbidden(ref m)) if m.contains("1 to 5")), "{refused:?}");
    assert!(k.use_link("acme", &t, "rate", &given(json!({"rating": "four"})), None).await.is_err(), "a rating is a number");
    k.use_link("acme", &t, "rate", &given(json!({"rating": "4", "note": "Quick and tidy"})), None).await.unwrap().expect("rated");
    assert_eq!((get(q2).await["rating"].clone(), get(q2).await["feedback"].clone()), (json!(4), json!("Quick and tidy")));
    k.use_link("acme", &t, "rate", &given(json!({"rating": "5"})), None).await.unwrap().expect("its second use: refusals counted none");
    assert!(k.use_link("acme", &t, "rate", &given(json!({"rating": "1"})), None).await.unwrap().is_none(), "a third");
    assert_eq!(get(q2).await["rating"], json!(5));

    // ───────────── what the issuer fixed stands ─────────────
    let q3 = quote("Q-3 Toner").await;
    let fixed =
        k.issue_link_now(&admin, "XQuote", q3, &spec(json!({"actions": ["accept"], "params": {"signed_by": "Purchasing, by contract"}}))).await.unwrap();
    let page = k.link_page("acme", &token(&fixed.url)).await.unwrap().unwrap();
    assert!(page.actions[0].2.is_empty(), "a fixed value is not asked");
    k.use_link("acme", &token(&fixed.url), "accept", &given(json!({"signed_by": "Someone else"})), None).await.unwrap().unwrap();
    assert_eq!(get(q3).await["accepted_by"], json!("Purchasing, by contract"));

    // ───────────── every link that does not work answers alike ─────────────
    let q4 = quote("Q-4 Paper").await;
    let revoked = k.issue_link_now(&admin, "XQuote", q4, &spec(json!({"actions": ["accept"]}))).await.unwrap();
    k.revoke_link(&admin, revoked.id).await.unwrap();
    let expired = k.issue_link_now(&admin, "XQuote", q4, &spec(json!({"actions": ["accept"]}))).await.unwrap();
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    sqlx::query("update action_link set expires_at = now() - interval '1 second' where tenant_id = $1 and id = $2")
        .bind(admin.tenant)
        .bind(expired.id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let live = k.issue_link_now(&admin, "XQuote", q4, &spec(json!({"actions": ["accept"]}))).await.unwrap();
    let _other = workspace(&k, "globex").await;
    let made_up = "A".repeat(43);
    for (slug, t, why) in [
        ("acme", token(&revoked.url), "revoked"),
        ("acme", token(&expired.url), "expired"),
        ("acme", made_up.clone(), "unknown"),
        ("acme", "not-a-token".to_string(), "malformed"),
        ("globex", token(&live.url), "another workspace's address"),
        ("nowhere", token(&live.url), "no such workspace"),
    ] {
        assert!(k.link_page(slug, &t).await.unwrap().is_none(), "{why}: page");
        let used = k.use_link(slug, &t, "accept", &given(json!({"signed_by": "X"})), None).await;
        assert!(matches!(used, Ok(None)), "{why}: use answered {used:?}");
    }
    assert_eq!(get(q4).await["status"], json!("Open"));
    let listed = k.record_links(&admin, "XQuote", q4).await.unwrap();
    assert_eq!(listed.len(), 3);
    assert!(listed.iter().all(|l| l.get("digest").is_none() && !l.to_string().contains(&token(&live.url))), "never a token: {listed:?}");

    // ───────────── a code before the action ─────────────
    let q5 = quote("Q-5 Ink").await;
    let coded = k.issue_link_now(&admin, "XQuote", q5, &spec(json!({"actions": ["accept"], "code_to": "ann@buyer.test"}))).await.unwrap();
    let t = token(&coded.url);
    assert!(k.link_page("acme", &t).await.unwrap().unwrap().code.is_some(), "the page asks for it");
    let none = k.use_link("acme", &t, "accept", &given(json!({"signed_by": "Ann"})), None).await;
    assert!(matches!(none, Err(Error::Validation(_))), "no code: {none:?}");
    assert!(k.send_link_code("acme", &t).await.unwrap(), "a code goes to the address the issuer named, never one the visitor gives");
    let wrong = k.use_link("acme", &t, "accept", &given(json!({"signed_by": "Ann"})), Some("000000")).await;
    let wrong = match wrong {
        Err(e) => e,
        // One in a million: the code really was 000000.
        Ok(_) => return,
    };
    assert!(matches!(wrong, Error::Validation(_)), "{wrong}");
    assert_eq!(get(q5).await["status"], json!("Open"));
    assert!(k.link_page("acme", &t).await.unwrap().is_some(), "a wrong code uses nothing");

    // ───────────── two uses at once of a single-use link: one runs ─────────────
    let q6 = quote("Q-6 Stapler").await;
    let once = k.issue_link_now(&admin, "XQuote", q6, &spec(json!({"actions": ["rate"]}))).await.unwrap();
    let t = token(&once.url);
    let (two, three) = (given(json!({"rating": "2"})), given(json!({"rating": "3"})));
    let (a, b) = tokio::join!(k.use_link("acme", &t, "rate", &two, None), k.use_link("acme", &t, "rate", &three, None));
    let ran = [a, b].into_iter().filter(|r| matches!(r, Ok(Some(_)))).count();
    assert_eq!(ran, 1, "exactly once");

    // ───────────── the `link` effect, sent by a `message` that keeps it secret ─────────────
    let q7 = quote("Q-7 Desk").await;
    k.run_action(&admin, "XQuote", q7, "send", json!({})).await.expect("send");
    let links = k.record_links(&admin, "XQuote", q7).await.unwrap();
    assert_eq!((links.len(), links[0]["recipient"].clone(), links[0]["actions"].clone()), (1, json!("ann@buyer.test"), json!(["accept"])));
    let mut tx = k.db.tenant_tx(admin.tenant).await.unwrap();
    let kept: Vec<Value> = sqlx::query_scalar("select to_jsonb(m) from message_send m where tenant_id = $1 and template = 'x_quote_link'")
        .bind(admin.tenant)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(kept.len(), 1, "sent once");
    assert!(!kept[0].to_string().contains("/links/"), "the address is not kept in the clear: {}", kept[0]);

    // ───────────── staff only ─────────────
    let ann = {
        let id = k.create_user(&admin, "ann@buyer.test", "Ann", None, &[], false).await.unwrap();
        k.set_user_kind(&admin, id, "portal").await.unwrap();
        k.load_ctx(admin.tenant, id, None, "it".into()).await.unwrap()
    };
    assert!(k.issue_link_now(&ann, "XQuote", q7, &spec(json!({"actions": ["accept"]}))).await.is_err(), "an outside person issues nothing");
    assert!(k.revoke_link(&ann, links[0]["id"].as_str().unwrap().parse().unwrap()).await.is_err());
}
