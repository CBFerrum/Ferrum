//! Helpers document templates print with (docs/builder/decisions.md BD-21): numbers, amounts and dates as the
//! document's language writes them, an amount in words in that language and currency, barcodes, fixed-width and
//! delimited fields, and escaping for CSV and XML. Every language rule here is data in one table or one small function
//! behind [`speaker`]: another language is a row or a function more, never a change to a caller.

use minijinja::value::{Kwargs, Value as JValue};
use minijinja::{Error as JError, ErrorKind, State};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::Value;

// ── how a language writes numbers and dates ──

/// How one language (or language and region) writes numbers and dates.
#[derive(Debug, Clone, Copy)]
pub struct Locale {
    pub tag: &'static str,
    pub group: &'static str,
    pub decimal: &'static str,
    /// Groups of two above the first three (1,23,45,678).
    pub lakh: bool,
    /// Arabic-Indic digits (٠١٢…).
    pub arabic_digits: bool,
    /// An amount's symbol or code after the number.
    pub symbol_after: bool,
    /// `strftime` patterns; `%B` is the month's name in this language.
    pub short: &'static str,
    pub long: &'static str,
    pub months: Option<[&'static str; 12]>,
}

const EN_MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const HI_MONTHS: [&str; 12] = ["जनवरी", "फ़रवरी", "मार्च", "अप्रैल", "मई", "जून", "जुलाई", "अगस्त", "सितंबर", "अक्तूबर", "नवंबर", "दिसंबर"];
const AR_MONTHS: [&str; 12] = ["يناير", "فبراير", "مارس", "أبريل", "مايو", "يونيو", "يوليو", "أغسطس", "سبتمبر", "أكتوبر", "نوفمبر", "ديسمبر"];

const fn loc(tag: &'static str, group: &'static str, decimal: &'static str, symbol_after: bool, short: &'static str) -> Locale {
    Locale { tag, group, decimal, lakh: false, arabic_digits: false, symbol_after, short, long: short, months: None }
}

/// The languages whose number and date forms are known; any other is written as English is. The first row is the
/// fallback.
pub const LOCALES: &[Locale] = &[
    Locale { long: "%B %-d, %Y", months: Some(EN_MONTHS), ..loc("en", ",", ".", false, "%m/%d/%Y") },
    Locale { long: "%-d %B %Y", months: Some(EN_MONTHS), ..loc("en-GB", ",", ".", false, "%d/%m/%Y") },
    Locale { lakh: true, long: "%-d %B %Y", months: Some(EN_MONTHS), ..loc("en-IN", ",", ".", false, "%d/%m/%Y") },
    Locale { lakh: true, long: "%-d %B %Y", months: Some(HI_MONTHS), ..loc("hi", ",", ".", false, "%d/%m/%Y") },
    Locale { arabic_digits: true, long: "%-d %B %Y", months: Some(AR_MONTHS), ..loc("ar", "٬", "٫", true, "%d/%m/%Y") },
    Locale { long: "%-d %B %Y", months: Some(AR_MONTHS), ..loc("ar-AE", ",", ".", true, "%d/%m/%Y") },
    loc("de", ".", ",", true, "%d.%m.%Y"),
    loc("fr", "\u{202f}", ",", true, "%d/%m/%Y"),
    loc("es", ".", ",", true, "%d/%m/%Y"),
    loc("it", ".", ",", true, "%d/%m/%Y"),
    loc("pt", ".", ",", true, "%d/%m/%Y"),
    loc("nl", ".", ",", false, "%d-%m-%Y"),
    loc("ru", "\u{a0}", ",", true, "%d.%m.%Y"),
    loc("sv", "\u{a0}", ",", true, "%Y-%m-%d"),
    loc("ja", ",", ".", false, "%Y/%m/%d"),
    loc("zh", ",", ".", false, "%Y/%m/%d"),
];

/// The row for `tag` (`en-IN`), else for its language (`en`), else English.
pub fn locale(tag: &str) -> &'static Locale {
    let t = tag.trim().to_ascii_lowercase();
    let base = t.split(['-', '_']).next().unwrap_or_default();
    LOCALES
        .iter()
        .find(|l| l.tag.eq_ignore_ascii_case(&t.replace('_', "-")))
        .or_else(|| LOCALES.iter().find(|l| l.tag.eq_ignore_ascii_case(base)))
        .unwrap_or(&LOCALES[0])
}

fn digits(s: String, l: &Locale) -> String {
    if !l.arabic_digits {
        return s;
    }
    s.chars().map(|c| c.to_digit(10).and_then(|d| char::from_u32(0x0660 + d)).unwrap_or(c)).collect()
}

/// `d` rounded half away from zero to `decimals`, grouped and marked as `l` writes it.
pub fn number(d: Decimal, decimals: u32, l: &Locale) -> String {
    let mut d = d.round_dp_with_strategy(decimals, RoundingStrategy::MidpointAwayFromZero);
    d.rescale(decimals);
    let s = d.abs().to_string();
    let (int, frac) = s.split_once('.').unwrap_or((&s, ""));
    let n = int.len();
    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        let left = n - i;
        if i > 0 && (left == 3 || (left > 3 && if l.lakh { (left - 3) % 2 == 0 } else { left % 3 == 0 })) {
            out.push_str(l.group);
        }
        out.push(c);
    }
    if !frac.is_empty() {
        out.push_str(l.decimal);
        out.push_str(frac);
    }
    let sign = if d.is_sign_negative() && !d.is_zero() { "-" } else { "" };
    digits(format!("{sign}{out}"), l)
}

/// A currency as a document prints it: its code, symbol and number of decimals.
#[derive(Debug, Clone, PartialEq)]
pub struct Currency {
    pub code: String,
    pub symbol: Option<String>,
    pub precision: u32,
}

impl Currency {
    /// `code` with ISO 4217's number of decimals, when no currency record says otherwise.
    pub fn iso(code: &str) -> Currency {
        let code = code.trim().to_ascii_uppercase();
        let precision = match code.as_str() {
            "JPY" | "KRW" | "VND" | "CLP" | "ISK" | "UGX" | "XAF" | "XOF" | "PYG" | "RWF" | "KMF" | "GNF" | "DJF" | "VUV" | "XPF" => 0,
            "BHD" | "KWD" | "OMR" | "JOD" | "TND" | "LYD" | "IQD" => 3,
            _ => 2,
        };
        Currency { code, symbol: None, precision }
    }

