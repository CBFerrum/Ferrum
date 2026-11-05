//! What a person's data request reaches, against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the
//! database is reset first): under every address they have had, with their files; what stays says
//! why; what is not searched is named; a legal hold stops erasure and deletion; and a database
//! restored from before an erasure can be erased again from the journal.

use ferrum_kernel::files::FileStore;
use ferrum_kernel::privacy::{JOURNAL_PREFIX, Person};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Default)]
struct MemFiles(Mutex<HashMap<String, Vec<u8>>>);

#[async_trait::async_trait]
impl FileStore for MemFiles {
    async fn put(&self, key: &str, data: &[u8]) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().insert(key.to_string(), data.to_vec());
        Ok(())
    }
    async fn get(&self, key: &str) -> ferrum_kernel::Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| Error::NotFound("object".into()))
    }
    async fn delete(&self, key: &str) -> ferrum_kernel::Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

const PEOPLE: &str = r#"
- name: Visitor
  label: Visitor
  kind: master
  title_field: full_name
  fields:
    - {name: full_name, type: data, required: true}
    - {name: email, type: email}
    - {name: phone, type: phone}
    - {name: notes, type: text}
- name: Receipt
  label: Receipt
  kind: master
  fields:
    - {name: payer, type: email}
    - {name: amount, type: decimal}
"#;

fn write_catalog(dir: &Path, personal: &str) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir.join("people/entities")).unwrap();
    std::fs::create_dir_all(dir.join("silent/entities")).unwrap();
    std::os::unix::fs::symlink(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/core").canonicalize().unwrap(), dir.join("core")).unwrap();
    std::fs::write(dir.join("people/package.yaml"), format!("package: people\nversion: 0.1.0\nlabel: People\ndepends: [core]\npersonal_data:\n{personal}"))
        .unwrap();
    std::fs::write(dir.join("people/entities/people.yaml"), PEOPLE).unwrap();
    std::fs::write(dir.join("silent/package.yaml"), "package: silent\nversion: 0.1.0\nlabel: Silent\ndepends: [core]\n").unwrap();
    std::fs::write(dir.join("silent/entities/quiet.yaml"), "- name: Quiet\n  label: Quiet\n  kind: master\n  fields:\n    - {name: title, type: data}\n")
        .unwrap();
}

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ferrum-privacy-it-{name}-{}", std::process::id()))
}

fn catalog() -> Catalog {
    let dir = temp("ok");
    write_catalog(
        &dir,
        "  - {entity: Visitor, match: email, phone: phone, name: full_name, erase: [full_name, email, phone, notes]}\n  - {entity: Receipt, match: payer, retain: \"Accounting records\"}\n",
    );
    let c = Catalog::load_dir(&dir).expect("catalog");
    let _ = std::fs::remove_dir_all(&dir);
    c
}

async fn workspace(k: &Kernel, slug: &str) -> Uuid {
    k.create_tenant(NewTenant {
        slug: slug.into(),
        name: slug.into(),
        admin_email: format!("admin@{slug}.test"),
        admin_password: "Correct-Horse-9".into(),
        admin_name: None,
        packages: vec!["people".into(), "silent".into()],
        settings: None,
    })
    .await
    .expect("workspace")
}

fn ids(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect()
}

