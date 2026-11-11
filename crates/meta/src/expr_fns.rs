//! What record expressions add to CEL's own functions (docs/builder/expressions.md): dates, text, and CEL's
//! strings extension. Additions only: a name CEL already gives a meaning keeps it, and every function here answers an
//! empty (null or '') date with null rather than failing.

use cel::extractors::Arguments;
use cel::{Context, Env, ExecutionError, FunctionContext, Value as C};
use chrono::{DateTime, Datelike, FixedOffset, Months, NaiveDate, NaiveDateTime, SecondsFormat};
use std::fmt::Write;
use std::sync::{Arc, LazyLock};

/// CEL's standard library with its strings extension (`lowerAscii`, `trim`, `replace`, `split`, `join`,
/// `substring`, `indexOf`, `format`...), shared by every evaluation.
pub(crate) static ENV: LazyLock<Arc<Env>> = LazyLock::new(|| {
    let mut env = Env::stdlib();
    env.add_extension(cel::extensions::strings).expect("CEL's strings extension");
    Arc::new(env)
});

/// The functions added, each bound only to the expressions that call it.
pub(crate) const NAMES: &[&str] = &[
    "addDays",
    "addMonths",
    "addYears",
    "startOfMonth",
    "endOfMonth",
    "weekday",
    "daysBetween",
    "monthsBetween",
    "year",
    "month",
    "day",
    "formatDate",
    "lower",
    "upper",
    "padStart",
    "padEnd",
    "concat",
];

/// Binds `name` (one of [`NAMES`]) on `ctx`; it is called as `name(a, b)` or as `a.name(b)`.
pub(crate) fn bind(ctx: &mut Context, name: &'static str) {
    let f = move |ftx: &FunctionContext, Arguments(args): Arguments| -> Result<C, ExecutionError> {
        let this = ftx.this.as_ref().map(|t| C::try_from(t.as_ref())).transpose()?;
        let all: Vec<C> = this.into_iter().chain(args.iter().cloned()).collect();
        call(name, &all).map_err(|e| ExecutionError::function_error(name, e))
    };
    // Only names CEL does not declare are listed, so this cannot fail.
    let _ = ctx.add_function(name, f);
}

fn call(name: &str, a: &[C]) -> Result<C, String> {
    let arity = |n: usize| if a.len() == n { Ok(()) } else { Err(format!("takes {n} arguments, not {}", a.len())) };
    let one = || -> Result<Option<When>, String> {
        arity(1)?;
        when(&a[0])
    };
    let int = |i: i64| C::Int(i);
    Ok(match name {
        "addDays" | "addMonths" | "addYears" => {
            arity(2)?;
            let (Some(d), Some(n)) = (when(&a[0])?, whole(&a[1])?) else { return Ok(C::Null) };
            let to = match name {
                "addDays" => chrono::Duration::try_days(n).and_then(|n| d.date().checked_add_signed(n)),
                "addMonths" => add_months(d.date(), n),
                _ => n.checked_mul(12).and_then(|n| add_months(d.date(), n)),
            };
            d.with(to.ok_or("the date is out of range")?)
        }
        "startOfMonth" => one()?.map_or(C::Null, |d| text(d.date().with_day(1).unwrap_or(d.date()))),
        "endOfMonth" => one()?.map_or(C::Null, |d| text(month_end(d.date()))),
        "weekday" => one()?.map_or(C::Null, |d| int(d.date().weekday().number_from_monday().into())),
        "year" => one()?.map_or(C::Null, |d| int(d.date().year().into())),
        "month" => one()?.map_or(C::Null, |d| int(d.date().month().into())),
        "day" => one()?.map_or(C::Null, |d| int(d.date().day().into())),
        "daysBetween" | "monthsBetween" => {
            arity(2)?;
            let (Some(x), Some(y)) = (when(&a[0])?, when(&a[1])?) else { return Ok(C::Null) };
            let (x, y) = (x.date(), y.date());
            int(if name == "daysBetween" { (y - x).num_days() } else { months_between(x, y) })
        }
        "formatDate" => {
            arity(2)?;
            let Some(d) = when(&a[0])? else { return Ok(C::Null) };
            let C::String(p) = &a[1] else { return Err("the pattern is text, such as '%d %B %Y'".into()) };
            let mut out = String::new();
            let written = match d {
                When::Date(x) => write!(out, "{}", x.format(p)),
                When::Zoned(x) | When::Stamp(x) => write!(out, "{}", x.format(p)),
                When::Naive(x) => write!(out, "{}", x.format(p)),
            };
            written.map_err(|_| format!("'{p}' is not a pattern this date can be written with"))?;
            C::String(Arc::new(out))
        }
        "lower" | "upper" => match (arity(1).map(|_| &a[0])?, name) {
            (C::Null, _) => C::Null,
            (C::String(s), "lower") => C::String(Arc::new(s.to_lowercase())),
            (C::String(s), _) => C::String(Arc::new(s.to_uppercase())),
            (other, _) => return Err(format!("expected text, got {other:?}")),
        },
        "padStart" | "padEnd" => {
            if !(2..=3).contains(&a.len()) {
                return Err(format!("takes 2 or 3 arguments, not {}", a.len()));
            }
            let fill = match a.get(2) {
                None => " ",
                Some(C::String(f)) if !f.is_empty() => f.as_str(),
                Some(_) => return Err("the fill is text of at least one character".into()),
            };
            let (s, Some(n)) = (&a[0], whole(&a[1])?) else { return Ok(C::Null) };
            let s = match s {
                C::Null => return Ok(C::Null),
                C::String(s) => s.as_str(),
                other => return Err(format!("expected text, got {other:?}")),
            };
            let short = usize::try_from(n).unwrap_or(0).saturating_sub(s.chars().count()).min(10_000);
            let pad: String = fill.chars().cycle().take(short).collect();
            C::String(Arc::new(if name == "padStart" { pad + s } else { format!("{s}{pad}") }))
        }
        "concat" => {
            let mut out = String::new();
            for v in a {
                match v {
                    C::Null => {}
                    C::String(s) => out.push_str(s),
                    C::Int(i) => out.push_str(&i.to_string()),
                    C::UInt(u) => out.push_str(&u.to_string()),
                    C::Float(f) => out.push_str(&f.to_string()),
                    C::Bool(b) => out.push_str(&b.to_string()),
                    C::Timestamp(t) => out.push_str(&t.to_rfc3339()),
                    other => return Err(format!("cannot write {other:?} as text")),
                }
            }
            C::String(Arc::new(out))
        }
        _ => return Err("unknown function".into()),
    })
}