    /// From a currency record's `{code, symbol, precision}` (what the `Currency` master holds).
    pub fn from_record(v: &Value) -> Option<Currency> {
        let code = v.get("code")?.as_str()?.to_string();
        let iso = Currency::iso(&code);
        let precision = v.get("precision").and_then(|p| p.as_u64().or_else(|| p.as_str()?.parse().ok())).map_or(iso.precision, |p| p.min(6) as u32);
        let symbol = v.get("symbol").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(str::to_string);
        Some(Currency { code: iso.code, symbol, precision })
    }
}

/// How an amount shows its currency.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Show {
    Symbol,
    Code,
    Nothing,
}

/// `d` with the currency's decimals, as `l` writes it, with its symbol (else its code), its code, or nothing.
pub fn amount(d: Decimal, c: &Currency, l: &Locale, show: Show) -> String {
    let n = number(d, c.precision, l);
    let mark = match (show, &c.symbol) {
        (Show::Nothing, _) => return n,
        (Show::Symbol, Some(s)) => s.clone(),
        _ => c.code.clone(),
    };
    let tight = show == Show::Symbol && c.symbol.is_some() && !l.symbol_after;
    match (l.symbol_after, tight) {
        (true, _) => format!("{n}\u{a0}{mark}"),
        (false, true) => format!("{mark}{n}"),
        (false, false) => format!("{mark}\u{a0}{n}"),
    }
}

/// A date (`2026-11-12`, or a moment's date part) in `style` `short` or `long`, or a `strftime` pattern of its own,
/// `%B` the month's name in the language.
pub fn date(v: &str, style: &str, l: &Locale) -> Result<String, String> {
    let day = chrono::NaiveDate::parse_from_str(v.get(..10).unwrap_or(v), "%Y-%m-%d").map_err(|_| format!("'{v}' is not a date"))?;
    let pattern = match style {
        "short" => l.short,
        "long" => l.long,
        own => own,
    };
    let month = |i: u32| l.months.or_else(|| locale("en").months).map(|m| m[i as usize - 1]).unwrap_or_default();
    let pattern = pattern.replace("%B", &month(chrono::Datelike::month(&day)).replace('%', "%%"));
    let mut out = String::new();
    std::fmt::Write::write_fmt(&mut out, format_args!("{}", day.format(&pattern))).map_err(|_| format!("'{style}' is not a date pattern"))?;
    Ok(digits(out, l))
}

// ── amounts in words ──

/// How one language says a number and counts a unit: `words(n)`, `count(n, forms)` (the number with the unit's form it
/// takes), what joins the main unit to the fraction, and what says "minus".
pub struct Speaker {
    pub words: fn(u64) -> String,
    pub count: fn(u64, fn(u64) -> String, &[&str]) -> String,
    pub and: &'static str,
    pub minus: &'static str,
}

/// The languages an amount can be said in: those the interface ships (English, Hindi, Arabic). `en-IN` counts in
/// lakhs and crores as English is written there.
pub fn speaker(tag: &str) -> Option<Speaker> {
    let t = tag.to_ascii_lowercase().replace('_', "-");
    Some(match t.split('-').next().unwrap_or_default() {
        "en" if t == "en-in" => Speaker { words: en_indian, count: plain_count, and: " and ", minus: "minus " },
        "en" => Speaker { words: en_short, count: plain_count, and: " and ", minus: "minus " },
        "hi" => Speaker { words: hi, count: plain_count, and: " और ", minus: "ऋण " },
        "ar" => Speaker { words: ar, count: ar_count, and: " و", minus: "سالب " },
        _ => return None,
    })
}

/// `n` said, and the unit's singular (`forms[0]`) for one, its plural (`forms[1]`) otherwise.
fn plain_count(n: u64, words: fn(u64) -> String, forms: &[&str]) -> String {
    let f = if n == 1 { forms[0] } else { forms.get(1).copied().unwrap_or(forms[0]) };
    format!("{} {f}", words(n))
}

const EN_SMALL: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const EN_TENS: [&str; 10] = ["", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];

fn en_99(n: u64) -> String {
    match n {
        0..20 => EN_SMALL[n as usize].into(),
        _ if n.is_multiple_of(10) => EN_TENS[n as usize / 10].into(),
        _ => format!("{}-{}", EN_TENS[n as usize / 10], EN_SMALL[n as usize % 10]),
    }
}

fn en_999(n: u64) -> String {
    match (n / 100, n % 100) {
        (0, r) => en_99(r),
        (h, 0) => format!("{} hundred", EN_SMALL[h as usize]),
        (h, r) => format!("{} hundred {}", EN_SMALL[h as usize], en_99(r)),
    }
}

/// Thousands, millions, billions… (the short scale).
fn en_short(n: u64) -> String {
    const SCALES: [(u64, &str); 6] = [
        (1_000_000_000_000_000_000, "quintillion"),
        (1_000_000_000_000_000, "quadrillion"),
        (1_000_000_000_000, "trillion"),
        (1_000_000_000, "billion"),
        (1_000_000, "million"),
        (1_000, "thousand"),
    ];
    if n == 0 {
        return EN_SMALL[0].into();
    }
    let (mut n, mut parts) = (n, Vec::new());
    for (size, name) in SCALES {
        if n >= size {
            parts.push(format!("{} {name}", en_999(n / size)));
            n %= size;
        }
    }
    if n > 0 {
        parts.push(en_999(n));
    }
    parts.join(" ")
}

/// Hundreds, thousands, lakhs and crores, crores counted the same way again above a hundred: what English and Hindi
/// say in India. `small` says 1 to 99.
fn indian(n: u64, small: fn(u64) -> String, [hundred, thousand, lakh, crore]: [&str; 4]) -> String {
    let mut parts = Vec::new();
    let mut n = n;
    if n >= 10_000_000 {
        parts.push(format!("{} {crore}", indian(n / 10_000_000, small, [hundred, thousand, lakh, crore])));
        n %= 10_000_000;
    }
    for (size, name) in [(100_000, lakh), (1_000, thousand), (100, hundred)] {
        if n >= size {
            parts.push(format!("{} {name}", small(n / size)));
            n %= size;
        }
    }
    if n > 0 {
        parts.push(small(n));
    }
    parts.join(" ")
}

fn en_indian(n: u64) -> String {
    if n == 0 { EN_SMALL[0].into() } else { indian(n, en_99, ["hundred", "thousand", "lakh", "crore"]) }
}

