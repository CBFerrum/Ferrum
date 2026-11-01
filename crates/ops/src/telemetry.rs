//! Measurements, outside the document engine. A site may send thousands at a time and repeat a
//! batch after a lost answer: a measurement is unique by device, boot and sequence, so a repeat
//! lands once, and only what actually landed counts in the rollups.
//!
//! Retention drops a day's partition, for every workspace at once. A day cannot be kept for one
//! workspace and dropped for another; rollups outlive the samples for that reason.
//
// ponytail: PostgreSQL carries this up to what one database can take. The strategy's figure of
// 20,000 samples a second is 345 GB a day, which belongs on an analytical store fed by sites; nothing
// here has been measured against that, and a site should aggregate at the edge first.

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use ferrum_kernel::{Ctx, Error, Kernel, Result, Tx};
use ferrum_ops_contract::telemetry::Measurement;
use ferrum_ops_contract::telemetry::Quality;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::atomic::{AtomicI32, Ordering};
use uuid::Uuid;

/// Days of samples, from `FERRUM_OPS_TELEMETRY_DAYS` (default 30).
pub fn retention_days() -> i64 {
    std::env::var("FERRUM_OPS_TELEMETRY_DAYS").ok().and_then(|v| v.parse().ok()).filter(|d| *d >= 1).unwrap_or(30)
}

/// Days of minute rollups.
const ROLLUP_DAYS: i64 = 400;

fn partition(day: NaiveDate) -> String {
    format!("op_measurement_{}", day.format("%Y%m%d"))
}

/// The day a measurement is filed under: its own, held to the days that exist, so a device whose
/// clock says 1970 or 2099 is still stored (and marked: see `Measurement::check`).
fn file_day(source: DateTime<Utc>, now: DateTime<Utc>, retention: i64) -> NaiveDate {
    let (oldest, newest) = ((now - Duration::days(retention)).date_naive(), (now + Duration::days(1)).date_naive());
    source.date_naive().clamp(oldest, newest)
}

/// The day partitions were last made for: every sync with measurements asks, and the answer changes
/// once a day.
static READY: AtomicI32 = AtomicI32::new(0);

/// Makes sure every day from `retention` ago to tomorrow has its partition.
pub async fn ensure_partitions(k: &Kernel, now: DateTime<Utc>) -> Result<()> {
    let today = now.date_naive().num_days_from_ce();
    if READY.load(Ordering::Relaxed) == today {
        return Ok(());
    }
    let retention = retention_days();
    let mut tx = k.db.system_tx().await?;
    sqlx::query("select pg_advisory_xact_lock(hashtext('ferrum.op_measurement'))").execute(&mut *tx).await?;
    let have: HashSet<String> =
        sqlx::query_scalar("select c.relname::text from pg_inherits i join pg_class c on c.oid = i.inhrelid where i.inhparent = 'op_measurement'::regclass")
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .collect();
    let mut day = (now - Duration::days(retention)).date_naive();
    let newest = (now + Duration::days(1)).date_naive();
    while day <= newest {
        let name = partition(day);
        if !have.contains(&name) {
            // The names and days are made here, from dates, never from input.
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "create table {name} partition of op_measurement for values from ('{day}') to ('{}')",
                day.succ_opt().ok_or_else(|| Error::internal("no day follows"))?
            )))
            .execute(&mut *tx)
            .await?;
        }
        day = day.succ_opt().ok_or_else(|| Error::internal("no day follows"))?;
    }
    tx.commit().await?;
    READY.store(today, Ordering::Relaxed);
    Ok(())
}

/// Drops days past retention and rollups past theirs.
pub async fn purge(k: &Kernel, now: DateTime<Utc>) -> Result<u64> {
    let oldest = (now - Duration::days(retention_days())).date_naive();
    let mut tx = k.db.system_tx().await?;
    let names: Vec<String> =
        sqlx::query_scalar("select c.relname::text from pg_inherits i join pg_class c on c.oid = i.inhrelid where i.inhparent = 'op_measurement'::regclass")
            .fetch_all(&mut *tx)
            .await?;
    let mut dropped = 0;
    for name in names {
        let Some(day) = name.strip_prefix("op_measurement_").and_then(|d| NaiveDate::parse_from_str(d, "%Y%m%d").ok()) else { continue };
        if day < oldest {
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!("drop table {name}"))).execute(&mut *tx).await?;
            dropped += 1;
        }
    }
    sqlx::query("delete from op_rollup_minute where minute < $1").bind(now - Duration::days(ROLLUP_DAYS)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(dropped)
}

