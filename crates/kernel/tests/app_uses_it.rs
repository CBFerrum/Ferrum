//! One app using another's operation through the gate (docs/app-contract.md, *Using another app's operations*), against a
//! real PostgreSQL (`FERRUM_TEST_DATABASE_URL`; the kit empties it). A carrier offers a hidden action with a contract; a
//! shop and a mall each declare that they use it. A call is the app's own, proved by the vault handle its registration
//! took, in the caller's transaction: it does only what one of the app's packages installed there declares, at a version
//! within the range declared, with the params the action declares, for the person it acts for and within their zone;
//! the key is the app's, the receipt names the app and the person, and an approval it would wait for is refused. A
//! refused call leaves nothing of itself in the caller's transaction, calls nest, and an app repeating its own key is
//! given what it did the first time.

use async_trait::async_trait;
use ferrum_kernel::capability::{Invocation, Receipt};
use ferrum_kernel::vault::Vault;
use ferrum_kernel::zones::{Credential, Entry, Placed, Zones};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, HookCtx, HookRegistry, Kernel, Result};
use ferrum_meta::{MetaError, Package};
use ferrum_testkit::{App, Kit, Upkeep, Workspace, database, obj, refused, run, uid};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

const CARRIER: &str = "package: carrier\nversion: 1.2.0\ndepends: [core]\nroles:\n  - name: Dispatcher\n    permissions:\n      - {entity: Parcel, read: true, write: true, create: true}\n      - {entity: Shipment, read: true}\n\
approvals:\n  - entity: Parcel\n    on: {action: hold}\n    policy: {steps: [{label: Desk, approvers: {role: Dispatcher}}]}\n";
const PARCEL: &str = r#"
- name: Parcel
  label: Parcel
  kind: master
  title_field: title
  fields:
    - {name: title, type: data, required: true}
  actions:
    - name: ship
      label: Ship
      permission: write
      hidden: true
      params:
        - {name: carrier, label: Carrier, required: true}
        - {name: note, label: Note}
      contract: {completion: "A shipment of the parcel, committed with this receipt", returns: {type: object, properties: {shipment: {type: string}}}}
    - {name: hold, label: Hold, permission: write, hidden: true, contract: {completion: The parcel is held}}
    - {name: pack, label: Pack, permission: write, hidden: true, contract: {completion: The parcel is packed}}
- name: Shipment
  label: Shipment
  kind: master
  fields:
    - {name: parcel, type: link, to: Parcel, required: true}
    - {name: carrier, type: data}
"#;
/// What the shop uses: shipping, holding, packing, the mall's dispatch, an operation the carrier does not offer, and
/// reading at a version it is not.
const SHOP: &str = r#"
package: shop
version: 0.1.0
depends: [carrier, mall]
uses:
  - {capability: carrier.parcel.ship, version: '^1.2'}
  - {capability: carrier.parcel.hold, version: '^1'}
  - {capability: carrier.parcel.pack, version: '^1'}
  - {capability: mall.mall_order.dispatch, version: '^0.1'}
  - {capability: carrier.parcel.weigh, version: '^1'}
  - {capability: carrier.parcel.read, version: '^2'}
"#;
const MALL: &str = "package: mall\nversion: 0.1.0\ndepends: [carrier]\nuses:\n  - {capability: carrier.parcel.ship, version: '>=1, <2'}\n  - {capability: carrier.parcel.pack, version: '^1'}\n";
/// The mall's own operation, which calls the carrier's in turn.
const MALL_ORDER: &str = r#"
- name: MallOrder
  label: Mall order
  kind: master
  fields:
    - {name: parcel, type: link, to: Parcel, required: true}
  actions:
    - name: dispatch
      label: Dispatch
      permission: write
      hidden: true
      params:
        - {name: then_fail, label: Then fail}
      contract: {completion: "The order's parcel shipped, committed with this receipt"}
"#;

/// The carrier's code: a shipment for `ship`, nothing for `hold` (which waits for the desk's approval), and for `pack` a
/// shipment written and then a refusal of its own.
struct Carrier;