/// Hindi's own word for each number to ninety-nine.
const HI_99: [&str; 100] = [
    "शून्य",
    "एक",
    "दो",
    "तीन",
    "चार",
    "पाँच",
    "छह",
    "सात",
    "आठ",
    "नौ",
    "दस",
    "ग्यारह",
    "बारह",
    "तेरह",
    "चौदह",
    "पंद्रह",
    "सोलह",
    "सत्रह",
    "अठारह",
    "उन्नीस",
    "बीस",
    "इक्कीस",
    "बाईस",
    "तेईस",
    "चौबीस",
    "पच्चीस",
    "छब्बीस",
    "सत्ताईस",
    "अट्ठाईस",
    "उनतीस",
    "तीस",
    "इकतीस",
    "बत्तीस",
    "तैंतीस",
    "चौंतीस",
    "पैंतीस",
    "छत्तीस",
    "सैंतीस",
    "अड़तीस",
    "उनतालीस",
    "चालीस",
    "इकतालीस",
    "बयालीस",
    "तैंतालीस",
    "चवालीस",
    "पैंतालीस",
    "छियालीस",
    "सैंतालीस",
    "अड़तालीस",
    "उनचास",
    "पचास",
    "इक्यावन",
    "बावन",
    "तिरपन",
    "चौवन",
    "पचपन",
    "छप्पन",
    "सत्तावन",
    "अट्ठावन",
    "उनसठ",
    "साठ",
    "इकसठ",
    "बासठ",
    "तिरसठ",
    "चौंसठ",
    "पैंसठ",
    "छियासठ",
    "सड़सठ",
    "अड़सठ",
    "उनहत्तर",
    "सत्तर",
    "इकहत्तर",
    "बहत्तर",
    "तिहत्तर",
    "चौहत्तर",
    "पचहत्तर",
    "छिहत्तर",
    "सतहत्तर",
    "अठहत्तर",
    "उन्यासी",
    "अस्सी",
    "इक्यासी",
    "बयासी",
    "तिरासी",
    "चौरासी",
    "पचासी",
    "छियासी",
    "सत्तासी",
    "अट्ठासी",
    "नवासी",
    "नब्बे",
    "इक्यानवे",
    "बानवे",
    "तिरानवे",
    "चौरानवे",
    "पचानवे",
    "छियानवे",
    "सत्तानवे",
    "अट्ठानवे",
    "निन्यानवे",
];

fn hi(n: u64) -> String {
    if n == 0 { HI_99[0].into() } else { indian(n, |n| HI_99[n as usize].into(), ["सौ", "हज़ार", "लाख", "करोड़"]) }
}

const AR_ONES: [&str; 10] = ["", "واحد", "اثنان", "ثلاثة", "أربعة", "خمسة", "ستة", "سبعة", "ثمانية", "تسعة"];
const AR_TENS: [&str; 10] = ["", "عشرة", "عشرون", "ثلاثون", "أربعون", "خمسون", "ستون", "سبعون", "ثمانون", "تسعون"];
const AR_HUNDREDS: [&str; 10] = ["", "مائة", "مائتان", "ثلاثمائة", "أربعمائة", "خمسمائة", "ستمائة", "سبعمائة", "ثمانمائة", "تسعمائة"];

fn ar_999(n: u64) -> String {
    let (h, r) = ((n / 100) as usize, n % 100);
    let below = match r {
        0 => String::new(),
        11 => "أحد عشر".into(),
        12 => "اثنا عشر".into(),
        13..20 => format!("{} عشر", AR_ONES[r as usize - 10]),
        _ if r.is_multiple_of(10) => AR_TENS[r as usize / 10].into(),
        1..=9 => AR_ONES[r as usize].into(),
        _ => format!("{} و{}", AR_ONES[r as usize % 10], AR_TENS[r as usize / 10]),
    };
    [AR_HUNDREDS[h].to_string(), below].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" و")
}

/// Arabic with its counted forms: one, two (the dual), three to ten (the plural), more (the singular). The counted
/// noun's gender is not followed (the numbers are said as for a masculine one, as the units of money are).
fn ar(n: u64) -> String {
    const SCALES: [(u64, [&str; 3]); 3] =
        [(1_000_000_000, ["مليار", "ملياران", "مليارات"]), (1_000_000, ["مليون", "مليونان", "ملايين"]), (1_000, ["ألف", "ألفان", "آلاف"])];
    if n == 0 {
        return "صفر".into();
    }
    let (mut n, mut parts) = (n, Vec::new());
    // Only the milliards can count past 999: they are said as a number of their own.
    let say = |c: u64| if c < 1000 { ar_999(c) } else { ar(c) };
    for (size, [one, two, many]) in SCALES {
        let c = n / size;
        n %= size;
        parts.push(match c {
            0 => continue,
            1 => one.into(),
            2 => two.into(),
            3..=10 => format!("{} {many}", say(c)),
            _ => format!("{} {one}", say(c)),
        });
    }
    if n > 0 {
        parts.push(ar_999(n));
    }
    parts.join(" و")
}

/// `forms`: singular, dual, plural (three to ten), and the singular said after eleven to ninety-nine.
fn ar_count(n: u64, words: fn(u64) -> String, forms: &[&str]) -> String {
    let f = |i: usize| forms.get(i).copied().unwrap_or(forms[0]);
    match (n, n % 100) {
        (1, _) => format!("{} {}", f(0), AR_ONES[1]),
        (2, _) => f(1).to_string(),
        (_, 3..=10) => format!("{} {}", words(n), f(2)),
        (_, 11..=99) => format!("{} {}", words(n), f(3)),
        _ => format!("{} {}", words(n), f(0)),
    }
}

