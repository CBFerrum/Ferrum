//! Pure finance arithmetic: installment due dates and exact splits, payments settling
//! installments in order, interest by the actual/actual day count, budget phasing, billing
//! periods, statement parsing and IBAN checks. No I/O, so it is unit-tested here and shared by
//! the hooks, the upkeep and the reports.

use chrono::{Datelike, Duration, NaiveDate};
use ferrum_domain::util::round;
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap_or(NaiveDate::MIN)
}

pub fn month_start(d: NaiveDate) -> NaiveDate {
    ymd(d.year(), d.month(), 1)
}

pub fn month_end(d: NaiveDate) -> NaiveDate {
    let (y, m) = if d.month() == 12 { (d.year() + 1, 1) } else { (d.year(), d.month() + 1) };
    ymd(y, m, 1) - Duration::days(1)
}

/// `d` plus `months` calendar months (negative goes back), the day clamped to the month's length.
pub fn add_months(d: NaiveDate, months: i64) -> NaiveDate {
    let total = d.year() as i64 * 12 + d.month0() as i64 + months;
    let first = ymd(total.div_euclid(12) as i32, total.rem_euclid(12) as u32 + 1, 1);
    ymd(first.year(), first.month(), d.day().min(month_end(first).day()))
}

// ───────────────────────────── payment terms ─────────────────────────────

/// Due date of an installment under one of the payment term rules.
pub fn due_date(rule: &str, invoice: NaiveDate, credit_days: i64, credit_months: i64, day_of_month: u32) -> NaiveDate {
    match rule {
        "Day(s) after the end of the invoice month" => month_end(invoice) + Duration::days(credit_days),
        "Month(s) after the end of the invoice month" => month_end(add_months(invoice, credit_months)),
        "Day of the month" => {
            let in_month = |m: NaiveDate| ymd(m.year(), m.month(), day_of_month.clamp(1, 31).min(month_end(m).day()));
            let target = in_month(add_months(month_start(invoice), credit_months));
            if target < invoice { in_month(add_months(month_start(invoice), credit_months + 1)) } else { target }
        }
        _ => invoice + Duration::days(credit_days),
    }
}

/// `total` split by `portions` (percentages) at `precision` decimals; the last part absorbs the
/// rounding, so the parts always add up to `total` exactly.
pub fn split(total: Decimal, portions: &[Decimal], precision: u32) -> Vec<Decimal> {
    let mut used = Decimal::ZERO;
    portions
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if i + 1 == portions.len() {
                total - used
            } else {
                let part = round(total * p / Decimal::ONE_HUNDRED, precision);
                used += part;
                part
            }
        })
        .collect()
}