#[async_trait]
impl DocHook for Carrier {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        Ok(match name {
            "ship" => {
                let made = hc.kernel.insert_tx(hc.ctx, hc.tx, "Shipment", obj(json!({"parcel": doc["id"], "carrier": params["carrier"]}))).await?;
                Some(json!({"shipment": made["id"]}))
            }
            "hold" => Some(json!({"held": true})),
            "pack" => {
                hc.kernel.insert_tx(hc.ctx, hc.tx, "Shipment", obj(json!({"parcel": doc["id"], "carrier": "Scale"}))).await?;
                return Err(Error::invalid("the scale is broken"));
            }
            _ => None,
        })
    }
}

/// The mall's code, calling as the mall: dispatching an order tries to pack its parcel, which the carrier refuses, carries
/// on and ships it; asked to, it then refuses itself.
struct Mall(Vault);

#[async_trait]
impl DocHook for Mall {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if name != "dispatch" {
            return Ok(None);
        }
        let order = doc["id"].as_str().unwrap_or_default();
        let pack = Invocation::new("carrier.parcel.pack", json!({"id": doc["parcel"], "params": {}})).key(format!("pack:{order}"));
        if hc.kernel.call_tx(&self.0, hc.ctx, hc.tx, &pack).await.is_ok() {
            return Err(Error::internal("the carrier packed the parcel"));
        }
        let ship = Invocation::new("carrier.parcel.ship", json!({"id": doc["parcel"], "params": {"carrier": "Mall Post"}})).key(format!("ship:{order}"));
        let shipped = hc.kernel.call_tx(&self.0, hc.ctx, hc.tx, &ship).await?;
        if params["then_fail"] == "yes" {
            return Err(Error::invalid("the order was dispatched and then refused"));
        }
        Ok(Some(json!({"shipment": shipped.result["shipment"]})))
    }
}

fn load(files: &[(&str, &str)]) -> Result<Vec<Package>, MetaError> {
    let files: BTreeMap<String, String> = files.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    Ok(vec![ferrum_meta::catalog::load_package_files(&files)?])
}

fn carrier_packages() -> Result<Vec<Package>, MetaError> {
    load(&[("package.yaml", CARRIER), ("entities/parcel.yaml", PARCEL)])
}

fn shop_packages() -> Result<Vec<Package>, MetaError> {
    load(&[("package.yaml", SHOP)])
}

fn mall_packages() -> Result<Vec<Package>, MetaError> {
    load(&[("package.yaml", MALL), ("entities/order.yaml", MALL_ORDER)])
}

fn carrier_register(h: &mut HookRegistry) {
    h.register("Parcel", Arc::new(Carrier));
}

fn mall_register(h: &mut HookRegistry) {
    let own = h.vault();
    h.register("MallOrder", Arc::new(Mall(own)));
}

fn nothing_registered(_: &mut HookRegistry) {}

fn no_upkeep(_: &Kernel) -> Upkeep<'_> {
    Box::pin(async { Ok(()) })
}

fn app(id: &'static str, packages: fn() -> Result<Vec<Package>, MetaError>, register: fn(&mut HookRegistry)) -> App {
    App { id, packages, register, reports: Vec::new, maintenance: no_upkeep }
}

/// App `app`'s call for `ctx`'s person in a transaction of its own, committed when it is done.
async fn call(kit: &Kit, app: &str, ctx: &Ctx, inv: Invocation) -> Result<Receipt> {
    let mut tx = kit.k.db.tenant_tx(ctx.tenant).await.unwrap();
    let done = kit.k.call_tx(&kit.vault(app), ctx, &mut tx, &inv).await?;
    tx.commit().await.unwrap();
    Ok(done)
}

async fn count(ws: &Workspace, sql: &'static str) -> i64 {
    let mut tx = ws.k.db.tenant_tx(ws.tenant).await.unwrap();
    let n: i64 = sqlx::query_scalar(sql).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    n
}

