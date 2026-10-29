//! The analytics query spec: one self-describing JSON shape sent by the builder UI, stored in saved
//! reports, charts and number cards, and written by AI agents. Unknown keys are refused, so a typo
//! is an error that names the expected keys instead of a silently ignored setting.

use chrono::{Datelike, Duration, Months, NaiveDate};
use ferrum_kernel::{Error, FieldError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Rows a query may return.
pub const MAX_LIMIT: u32 = 10_000;
pub const DEFAULT_LIMIT: u32 = 500;
/// Columns a pivot may spread its values over.
pub const MAX_PIVOT_COLUMNS: u32 = 100;
pub const DEFAULT_PIVOT_COLUMNS: u32 = 20;
/// Cells (rows × columns × measures) a pivot may hold.
pub const MAX_PIVOT_CELLS: u64 = 200_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The base entity, e.g. `SalesInvoice`.
    pub entity: String,
    /// The kernel's filter syntax: `[[path, op, value], ...]` or `{path: value}`; paths may be
    /// `field`, `link.field`, `table.field`, `table.link.field` or `_tag`.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub filters: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    /// Plain record columns (a table report). Ignored when grouping or measuring.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldSel>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_by: Vec<GroupBy>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub measures: Vec<Measure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pivot: Option<Pivot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sort: Vec<Sort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_series: Option<TimeSeries>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_range: Option<DateRange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compare: Option<Compare>,
    /// Calculated columns over the result's other columns (see [`crate::formula`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub formulas: Vec<FormulaSel>,
    /// With two or more groupings: the measures per value of the first one, as `subtotals`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub subtotals: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FormulaSel {
    pub label: String,
    /// E.g. `[Revenue] - [Cost]`: other columns by name or label in brackets.
    pub expr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FieldSel {
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GroupBy {
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<Bucket>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Measure {
    #[serde(rename = "fn")]
    pub func: Agg,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Pivot {
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<Bucket>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_columns: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Sort {
    /// A column name (`customer`, `month(posting_date)`, `sum(grand_total)`), a group-by path or
    /// a label.
    pub by: String,
    #[serde(default)]
    pub dir: Dir,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TimeSeries {
    pub field: String,
    pub bucket: Bucket,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default)]
    pub fill_zero: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DateRange {
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Agg {
    Count,
    CountDistinct,
    Sum,
    Avg,
    Min,
    Max,
}

impl Agg {
    pub fn name(self) -> &'static str {
        match self {
            Agg::Count => "count",
            Agg::CountDistinct => "count_distinct",
            Agg::Sum => "sum",
            Agg::Avg => "avg",
            Agg::Min => "min",
            Agg::Max => "max",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Agg::Count => "Count",
            Agg::CountDistinct => "Distinct count of",
            Agg::Sum => "Sum of",
            Agg::Avg => "Average of",
            Agg::Min => "Minimum of",
            Agg::Max => "Maximum of",
        }
    }

    /// Whether a missing group is worth zero (an empty sum) rather than unknown (an empty average).
    pub fn additive(self) -> bool {
        matches!(self, Agg::Count | Agg::CountDistinct | Agg::Sum)
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Dir {
    #[default]
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Bucket {
    Day,
    Week,
    Month,
    Quarter,
    Year,
    /// Quarters of the workspace's fiscal year (fiscal_year_start_month).
    FiscalQuarter,
    FiscalYear,
}

impl Bucket {
    pub const ALL: [Bucket; 7] = [Bucket::Day, Bucket::Week, Bucket::Month, Bucket::Quarter, Bucket::Year, Bucket::FiscalQuarter, Bucket::FiscalYear];

    pub fn name(self) -> &'static str {
        match self {
            Bucket::Day => "day",
            Bucket::Week => "week",
            Bucket::Month => "month",
            Bucket::Quarter => "quarter",
            Bucket::Year => "year",
            Bucket::FiscalQuarter => "fiscal_quarter",
            Bucket::FiscalYear => "fiscal_year",
        }
    }

    /// SQL for the first day of the bucket holding the timestamp `ts`. Units are fixed literals;
    /// fiscal buckets shift by the fiscal start month (a number from the workspace settings).
    pub fn sql(self, ts: &str, fy_month: u32) -> String {
        let shift = fy_month.clamp(1, 12) - 1;
        match self {
            Bucket::FiscalQuarter | Bucket::FiscalYear => {
                let unit = if self == Bucket::FiscalYear { "year" } else { "quarter" };
                format!("(date_trunc('{unit}', {ts} - interval '{shift} months') + interval '{shift} months')::date")
            }
            other => format!("date_trunc('{}', {ts})::date", other.name()),
        }
    }

    /// First day of the bucket holding `d`, exactly as the SQL computes it (ISO weeks start on
    /// Monday; fiscal buckets start in the fiscal start month).
    pub fn start(self, d: NaiveDate, fy_month: u32) -> NaiveDate {
        match self {
            Bucket::Day => d,
            Bucket::Week => d - Duration::days(d.weekday().num_days_from_monday() as i64),
            Bucket::Month => d.with_day(1).unwrap_or(d),
            Bucket::Quarter => NaiveDate::from_ymd_opt(d.year(), (d.month0() / 3) * 3 + 1, 1).unwrap_or(d),
            Bucket::Year => NaiveDate::from_ymd_opt(d.year(), 1, 1).unwrap_or(d),
            Bucket::FiscalYear => ferrum_domain::reports::fy_start(d, fy_month.clamp(1, 12)),
            Bucket::FiscalQuarter => {
                let fy = ferrum_domain::reports::fy_start(d, fy_month.clamp(1, 12));
                let months = (d.year() - fy.year()) * 12 + d.month() as i32 - fy.month() as i32;
                fy + Months::new((months / 3 * 3) as u32)
            }
        }
    }

    /// The bucket after the one starting at `start`.
    pub fn next(self, start: NaiveDate) -> NaiveDate {
        match self {
            Bucket::Day => start + Duration::days(1),
            Bucket::Week => start + Duration::days(7),
            Bucket::Month => start + Months::new(1),
            Bucket::Quarter | Bucket::FiscalQuarter => start + Months::new(3),
            Bucket::Year | Bucket::FiscalYear => start + Months::new(12),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Bucket::Day => "Day",
            Bucket::Week => "Week",
            Bucket::Month => "Month",
            Bucket::Quarter => "Quarter",
            Bucket::Year => "Year",
            Bucket::FiscalQuarter => "Fiscal Quarter",
            Bucket::FiscalYear => "Fiscal Year",
        }
    }
}

/// Every bucket start from the one holding `from` to the one holding `to`; `None` when there
/// would be more than `cap`.
pub fn buckets(b: Bucket, from: NaiveDate, to: NaiveDate, fy_month: u32, cap: usize) -> Option<Vec<NaiveDate>> {
    let mut out = Vec::new();
    let mut d = b.start(from, fy_month);
    while d <= to {
        if out.len() >= cap {
            return None;
        }
        out.push(d);
        d = b.next(d);
    }
    Some(out)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Compare {
    /// The period of the same shape just before (month-aligned ranges move by whole months).
    PreviousPeriod,
    /// The same dates one year earlier.
    PreviousYear,
}

fn last_of_month(d: NaiveDate) -> NaiveDate {
    (d.with_day(1).unwrap_or(d) + Months::new(1)) - Duration::days(1)
}

fn shift_months(d: NaiveDate, months: u32, keep_month_end: bool) -> NaiveDate {
    let shifted = d - Months::new(months);
    if keep_month_end && d == last_of_month(d) { last_of_month(shifted) } else { shifted }
}

/// The range to compare `[from, to]` with. A range starting on the 1st of a month moves back by
/// the months it spans (October 1–31 → September 1–30; month-to-date October 1–29 → September
/// 1–29; a quarter → the previous quarter); any other range moves back by its own length.
pub fn previous(kind: Compare, from: NaiveDate, to: NaiveDate) -> (NaiveDate, NaiveDate) {
    match kind {
        Compare::PreviousYear => (shift_months(from, 12, false), shift_months(to, 12, true)),
        Compare::PreviousPeriod if from.day() == 1 => {
            let span = (to.year() * 12 + to.month0() as i32) - (from.year() * 12 + from.month0() as i32) + 1;
            let span = span.max(1) as u32;
            (shift_months(from, span, false), shift_months(to, span, true))
        }
        Compare::PreviousPeriod => {
            let len = (to - from).num_days();
            let prev_to = from - Duration::days(1);
            (prev_to - Duration::days(len), prev_to)
        }
    }
}

/// Resolves a date: `YYYY-MM-DD`, or an anchor — `today`, `start_of_week|month|quarter|year|fiscal_year`,
/// `end_of_week|month|quarter|year|fiscal_year` — followed by offsets like `-30d`, `+1w`, `-11m`,
/// `-1q`, `-1y` (an offset alone counts from today). Saved dashboards use these to stay current.
pub fn resolve_date(expr: &str, today: NaiveDate, fy_month: u32) -> std::result::Result<NaiveDate, String> {
    let s = expr.trim().to_lowercase();
    if s.is_empty() {
        return Err("is empty".into());
    }
    if s.as_bytes()[0].is_ascii_digit() {
        return ferrum_kernel::value::parse_date(&s);
    }
    let split = s.find(['+', '-']).unwrap_or(s.len());
    let (anchor, mut rest) = s.split_at(split);
    let fy = |d: NaiveDate| ferrum_domain::reports::fy_start(d, fy_month.clamp(1, 12));
    let mut d = match anchor {
        "" | "today" => today,
        "start_of_week" => Bucket::Week.start(today, fy_month),
        "end_of_week" => Bucket::Week.start(today, fy_month) + Duration::days(6),
        "start_of_month" => Bucket::Month.start(today, fy_month),
        "end_of_month" => last_of_month(today),
        "start_of_quarter" => Bucket::Quarter.start(today, fy_month),
        "end_of_quarter" => Bucket::Quarter.start(today, fy_month) + Months::new(3) - Duration::days(1),
        "start_of_year" => Bucket::Year.start(today, fy_month),
        "end_of_year" => Bucket::Year.start(today, fy_month) + Months::new(12) - Duration::days(1),
        "start_of_fiscal_year" => fy(today),
        "end_of_fiscal_year" => fy(today) + Months::new(12) - Duration::days(1),
        other => {
            return Err(format!(
                "'{other}' is not a date: use YYYY-MM-DD or today, start_of_week|month|quarter|year|fiscal_year, \
                 end_of_..., optionally followed by offsets such as -30d, +1w, -11m, -1q, -1y"
            ));
        }
    };
    while !rest.is_empty() {
        let negative = rest.starts_with('-');
        let body = &rest[1..];
        let digits = body.find(|c: char| !c.is_ascii_digit()).unwrap_or(body.len());
        let n: u32 = body[..digits].parse().map_err(|_| format!("offset '{rest}' needs a number, e.g. -30d"))?;
        if n > 100_000 {
            return Err(format!("offset '{rest}' is too large"));
        }
        let unit = body[digits..].chars().next().ok_or_else(|| format!("offset '{rest}' needs a unit: d, w, m, q or y"))?;
        let months = |m: u32| if negative { d.checked_sub_months(Months::new(m)) } else { d.checked_add_months(Months::new(m)) };
        let days = |n: i64| if negative { d.checked_sub_signed(Duration::days(n)) } else { d.checked_add_signed(Duration::days(n)) };
        d = match unit {
            'd' => days(n as i64),
            'w' => days(n as i64 * 7),
            'm' => months(n),
            'q' => months(n * 3),
            'y' => months(n * 12),
            other => return Err(format!("unknown offset unit '{other}': use d, w, m, q or y")),
        }
        .ok_or_else(|| format!("'{expr}' is out of range"))?;
        rest = &body[digits + 1..];
    }
    if !(1900..=9999).contains(&d.year()) {
        return Err(format!("'{expr}' is out of range"));
    }
    Ok(d)
}

impl Spec {
    /// Parses a spec, refusing unknown keys and malformed values with the serde message (which
    /// names the expected keys and variants).
    pub fn parse(v: &Value) -> Result<Spec> {
        if !v.is_object() {
            return Err(Error::field("spec", "must be an object like {\"entity\": \"SalesInvoice\", \"measures\": [{\"fn\": \"count\"}]}"));
        }
        serde_json::from_value(v.clone()).map_err(|e| Error::field("spec", e.to_string()))
    }

    pub fn limit(&self) -> Result<u32> {
        match self.limit {
            None => Ok(DEFAULT_LIMIT),
            Some(n) if (1..=MAX_LIMIT).contains(&n) => Ok(n),
            Some(_) => Err(Error::field("limit", format!("must be between 1 and {MAX_LIMIT}"))),
        }
    }

    /// Adds runtime filters (AND) to the saved ones.
    pub fn and_filters(&mut self, extra: &Value) -> Result<()> {
        let mut list = filter_list(&self.filters, "filters")?;
        list.extend(filter_list(extra, "filters")?);
        self.filters = Value::Array(list);
        Ok(())
    }
}

/// Filters in their list form `[[path, op, value], ...]` (the object form `{path: value}` and
/// `{path: [op, value]}` is converted), checked with the kernel's parser.
pub fn filter_list(v: &Value, at: &str) -> Result<Vec<Value>> {
    ferrum_kernel::query::parse_filters(v).map_err(|e| Error::Validation(vec![FieldError::new(at, e.to_string())]))?;
    Ok(match v {
        Value::Null => vec![],
        Value::Array(items) => items.clone(),
        Value::Object(m) => m
            .iter()
            .map(|(k, val)| match val {
                Value::Array(a) if a.len() == 2 && a[0].is_string() => Value::Array(vec![Value::String(k.clone()), a[0].clone(), a[1].clone()]),
                other => Value::Array(vec![Value::String(k.clone()), Value::String("=".into()), other.clone()]),
            })
            .collect(),
        _ => vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn relative_dates() {
        let today = d(2026, 10, 29); // a Thursday
        let r = |e: &str| resolve_date(e, today, 4).unwrap();
        assert_eq!(r("2026-01-05"), d(2026, 1, 5));
        assert_eq!(r("today"), today);
        assert_eq!(r("today-29d"), d(2026, 9, 30));
        assert_eq!(r("-1w"), d(2026, 10, 22));
        assert_eq!(r("start_of_week"), d(2026, 10, 26));
        assert_eq!(r("end_of_week"), d(2026, 11, 1));
        assert_eq!(r("start_of_month-11m"), d(2025, 11, 1));
        assert_eq!(r("end_of_month-1m"), d(2026, 9, 30));
        assert_eq!(r("start_of_quarter"), d(2026, 10, 1));
        assert_eq!(r("end_of_quarter"), d(2026, 12, 31));
        assert_eq!(r("start_of_year-1y"), d(2025, 1, 1));
        assert_eq!(r("start_of_fiscal_year"), d(2026, 4, 1));
        assert_eq!(r("end_of_fiscal_year"), d(2027, 3, 31));
        assert_eq!(r("start_of_month+1m-1d"), d(2026, 10, 31));
        assert_eq!(r("START_OF_MONTH"), d(2026, 10, 1));
        for bad in ["yesterday", "today-3x", "today-d", "2026-13-01", "", "today-999999y"] {
            assert!(resolve_date(bad, today, 4).is_err(), "{bad}");
        }
    }

    #[test]
    fn previous_periods() {
        let p = |a, b| previous(Compare::PreviousPeriod, a, b);
        assert_eq!(p(d(2026, 10, 1), d(2026, 10, 31)), (d(2026, 9, 1), d(2026, 9, 30)), "a whole month");
        assert_eq!(p(d(2026, 10, 1), d(2026, 10, 29)), (d(2026, 9, 1), d(2026, 9, 29)), "month to date");
        assert_eq!(p(d(2026, 3, 1), d(2026, 3, 31)), (d(2026, 2, 1), d(2026, 2, 28)), "month ends stay month ends");
        assert_eq!(p(d(2026, 7, 1), d(2026, 9, 30)), (d(2026, 4, 1), d(2026, 6, 30)), "a quarter");
        assert_eq!(p(d(2026, 10, 10), d(2026, 10, 19)), (d(2026, 9, 30), d(2026, 10, 9)), "ten days");
        assert_eq!(previous(Compare::PreviousYear, d(2024, 2, 1), d(2024, 2, 29)), (d(2023, 2, 1), d(2023, 2, 28)));
        assert_eq!(previous(Compare::PreviousYear, d(2026, 5, 3), d(2026, 6, 9)), (d(2025, 5, 3), d(2025, 6, 9)));
    }

    #[test]
    fn bucket_starts_match_date_trunc() {
        assert_eq!(Bucket::Week.start(d(2026, 9, 2), 4), d(2026, 8, 31));
        assert_eq!(Bucket::Quarter.start(d(2026, 8, 15), 4), d(2026, 7, 1));
        assert_eq!(Bucket::Year.start(d(2026, 8, 15), 4), d(2026, 1, 1));
        // An April fiscal year: Q1 is April-June, Q4 January-March.
        assert_eq!(Bucket::FiscalYear.start(d(2026, 3, 31), 4), d(2025, 4, 1));
        assert_eq!(Bucket::FiscalQuarter.start(d(2026, 5, 15), 4), d(2026, 4, 1));
        assert_eq!(Bucket::FiscalQuarter.start(d(2026, 3, 10), 4), d(2026, 1, 1));
        assert_eq!(Bucket::FiscalQuarter.start(d(2026, 12, 31), 4), d(2026, 10, 1));
        assert_eq!(Bucket::FiscalQuarter.start(d(2026, 2, 1), 1), d(2026, 1, 1));
        assert_eq!(Bucket::FiscalYear.sql("x", 4), "(date_trunc('year', x - interval '3 months') + interval '3 months')::date");
        assert_eq!(Bucket::Week.sql("x", 4), "date_trunc('week', x)::date");
        let months = buckets(Bucket::Month, d(2026, 7, 15), d(2026, 10, 2), 4, 100).unwrap();
        assert_eq!(months, vec![d(2026, 7, 1), d(2026, 8, 1), d(2026, 9, 1), d(2026, 10, 1)]);
        let fq = buckets(Bucket::FiscalQuarter, d(2026, 2, 1), d(2026, 7, 1), 4, 100).unwrap();
        assert_eq!(fq, vec![d(2026, 1, 1), d(2026, 4, 1), d(2026, 7, 1)]);
        assert!(buckets(Bucket::Day, d(2000, 1, 1), d(2026, 1, 1), 4, 1000).is_none(), "capped");
        assert_eq!(buckets(Bucket::Day, d(2026, 1, 2), d(2026, 1, 1), 4, 10).unwrap(), Vec::<NaiveDate>::new());
    }

    #[test]
    fn specs_are_strict_and_self_describing() {
        let ok = Spec::parse(
            &json!({"entity": "SalesInvoice", "group_by": [{"field": "posting_date", "bucket": "month"}], "measures": [{"fn": "sum", "field": "grand_total"}]}),
        )
        .unwrap();
        assert_eq!(ok.group_by[0].bucket, Some(Bucket::Month));
        let typo = Spec::parse(&json!({"entity": "SalesInvoice", "grup_by": []})).unwrap_err();
        assert!(matches!(&typo, Error::Validation(f) if f[0].message.contains("unknown field `grup_by`") && f[0].message.contains("group_by")), "{typo:?}");
        let bucket = Spec::parse(&json!({"entity": "X", "group_by": [{"field": "d", "bucket": "monthly"}]})).unwrap_err();
        assert!(matches!(&bucket, Error::Validation(f) if f[0].message.contains("expected one of")), "{bucket:?}");
        assert!(Spec::parse(&json!("SalesInvoice")).is_err());
        assert_eq!(Spec { limit: Some(10_001), ..Default::default() }.limit().unwrap_err().to_string(), "validation failed");
    }

    #[test]
    fn runtime_filters_are_anded() {
        let mut s = Spec { entity: "SalesInvoice".into(), filters: json!({"status": "Paid", "grand_total": [">", 10]}), ..Default::default() };
        s.and_filters(&json!([["customer", "=", "x"]])).unwrap();
        assert_eq!(s.filters, json!([["grand_total", ">", 10], ["status", "=", "Paid"], ["customer", "=", "x"]]));
        assert!(s.and_filters(&json!([["a", "~", 1]])).is_err());
    }
}