/// Stores what `gateway`'s site may report. Returns how many were new. Measurements for devices of
/// another site, and ones that cannot be stored honestly, are left out and counted.
pub async fn ingest(tx: &mut Tx, tenant: Uuid, site: Uuid, batch: Vec<Measurement>, now: DateTime<Utc>) -> Result<(i64, i64)> {
    if batch.is_empty() {
        return Ok((0, 0));
    }
    let ours: HashSet<Uuid> = sqlx::query_scalar("select id from e_device where tenant_id = $1 and site = $2")
        .bind(tenant)
        .bind(site)
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .collect();
    let retention = retention_days();
    let total = batch.len() as i64;
    let (mut devices, mut days, mut boots, mut seqs, mut metrics, mut times, mut values, mut units, mut qualities, mut classes) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for mut m in batch {
        if !ours.contains(&m.device_id) || m.check(now).is_err() {
            continue;
        }
        devices.push(m.device_id);
        days.push(file_day(m.source_time, now, retention));
        boots.push(m.boot);
        seqs.push(m.seq);
        metrics.push(m.metric);
        times.push(m.source_time);
        values.push(m.value);
        units.push(m.unit);
        qualities.push(m.quality.code());
        classes.push(m.class.code());
    }
    let refused = total - devices.len() as i64;
    let stored: i64 = sqlx::query_scalar(
        "with incoming as ( \
           select * from unnest($2::uuid[], $3::date[], $4::uuid[], $5::bigint[], $6::text[], $7::timestamptz[], $8::float8[], $9::text[], $10::int2[], $11::int2[]) \
             as t(device_id, day, boot, seq, metric, source_time, value, unit, quality, class)), \
         landed as ( \
           insert into op_measurement (tenant_id, device_id, day, boot, seq, metric, source_time, value, unit, quality, class) \
           select $1, device_id, day, boot, seq, metric, source_time, value, unit, quality, class from incoming \
           on conflict do nothing returning device_id, metric, source_time, value, quality), \
         rolled as ( \
           insert into op_rollup_minute (tenant_id, device_id, metric, minute, samples, bad, min_value, max_value, sum_value, last_value, last_time) \
           select $1, device_id, metric, date_trunc('minute', source_time), count(*) filter (where quality < 2), count(*) filter (where quality = 2), \
                  min(value) filter (where quality < 2), max(value) filter (where quality < 2), coalesce(sum(value) filter (where quality < 2), 0), \
                  (array_agg(value order by source_time desc) filter (where quality < 2))[1], max(source_time) filter (where quality < 2) \
           from landed group by device_id, metric, date_trunc('minute', source_time) \
           on conflict (tenant_id, device_id, metric, minute) do update set \
             samples = op_rollup_minute.samples + excluded.samples, bad = op_rollup_minute.bad + excluded.bad, \
             min_value = least(op_rollup_minute.min_value, excluded.min_value), max_value = greatest(op_rollup_minute.max_value, excluded.max_value), \
             sum_value = op_rollup_minute.sum_value + excluded.sum_value, \
             last_value = case when excluded.last_time >= coalesce(op_rollup_minute.last_time, excluded.last_time) then excluded.last_value else op_rollup_minute.last_value end, \
             last_time = greatest(op_rollup_minute.last_time, excluded.last_time) \
           returning 1) \
         select (select count(*) from landed) + 0 * (select count(*) from rolled)",
    )
    .bind(tenant)
    .bind(&devices)
    .bind(&days)
    .bind(&boots)
    .bind(&seqs)
    .bind(&metrics)
    .bind(&times)
    .bind(&values)
    .bind(&units)
    .bind(&qualities)
    .bind(&classes)
    .fetch_one(&mut **tx)
    .await?;
    Ok((stored, refused))
}

/// Minute rollups of one metric, for dashboards, averaged up to `bucket_mins`.
pub async fn series(k: &Kernel, ctx: &Ctx, device: Uuid, metric: &str, from: DateTime<Utc>, to: DateTime<Utc>, bucket_mins: i64) -> Result<Vec<Value>> {
    if to <= from {
        return Err(Error::invalid("the period ends before it starts"));
    }
    let bucket = bucket_mins.clamp(1, 1440);
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let rows: Vec<(DateTime<Utc>, i64, i64, Option<f64>, Option<f64>, Option<f64>)> = sqlx::query_as(
        "select to_timestamp(floor(extract(epoch from minute) / ($6 * 60)) * ($6 * 60)) as bucket, sum(samples)::int8, sum(bad)::int8, min(min_value), max(max_value), \
                sum(sum_value) / nullif(sum(samples), 0) \
         from op_rollup_minute where tenant_id = $1 and device_id = $2 and metric = $3 and minute >= $4 and minute < $5 group by 1 order by 1 limit 2000",
    )
    .bind(ctx.tenant)
    .bind(device)
    .bind(metric)
    .bind(from)
    .bind(to)
    .bind(bucket as f64)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows
        .into_iter()
        .map(|(at, samples, bad, min, max, avg)| json!({"at": at, "samples": samples, "bad": bad, "min": min, "max": max, "avg": avg}))
        .collect())
}

/// The latest good value of each metric of a device, and how old it is. Staleness is shown, not
/// hidden: an old value is not the current state.
pub async fn latest(k: &Kernel, ctx: &Ctx, device: Uuid) -> Result<Vec<Value>> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let rows: Vec<Value> = sqlx::query_scalar(
        "select distinct on (metric) jsonb_build_object('metric', metric, 'value', last_value, 'at', last_time, 'age_secs', extract(epoch from now() - last_time)::int8) \
         from op_rollup_minute where tenant_id = $1 and device_id = $2 and last_time is not null order by metric, minute desc",
    )
    .bind(ctx.tenant)
    .bind(device)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// A measurement's quality, by the code stored.
pub fn quality_name(code: i16) -> &'static str {
    match code {
        c if c == Quality::Good.code() => "good",
        c if c == Quality::Uncertain.code() => "uncertain",
        _ => "bad",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wrong_clock_lands_in_a_day_that_exists() {
        let now = "2026-11-02T10:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let day = |s: &str| file_day(s.parse().unwrap(), now, 30).to_string();
        assert_eq!(day("2026-11-02T09:00:00Z"), "2026-11-02");
        assert_eq!(day("2026-10-20T09:00:00Z"), "2026-10-20", "a batch that waited out an outage keeps its day");
        assert_eq!(day("1970-01-01T00:00:00Z"), "2026-10-03", "the oldest day kept");
        assert_eq!(day("2099-01-01T00:00:00Z"), "2026-11-03", "tomorrow");
    }
}