/// The workspace's receipts: (caller, actor, key), oldest first.
async fn receipts(ws: &Workspace) -> Vec<(Option<String>, Option<Uuid>, String)> {
    let mut tx = ws.k.db.tenant_tx(ws.tenant).await.unwrap();
    let rows = sqlx::query_as("select caller, actor, effect_key from logic_effect order by created_at, effect_key").fetch_all(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    rows
}

#[test]
fn an_app_calls_what_it_declared_for_the_person_it_acts_for() {
    run(scenario());
}

async fn scenario() {
    let apps = [app("carrier", carrier_packages, carrier_register), app("shop", shop_packages, nothing_registered), app("mall", mall_packages, mall_register)];
    let kit = Kit::boot(&database("FERRUM_TEST_DATABASE_URL"), &apps).await;
    let k = kit.k.clone();
    let ws = kit.workspace("acme", &["shop", "mall"], Value::Null).await;
    let _desk = ws.user("desk@acme.test", &["Dispatcher"]).await;
    // Pat holds no right on the carrier's records.
    let pat = ws.user("pat@acme.test", &[]).await;
    let parcel = k.insert(&ws.admin, "Parcel", obj(json!({"title": "Box 1"}))).await.unwrap();
    let ship = |carrier: &str| Invocation::new("carrier.parcel.ship", json!({"id": uid(&parcel), "params": {"carrier": carrier}}));

    // Pat may not ship a parcel. The shop, which declared that it uses shipping, does it for them, in their transaction:
    // one shipment and one receipt, naming the shop and Pat, under the shop's own key.
    refused(k.invoke(&pat, ship("DHL").key("direct")).await, "forbidden", "Parcel:write");
    let first = call(&kit, "shop", &pat, ship("DHL").key("order-1")).await.expect("granted");
    assert!(!first.repeated);
    assert_eq!(first.effect_key.as_deref(), Some("shop:order-1"), "the key is the shop's own");
    let shipment = k.get(&ws.admin, "Shipment", uid(&first.result["shipment"]), false).await.expect("the shipment");
    assert_eq!((shipment["carrier"].as_str(), uid(&shipment["parcel"])), (Some("DHL"), uid(&parcel)));
    assert_eq!(count(&ws, "select count(*) from e_shipment").await, 1);
    assert_eq!(receipts(&ws).await, [(Some("shop".to_string()), Some(pat.user), "shop:order-1".to_string())], "by the shop, for Pat");

    // The same key again answers what was done; the same key with other input is refused.
    let again = call(&kit, "shop", &pat, ship("DHL").key("order-1")).await.unwrap();
    assert!(again.repeated && again.result == first.result, "{again:?}");
    // The shop repeating its own key gets what it did, whoever it acts for now: as a job, with no person, or for
    // someone else. It made the shipment, and needs its id.
    let job = Ctx::system(ws.tenant, pat.schema.clone());
    let sam = ws.user("sam@acme.test", &[]).await;
    for (who, ctx) in [("a job", &job), ("Sam", &sam)] {
        let repeated = call(&kit, "shop", ctx, ship("DHL").key("order-1")).await.unwrap();
        assert!(repeated.repeated && repeated.result == first.result, "for {who}: {repeated:?}");
    }
    refused(call(&kit, "shop", &pat, ship("UPS").key("order-1")).await, "input changed", "order-1");
    assert_eq!(count(&ws, "select count(*) from e_shipment").await, 1, "once");
    // The key the shop took answers nobody else: an administrator sending it, with the same input, is refused.
    refused(k.invoke(&ws.admin, ship("DHL").key("shop:order-1")).await, "input changed", "another caller");

    // A caller that rolls back keeps neither the shipment nor the receipt, and the key is free for the next try.
    let mut tx = k.db.tenant_tx(ws.tenant).await.unwrap();
    k.call_tx(&kit.vault("shop"), &pat, &mut tx, &ship("DHL").key("order-2")).await.expect("done in the caller's transaction");
    tx.rollback().await.unwrap();
    assert_eq!(count(&ws, "select count(*) from e_shipment").await, 1, "the shipment rolled back with its caller");
    assert_eq!(count(&ws, "select count(*) from logic_effect where effect_key = 'shop:order-2'").await, 0, "and so did its receipt");
    assert!(!call(&kit, "shop", &pat, ship("DHL").key("order-2")).await.unwrap().repeated, "the retry does it");

    // Two apps choosing one key string do not meet: each key is its app's.
    let mall = call(&kit, "mall", &pat, ship("UPS").key("order-1")).await.expect("the mall's own");
    assert!(!mall.repeated && mall.effect_key.as_deref() == Some("mall:order-1"), "{mall:?}");
    assert_eq!(count(&ws, "select count(*) from e_shipment").await, 3);

    // The params are the action's: one it does not ask for, or a needed one missing, is refused, and nothing is done.
    let with = |params: Value, key: &str| Invocation::new("carrier.parcel.ship", json!({"id": uid(&parcel), "params": params})).key(key);
    refused(call(&kit, "shop", &pat, with(json!({"carrier": "DHL", "insure": true}), "p-1")).await, "invalid", "insure");
    refused(call(&kit, "shop", &pat, with(json!({"note": "fragile"}), "p-2")).await, "invalid", "carrier");
    refused(call(&kit, "shop", &pat, with(json!({"carrier": ""}), "p-3")).await, "invalid", "carrier");

    // Only what it declared: not another operation of the carrier, and nothing at all for an app that declared nothing.
    let create = Invocation::new("carrier.parcel.create", json!({"values": {"title": "Box 2"}})).key("c-1");
    refused(call(&kit, "shop", &pat, create.clone()).await, "forbidden", "shop does not use carrier.parcel.create");
    refused(call(&kit, "carrier", &pat, ship("DHL").key("own")).await, "forbidden", "carrier does not use carrier.parcel.ship");
    // Declared, but the carrier installed here does not offer it, or not at a version within the range declared.
    refused(call(&kit, "shop", &pat, Invocation::new("carrier.parcel.weigh", json!({"id": uid(&parcel)})).key("w-1")).await, "capability unavailable", "weigh");
    let read = Invocation::new("carrier.parcel.read", json!({"id": uid(&parcel)}));
    refused(call(&kit, "shop", &pat, read).await, "capability unavailable", "1.2.0");

    // What would wait for a person's approval is refused: a call does the thing or fails. Rolled back, nothing waits.
    let hold = Invocation::new("carrier.parcel.hold", json!({"id": uid(&parcel), "params": {}})).key("hold-1");
    let mut tx = k.db.tenant_tx(ws.tenant).await.unwrap();
    refused(k.call_tx(&kit.vault("shop"), &pat, &mut tx, &hold).await, "forbidden", "approval");
    tx.rollback().await.unwrap();
    assert_eq!(count(&ws, "select count(*) from approval_request").await, 0);

    // A zone that leaves the carrier out leaves it out of what the shop does for Pat there.
    let zones = Arc::new(Zones::parse(r#"{"floor": {"staff": ["front"]}}"#, "", "", "").unwrap());
    let entry = Arc::new(Entry { zones, placed: Placed::In("floor".into()), address: "10.1.2.3".into(), credential: Credential::Token });
    let zoned = k.request_ctx(ws.tenant, pat.user, None, "floor".into(), &entry).await.expect("Pat in the zone");
    refused(call(&kit, "shop", &zoned, ship("DHL").key("zoned")).await, "forbidden", "you may not use carrier.parcel.ship");

    // A handle another kernel gave out proves nothing here, nor this one there.
    let replica = kit.replica().await;
    let mut tx = replica.db.tenant_tx(ws.tenant).await.unwrap();
    refused(replica.call_tx(&kit.vault("shop"), &pat, &mut tx, &ship("DHL").key("stray")).await, "forbidden", "not given out by this server");
    drop(tx);

    // Where the shop is not installed, nothing of it is granted: refused before anything is written, even when the
    // caller commits.
    let plain = kit.workspace("plain", &["carrier"], Value::Null).await;
    let other = k.insert(&plain.admin, "Parcel", obj(json!({"title": "Box 9"}))).await.unwrap();
    let mut tx = k.db.tenant_tx(plain.tenant).await.unwrap();
    let there = Invocation::new("carrier.parcel.ship", json!({"id": uid(&other), "params": {"carrier": "DHL"}})).key("order-1");
    refused(k.call_tx(&kit.vault("shop"), &plain.admin, &mut tx, &there).await, "forbidden", "shop does not use carrier.parcel.ship");
    tx.commit().await.unwrap();
    assert_eq!((count(&plain, "select count(*) from e_shipment").await, count(&plain, "select count(*) from logic_effect").await), (0, 0));

    assert_eq!(count(&ws, "select count(*) from e_shipment").await, 3, "no refusal shipped anything");
    let callers: Vec<Option<String>> = receipts(&ws).await.into_iter().map(|r| r.0).collect();
    assert_eq!(callers, [Some("shop".to_string()), Some("shop".to_string()), Some("mall".to_string())]);

    // A refused call leaves nothing of itself, even when the caller carries on and commits: neither what the carrier's
    // own code wrote before it refused, nor an approval parked for a hold. What the caller wrote before the call stays.
    let mut tx = k.db.tenant_tx(ws.tenant).await.unwrap();
    k.insert_tx(&ws.admin, &mut tx, "Parcel", obj(json!({"title": "Box 3"}))).await.expect("the caller's own write");
    let pack = Invocation::new("carrier.parcel.pack", json!({"id": uid(&parcel), "params": {}})).key("pack-1");
    refused(k.call_tx(&kit.vault("shop"), &pat, &mut tx, &pack).await, "invalid", "the scale is broken");
    let hold = Invocation::new("carrier.parcel.hold", json!({"id": uid(&parcel), "params": {}})).key("hold-2");
    refused(k.call_tx(&kit.vault("shop"), &pat, &mut tx, &hold).await, "forbidden", "approval");
    tx.commit().await.expect("the caller commits all the same");
    assert_eq!(count(&ws, "select count(*) from e_shipment").await, 3, "the refused packing shipped nothing");
    assert_eq!(count(&ws, "select count(*) from approval_request").await, 0, "nothing waits for an approval");
    assert_eq!(count(&ws, "select count(*) from logic_effect").await, 3, "no receipt: the keys are free");
    assert_eq!(count(&ws, "select count(*) from e_parcel where title = 'Box 3'").await, 1, "the caller's own write stays");

    // Calls nest: the shop's call of the mall's dispatch runs the mall's calls of the carrier inside it. The carrier's
    // refusal of the packing is undone alone, and the mall ships instead: one shipment, a receipt for each call made.
    let order = obj(json!({"parcel": uid(&parcel)}));
    let dispatch = |order: &Doc, then: &str| {
        Invocation::new("mall.mall_order.dispatch", json!({"id": uid(order), "params": {"then_fail": then}})).key(format!("dispatch:{}", uid(order)))
    };
    let first = k.insert(&ws.admin, "MallOrder", order.clone()).await.unwrap();
    let done = call(&kit, "shop", &pat, dispatch(&first, "no")).await.expect("dispatched");
    assert!(!done.result["shipment"].is_null(), "{done:?}");
    assert_eq!(count(&ws, "select count(*) from e_shipment").await, 4);
    let callers: Vec<(Option<String>, String)> = receipts(&ws).await.into_iter().skip(3).map(|r| (r.0, r.2)).collect();
    assert_eq!(callers.len(), 2, "{callers:?}");
    assert!(callers.contains(&(Some("shop".into()), format!("shop:dispatch:{}", uid(&first)))), "{callers:?}");
    assert!(callers.contains(&(Some("mall".into()), format!("mall:ship:{}", uid(&first)))), "{callers:?}");
    // The outer operation refusing after its inner call succeeded takes the inner call with it, the caller committing
    // all the same.
    let second = k.insert(&ws.admin, "MallOrder", order).await.unwrap();
    let mut tx = k.db.tenant_tx(ws.tenant).await.unwrap();
    refused(k.call_tx(&kit.vault("shop"), &pat, &mut tx, &dispatch(&second, "yes")).await, "invalid", "dispatched and then refused");
    tx.commit().await.unwrap();
    assert_eq!(count(&ws, "select count(*) from e_shipment").await, 4, "the inner shipment went with the outer refusal");
    assert_eq!(count(&ws, "select count(*) from logic_effect").await, 5, "and so did both receipts");

    // Disabling the shop takes its packages out of the workspace, and its uses with them: nothing else withdraws them.
    k.disable_packages(&ws.admin, &["shop".to_string()]).await.unwrap();
    let pat = k.load_ctx(ws.tenant, pat.user, None, "it".into()).await.unwrap();
    refused(call(&kit, "shop", &pat, ship("DHL").key("after")).await, "forbidden", "shop does not use carrier.parcel.ship");
    assert!(call(&kit, "mall", &pat, ship("DHL").key("after")).await.is_ok(), "the mall's stay");
}