/// A date as an expression may hold one: a date field (`YYYY-MM-DD`), a date-time field (RFC 3339, or without
/// a zone), or a CEL timestamp. Each keeps its own form when a function gives one back.
#[derive(Clone, Copy)]
enum When {
    Date(NaiveDate),
    Zoned(DateTime<FixedOffset>),
    Naive(NaiveDateTime),
    Stamp(DateTime<FixedOffset>),
}

impl When {
    fn date(self) -> NaiveDate {
        match self {
            When::Date(d) => d,
            When::Zoned(t) | When::Stamp(t) => t.date_naive(),
            When::Naive(t) => t.date(),
        }
    }

    /// The same moment of the day on `d`, in this one's form.
    fn with(self, d: NaiveDate) -> C {
        let moved = |t: DateTime<FixedOffset>| d.and_time(t.time()).and_local_timezone(*t.offset()).single();
        match self {
            When::Date(_) => text(d),
            When::Zoned(t) => moved(t).map_or(C::Null, |t| C::String(Arc::new(t.to_rfc3339_opts(SecondsFormat::AutoSi, true)))),
            When::Stamp(t) => moved(t).map_or(C::Null, C::Timestamp),
            When::Naive(t) => C::String(Arc::new(d.and_time(t.time()).format("%Y-%m-%dT%H:%M:%S%.f").to_string())),
        }
    }
}

/// Null or '' is an empty date: no date, and no error.
fn when(v: &C) -> Result<Option<When>, String> {
    match v {
        C::Null => Ok(None),
        C::String(s) if s.is_empty() => Ok(None),
        C::String(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map(When::Date)
            .or_else(|_| DateTime::parse_from_rfc3339(s).map(When::Zoned))
            .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").map(When::Naive))
            .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f").map(When::Naive))
            .map(Some)
            .map_err(|_| format!("'{s}' is not a date")),
        C::Timestamp(t) => Ok(Some(When::Stamp(*t))),
        other => Err(format!("expected a date, got {other:?}")),
    }
}

/// A whole number of days, months or years; null when empty.
fn whole(v: &C) -> Result<Option<i64>, String> {
    match v {
        C::Null => Ok(None),
        C::Int(i) => Ok(Some(*i)),
        C::UInt(u) => i64::try_from(*u).map(Some).map_err(|_| "the number is too large".into()),
        C::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => Ok(Some(*f as i64)),
        other => Err(format!("expected a whole number, got {other:?}")),
    }
}

fn text(d: NaiveDate) -> C {
    C::String(Arc::new(d.format("%Y-%m-%d").to_string()))
}

/// `n` calendar months on, kept within the month it lands in: 31 January + 1 month is the last of February.
fn add_months(d: NaiveDate, n: i64) -> Option<NaiveDate> {
    let m = Months::new(u32::try_from(n.unsigned_abs()).ok()?);
    if n >= 0 { d.checked_add_months(m) } else { d.checked_sub_months(m) }
}

fn month_end(d: NaiveDate) -> NaiveDate {
    add_months(d.with_day(1).unwrap_or(d), 1).and_then(|n| n.pred_opt()).unwrap_or(d)
}

/// Whole months from `x` to `y` as `addMonths` counts them: the most `n` with `addMonths(x, n)` not past `y`.
fn months_between(x: NaiveDate, y: NaiveDate) -> i64 {
    if y < x {
        return -months_between(y, x);
    }
    let n = i64::from(y.year() - x.year()) * 12 + i64::from(y.month()) - i64::from(x.month());
    if add_months(x, n).is_some_and(|d| d > y) { n - 1 } else { n }
}