/// The names of a currency's unit and of its fraction in a language, in the forms its [`Speaker`] counts with.
/// A currency or language not here is said with its code, and its fraction as a number over its whole.
const UNITS: &[(&str, &str, &[&str], &[&str])] = &[
    ("en", "INR", &["rupee", "rupees"], &["paisa", "paise"]),
    ("en", "USD", &["dollar", "dollars"], &["cent", "cents"]),
    ("en", "EUR", &["euro", "euros"], &["cent", "cents"]),
    ("en", "GBP", &["pound", "pounds"], &["penny", "pence"]),
    ("en", "AED", &["dirham", "dirhams"], &["fils", "fils"]),
    ("en", "SAR", &["riyal", "riyals"], &["halala", "halalas"]),
    ("en", "QAR", &["riyal", "riyals"], &["dirham", "dirhams"]),
    ("en", "OMR", &["rial", "rials"], &["baisa", "baisa"]),
    ("en", "KWD", &["dinar", "dinars"], &["fils", "fils"]),
    ("en", "BHD", &["dinar", "dinars"], &["fils", "fils"]),
    ("en", "JOD", &["dinar", "dinars"], &["fils", "fils"]),
    ("en", "EGP", &["pound", "pounds"], &["piastre", "piastres"]),
    ("en", "AUD", &["dollar", "dollars"], &["cent", "cents"]),
    ("en", "CAD", &["dollar", "dollars"], &["cent", "cents"]),
    ("en", "SGD", &["dollar", "dollars"], &["cent", "cents"]),
    ("en", "NZD", &["dollar", "dollars"], &["cent", "cents"]),
    ("en", "ZAR", &["rand", "rand"], &["cent", "cents"]),
    ("en", "CHF", &["franc", "francs"], &["centime", "centimes"]),
    ("en", "JPY", &["yen", "yen"], &[]),
    ("en", "CNY", &["yuan", "yuan"], &["fen", "fen"]),
    ("en", "NPR", &["rupee", "rupees"], &["paisa", "paise"]),
    ("en", "PKR", &["rupee", "rupees"], &["paisa", "paise"]),
    ("en", "LKR", &["rupee", "rupees"], &["cent", "cents"]),
    ("en", "BDT", &["taka", "taka"], &["poisha", "poisha"]),
    ("en", "MYR", &["ringgit", "ringgit"], &["sen", "sen"]),
    ("hi", "INR", &["रुपया", "रुपये"], &["पैसा", "पैसे"]),
    ("hi", "NPR", &["रुपया", "रुपये"], &["पैसा", "पैसे"]),
    ("hi", "USD", &["डॉलर", "डॉलर"], &["सेंट", "सेंट"]),
    ("hi", "EUR", &["यूरो", "यूरो"], &["सेंट", "सेंट"]),
    ("hi", "GBP", &["पाउंड", "पाउंड"], &["पेंस", "पेंस"]),
    ("hi", "AED", &["दिरहम", "दिरहम"], &["फ़िल्स", "फ़िल्स"]),
    ("ar", "SAR", &["ريال", "ريالان", "ريالات", "ريالاً"], &["هللة", "هللتان", "هللات", "هللة"]),
    ("ar", "QAR", &["ريال", "ريالان", "ريالات", "ريالاً"], &["درهم", "درهمان", "دراهم", "درهماً"]),
    ("ar", "OMR", &["ريال", "ريالان", "ريالات", "ريالاً"], &["بيسة", "بيستان", "بيسات", "بيسة"]),
    ("ar", "AED", &["درهم", "درهمان", "دراهم", "درهماً"], &["فلس", "فلسان", "فلوس", "فلساً"]),
    ("ar", "KWD", &["دينار", "ديناران", "دنانير", "ديناراً"], &["فلس", "فلسان", "فلوس", "فلساً"]),
    ("ar", "BHD", &["دينار", "ديناران", "دنانير", "ديناراً"], &["فلس", "فلسان", "فلوس", "فلساً"]),
    ("ar", "JOD", &["دينار", "ديناران", "دنانير", "ديناراً"], &["فلس", "فلسان", "فلوس", "فلساً"]),
    ("ar", "EGP", &["جنيه", "جنيهان", "جنيهات", "جنيهاً"], &["قرش", "قرشان", "قروش", "قرشاً"]),
    ("ar", "USD", &["دولار", "دولاران", "دولارات", "دولاراً"], &["سنت", "سنتان", "سنتات", "سنتاً"]),
    ("ar", "EUR", &["يورو", "يوروان", "يوروات", "يورو"], &["سنت", "سنتان", "سنتات", "سنتاً"]),
    ("ar", "INR", &["روبية", "روبيتان", "روبيات", "روبية"], &["بيسة", "بيستان", "بيسات", "بيسة"]),
];

/// The unit names of `code` in `tag`'s language, from [`UNITS`].
pub fn units(tag: &str, code: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
    let base = tag.to_ascii_lowercase();
    let base = base.split(['-', '_']).next().unwrap_or_default();
    UNITS.iter().find(|u| u.0 == base && u.1.eq_ignore_ascii_case(code)).map(|u| (u.2, u.3))
}

/// `d` in words in `tag`'s language: its whole units and its fraction (to the currency's decimals) each counted with
/// the unit's name (`given`, else [`units`], else the currency's code with the fraction as a number over its whole).
/// Refused for a language no [`speaker`] says.
pub fn words(d: Decimal, c: &Currency, tag: &str, given: Option<(&[&str], &[&str])>) -> Result<String, String> {
    let s = speaker(tag).ok_or_else(|| format!("amounts are not said in words in '{tag}' yet"))?;
    let d = d.round_dp_with_strategy(c.precision, RoundingStrategy::MidpointAwayFromZero);
    let whole = d.abs().trunc().to_u64().ok_or("too large to say in words")?;
    let scale = 10u64.pow(c.precision);
    let frac = (d.abs().fract() * Decimal::from(scale)).round().to_u64().unwrap_or(0);
    let (major, minor) = given.or_else(|| units(tag, &c.code)).unwrap_or((&[], &[]));
    let mut out = if major.is_empty() { format!("{} {}", (s.words)(whole), c.code) } else { (s.count)(whole, s.words, major) };
    if frac > 0 {
        out.push_str(s.and);
        if minor.is_empty() {
            out.push_str(&format!("{frac}/{scale}"));
        } else {
            out.push_str(&(s.count)(frac, s.words, minor));
        }
    }
    Ok(if d.is_sign_negative() && !d.is_zero() { format!("{}{out}", s.minus) } else { out })
}

// ── barcodes ──

/// Code 128's symbols as bar and space widths, values 0 to 105, then stop (106).
const CODE128: [&str; 107] = [
    "212222", "222122", "222221", "121223", "121322", "131222", "122213", "122312", "132212", "221213", "221312", "231212", "112232", "122132", "122231",
    "113222", "123122", "123221", "223211", "221132", "221231", "213212", "223112", "312131", "311222", "321122", "321221", "312212", "322112", "322211",
    "212123", "212321", "232121", "111323", "131123", "131321", "112313", "132113", "132311", "211313", "231113", "231311", "112133", "112331", "132131",
    "113123", "113321", "133121", "313121", "211331", "231131", "213113", "213311", "213131", "311123", "311321", "331121", "312113", "312311", "332111",
    "314111", "221411", "431111", "111224", "111422", "121124", "121421", "141122", "141221", "112214", "112412", "122114", "122411", "142112", "142211",
    "241211", "221114", "413111", "241112", "134111", "111242", "121142", "121241", "114212", "124112", "124211", "411212", "421112", "421211", "212141",
    "214121", "412121", "111143", "111341", "131141", "114113", "114311", "411113", "411311", "113141", "114131", "311141", "411131", "211412", "211214",
    "211232", "2331112",
];