/// What stays open of each installment once `paid` has settled them in order (earliest first).
pub fn installments_open(amounts: &[Decimal], paid: Decimal) -> Vec<Decimal> {
    let mut left = paid.max(Decimal::ZERO);
    amounts
        .iter()
        .map(|a| {
            let take = left.min(*a).max(Decimal::ZERO);
            left -= take;
            *a - take
        })
        .collect()
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Simple interest at `rate_pct` a year on `principal` for every day after `due` up to and
/// including `on`, by the actual/actual (ISDA) day count: a day in a leap year is 1/366 of a year,
/// any other day 1/365. Days are weighted over the common denominator 365 × 366, so the only
/// division happens once, before the single rounding.
pub fn interest(principal: Decimal, rate_pct: Decimal, due: NaiveDate, on: NaiveDate, precision: u32) -> Decimal {
    if on <= due || principal <= Decimal::ZERO || rate_pct <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let mut weighted: i64 = 0;
    let mut start = due + Duration::days(1);
    while start <= on {
        let end = ymd(start.year(), 12, 31).min(on);
        let days = (end - start).num_days() + 1;
        weighted += days * if is_leap(start.year()) { 365 } else { 366 };
        start = end + Duration::days(1);
    }
    round(principal * rate_pct * Decimal::from(weighted) / Decimal::from(100i64 * 365 * 366), precision)
}

// ───────────────────────────── budgets ─────────────────────────────

/// An even split of 100% over the twelve months (two decimals, December takes the remainder).
pub fn even_distribution() -> Vec<Decimal> {
    split(Decimal::ONE_HUNDRED, &[Decimal::ONE; 12].map(|_| Decimal::from(100) / Decimal::from(12)), 2)
}

/// The share (0..=1) of an annual budget available through the month containing `on` in the
/// fiscal year [`start`, `end`]: the distribution's percentages (by calendar month) of the months
/// elapsed, or an even share per month without a distribution.
pub fn budget_share(start: NaiveDate, end: NaiveDate, on: NaiveDate, distribution: Option<&[Decimal; 12]>) -> Decimal {
    let mut months = Vec::new();
    let mut m = month_start(start);
    while m <= end && months.len() < 24 {
        months.push(m);
        m = add_months(m, 1);
    }
    if months.is_empty() {
        return Decimal::ONE;
    }
    let elapsed: Vec<&NaiveDate> = months.iter().filter(|m| **m <= on).collect();
    let share = match distribution {
        Some(pcts) => elapsed.iter().map(|m| pcts[m.month0() as usize]).sum::<Decimal>() / Decimal::ONE_HUNDRED,
        None => Decimal::from(elapsed.len() as i64) / Decimal::from(months.len() as i64),
    };
    share.min(Decimal::ONE)
}

// ───────────────────────────── billing periods ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interval {
    Day,
    Week,
    Month,
    Year,
}

impl Interval {
    pub fn parse(s: &str) -> Option<Interval> {
        Some(match s {
            "Day" => Interval::Day,
            "Week" => Interval::Week,
            "Month" => Interval::Month,
            "Year" => Interval::Year,
            _ => return None,
        })
    }

    /// Monthly recurring revenue of `price` billed every `count` intervals (a year is 365 days or 52 weeks).
    pub fn monthly(self, price: Decimal, count: u32) -> Decimal {
        let per_year = Decimal::from(match self {
            Interval::Day => 365,
            Interval::Week => 52,
            Interval::Month => 12,
            Interval::Year => 1,
        });
        price * per_year / Decimal::from(12 * count.max(1) as i64)
    }
}

/// The `k`-th period boundary from `anchor` (negative `k` goes back). Months are added to the
/// anchor itself, never chained, so an anchor on the 31st keeps returning to month ends.
pub fn boundary(anchor: NaiveDate, iv: Interval, count: u32, k: i64) -> NaiveDate {
    let n = count.max(1) as i64;
    match iv {
        Interval::Day => anchor + Duration::days(k * n),
        Interval::Week => anchor + Duration::days(7 * k * n),
        Interval::Month => add_months(anchor, k * n),
        Interval::Year => add_months(anchor, 12 * k * n),
    }
}

/// The boundaries `(b, next)` with `b <= x < next`.
pub fn bracket(anchor: NaiveDate, iv: Interval, count: u32, x: NaiveDate) -> (NaiveDate, NaiveDate) {
    let n = count.max(1) as i64;
    let mut k = match iv {
        Interval::Day => (x - anchor).num_days().div_euclid(n),
        Interval::Week => (x - anchor).num_days().div_euclid(7 * n),
        Interval::Month | Interval::Year => {
            let months = (x.year() as i64 - anchor.year() as i64) * 12 + x.month() as i64 - anchor.month() as i64;
            months.div_euclid(if iv == Interval::Year { 12 * n } else { n })
        }
    };
    while boundary(anchor, iv, count, k) > x {
        k -= 1;
    }
    while boundary(anchor, iv, count, k + 1) <= x {
        k += 1;
    }
    (boundary(anchor, iv, count, k), boundary(anchor, iv, count, k + 1))
}

