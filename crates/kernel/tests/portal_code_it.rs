//! Signing in with an emailed code (A11 D10, APIQ-1), against a real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the database
//! is reset by each test, which take their turn): every failure answers alike, so the answer tells no one whether an
//! address is registered, used or locked; wrong codes never lock the person, so a stranger who knows an address cannot
//! keep its owner out; after five wrong codes for an address within an hour no further code is mailed to it.

use ferrum_kernel::Error;
use ferrum_testkit::{Kit, Workspace, database, run};
use serde_json::Value;
use std::sync::Mutex;
use uuid::Uuid;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn go<F: std::future::Future<Output = ()> + Send + 'static>(f: impl FnOnce() -> F) {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    run(f());
}

/// A workspace with a portal person, `ann@client.test`.
async fn world() -> (Kit, Workspace, Uuid) {
    let kit = Kit::boot(&database("FERRUM_TEST_DATABASE_URL"), &[]).await;
    let ws = kit.workspace("codes", &[], Value::Null).await;
    let ann = kit.k.create_user(&ws.admin, "ann@client.test", "Ann", None, &[], false).await.unwrap();
    kit.k.set_user_kind(&ws.admin, ann, "portal").await.unwrap();
    (kit, ws, ann)
}

/// The code in the last mail to `to`.
async fn last_code(kit: &Kit, ws: &Workspace, to: &str) -> Option<String> {
    ws.drain().await;
    let mail = kit.mail().into_iter().rev().find(|m| m.to == to)?;
    let digits: Vec<char> = mail.subject.chars().take_while(|c| c.is_ascii_digit()).collect();
    Some(digits.into_iter().collect())
}

fn said(r: ferrum_kernel::Result<ferrum_kernel::auth::SignIn>) -> String {
    match r {
        Ok(_) => "signed in".into(),
        Err(e @ Error::Unauthorized(_)) => format!("unauthorized: {e}"),
        Err(e) => format!("other: {e}"),
    }
}

fn wrong(code: &str) -> String {
    format!("{:06}", (code.parse::<u32>().unwrap() + 1) % 1_000_000)
}

#[test]
fn a_wrong_code_answers_alike_for_unknown_known_used_and_locked_addresses() {
    go(|| async {
        let (kit, ws, ann) = world().await;
        let k = &kit.k;
        let unknown = said(k.sign_in_with_code("codes", "nobody@client.test", "123456").await);
        // Known, with a live code, a wrong one; then that code used, and tried again.
        k.send_sign_in_code("codes", "ann@client.test").await.unwrap();
        let code = last_code(&kit, &ws, "ann@client.test").await.expect("a code");
        let known = said(k.sign_in_with_code("codes", "ann@client.test", &wrong(&code)).await);
        assert_eq!(said(k.sign_in_with_code("codes", "ann@client.test", &code).await), "signed in");
        let used = said(k.sign_in_with_code("codes", "ann@client.test", &code).await);
        // The fifth wrong try at a second code, which ends it.
        k.send_sign_in_code("codes", "ann@client.test").await.unwrap();
        let code = last_code(&kit, &ws, "ann@client.test").await.expect("a second code");
        for _ in 0..4 {
            k.sign_in_with_code("codes", "ann@client.test", &wrong(&code)).await.unwrap_err();
        }
        let fifth = said(k.sign_in_with_code("codes", "ann@client.test", &wrong(&code)).await);
        // A person locked by failed sign-ins.
        let mut tx = k.db.system_tx().await.unwrap();
        let n = sqlx::query("update app_user set locked_until = now() + interval '1 hour' where id = $1").bind(ann).execute(&mut *tx).await.unwrap();
        assert_eq!(n.rows_affected(), 1);
        tx.commit().await.unwrap();
        let locked = said(k.sign_in_with_code("codes", "ann@client.test", "123456").await);
        for (what, answer) in [("a known address", &known), ("the fifth try", &fifth), ("a used code", &used), ("a locked person", &locked)] {
            assert_eq!(answer, &unknown, "{what} answers as an unknown address does");
        }
    });
}

#[test]
fn wrong_codes_never_lock_the_person() {
    go(|| async {
        let (kit, ws, ann) = world().await;
        let k = &kit.k;
        k.send_sign_in_code("codes", "ann@client.test").await.unwrap();
        let code = last_code(&kit, &ws, "ann@client.test").await.expect("a code");
        for _ in 0..12 {
            k.sign_in_with_code("codes", "ann@client.test", &wrong(&code)).await.unwrap_err();
        }
        let mut tx = k.db.system_tx().await.unwrap();
        let (failed, locked): (i32, bool) = sqlx::query_as("select failed_logins, coalesce(locked_until > now(), false) from app_user where id = $1")
            .bind(ann)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!((failed, locked), (0, false), "a stranger's guesses never lock the owner out");
    });
}

#[test]
fn after_five_wrong_codes_the_next_request_needs_a_guard() {
    go(|| async {
        let (kit, ws, _) = world().await;
        let k = &kit.k;
        k.send_sign_in_code("codes", "ann@client.test").await.unwrap();
        let code = last_code(&kit, &ws, "ann@client.test").await.expect("a code");
        for _ in 0..5 {
            k.sign_in_with_code("codes", "ann@client.test", &wrong(&code)).await.unwrap_err();
        }
        let before = kit.mail().len();
        // The next request answers as any does, and mails nothing (S4-P5's guard is the way past it).
        k.send_sign_in_code("codes", "ann@client.test").await.expect("the same answer");
        ws.drain().await;
        assert_eq!(kit.mail().len(), before, "no mail to the address after five wrong codes in an hour");
    });
}