/// The symbol values of `text` in Code 128 (set B, printable ASCII; set C for an even run of digits), its start, its
/// check symbol and its stop.
pub fn code128_values(text: &str) -> Result<Vec<usize>, String> {
    if text.is_empty() || text.len() > 80 || !text.bytes().all(|b| (32..127).contains(&b)) {
        return Err(format!("'{text}' is not 1 to 80 printable ASCII characters, what Code 128 encodes here"));
    }
    let numeric = text.len() >= 4 && text.len().is_multiple_of(2) && text.bytes().all(|b| b.is_ascii_digit());
    let mut v: Vec<usize> = if numeric {
        std::iter::once(105).chain(text.as_bytes().chunks(2).map(|p| ((p[0] - b'0') * 10 + p[1] - b'0') as usize)).collect()
    } else {
        std::iter::once(104).chain(text.bytes().map(|b| (b - 32) as usize)).collect()
    };
    let check = v.iter().enumerate().map(|(i, s)| s * i.max(1)).sum::<usize>() % 103;
    v.push(check);
    v.push(106);
    Ok(v)
}

/// The check digit of an EAN-13's first twelve digits.
pub fn ean13_check(first12: &[u8]) -> u8 {
    let sum: u32 = first12.iter().enumerate().map(|(i, d)| u32::from(*d) * if i % 2 == 0 { 1 } else { 3 }).sum();
    ((10 - sum % 10) % 10) as u8
}

const EAN_L: [&str; 10] = ["0001101", "0011001", "0010011", "0111101", "0100011", "0110001", "0101111", "0111011", "0110111", "0001011"];
const EAN_PARITY: [&str; 10] = ["LLLLLL", "LLGLGG", "LLGGLG", "LLGGGL", "LGLLGG", "LGGLLG", "LGGGLL", "LGLGLG", "LGLGGL", "LGGLGL"];

/// An EAN-13's modules (`1` a bar), from twelve digits (its check digit added) or thirteen (its check digit checked).
pub fn ean13_modules(code: &str) -> Result<(String, String), String> {
    let d: Vec<u8> = code.bytes().map(|b| b.wrapping_sub(b'0')).collect();
    if !(12..=13).contains(&d.len()) || d.iter().any(|x| *x > 9) {
        return Err(format!("'{code}' is not the 12 or 13 digits of an EAN-13"));
    }
    let check = ean13_check(&d[..12]);
    if d.len() == 13 && d[12] != check {
        return Err(format!("{code}'s check digit is {}, not {check}", d[12]));
    }
    let r = |x: u8| EAN_L[x as usize].chars().map(|c| if c == '0' { '1' } else { '0' }).collect::<String>();
    let mut m = String::from("101");
    for (i, p) in EAN_PARITY[d[0] as usize].chars().enumerate() {
        let x = d[i + 1];
        m.push_str(&if p == 'L' { EAN_L[x as usize].to_string() } else { r(x).chars().rev().collect() });
    }
    m.push_str("01010");
    for x in d[7..12].iter().copied().chain([check]) {
        m.push_str(&r(x));
    }
    m.push_str("101");
    let digits: String = d[..12].iter().chain([&check]).map(|x| char::from(b'0' + x)).collect();
    Ok((m, digits))
}

/// Bars (`1`) as an inline SVG, `module` pixels a module, `height` tall, the text written under them when given.
fn bars_svg(modules: &str, module: u32, height: u32, text: Option<&str>) -> String {
    let quiet = 10 * module;
    let width = modules.len() as u32 * module + 2 * quiet;
    let full = height + if text.is_some() { 14 } else { 0 };
    let mut path = String::new();
    let mut run = 0;
    for (i, c) in modules.chars().chain(['0']).enumerate() {
        if c == '1' {
            run += 1;
        } else if run > 0 {
            let x = quiet + (i as u32 - run) * module;
            path.push_str(&format!("M{x} 0h{}v{height}h-{}z", run * module, run * module));
            run = 0;
        }
    }
    let label = text
        .map(|t| format!(r#"<text x="{}" y="{}" font-family="monospace" font-size="12" text-anchor="middle">{}</text>"#, width / 2, full - 2, xml(t)))
        .unwrap_or_default();
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{full}" viewBox="0 0 {width} {full}"><rect width="100%" height="100%" fill="#fff"/><path d="{path}" fill="#000"/>{label}</svg>"##
    )
}

/// `value` as a barcode of `kind` (`code128`, `ean13`), an inline SVG.
pub fn barcode(value: &str, kind: &str, module: u32, height: u32, show_text: bool) -> Result<String, String> {
    let module = module.clamp(1, 10);
    let height = height.clamp(10, 400);
    match kind {
        "code128" => {
            let modules: String = code128_values(value)?
                .into_iter()
                .flat_map(|s| CODE128[s].chars().enumerate().map(|(i, w)| (if i % 2 == 0 { '1' } else { '0' }, w.to_digit(10).unwrap_or(1) as usize)))
                .flat_map(|(c, n)| std::iter::repeat_n(c, n))
                .collect();
            Ok(bars_svg(&modules, module, height, show_text.then_some(value)))
        }
        "ean13" => {
            let (modules, digits) = ean13_modules(value)?;
            Ok(bars_svg(&modules, module, height, show_text.then_some(digits.as_str())))
        }
        other => Err(format!("'{other}' is not a barcode here (code128, ean13)")),
    }
}

// ── fields and escaping ──

/// `s` in exactly `width` characters: cut when longer, filled with `fill` on the right (`align` `left`) or the left
/// (`right`) when shorter. A line break in it becomes a space: a field never breaks a line.
pub fn pad(s: &str, width: usize, align: &str, fill: char) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).take(width).collect();
    let more = width - flat.chars().count();
    let fill: String = std::iter::repeat_n(fill, more).collect();
    if align == "right" { format!("{fill}{flat}") } else { format!("{flat}{fill}") }
}