/// A billing period: `[start, end]` billed, inside the full period `[full_start, full_end]` the
/// anchor defines (they differ for a first period that starts between two boundaries).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Period {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub full_start: NaiveDate,
    pub full_end: NaiveDate,
}

impl Period {
    /// The period that starts at `start` (the billing start or a boundary) and runs to the next boundary.
    pub fn from(anchor: NaiveDate, iv: Interval, count: u32, start: NaiveDate) -> Period {
        let (b, next) = bracket(anchor, iv, count, start);
        let full_end = next - Duration::days(1);
        Period { start, end: full_end, full_start: b, full_end }
    }

    pub fn full_days(&self) -> i64 {
        (self.full_end - self.full_start).num_days() + 1
    }

    /// Days billed from `from` (inclusive) to the period end, the end cut at `stop`.
    pub fn days_from(&self, from: NaiveDate, stop: Option<NaiveDate>) -> i64 {
        let end = stop.map_or(self.end, |s| s.min(self.end));
        ((end - from).num_days() + 1).max(0)
    }
}

// ───────────────────────────── bank statements ─────────────────────────────

/// RFC 4180 records: quoted fields may contain the delimiter, doubled quotes and line breaks.
/// Blank lines are dropped.
pub fn parse_csv(text: &str, delimiter: char) -> Vec<Vec<String>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    let end_row = |row: &mut Vec<String>, field: &mut String, rows: &mut Vec<Vec<String>>| {
        row.push(std::mem::take(field));
        let r = std::mem::take(row);
        if !(r.len() == 1 && r[0].trim().is_empty()) {
            rows.push(r);
        }
    };
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
        } else if c == '"' && field.trim().is_empty() {
            field.clear();
            quoted = true;
        } else if c == delimiter {
            row.push(std::mem::take(&mut field));
        } else if c == '\n' || c == '\r' {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            end_row(&mut row, &mut field, &mut rows);
        } else {
            field.push(c);
        }
    }
    if !field.is_empty() || !row.is_empty() {
        end_row(&mut row, &mut field, &mut rows);
    }
    rows
}

/// The delimiter used by the first line: the most frequent of `,` `;` tab `|` outside quotes.
pub fn detect_delimiter(text: &str) -> char {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or_default();
    let mut quoted = false;
    let mut counts = [0usize; 4];
    let cands = [',', ';', '\t', '|'];
    for c in first.chars() {
        if c == '"' {
            quoted = !quoted;
        } else if !quoted && let Some(i) = cands.iter().position(|x| *x == c) {
            counts[i] += 1;
        }
    }
    let best = (0..4).max_by_key(|i| (counts[*i], 3 - *i)).unwrap_or(0);
    cands[best]
}

/// A statement amount. Currency symbols, spaces and thousands separators are ignored; a leading
/// or trailing minus or parentheses make it negative. `Ok(None)` for a blank cell.
pub fn parse_amount(raw: &str, decimal_comma: bool) -> Result<Option<Decimal>, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let negative = t.starts_with('-') || t.ends_with('-') || (t.starts_with('(') && t.ends_with(')'));
    let (decimal, thousands) = if decimal_comma { (',', '.') } else { ('.', ',') };
    let mut digits = String::new();
    for c in t.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else if c == decimal {
            digits.push('.');
        } else if c == thousands || c.is_whitespace() || c == '\'' || c == '-' || c == '+' || c == '(' || c == ')' || !c.is_ascii() || c.is_ascii_alphabetic() {
            continue;
        } else {
            return Err(format!("'{raw}' is not an amount"));
        }
    }
    if digits.is_empty() || digits.matches('.').count() > 1 {
        return Err(format!("'{raw}' is not an amount"));
    }
    let v: Decimal = digits.parse().map_err(|_| format!("'{raw}' is not an amount"))?;
    Ok(Some(if negative { -v } else { v }))
}