#[test]
fn a_request_reaches_what_a_person_left() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    // A declaration says what an erasure does, or why not.
    for (personal, why) in [
        ("  - {entity: Receipt, match: payer}\n", "neither erase nor retain"),
        ("  - {entity: Receipt, match: payer, erase: [payer], retain: \"Both\"}\n", "both erase and retain"),
        ("  - {entity: Visitor, match: email, phone: notes, erase: [email]}\n", "a phone that is no phone field"),
        ("  - {entity: Visitor, match: email, name: nickname, erase: [email]}\n", "a name that is no field"),
    ] {
        let dir = temp("bad");
        write_catalog(&dir, personal);
        assert!(Catalog::load_dir(&dir).is_err(), "{why}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    let url = std::env::var("FERRUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let k = Kernel::boot(db, catalog(), HookRegistry::default(), KernelSettings::default(), None).await.expect("boot");
    let store = Arc::new(MemFiles::default());
    k.set_file_store(store.clone());
    let tenant = workspace(&k, "acme").await;
    let other = workspace(&k, "other").await;
    let session = k.login("acme", "admin@acme.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let outsider = k.login("other", "admin@other.test", "Correct-Horse-9").await.unwrap();
    let outsider = k.load_ctx(other, outsider.user, Some(outsider.token_version), "it".into()).await.unwrap();

    // ───────────── someone with a past ─────────────
    let pat = k.create_user(&admin, "pat@old.test", "Pat Lee", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let pat_ctx = k.load_ctx(tenant, pat, None, "it".into()).await.unwrap();
    let (_, key) = k.create_api_key(&pat_ctx, pat, "laptop", None, None).await.unwrap();
    let early = k
        .insert(&admin, "Visitor", obj(json!({"full_name": "Pat Lee", "email": "pat@old.test", "phone": "+15550100", "notes": "Allergic to nuts"})))
        .await
        .unwrap();
    let early_id: Uuid = early["id"].as_str().unwrap().parse().unwrap();
    let receipt = k.insert(&admin, "Receipt", obj(json!({"payer": "pat@old.test", "amount": "12.50"}))).await.unwrap();
    let scan = k.store_file(&admin, Some(("Visitor", early_id)), "passport.pdf", "application/pdf", b"secret scan").await.expect("attached");
    let scan_key: String = {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        sqlx::query_scalar("select storage_key from file_object where tenant_id = $1 and id = $2")
            .bind(tenant)
            .bind(scan["id"].as_str().unwrap().parse::<Uuid>().unwrap())
            .fetch_one(&mut *tx)
            .await
            .unwrap()
    };
    k.scim_patch_user(&admin, pat, &json!({"Operations": [{"op": "replace", "path": "userName", "value": "pat@new.test"}]})).await.expect("a new address");
    let late = k.insert(&admin, "Visitor", obj(json!({"full_name": "Pat Lee", "email": "pat@new.test"}))).await.unwrap();
    let late_id: Uuid = late["id"].as_str().unwrap().parse().unwrap();

    // ───────────── a request finds them under any address they have had ─────────────
    for asked in ["pat@old.test", "PAT@new.test"] {
        let found = k.personal_data(&admin, asked).await.unwrap();
        assert_eq!(found["addresses"], json!(["pat@new.test", "pat@old.test"]), "{asked}");
        assert_eq!(found["records"].as_array().unwrap().len(), 3, "both visitors and the receipt: {found}");
        assert_eq!(found["account"]["id"], json!(pat), "their account, under either address");
    }
    let found = k.personal_data(&admin, "pat@old.test").await.unwrap();
    let record =
        |entity: &str, id: &str| found["records"].as_array().unwrap().iter().find(|r| r["entity"] == json!(entity) && r["id"] == json!(id)).unwrap().clone();
    assert_eq!(record("Visitor", early["id"].as_str().unwrap())["files"][0]["file_name"], json!("passport.pdf"), "their files are named");
    assert_eq!(record("Receipt", receipt["id"].as_str().unwrap())["retained_because"], json!("Accounting records"));
    let unsearched = found["not_searched"].as_array().unwrap();
    assert!(unsearched.contains(&json!("silent")), "a package that declares nothing is named, not skipped in silence: {unsearched:?}");
    assert!(!unsearched.contains(&json!("people")));
    let stranger = k.personal_data(&admin, "nobody@else.test").await.unwrap();
    assert!(stranger["records"].as_array().unwrap().is_empty() && stranger["account"].is_null());
    assert_eq!(stranger["addresses"], json!(["nobody@else.test"]));
    assert!(k.personal_data(&outsider, "pat@old.test").await.unwrap()["account"].is_null(), "another workspace finds nothing");

    // ───────────── and by phone number, or name ─────────────
    // A walk-in recorded with only a phone number is theirs when asked with that number, however
    // either was written; a namesake is only possibly theirs, listed for a person to judge.
    let walk_in = k.insert(&admin, "Visitor", obj(json!({"full_name": "Pat Lee", "phone": "+1 (555) 0100"}))).await.unwrap();
    let namesake = k.insert(&admin, "Visitor", obj(json!({"full_name": "pat   LEE", "email": "p.lee@else.test"}))).await.unwrap();
    k.insert(&admin, "Visitor", obj(json!({"full_name": "Sam Roe", "phone": "+1 555 0199"}))).await.unwrap();
    let person = |phone: &str, name: &str| Person { email: "pat@old.test".into(), phone: Some(phone.into()), name: Some(name.into()) };
    let wider = k.personal_data_of(&admin, &person("+1-555-0100", "Pat Lee")).await.unwrap();
    assert_eq!(wider["records"].as_array().unwrap().len(), 4, "the walk-in as well: {wider}");
    assert!(ids(&wider["records"]).contains(&walk_in["id"].as_str().unwrap().to_string()));
    assert_eq!(ids(&wider["possibly"]), vec![namesake["id"].as_str().unwrap().to_string()], "the namesake, apart from what is theirs");
    assert_eq!(wider["phone"], json!("15550100"));
    assert!(matches!(k.personal_data_of(&admin, &person("55", "")).await, Err(Error::Validation(_))), "too few digits to name anyone");

    // ───────────── legal holds come before erasure ─────────────
    assert!(k.place_legal_hold(&admin, None, None, "Matter 7").await.is_err(), "of something");
    assert!(k.place_legal_hold(&admin, Some("pat@new.test"), None, " ").await.is_err(), "for a reason");
    assert!(matches!(k.place_legal_hold(&pat_ctx, Some("pat@new.test"), None, "Matter 7").await, Err(Error::Forbidden(_))));
    let person_hold = k.place_legal_hold(&admin, Some("pat@old.test"), None, "Matter 7").await.unwrap();
    assert_eq!(k.personal_data(&admin, "pat@new.test").await.unwrap()["held"], json!(true), "whichever address the hold named");
    assert!(matches!(k.erase_personal_data(&admin, "pat@new.test", "Request 1").await, Err(Error::Conflict(_))), "a person on hold is not erased");
    assert!(matches!(k.release_legal_hold(&outsider, person_hold).await, Err(Error::NotFound(_))), "nor released from outside");
    k.release_legal_hold(&admin, person_hold).await.unwrap();
    assert!(matches!(k.release_legal_hold(&admin, person_hold).await, Err(Error::NotFound(_))), "once");

    let record_hold = k.place_legal_hold(&admin, None, Some(("Visitor", late_id)), "Matter 8").await.unwrap();
    assert!(matches!(k.delete(&admin, "Visitor", late_id, None).await, Err(Error::Conflict(_))), "nor deleted");
    assert_eq!(k.legal_holds(&admin).await.unwrap().len(), 2, "released ones stay on the record");
    assert!(k.legal_holds(&outsider).await.unwrap().is_empty());

    // ───────────── erasure ─────────────
    assert!(matches!(k.erase_personal_data(&pat_ctx, "pat@new.test", "Mine").await, Err(Error::Forbidden(_))));
    let done = k.erase_personal_data(&admin, "pat@old.test", "Request 1").await.unwrap();
    assert_eq!(ids(&done["erased"]), vec![early["id"].as_str().unwrap().to_string()], "{done}");
    assert_eq!(ids(&done["held"]), vec![late["id"].as_str().unwrap().to_string()], "what a hold covers stays, and says so");
    assert_eq!(done["retained"][0]["because"], json!("Accounting records"), "what is kept says why");
    assert_eq!(done["files_deleted"], json!(1));
    assert_eq!(done["account_erased"], json!(true));
    assert!(done["not_searched"].as_array().unwrap().contains(&json!("silent")));

    let erased = k.get(&admin, "Visitor", early_id, false).await.unwrap();
    assert!(erased["full_name"].as_str().unwrap().starts_with("Erased ") && erased["email"].is_null() && erased["notes"].is_null(), "{erased:?}");
    let kept = k.get(&admin, "Receipt", receipt["id"].as_str().unwrap().parse().unwrap(), false).await.unwrap();
    assert_eq!(kept["payer"], json!("pat@old.test"), "retained means untouched");
    assert_eq!(k.get(&admin, "Visitor", late_id, false).await.unwrap()["email"], json!("pat@new.test"));

    // Everything that hung off the account has gone, and every replica has heard.
    assert!(k.load_ctx(tenant, pat, None, "it".into()).await.is_err());
    assert!(k.verify_api_key(&key).await.is_err());
    let left = |table: &'static str| {
        let k = k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from {table} where user_id = $1")))
                .bind(pat)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            n
        }
    };
    for table in ["api_key", "person_identifier", "user_role", "notification"] {
        assert_eq!(left(table).await, 0, "{table}");
    }
    let again = k.personal_data(&admin, "pat@old.test").await.unwrap();
    assert!(again["account"].is_null(), "no address leads back to them");
    assert_eq!(again["addresses"], json!(["pat@old.test"]));

    // The attached file goes from storage once the transaction has committed, and the journal is
    // written: by ids and field names, nothing of what it erased.
    assert!(store.0.lock().unwrap().contains_key(&scan_key), "not before the commit's events run");
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    assert!(!store.0.lock().unwrap().contains_key(&scan_key), "the object is gone");
    let journal: Vec<String> = store.0.lock().unwrap().keys().filter(|k| k.starts_with(JOURNAL_PREFIX)).cloned().collect();
    assert_eq!(journal.len(), 1, "{journal:?}");
    let entry = String::from_utf8(store.0.lock().unwrap()[&journal[0]].clone()).unwrap();
    for secret in ["Pat Lee", "pat@", "Allergic", "passport"] {
        assert!(!entry.contains(secret), "the journal holds no {secret}: {entry}");
    }

    // ───────────── a restore brings it back; the journal erases it again ─────────────
    let mut tx = k.db.system_tenant_tx(tenant).await.unwrap();
    sqlx::query("update e_visitor set full_name = 'Pat Lee', email = 'pat@old.test', notes = 'Allergic to nuts' where tenant_id = $1 and id = $2")
        .bind(tenant)
        .bind(early_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("update app_user set email = 'pat@old.test', full_name = 'Pat Lee', is_active = true where tenant_id = $1 and id = $2")
        .bind(tenant)
        .bind(pat)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    k.invalidate_user(tenant, pat);
    assert_eq!(k.get(&admin, "Visitor", early_id, false).await.unwrap()["email"], json!("pat@old.test"), "as a backup from before would have it");
    assert!(k.load_ctx(tenant, pat, None, "it".into()).await.is_ok(), "and the account is back");

    assert_eq!(k.reapply_erasure(&journal[0]).await.unwrap(), 2, "the record and the account");
    let erased = k.get(&admin, "Visitor", early_id, false).await.unwrap();
    assert!(erased["full_name"].as_str().unwrap().starts_with("Erased ") && erased["email"].is_null() && erased["notes"].is_null(), "{erased:?}");
    assert!(k.load_ctx(tenant, pat, None, "it".into()).await.is_err(), "and the account is gone again");
    assert!(k.personal_data(&admin, "pat@old.test").await.unwrap()["account"].is_null());
    let first = serde_json::to_string(&erased).unwrap();
    assert_eq!(k.reapply_erasure(&journal[0]).await.unwrap(), 1, "the record again, the account already done");
    let twice = k.get(&admin, "Visitor", early_id, false).await.unwrap();
    assert_eq!(twice["full_name"], erased["full_name"], "the same placeholder: {first}");

    // A record the backup does not have is skipped; so is a workspace it does not have.
    let mut tx = k.db.system_tenant_tx(tenant).await.unwrap();
    sqlx::query("delete from e_visitor where tenant_id = $1 and id = $2").bind(tenant).bind(early_id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(k.reapply_erasure(&journal[0]).await.unwrap(), 0);

    k.release_legal_hold(&admin, record_hold).await.unwrap();
    k.delete(&admin, "Visitor", late_id, None).await.expect("deletable once released");

    // ───────────── what a reviewer is handed ─────────────
    let plain = k.create_user(&admin, "pia@acme.test", "Pia Plain", Some("Correct-Horse-9"), &[], false).await.unwrap();
    let plain = k.load_ctx(tenant, plain, None, "it".into()).await.unwrap();
    assert!(matches!(k.evidence_pack(&plain).await, Err(Error::Forbidden(_))), "for administrators");
    let pack = k.evidence_pack(&admin).await.expect("a pack");
    let people = pack["access"]["people"].as_array().unwrap();
    let ada = people.iter().find(|p| p["email"] == "admin@acme.test").expect("the administrator");
    assert_eq!((&ada["administrator"], &ada["two_factor"], &ada["active"]), (&json!(true), &json!(false), &json!(true)));
    assert!(!pack.to_string().contains("admin@other.test"), "this workspace only");
    assert!(pack["note"].as_str().unwrap().contains("Not a claim of compliance"));
    assert!(pack["personal_data"]["declared"].as_array().is_some_and(|d| !d.is_empty()));
    assert!(pack["history"]["events"].as_i64().unwrap() > 0);
    let text = pack.to_string();
    for secret in ["password", "secret_hash", "totp_secret", "sealed", "Correct-Horse"] {
        assert!(!text.contains(secret), "no {secret} in it");
    }
    let mut tx = k.db.system_tenant_tx(tenant).await.unwrap();
    let taken: i64 = sqlx::query_scalar("select count(*) from audit_event where tenant_id = $1 and action = 'evidence_pack' and actor = $2")
        .bind(tenant)
        .bind(admin.user)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(taken, 1, "and taking one is itself in the history");

    // An erasure asked with their phone number reaches the walk-in; a name alone erases nothing.
    let by_phone = k.erase_personal_data_of(&admin, &person("+1 555 0100", "Pat Lee"), "Request 2").await.unwrap();
    assert_eq!(ids(&by_phone["erased"]), vec![walk_in["id"].as_str().unwrap().to_string()], "{by_phone}");
    let kept = k.get(&admin, "Visitor", namesake["id"].as_str().unwrap().parse().unwrap(), false).await.unwrap();
    assert_eq!(kept["email"], json!("p.lee@else.test"), "the namesake is untouched");
}