/// One CSV field: quoted when it holds a separator (`,`, `;`, a tab), a quote or a line break, its quotes doubled; a
/// value a spreadsheet would run as a formula is kept as text (the platform's one guard, `published::csv_cell`).
pub fn csv(v: &Value) -> String {
    let cell = crate::published::csv_cell(v);
    if !cell.starts_with('"') && cell.contains([';', '\t']) { format!("\"{}\"", cell.replace('"', "\"\"")) } else { cell }
}

/// `s` as XML text or an attribute's value: markup characters as entities, and the characters XML 1.0 cannot hold at
/// all (control characters other than tab and line breaks) left out.
pub fn xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{fffe}' || c == '\u{ffff}' => {}
            c => out.push(c),
        }
    }
    out
}

/// A text file's bytes: its lines ended as stated (CR LF, else LF), in `encoding`. A character the encoding has no
/// place for is refused, with where it is.
pub fn encode(text: &str, encoding: Option<&str>, crlf: bool) -> Result<Vec<u8>, String> {
    let lf = text.replace("\r\n", "\n");
    let text = if crlf { lf.replace('\n', "\r\n") } else { lf };
    let limit = match encoding.unwrap_or("utf-8") {
        "utf-8" => return Ok(text.into_bytes()),
        "iso-8859-1" => 0xff,
        "ascii" => 0x7f,
        other => return Err(format!("'{other}' is not an encoding here")),
    };
    text.chars()
        .enumerate()
        .map(|(i, c)| {
            u8::try_from(c as u32)
                .ok()
                .filter(|b| u32::from(*b) <= limit)
                .ok_or_else(|| format!("'{c}' (character {}) has no place in {}", i + 1, encoding.unwrap_or_default()))
        })
        .collect()
}

// ── in the template language ──

fn bad(msg: impl Into<String>) -> JError {
    JError::new(ErrorKind::InvalidOperation, msg.into())
}

fn json(v: &JValue) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn decimal(v: &JValue) -> Result<Option<Decimal>, JError> {
    match json(v) {
        Value::Null => Ok(None),
        Value::String(s) if s.trim().is_empty() => Ok(None),
        j => crate::value::parse_decimal(&j).map(Some).map_err(|e| bad(format!("{j} is not a number: {e}"))),
    }
}

/// The language a helper writes in: the one it is given, else the document's (`language`), else English.
fn lang(state: &State, given: Option<String>) -> String {
    given.or_else(|| state.lookup("language").and_then(|v| v.as_str().map(str::to_string))).unwrap_or_else(|| "en".into())
}

/// The currency a helper counts in: the code it is given, else the document's (`currency`), with what the workspace's
/// currency records say of it (`currencies`), else ISO's decimals and no symbol.
fn currency(state: &State, given: Option<String>) -> Result<Currency, JError> {
    let code = given
        .or_else(|| state.lookup("currency").and_then(|v| v.as_str().map(str::to_string)))
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| bad("which currency: the document has none, so name it (amount(currency=\"EUR\"))"))?;
    let known = state.lookup("currencies").map(|c| json(&c)).and_then(|c| c.get(code.trim().to_ascii_uppercase()).and_then(Currency::from_record));
    Ok(known.unwrap_or_else(|| Currency::iso(&code)))
}