/// A statement date in `format` (chrono syntax), or in a common unambiguous format: ISO,
/// day/month/year with `/`, `.` or `-`, `12 Mar 2026`. Month-first dates need an explicit format.
pub fn parse_statement_date(raw: &str, format: Option<&str>) -> Option<NaiveDate> {
    let t = raw.trim();
    let sane = |d: NaiveDate| (1900..=9999).contains(&d.year());
    if let Some(f) = format.filter(|f| !f.trim().is_empty()) {
        return NaiveDate::parse_from_str(t, f).ok().filter(|d| sane(*d));
    }
    let head = t.split(['T', ' ']).next().unwrap_or(t);
    for f in ["%Y-%m-%d", "%d/%m/%Y", "%d.%m.%Y", "%d-%m-%Y", "%Y/%m/%d"] {
        if let Ok(d) = NaiveDate::parse_from_str(head, f)
            && sane(d)
        {
            return Some(d);
        }
    }
    for f in ["%d %b %Y", "%d-%b-%Y", "%d %B %Y"] {
        if let Ok(d) = NaiveDate::parse_from_str(t, f)
            && sane(d)
        {
            return Some(d);
        }
    }
    None
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Identity of a statement line for duplicate detection. `occurrence` numbers identical lines of
/// one statement (1, 2, ...), so two equal payments on one day both import while importing the
/// same (or an overlapping) statement again finds every line already there.
pub fn statement_hash(bank_account: Uuid, date: NaiveDate, amount: Decimal, description: &str, reference: &str, occurrence: u32) -> String {
    let key = format!("{bank_account}|{date}|{}|{}|{}|{occurrence}", amount.normalize(), squash(description), squash(reference));
    Sha256::digest(key.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// ISO 13616 check: country code, check digits, BBAN, remainder 1 modulo 97.
pub fn iban_valid(iban: &str) -> bool {
    let s: String = iban.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_uppercase();
    let b = s.as_bytes();
    if !(15..=34).contains(&b.len())
        || !b.iter().all(u8::is_ascii_alphanumeric)
        || !b[..2].iter().all(u8::is_ascii_alphabetic)
        || !b[2..4].iter().all(u8::is_ascii_digit)
    {
        return false;
    }
    let mut rem: u32 = 0;
    for c in s[4..].chars().chain(s[..4].chars()) {
        let v = c.to_digit(36).unwrap_or(0);
        rem = if v >= 10 { (rem * 100 + v) % 97 } else { (rem * 10 + v) % 97 };
    }
    rem == 1
}

/// `•••• 1234`: the last four characters of an account number, for lists.
pub fn mask(number: &str) -> String {
    let chars: Vec<char> = number.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if chars.is_empty() {
        return String::new();
    }
    let tail: String = chars[chars.len().saturating_sub(4)..].iter().collect();
    format!("•••• {tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }
    fn n(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn months_clamp_to_month_ends() {
        assert_eq!(add_months(d("2026-01-31"), 1), d("2026-02-28"));
        assert_eq!(add_months(d("2028-01-31"), 1), d("2028-02-29"));
        assert_eq!(add_months(d("2026-01-31"), -2), d("2025-11-30"));
        assert_eq!(add_months(d("2026-12-15"), 1), d("2027-01-15"));
        assert_eq!(month_end(d("2026-12-05")), d("2026-12-31"));
    }

    #[test]
    fn due_date_rules() {
        let inv = d("2026-01-20");
        assert_eq!(due_date("Day(s) after invoice date", inv, 30, 0, 0), d("2026-02-19"));
        assert_eq!(due_date("Day(s) after the end of the invoice month", inv, 10, 0, 0), d("2026-02-10"));
        assert_eq!(due_date("Month(s) after the end of the invoice month", inv, 0, 1, 0), d("2026-02-28"));
        assert_eq!(due_date("Day of the month", inv, 0, 1, 15), d("2026-02-15"));
        assert_eq!(due_date("Day of the month", inv, 0, 0, 15), d("2026-02-15"), "the 15th has passed this month");
        assert_eq!(due_date("Day of the month", inv, 0, 0, 25), d("2026-01-25"));
        assert_eq!(due_date("Day of the month", inv, 0, 1, 31), d("2026-02-28"), "clamped");
    }

    #[test]
    fn splits_absorb_rounding_in_the_last_part() {
        let parts = split(n("100"), &[n("33.333"), n("33.333"), n("33.334")], 2);
        assert_eq!(parts, vec![n("33.33"), n("33.33"), n("33.34")]);
        let parts = split(n("1000.01"), &[n("50"), n("50")], 2);
        assert_eq!(parts, vec![n("500.01"), n("500.00")]);
        assert_eq!(parts.iter().sum::<Decimal>(), n("1000.01"));
        let even = even_distribution();
        assert_eq!(even.iter().sum::<Decimal>(), n("100"));
        assert_eq!((even[0], even[11]), (n("8.33"), n("8.37")));
    }

    #[test]
    fn payments_settle_installments_in_order() {
        assert_eq!(installments_open(&[n("300"), n("300"), n("400")], n("450")), vec![n("0"), n("150"), n("400")]);
        assert_eq!(installments_open(&[n("300"), n("700")], n("1000")), vec![n("0"), n("0")]);
    }

    #[test]
    fn interest_uses_actual_actual_across_leap_years() {
        // 10 days in 2027 (non-leap) + 20 days in 2028 (leap): 1000 × 10% × (10/365 + 20/366).
        let i = interest(n("1000"), n("10"), d("2027-12-21"), d("2028-01-20"), 2);
        assert_eq!(i, n("8.20"), "2.739726 + 5.464481 = 8.204207");
        // A full leap year is exactly one year of interest.
        assert_eq!(interest(n("1000"), n("10"), d("2027-12-31"), d("2028-12-31"), 2), n("100.00"));
        assert_eq!(interest(n("1000"), n("10"), d("2026-12-31"), d("2027-12-31"), 2), n("100.00"));
        assert_eq!(interest(n("1000"), n("10"), d("2026-03-01"), d("2026-03-01"), 2), Decimal::ZERO, "not overdue yet");
        assert_eq!(interest(n("365"), n("100"), d("2026-01-01"), d("2026-01-02"), 6), n("1"), "one day at 100% of 365");
    }

    #[test]
    fn budget_phasing() {
        let (s, e) = (d("2026-04-01"), d("2027-03-31"));
        assert_eq!(budget_share(s, e, d("2026-04-15"), None), n("1") / n("12"));
        assert_eq!(budget_share(s, e, d("2026-06-30"), None), n("0.25"));
        assert_eq!(budget_share(s, e, d("2027-03-31"), None), n("1"));
        let mut pcts = [Decimal::ZERO; 12];
        pcts[3] = n("50"); // April
        pcts[4] = n("10"); // May
        pcts[2] = n("40"); // March
        assert_eq!(budget_share(s, e, d("2026-04-01"), Some(&pcts)), n("0.5"));
        assert_eq!(budget_share(s, e, d("2026-05-31"), Some(&pcts)), n("0.6"));
        assert_eq!(budget_share(s, e, d("2027-02-01"), Some(&pcts)), n("0.6"));
        assert_eq!(budget_share(s, e, d("2027-03-02"), Some(&pcts)), n("1"));
    }

    #[test]
    fn billing_periods_follow_the_anchor() {
        let anchor = d("2026-01-31");
        assert_eq!(bracket(anchor, Interval::Month, 1, d("2026-02-28")), (d("2026-02-28"), d("2026-03-31")));
        assert_eq!(bracket(anchor, Interval::Month, 1, d("2026-02-27")), (d("2026-01-31"), d("2026-02-28")));
        assert_eq!(bracket(anchor, Interval::Month, 1, d("2025-12-31")), (d("2025-12-31"), d("2026-01-31")));
        // A subscription starting on the 10th with periods on the 1st: the first period is a stub.
        let p = Period::from(d("2026-01-01"), Interval::Month, 1, d("2026-01-10"));
        assert_eq!((p.start, p.end, p.full_start, p.full_end), (d("2026-01-10"), d("2026-01-31"), d("2026-01-01"), d("2026-01-31")));
        assert_eq!((p.full_days(), p.days_from(p.start, None)), (31, 22));
        assert_eq!(p.days_from(d("2026-01-20"), Some(d("2026-01-25"))), 6);
        let next = Period::from(d("2026-01-01"), Interval::Month, 1, p.end + Duration::days(1));
        assert_eq!((next.start, next.end), (d("2026-02-01"), d("2026-02-28")));
        let q = Period::from(d("2026-01-01"), Interval::Month, 3, d("2026-02-15"));
        assert_eq!((q.full_start, q.end), (d("2026-01-01"), d("2026-03-31")));
        let w = Period::from(d("2026-01-05"), Interval::Week, 2, d("2026-01-05"));
        assert_eq!(w.end, d("2026-01-18"));
        let y = Period::from(d("2024-02-29"), Interval::Year, 1, d("2025-03-01"));
        assert_eq!((y.full_start, y.end), (d("2025-02-28"), d("2026-02-27")));
        assert_eq!(Interval::Year.monthly(n("1200"), 1), n("100"));
        assert_eq!(Interval::Month.monthly(n("300"), 3), n("100"));
    }

    #[test]
    fn statements_parse() {
        let rows = parse_csv("\u{feff}Date,Details,Amount\r\n2026-01-05,\"Rent, January\",\"-1,200.00\"\n\n05/01/2026,\"Say \"\"hi\"\"\nthere\",50\n", ',');
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1], vec!["2026-01-05", "Rent, January", "-1,200.00"]);
        assert_eq!(rows[2][1], "Say \"hi\"\nthere");
        assert_eq!(detect_delimiter("a;b;\"c;d\",e"), ';');
        assert_eq!(detect_delimiter("a,b,c"), ',');
        assert_eq!(parse_amount("-1,200.00", false), Ok(Some(n("-1200.00"))));
        assert_eq!(parse_amount("(45.10)", false), Ok(Some(n("-45.10"))));
        assert_eq!(parse_amount("1.234,56 €", true), Ok(Some(n("1234.56"))));
        assert_eq!(parse_amount("  ", false), Ok(None));
        assert!(parse_amount("12/3", false).is_err());
        assert_eq!(parse_statement_date("05/01/2026", None), Some(d("2026-01-05")));
        assert_eq!(parse_statement_date("2026-01-05T10:00:00", None), Some(d("2026-01-05")));
        assert_eq!(parse_statement_date("01/05/2026", Some("%m/%d/%Y")), Some(d("2026-01-05")));
        assert_eq!(parse_statement_date("5 Jan 2026", None), Some(d("2026-01-05")));
        assert_eq!(parse_statement_date("yesterday", None), None);
        let acc = Uuid::nil();
        let h = statement_hash(acc, d("2026-01-05"), n("-1200.00"), "Rent,  January", "R1", 1);
        assert_eq!(h, statement_hash(acc, d("2026-01-05"), n("-1200"), "rent, january", "r1", 1), "insensitive to formatting");
        assert_ne!(h, statement_hash(acc, d("2026-01-05"), n("-1200"), "rent, january", "r1", 2));
    }

    #[test]
    fn iban_and_masking() {
        assert!(iban_valid("GB82 WEST 1234 5698 7654 32"));
        assert!(iban_valid("DE89370400440532013000"));
        assert!(!iban_valid("GB82 WEST 1234 5698 7654 33"));
        assert!(!iban_valid("1234"));
        assert_eq!(mask("GB82 WEST 1234 5698 7654 32"), "•••• 5432");
        assert_eq!(mask(""), "");
    }
}