/// Adds the helpers to a template environment. Each reads the document's `language` and `currency` unless told.
pub(crate) fn add_helpers(env: &mut minijinja::Environment<'static>) {
    env.add_filter("number", |state: &State, v: JValue, decimals: Option<u32>, language: Option<String>| -> Result<String, JError> {
        let Some(d) = decimal(&v)? else { return Ok(String::new()) };
        let places = decimals.unwrap_or_else(|| d.normalize().scale());
        Ok(number(d, places.min(12), locale(&lang(state, language))))
    });
    // A number as a data file writes it: `places` decimals after a point, no grouping, whatever the language.
    env.add_filter("fixed", |v: JValue, places: Option<u32>| -> Result<String, JError> {
        let Some(d) = decimal(&v)? else { return Ok(String::new()) };
        let places = places.unwrap_or(2).min(12);
        let mut d = d.round_dp_with_strategy(places, RoundingStrategy::MidpointAwayFromZero);
        d.rescale(places);
        Ok(d.to_string())
    });
    env.add_filter(
        "amount",
        |state: &State, v: JValue, currency_code: Option<String>, show: Option<String>, language: Option<String>| -> Result<String, JError> {
            let Some(d) = decimal(&v)? else { return Ok(String::new()) };
            let show = match show.as_deref().unwrap_or("symbol") {
                "symbol" => Show::Symbol,
                "code" => Show::Code,
                "none" => Show::Nothing,
                other => return Err(bad(format!("amount shows its symbol, code or none, not {other}"))),
            };
            Ok(amount(d, &currency(state, currency_code)?, locale(&lang(state, language)), show))
        },
    );
    env.add_filter(
        "words",
        |state: &State, v: JValue, currency_code: Option<String>, language: Option<String>, names: Option<JValue>| -> Result<String, JError> {
            let Some(d) = decimal(&v)? else { return Ok(String::new()) };
            let c = currency(state, currency_code)?;
            // Names given in the template: `[unit, units]` or `[[unit, units], [fraction, fractions]]`.
            let given: Option<(Vec<String>, Vec<String>)> = names.map(|n| json(&n)).and_then(|n| {
                let list = |v: &Value| v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect::<Vec<_>>());
                match n.as_array()?.first()? {
                    Value::Array(_) => Some((list(&n[0])?, n.get(1).and_then(list).unwrap_or_default())),
                    _ => Some((list(&n)?, vec![])),
                }
            });
            let refs = given.as_ref().map(|(a, b)| (a.iter().map(String::as_str).collect::<Vec<_>>(), b.iter().map(String::as_str).collect::<Vec<_>>()));
            words(d, &c, &lang(state, language), refs.as_ref().map(|(a, b)| (a.as_slice(), b.as_slice()))).map_err(bad)
        },
    );
    env.add_filter("date", |state: &State, v: JValue, style: Option<String>, language: Option<String>| -> Result<String, JError> {
        match json(&v) {
            Value::Null => Ok(String::new()),
            Value::String(s) if s.is_empty() => Ok(String::new()),
            Value::String(s) => date(&s, style.as_deref().unwrap_or("short"), locale(&lang(state, language))).map_err(bad),
            other => Err(bad(format!("{other} is not a date"))),
        }
    });
    env.add_filter("total", |items: JValue, attribute: Option<String>| -> Result<String, JError> {
        let mut sum = Decimal::ZERO;
        for item in json(&items).as_array().ok_or_else(|| bad("total adds up a list"))? {
            let v = match &attribute {
                Some(a) => item.get(a).cloned().unwrap_or(Value::Null),
                None => item.clone(),
            };
            if !v.is_null() {
                sum += crate::value::parse_decimal(&v).map_err(|e| bad(format!("{v} is not a number: {e}")))?;
            }
        }
        Ok(sum.normalize().to_string())
    });
    env.add_filter("pad", |v: JValue, width: usize, align: Option<String>, fill: Option<String>| -> Result<String, JError> {
        if width > 10_000 {
            return Err(bad("a field is at most 10000 characters wide"));
        }
        let s = match json(&v) {
            Value::Null => String::new(),
            Value::String(s) => s,
            other => other.to_string(),
        };
        let fill = fill.and_then(|f| f.chars().next()).unwrap_or(' ');
        Ok(pad(&s, width, align.as_deref().unwrap_or("left"), fill))
    });
    // As written into a page, it is escaped as anything is there; anywhere else it is the field as it stands.
    env.add_filter("csv", |state: &State, v: JValue| {
        let field = csv(&json(&v));
        if matches!(state.auto_escape(), minijinja::AutoEscape::Html) { JValue::from(field) } else { JValue::from_safe_string(field) }
    });
    env.add_filter("xml", |v: JValue| {
        let s = match json(&v) {
            Value::Null => String::new(),
            Value::String(s) => s,
            other => other.to_string(),
        };
        JValue::from_safe_string(xml(&s))
    });
    // `barcode(value, "ean13", module=2, height=60, text=true)`. While a template is checked against a record of empty
    // values, a value no barcode can hold draws nothing.
    env.add_function("barcode", |state: &State, value: String, kind: Option<String>, kwargs: Kwargs| -> Result<JValue, JError> {
        let (module, height, text): (Option<u32>, Option<u32>, Option<bool>) = (kwargs.get("module")?, kwargs.get("height")?, kwargs.get("text")?);
        kwargs.assert_all_used()?;
        match barcode(&value, kind.as_deref().unwrap_or("code128"), module.unwrap_or(2), height.unwrap_or(60), text.unwrap_or(true)) {
            Ok(svg) => Ok(JValue::from_safe_string(svg)),
            Err(_) if state.lookup("checking").is_some_and(|c| c.is_true()) => Ok(JValue::from("")),
            Err(e) => Err(bad(e)),
        }
    });
    env.add_function("qr", |text: String| -> JValue {
        match qrcode::QrCode::new(text.as_bytes()) {
            Ok(code) => JValue::from_safe_string(code.render::<qrcode::render::svg::Color>().min_dimensions(150, 150).quiet_zone(true).build()),
            Err(_) => JValue::from(""),
        }
    });
    // Where a PDF's header and footer say which page this is, and of how many (the PDF service fills them in).
    env.add_function("page_number", || JValue::from_safe_string(r#"<span class="pageNumber"></span>"#.into()));
    env.add_function("page_count", || JValue::from_safe_string(r#"<span class="totalPages"></span>"#.into()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn numbers_as_each_language_writes_them() {
        assert_eq!(number(d("1234567.891"), 2, locale("en")), "1,234,567.89");
        assert_eq!(number(d("1234567.891"), 2, locale("en-IN")), "12,34,567.89");
        assert_eq!(number(d("1234567.891"), 2, locale("hi")), "12,34,567.89");
        assert_eq!(number(d("-1234567.5"), 2, locale("de-AT")), "-1.234.567,50");
        assert_eq!(number(d("1234.5"), 1, locale("ar")), "١٬٢٣٤٫٥");
        assert_eq!(number(d("999"), 0, locale("en-IN")), "999");
        assert_eq!(number(d("100000"), 0, locale("hi")), "1,00,000");
        assert_eq!(number(d("-0.004"), 2, locale("en")), "0.00");
        assert_eq!(locale("xx-YY").tag, "en");
    }

    #[test]
    fn amounts_take_the_currency_decimals_and_mark() {
        let inr = Currency { code: "INR".into(), symbol: Some("₹".into()), precision: 2 };
        assert_eq!(amount(d("1234567"), &inr, locale("en-IN"), Show::Symbol), "₹12,34,567.00");
        assert_eq!(amount(d("1234.5"), &Currency::iso("KWD"), locale("en"), Show::Symbol), "KWD\u{a0}1,234.500");
        assert_eq!(amount(d("1234.5"), &Currency::iso("JPY"), locale("ja"), Show::Code), "JPY\u{a0}1,235");
        let eur = Currency { code: "EUR".into(), symbol: Some("€".into()), precision: 2 };
        assert_eq!(amount(d("1234.5"), &eur, locale("de"), Show::Symbol), "1.234,50\u{a0}€");
        assert_eq!(Currency::from_record(&json!({"code": "BHD", "symbol": "", "precision": 3})).unwrap().precision, 3);
    }

    #[test]
    fn dates_as_each_language_writes_them() {
        assert_eq!(date("2026-11-02", "short", locale("en")).unwrap(), "11/02/2026");
        assert_eq!(date("2026-11-02T10:00:00Z", "long", locale("en-IN")).unwrap(), "2 November 2026");
        assert_eq!(date("2026-11-02", "long", locale("hi")).unwrap(), "2 नवंबर 2026");
        assert_eq!(date("2026-11-02", "long", locale("ar")).unwrap(), "٢ نوفمبر ٢٠٢٦");
        assert_eq!(date("2026-11-02", "%d.%m.%y", locale("en")).unwrap(), "02.11.26");
        assert!(date("yesterday", "short", locale("en")).is_err());
    }

    #[test]
    fn amounts_in_words_in_each_shipped_language() {
        let inr = Currency::iso("INR");
        assert_eq!(
            words(d("1234567.50"), &inr, "en", None).unwrap(),
            "one million two hundred thirty-four thousand five hundred sixty-seven rupees and fifty paise"
        );
        assert_eq!(words(d("1234567.50"), &inr, "en-IN", None).unwrap(), "twelve lakh thirty-four thousand five hundred sixty-seven rupees and fifty paise");
        assert_eq!(words(d("1"), &inr, "en", None).unwrap(), "one rupee");
        assert_eq!(words(d("-21.01"), &Currency::iso("USD"), "en", None).unwrap(), "minus twenty-one dollars and one cent");
        assert_eq!(words(d("1000000000"), &inr, "en-IN", None).unwrap(), "one hundred crore rupees");
        assert_eq!(words(d("123456789.05"), &inr, "hi", None).unwrap(), "बारह करोड़ चौंतीस लाख छप्पन हज़ार सात सौ नवासी रुपये और पाँच पैसे");
        assert_eq!(words(d("2"), &Currency::iso("SAR"), "ar", None).unwrap(), "ريالان");
        assert_eq!(words(d("1"), &Currency::iso("SAR"), "ar", None).unwrap(), "ريال واحد");
        assert_eq!(words(d("5"), &Currency::iso("SAR"), "ar", None).unwrap(), "خمسة ريالات");
        assert_eq!(words(d("21"), &Currency::iso("SAR"), "ar", None).unwrap(), "واحد وعشرون ريالاً");
        assert_eq!(words(d("2500.250"), &Currency::iso("KWD"), "ar", None).unwrap(), "ألفان وخمسمائة دينار ومائتان وخمسون فلساً");
        // Not in the table: said with its code, its fraction over its whole.
        assert_eq!(words(d("12.34"), &Currency::iso("XYZ"), "en", None).unwrap(), "twelve XYZ and 34/100");
        // Names the template gives.
        assert_eq!(words(d("3.5"), &Currency::iso("XYZ"), "en", Some((&["crown", "crowns"], &["öre", "öre"]))).unwrap(), "three crowns and fifty öre");
        assert!(words(d("1"), &inr, "de", None).is_err(), "a language not shipped is refused, never said in another");
    }

    #[test]
    fn every_hindi_number_to_ninety_nine_is_its_own_word() {
        let all: std::collections::HashSet<&str> = HI_99.iter().copied().collect();
        assert_eq!(all.len(), 100);
        assert_eq!((hi(100), hi(105), hi(100_000), hi(100_000_000)), ("एक सौ".into(), "एक सौ पाँच".into(), "एक लाख".into(), "दस करोड़".into()));
    }

    #[test]
    fn arabic_counts_by_its_forms() {
        assert_eq!(ar(0), "صفر");
        assert_eq!(ar(11), "أحد عشر");
        assert_eq!(ar(15), "خمسة عشر");
        assert_eq!(ar(1000), "ألف");
        assert_eq!(ar(2000), "ألفان");
        assert_eq!(ar(3000), "ثلاثة آلاف");
        assert_eq!(ar(11000), "أحد عشر ألف");
        assert_eq!(ar(2_000_105), "مليونان ومائة وخمسة");
    }

    #[test]
    fn code128_table_and_check_symbol() {
        // Every symbol is 11 modules, its bars an even number of them; the stop is 13; no two alike.
        for (i, s) in CODE128.iter().enumerate() {
            let w: Vec<u32> = s.chars().map(|c| c.to_digit(10).unwrap()).collect();
            assert_eq!(w.iter().sum::<u32>(), if i == 106 { 13 } else { 11 }, "symbol {i}");
            assert_eq!(w.iter().step_by(2).sum::<u32>() % 2, 0, "symbol {i}'s bars");
        }
        assert_eq!(CODE128.iter().collect::<std::collections::HashSet<_>>().len(), 107);
        // Start B, P J J 1 2 3 C, check (104 + 48·1 + 42·2 + 42·3 + 17·4 + 18·5 + 19·6 + 35·7) mod 103 = 55, stop.
        assert_eq!(code128_values("PJJ123C").unwrap(), [104, 48, 42, 42, 17, 18, 19, 35, 55, 106]);
        // An even run of digits in set C, two to a symbol: (105 + 12·1 + 34·2) mod 103 = 82.
        assert_eq!(code128_values("1234").unwrap(), [105, 12, 34, 82, 106]);
        assert!(code128_values("tab\there").is_err());
        assert!(code128_values("").is_err());
        let svg = barcode("A<1>", "code128", 2, 50, true).unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("A&lt;1&gt;"), "{svg}");
    }

    #[test]
    fn ean13_check_digit_and_bars() {
        assert_eq!(ean13_check(&[4, 0, 0, 6, 3, 8, 1, 3, 3, 3, 9, 3]), 1);
        let (m, digits) = ean13_modules("400638133393").unwrap();
        assert_eq!((m.len(), digits.as_str()), (95, "4006381333931"));
        assert!(m.starts_with("101") && m.ends_with("101") && &m[45..50] == "01010");
        assert!(ean13_modules("4006381333931").is_ok());
        assert_eq!(ean13_modules("4006381333932").unwrap_err(), "4006381333932's check digit is 2, not 1");
        assert!(ean13_modules("40063813339a").is_err());
        // 4 leads with parity L G L L G G: its first digit 0 in set L; the right half's first, 3, is set L inverted.
        assert_eq!((&m[3..10], &m[10..17], &m[50..57]), ("0001101", "0100111", "1000010"));
    }

    #[test]
    fn fields_cannot_break_a_file() {
        assert_eq!(pad("ACME Traders Ltd", 10, "left", ' '), "ACME Trade");
        assert_eq!(pad("42", 6, "right", '0'), "000042");
        assert_eq!(pad("a\r\nb", 5, "left", '.'), "a  b.");
        assert_eq!(csv(&json!("Acme, \"Ltd\"\nx")), "\"Acme, \"\"Ltd\"\"\nx\"");
        assert_eq!(csv(&json!("a;b")), "\"a;b\"");
        assert_eq!(csv(&json!("=HYPERLINK(1)")), "'=HYPERLINK(1)");
        assert_eq!(csv(&json!("-12.50")), "-12.50");
        assert_eq!(xml("</a><b x=\"1\">&'\u{1}"), "&lt;/a&gt;&lt;b x=&quot;1&quot;&gt;&amp;&apos;");
    }

    #[test]
    fn files_are_encoded_as_stated() {
        assert_eq!(encode("a\nb\r\n", None, true).unwrap(), b"a\r\nb\r\n");
        assert_eq!(encode("Müller\n", Some("iso-8859-1"), false).unwrap(), b"M\xfcller\n");
        assert_eq!(encode("Müller", Some("ascii"), false).unwrap_err(), "'ü' (character 2) has no place in ascii");
        assert!(encode("₹", Some("iso-8859-1"), false).is_err());
    }
}
