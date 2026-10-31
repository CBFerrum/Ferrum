//! Markdown in and out of documents: what templates and AI agents write, as the editor's own nodes
//! and marks (TipTap's names, as y-prosemirror stores them), and a document as Markdown for them
//! to read.
//!
//! The subset: ATX headings; paragraphs (a line ending in `\` or two spaces breaks); bullet,
//! numbered and task lists, nested by indentation; quotes; fenced code; rules; pipe tables;
//! bold, italic, strikethrough, code, and links to the web or mail (any other link keeps its text).

use super::FRAGMENT;
use std::collections::HashMap;
use std::sync::Arc;
use yrs::types::text::YChange;
use yrs::{Any, Doc, GetString, Out, ReadTxn, Text, Transact, TransactionMut, Xml, XmlElementPrelim, XmlElementRef, XmlFragment, XmlOut, XmlTextPrelim};

// ───────────────────────────── reading Markdown ─────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Block {
    Heading(usize, String),
    /// Its lines between hard breaks.
    Paragraph(Vec<String>),
    Quote(Vec<Block>),
    Code(Option<String>, String),
    Rule,
    List(List),
    Table(Vec<String>, Vec<Vec<String>>),
}

#[derive(Debug, Clone, PartialEq)]
struct List {
    ordered: bool,
    start: u64,
    tasks: bool,
    items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq)]
struct Item {
    checked: bool,
    blocks: Vec<Block>,
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// A code fence: its character, length and info string.
fn fence(line: &str) -> Option<(char, usize, &str)> {
    if indent(line) > 3 {
        return None;
    }
    let t = line.trim_start();
    let c = t.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = t.chars().take_while(|x| *x == c).count();
    let info = t[n..].trim();
    (n >= 3 && !(c == '`' && info.contains('`'))).then_some((c, n, info))
}

fn heading(line: &str) -> Option<(usize, &str)> {
    if indent(line) > 3 {
        return None;
    }
    let t = line.trim_start();
    let n = t.chars().take_while(|c| *c == '#').count();
    let rest = &t[n..];
    if !(1..=6).contains(&n) || !(rest.is_empty() || rest.starts_with(' ')) {
        return None;
    }
    let content = rest.trim();
    // A closing run of #s, when a space sets it apart.
    let stripped = content.trim_end_matches('#');
    Some((n, if stripped.len() < content.len() && (stripped.is_empty() || stripped.ends_with(' ')) { stripped.trim_end() } else { content }))
}

fn rule(line: &str) -> bool {
    if indent(line) > 3 {
        return false;
    }
    let t: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    t.len() >= 3 && ['-', '*', '_'].iter().any(|m| t.chars().all(|c| c == *m))
}

/// A list item's first line: `(indent, ordered, number, content column, content)`.
fn marker(line: &str) -> Option<(usize, bool, u64, usize, &str)> {
    let ind = indent(line);
    let t = &line[ind..];
    let (ordered, number, len) = match t.chars().next()? {
        '-' | '*' | '+' => (false, 0, 1),
        _ => {
            let digits = t.chars().take_while(char::is_ascii_digit).count();
            if digits == 0 || digits > 9 || !matches!(t[digits..].chars().next(), Some('.' | ')')) {
                return None;
            }
            (true, t[..digits].parse().ok()?, digits + 1)
        }
    };
    let rest = &t[len..];
    if rest.is_empty() {
        return Some((ind, ordered, number, ind + len + 1, ""));
    }
    if !rest.starts_with(' ') {
        return None;
    }
    let spaces = (rest.len() - rest.trim_start_matches(' ').len()).min(4);
    Some((ind, ordered, number, ind + len + spaces, rest[spaces..].trim_end()))
}

/// A table row's cells (`|` separated, `\|` kept), if the line has any pipe.
fn row(line: &str) -> Option<Vec<String>> {
    let t = line.trim();
    if !t.contains('|') {
        return None;
    }
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = if t.ends_with('|') && !t.ends_with("\\|") { &t[..t.len() - 1] } else { t };
    let (mut cells, mut cell, mut escaped) = (Vec::new(), String::new(), false);
    for c in t.chars() {
        match c {
            '|' if !escaped => cells.push(std::mem::take(&mut cell).trim().to_string()),
            '\\' if !escaped => escaped = true,
            _ => {
                if escaped && c != '|' {
                    cell.push('\\');
                }
                cell.push(c);
                escaped = false;
            }
        }
    }
    cells.push(cell.trim().to_string());
    Some(cells)
}

fn separator(line: &str) -> bool {
    row(line).is_some_and(|cells| {
        cells.iter().all(|c| {
            let c = c.trim_start_matches(':').trim_end_matches(':');
            !c.is_empty() && c.chars().all(|x| x == '-')
        })
    })
}

/// A line that starts a block of its own, ending a paragraph before it.
fn starts_block(line: &str) -> bool {
    fence(line).is_some()
        || heading(line).is_some()
        || rule(line)
        || (indent(line) <= 3 && line.trim_start().starts_with('>'))
        || marker(line).is_some_and(|(_, ordered, number, _, rest)| !rest.is_empty() && (!ordered || number == 1))
}

fn blocks(lines: &[&str]) -> Vec<Block> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if blank(line) {
            i += 1;
            continue;
        }
        if let Some((c, n, info)) = fence(line) {
            let ind = indent(line);
            let lang = info.split_whitespace().next().map(str::to_string);
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() {
                if let Some((c2, n2, rest)) = fence(lines[i])
                    && c2 == c
                    && n2 >= n
                    && rest.is_empty()
                {
                    i += 1;
                    break;
                }
                let l = lines[i];
                body.push(&l[indent(l).min(ind)..]);
                i += 1;
            }
            out.push(Block::Code(lang, body.join("\n")));
            continue;
        }
        if let Some((level, text)) = heading(line) {
            out.push(Block::Heading(level, text.to_string()));
            i += 1;
            continue;
        }
        if rule(line) {
            out.push(Block::Rule);
            i += 1;
            continue;
        }
        if indent(line) <= 3 && line.trim_start().starts_with('>') {
            let mut inner = Vec::new();
            while i < lines.len() && !blank(lines[i]) {
                let t = lines[i].trim_start();
                match t.strip_prefix('>') {
                    Some(rest) => inner.push(rest.strip_prefix(' ').unwrap_or(rest)),
                    None if starts_block(lines[i]) => break,
                    // A paragraph's lazy continuation.
                    None => inner.push(t),
                }
                i += 1;
            }
            out.push(Block::Quote(blocks(&inner)));
            continue;
        }
        if let Some((ind, ordered, start, _, _)) = marker(line)
            && ind <= 3
        {
            let (list, next) = list(lines, i, ind, ordered, start);
            out.push(Block::List(list));
            i = next;
            continue;
        }
        if i + 1 < lines.len()
            && let Some(header) = row(line)
            && separator(lines[i + 1])
        {
            let mut rows = Vec::new();
            i += 2;
            while i < lines.len()
                && !blank(lines[i])
                && let Some(r) = row(lines[i])
            {
                rows.push(r);
                i += 1;
            }
            out.push(Block::Table(header, rows));
            continue;
        }
        // A paragraph, up to a blank line or another block.
        let mut parts = vec![String::new()];
        while i < lines.len() && !blank(lines[i]) && (parts[0].is_empty() || !starts_block(lines[i])) {
            let l = lines[i].trim_start();
            let hard = l.ends_with('\\') || l.ends_with("  ");
            let text = l.strip_suffix('\\').unwrap_or(l).trim_end();
            let last = parts.last_mut().expect("a part");
            if !last.is_empty() {
                last.push(' ');
            }
            last.push_str(text);
            if hard {
                parts.push(String::new());
            }
            i += 1;
        }
        if parts.len() > 1 && parts.last().is_some_and(String::is_empty) {
            parts.pop();
        }
        out.push(Block::Paragraph(parts));
    }
    out
}

fn list(lines: &[&str], mut i: usize, ind: usize, ordered: bool, start: u64) -> (List, usize) {
    let mut items = Vec::new();
    let mut tasks = None;
    while i < lines.len() {
        let Some((mi, mo, _, col, rest)) = marker(lines[i]) else { break };
        if mi != ind || mo != ordered || rule(lines[i]) {
            break;
        }
        // The editor keeps checklists apart from other lists: a change of kind starts a new list.
        let boxed = !ordered && task(rest).is_some();
        if tasks.is_some_and(|t| t != boxed) {
            break;
        }
        let mut body = vec![rest.to_string()];
        i += 1;
        while i < lines.len() {
            let l = lines[i];
            if blank(l) {
                // A blank line stays in the item when what follows is indented into it.
                match lines[i..].iter().position(|x| !blank(x)) {
                    Some(p) if indent(lines[i + p]) >= col => {
                        body.push(String::new());
                        i += 1;
                        continue;
                    }
                    _ => break,
                }
            }
            if indent(l) >= col {
                body.push(l[col..].to_string());
            } else if !starts_block(l) && marker(l).is_none() && !body.iter().any(String::is_empty) {
                // The first paragraph's lazy continuation.
                body.push(l.trim_start().to_string());
            } else {
                break;
            }
            i += 1;
        }
        tasks.get_or_insert(boxed);
        let checked = match (!ordered).then(|| task(&body[0])).flatten() {
            Some((checked, rest)) => {
                body[0] = rest;
                checked
            }
            None => false,
        };
        let refs: Vec<&str> = body.iter().map(String::as_str).collect();
        items.push(Item { checked, blocks: blocks(&refs) });
        // The next item of the same list, past blank lines.
        let next = lines[i..].iter().position(|x| !blank(x)).map(|p| i + p);
        match next {
            Some(n) if marker(lines[n]).is_some_and(|m| m.0 == ind && m.1 == ordered) && !rule(lines[n]) => i = n,
            _ => break,
        }
    }
    (List { ordered, start, tasks: tasks.unwrap_or(false), items }, i)
}

/// `[ ] rest` or `[x] rest`.
fn task(first: &str) -> Option<(bool, String)> {
    let checked = match first.get(..3)? {
        "[ ]" => false,
        "[x]" | "[X]" => true,
        _ => return None,
    };
    let rest = &first[3..];
    (rest.is_empty() || rest.starts_with(' ')).then(|| (checked, rest.trim_start().to_string()))
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Marks {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
    link: Option<String>,
}

impl Marks {
    /// As y-prosemirror stores marks: each mark's attributes under its name.
    fn attrs(&self) -> HashMap<Arc<str>, Any> {
        let none = || Any::from(HashMap::<String, Any>::new());
        let mut a = HashMap::new();
        for (on, name) in [(self.bold, "bold"), (self.italic, "italic"), (self.strike, "strike"), (self.code, "code")] {
            if on {
                a.insert(Arc::from(name), none());
            }
        }
        if let Some(href) = &self.link {
            a.insert(Arc::from("link"), Any::from(HashMap::from([("href".to_string(), Any::from(href.as_str()))])));
        }
        a
    }
}

fn push(out: &mut Vec<(String, Marks)>, text: String, marks: &Marks) {
    if text.is_empty() {
        return;
    }
    match out.last_mut() {
        Some((t, m)) if m == marks => t.push_str(&text),
        _ => out.push((text, marks.clone())),
    }
}

/// The closing delimiter of `n` characters `c` from `from`: not escaped, not preceded by a space,
/// and (for `_`) not followed by a letter or digit. A longer run is a nested mark, skipped.
fn closing(chars: &[char], from: usize, c: char, n: usize) -> Option<usize> {
    let mut j = from;
    while j < chars.len() {
        if chars[j] == '\\' {
            j += 2;
            continue;
        }
        if chars[j] == '`' {
            // Delimiters inside a code span do not count.
            let run = chars[j..].iter().take_while(|x| **x == '`').count();
            j = (j + run..chars.len()).find(|&k| chars[k..].iter().take_while(|x| **x == '`').count() == run).map_or(j + run, |k| k + run);
            continue;
        }
        if chars[j] == c {
            let run = chars[j..].iter().take_while(|x| **x == c).count();
            let after = chars.get(j + run);
            if run == n && j > from && !chars[j - 1].is_whitespace() && !(c == '_' && after.is_some_and(|a| a.is_alphanumeric())) {
                return Some(j);
            }
            j += run;
            continue;
        }
        j += 1;
    }
    None
}

/// `[label](url)` at `i`: where the label ends, and the address's span.
fn link_at(chars: &[char], i: usize) -> Option<(usize, usize, usize)> {
    let mut j = i + 1;
    let mut depth = 0;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 1,
            '[' => depth += 1,
            ']' if depth == 0 => break,
            ']' => depth -= 1,
            _ => {}
        }
        j += 1;
    }
    if j + 1 >= chars.len() || chars[j + 1] != '(' {
        return None;
    }
    // Addresses may hold balanced parentheses.
    let mut depth = 0;
    let close = (j + 2..chars.len()).find(|&k| match chars[k] {
        '(' => {
            depth += 1;
            false
        }
        ')' if depth == 0 => true,
        ')' => {
            depth -= 1;
            false
        }
        _ => false,
    })?;
    Some((j, j + 2, close))
}

fn inline(s: &str, m: &Marks, out: &mut Vec<(String, Marks)>) {
    let chars: Vec<char> = s.chars().collect();
    let mut text = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if chars.get(i + 1).is_some_and(char::is_ascii_punctuation) => {
                text.push(chars[i + 1]);
                i += 2;
            }
            '`' if !m.code => {
                let n = chars[i..].iter().take_while(|x| **x == '`').count();
                let end = (i + n..chars.len()).find(|&k| chars[k..].iter().take_while(|x| **x == '`').count() == n && (k == 0 || chars[k - 1] != '`'));
                match end {
                    Some(end) => {
                        push(out, std::mem::take(&mut text), m);
                        let code: String = chars[i + n..end].iter().collect();
                        let code = if code.len() > 1 && code.starts_with(' ') && code.ends_with(' ') { code[1..code.len() - 1].to_string() } else { code };
                        push(out, code, &Marks { code: true, link: m.link.clone(), ..Marks::default() });
                        i = end + n;
                    }
                    None => {
                        text.extend(std::iter::repeat_n('`', n));
                        i += n;
                    }
                }
            }
            '*' | '_' | '~' => {
                let run = chars[i..].iter().take_while(|x| **x == c).count();
                let n = match (c, run) {
                    ('~', r) if r >= 2 => 2,
                    ('~', _) => 0,
                    (_, r) => r.min(3),
                };
                let opens = n > 0 && chars.get(i + n).is_some_and(|x| !x.is_whitespace()) && !(c == '_' && i > 0 && chars[i - 1].is_alphanumeric());
                match opens.then(|| closing(&chars, i + n, c, n)).flatten() {
                    Some(end) => {
                        push(out, std::mem::take(&mut text), m);
                        let inner: String = chars[i + n..end].iter().collect();
                        let mut m2 = m.clone();
                        match (c, n) {
                            ('~', _) => m2.strike = true,
                            (_, 1) => m2.italic = true,
                            (_, 2) => m2.bold = true,
                            _ => {
                                m2.bold = true;
                                m2.italic = true;
                            }
                        }
                        inline(&inner, &m2, out);
                        i = end + n;
                    }
                    None => {
                        text.extend(std::iter::repeat_n(c, run));
                        i += run;
                    }
                }
            }
            '[' => match link_at(&chars, i) {
                Some((label_end, from, to)) => {
                    push(out, std::mem::take(&mut text), m);
                    let label: String = chars[i + 1..label_end].iter().collect();
                    let address: String = chars[from..to].iter().collect();
                    let href = address.split_whitespace().next().unwrap_or("").trim_matches(|c| c == '<' || c == '>').to_string();
                    let mut m2 = m.clone();
                    if super::safe_href(&href) {
                        m2.link = Some(href);
                    }
                    inline(&label, &m2, out);
                    i = to + 1;
                }
                None => {
                    text.push(c);
                    i += 1;
                }
            },
            '<' => {
                let end = (i + 1..chars.len()).find(|&k| chars[k] == '>' || chars[k].is_whitespace() || chars[k] == '<');
                match end.filter(|&k| chars[k] == '>') {
                    Some(k) if super::safe_href(&chars[i + 1..k].iter().collect::<String>()) => {
                        push(out, std::mem::take(&mut text), m);
                        let href: String = chars[i + 1..k].iter().collect();
                        push(out, href.clone(), &Marks { link: Some(href), ..m.clone() });
                        i = k + 1;
                    }
                    _ => {
                        text.push(c);
                        i += 1;
                    }
                }
            }
            _ => {
                text.push(c);
                i += 1;
            }
        }
    }
    push(out, text, m);
}

// ───────────────────────────── writing the document ─────────────────────────────

fn write_inline(txn: &mut TransactionMut, el: &XmlElementRef, lines: &[String]) {
    for (n, line) in lines.iter().enumerate() {
        if n > 0 {
            el.push_back(txn, XmlElementPrelim::empty("hardBreak"));
        }
        let mut segments = Vec::new();
        inline(line, &Marks::default(), &mut segments);
        let plain: String = segments.iter().map(|s| s.0.as_str()).collect();
        if plain.is_empty() {
            continue;
        }
        // The text, then its marks by range: inserting marked runs one after another would make
        // each take the marks of the run before it.
        let t = el.push_back(txn, XmlTextPrelim::new(plain.as_str()));
        let mut at = 0u32;
        for (text, marks) in &segments {
            let len = text.encode_utf16().count() as u32;
            let attrs = marks.attrs();
            if !attrs.is_empty() {
                t.format(txn, at, len, attrs);
            }
            at += len;
        }
    }
}

fn write_blocks<P: XmlFragment>(txn: &mut TransactionMut, parent: &P, blocks: &[Block]) {
    for b in blocks {
        match b {
            Block::Heading(level, text) => {
                let h = parent.push_back(txn, XmlElementPrelim::empty("heading"));
                h.insert_attribute(txn, "level", Any::from(*level as i64));
                write_inline(txn, &h, std::slice::from_ref(text));
            }
            Block::Paragraph(lines) => {
                let p = parent.push_back(txn, XmlElementPrelim::empty("paragraph"));
                write_inline(txn, &p, lines);
            }
            Block::Quote(inner) => {
                let q = parent.push_back(txn, XmlElementPrelim::empty("blockquote"));
                // A quote holds at least one block.
                let inner = if inner.is_empty() { vec![Block::Paragraph(vec![String::new()])] } else { inner.clone() };
                write_blocks(txn, &q, &inner);
            }
            Block::Code(lang, code) => {
                let c = parent.push_back(txn, XmlElementPrelim::empty("codeBlock"));
                if let Some(l) = lang {
                    c.insert_attribute(txn, "language", l.as_str());
                }
                if !code.is_empty() {
                    c.push_back(txn, XmlTextPrelim::new(code.as_str()));
                }
            }
            Block::Rule => {
                parent.push_back(txn, XmlElementPrelim::empty("horizontalRule"));
            }
            Block::List(l) => {
                let (tag, item_tag) = match (l.tasks, l.ordered) {
                    (true, _) => ("taskList", "taskItem"),
                    (false, true) => ("orderedList", "listItem"),
                    (false, false) => ("bulletList", "listItem"),
                };
                let el = parent.push_back(txn, XmlElementPrelim::empty(tag));
                if l.ordered && l.start != 1 {
                    el.insert_attribute(txn, "start", Any::from(l.start.min(i64::MAX as u64) as i64));
                }
                for item in &l.items {
                    let li = el.push_back(txn, XmlElementPrelim::empty(item_tag));
                    if l.tasks {
                        li.insert_attribute(txn, "checked", item.checked);
                    }
                    // An item starts with a paragraph (`paragraph block*`).
                    let mut blocks = item.blocks.clone();
                    if !matches!(blocks.first(), Some(Block::Paragraph(_))) {
                        blocks.insert(0, Block::Paragraph(vec![String::new()]));
                    }
                    write_blocks(txn, &li, &blocks);
                }
            }
            Block::Table(header, rows) => {
                let width = rows.iter().map(Vec::len).chain([header.len()]).max().unwrap_or(1).clamp(1, 100);
                let t = parent.push_back(txn, XmlElementPrelim::empty("table"));
                for (r, cells) in std::iter::once(header).chain(rows).enumerate() {
                    let tr = t.push_back(txn, XmlElementPrelim::empty("tableRow"));
                    for c in 0..width {
                        let cell = tr.push_back(txn, XmlElementPrelim::empty(if r == 0 { "tableHeader" } else { "tableCell" }));
                        let p = cell.push_back(txn, XmlElementPrelim::empty("paragraph"));
                        write_inline(txn, &p, &[cells.get(c).cloned().unwrap_or_default()]);
                    }
                }
            }
        }
    }
}

/// Adds the blocks of `markdown` at the end of the document's fragment; returns how many.
pub(super) fn append(txn: &mut TransactionMut, fragment: &yrs::XmlFragmentRef, markdown: &str) -> usize {
    let text = markdown.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ");
    let lines: Vec<&str> = text.lines().collect();
    let parsed = blocks(&lines);
    write_blocks(txn, fragment, &parsed);
    parsed.len()
}

// ───────────────────────────── the document as Markdown ─────────────────────────────

fn escape(text: &str, out: &mut String) {
    for c in text.chars() {
        if matches!(c, '\\' | '*' | '_' | '`' | '[' | ']' | '~' | '<' | '|') {
            out.push('\\');
        }
        out.push(c);
    }
}

/// Keeps a line's start from reading as a block: `#`, `>`, `-`, `+`, `1.`.
fn guard_start(line: String) -> String {
    let t = line.trim_start();
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    let risky = t.starts_with('#')
        || t.starts_with('>')
        || ((t.starts_with('-') || t.starts_with('+')) && t[1..].starts_with(' '))
        || (digits > 0 && t[digits..].starts_with(". "));
    if !risky {
        return line;
    }
    if digits > 0 {
        return format!("{}\\{}", &t[..digits], &t[digits..]);
    }
    format!("\\{t}")
}

fn number(v: Option<Out>) -> Option<i64> {
    match v? {
        Out::Any(Any::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Out::Any(Any::String(s)) => s.parse().ok(),
        _ => None,
    }
}

fn inline_md<T: ReadTxn>(el: &XmlElementRef, txn: &T) -> String {
    let mut out = String::new();
    for child in el.children(txn) {
        match child {
            XmlOut::Text(t) => {
                for chunk in t.diff(txn, YChange::identity) {
                    let Out::Any(Any::String(s)) = chunk.insert else { continue };
                    let attrs = chunk.attributes.as_deref();
                    let on = |name: &str| attrs.and_then(|a| a.get(name)).is_some_and(|v| !matches!(v, Any::Null | Any::Undefined | Any::Bool(false)));
                    let link = attrs.and_then(|a| a.get("link")).and_then(|v| match v {
                        Any::Map(m) => m.get("href").and_then(|h| if let Any::String(s) = h { Some(s.to_string()) } else { None }),
                        _ => None,
                    });
                    let mut piece = String::new();
                    if on("code") {
                        let ticks = if s.contains('`') { "``" } else { "`" };
                        let pad = if s.starts_with('`') || s.ends_with('`') { " " } else { "" };
                        piece = format!("{ticks}{pad}{s}{pad}{ticks}");
                    } else {
                        escape(&s, &mut piece);
                        // Marks hug their words: spaces at the edges stay outside them.
                        let core = piece.trim().to_string();
                        if !core.is_empty() {
                            let (lead, trail) = (&piece[..piece.len() - piece.trim_start().len()], &piece[piece.trim_end().len()..]);
                            let mut wrapped = core;
                            for (name, d) in [("strike", "~~"), ("italic", "*"), ("bold", "**")] {
                                if on(name) {
                                    wrapped = format!("{d}{wrapped}{d}");
                                }
                            }
                            piece = format!("{lead}{wrapped}{trail}");
                        }
                    }
                    match link.filter(|h| super::safe_href(h)) {
                        Some(href) => {
                            let _ = std::fmt::Write::write_fmt(&mut out, format_args!("[{piece}]({})", href.replace(')', "%29").replace(' ', "%20")));
                        }
                        None => out.push_str(&piece),
                    }
                }
            }
            XmlOut::Element(e) if &**e.tag() == "hardBreak" => out.push_str("\\\n"),
            XmlOut::Element(e) => out.push_str(&inline_md(&e, txn)),
            XmlOut::Fragment(_) => {}
        }
    }
    out
}

fn text_of<T: ReadTxn>(el: &XmlElementRef, txn: &T) -> String {
    el.children(txn)
        .map(|c| match c {
            XmlOut::Text(t) => t.get_string(txn),
            XmlOut::Element(e) => text_of(&e, txn),
            XmlOut::Fragment(_) => String::new(),
        })
        .collect()
}

fn indent_lines(text: &str, first: &str, rest: &str) -> String {
    let mut out = String::new();
    for (n, line) in text.lines().enumerate() {
        if n > 0 {
            out.push('\n');
        }
        if !line.is_empty() {
            out.push_str(if n == 0 { first } else { rest });
        }
        out.push_str(line);
    }
    out
}

fn blocks_md<T: ReadTxn>(children: impl Iterator<Item = XmlOut>, txn: &T) -> Vec<String> {
    let mut out = Vec::new();
    for child in children {
        let XmlOut::Element(e) = child else { continue };
        let block = match &**e.tag() {
            "paragraph" => {
                let text = inline_md(&e, txn);
                if text.trim().is_empty() {
                    continue;
                }
                text.split('\n').map(|l| guard_start(l.to_string())).collect::<Vec<_>>().join("\n")
            }
            "heading" => {
                let level = number(e.get_attribute(txn, "level")).unwrap_or(1).clamp(1, 6) as usize;
                format!("{} {}", "#".repeat(level), inline_md(&e, txn).replace("\\\n", " "))
            }
            "blockquote" => {
                let inner = blocks_md(e.children(txn), txn).join("\n\n");
                inner.lines().map(|l| if l.is_empty() { ">".to_string() } else { format!("> {l}") }).collect::<Vec<_>>().join("\n")
            }
            "codeBlock" => {
                let code = text_of(&e, txn);
                let lang = match e.get_attribute(txn, "language") {
                    Some(Out::Any(Any::String(s))) => s.to_string(),
                    _ => String::new(),
                };
                let longest = code.split(|c| c != '`').map(str::len).max().unwrap_or(0);
                let fence = "`".repeat(longest.max(2) + 1);
                format!("{fence}{lang}\n{code}\n{fence}")
            }
            "horizontalRule" => "---".to_string(),
            // Images stay with their document: Markdown (templates, agents) carries the words.
            super::IMAGE => continue,
            tag @ ("bulletList" | "orderedList" | "taskList") => {
                let start = number(e.get_attribute(txn, "start")).unwrap_or(1);
                let mut items = Vec::new();
                for (n, item) in e.children(txn).enumerate() {
                    let XmlOut::Element(li) = item else { continue };
                    let mark = match tag {
                        "orderedList" => format!("{}. ", start + n as i64),
                        "taskList" => {
                            let checked = matches!(li.get_attribute(txn, "checked"), Some(Out::Any(Any::Bool(true))));
                            format!("- [{}] ", if checked { "x" } else { " " })
                        }
                        _ => "- ".to_string(),
                    };
                    let width = if tag == "taskList" { 2 } else { mark.len() };
                    // A tight list: a nested list follows its item's line directly.
                    let parts = blocks_md(li.children(txn), txn);
                    let tight = parts.iter().skip(1).all(|p| p.starts_with("- ") || p.chars().next().is_some_and(|c| c.is_ascii_digit()));
                    let body = parts.join(if tight { "\n" } else { "\n\n" });
                    items.push(indent_lines(if body.is_empty() { " " } else { &body }, &mark, &" ".repeat(width)).trim_end().to_string());
                }
                items.join("\n")
            }
            "table" => {
                let rows: Vec<Vec<String>> = e
                    .children(txn)
                    .filter_map(|r| match r {
                        XmlOut::Element(r) => Some(
                            r.children(txn)
                                .filter_map(|c| match c {
                                    XmlOut::Element(c) => Some(blocks_md(c.children(txn), txn).join(" ").replace('\n', " ")),
                                    _ => None,
                                })
                                .collect(),
                        ),
                        _ => None,
                    })
                    .collect();
                let Some(width) = rows.iter().map(Vec::len).max().filter(|w| *w > 0) else { continue };
                let line = |cells: &[String]| format!("| {} |", (0..width).map(|c| cells.get(c).map_or("", String::as_str)).collect::<Vec<_>>().join(" | "));
                let mut t = vec![line(&rows[0]), format!("|{}|", vec!["---"; width].join("|"))];
                t.extend(rows[1..].iter().map(|r| line(r)));
                t.join("\n")
            }
            _ => blocks_md(e.children(txn), txn).join("\n\n"),
        };
        out.push(block);
    }
    out
}

/// The document as Markdown (marks without a Markdown form, such as highlight, keep only their text).
pub(super) fn read(doc: &Doc) -> String {
    let txn = doc.transact();
    let Some(fragment) = txn.get_xml_fragment(FRAGMENT) else { return String::new() };
    let mut out = blocks_md(fragment.children(&txn), &txn).join("\n\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::{FRAGMENT, html, new_doc, plain_text};
    use super::*;

    fn doc_of(md: &str) -> Doc {
        let doc = new_doc();
        let fragment = doc.get_or_insert_xml_fragment(FRAGMENT);
        append(&mut doc.transact_mut(), &fragment, md);
        doc
    }

    #[test]
    fn markdown_becomes_the_editors_nodes_and_marks() {
        let doc = doc_of(
            "# Q4 plan\n\nShip **live _documents_** with `code`, ~~old~~ and [a link](https://example.com/a?b=1).\nSame paragraph.\\\nAfter a break.\n\n\
             - one\n- two\n  - nested\n\n3. third\n4. fourth\n\n- [x] done\n- [ ] todo\n\n> quoted **text**\n\n```rust\nfn main() {}\n```\n\n---\n\n\
             | Owner | Due |\n|---|:-:|\n| Ana | Friday |\n\n[bad](javascript:alert(1)) and <https://x.test/p>",
        );
        let h = html(&doc);
        for part in [
            "<h1>Q4 plan</h1>",
            "Ship <strong>live </strong><strong><em>documents</em></strong> with <code>code</code>, <s>old</s> and <a href=\"https://example.com/a?b=1\" rel=\"noopener noreferrer nofollow\">a link</a>. Same paragraph.<br>After a break.</p>",
            "<ul><li><p>one</p>\n</li>\n<li><p>two</p>\n<ul><li><p>nested</p>",
            "<ol start=\"3\"><li><p>third</p>",
            "<li><input type=\"checkbox\" disabled checked><div><p>done</p>",
            "<blockquote><p>quoted <strong>text</strong></p>",
            "<pre><code>fn main() {}</code></pre>",
            "<hr>",
            "<th><p>Owner</p>\n</th>\n<th><p>Due</p>",
            "<td><p>Ana</p>",
            "<p>bad and <a href=\"https://x.test/p\" rel=\"noopener noreferrer nofollow\">https://x.test/p</a></p>",
        ] {
            assert!(h.contains(part), "missing {part}\nin {h}");
        }
        assert!(!h.contains("javascript"));
    }

    #[test]
    fn documents_read_back_as_markdown() {
        let md = "# Q4 plan\n\nShip **live** *docs* with `code`, ~~old~~ and [a link](https://example.com/a).\\\nNext line.\n\n\
                  - one\n- two\n  - nested\n\n1. first\n2. second\n\n- [x] done\n- [ ] todo\n\n> quoted\n\n```rust\nfn main() {}\n```\n\n---\n\n\
                  | Owner | Due |\n|---|---|\n| Ana | Friday |\n\nStars \\* and 1\\. kept.\n";
        let doc = doc_of(md);
        let back = read(&doc);
        assert_eq!(read(&doc_of(&back)), back, "reading what was written gives it back");
        for line in [
            "# Q4 plan",
            "Ship **live** *docs* with `code`, ~~old~~ and [a link](https://example.com/a).\\",
            "- two\n  - nested",
            "1. first\n2. second",
            "- [x] done\n- [ ] todo",
            "> quoted",
            "```rust\nfn main() {}\n```",
            "| Owner | Due |\n|---|---|\n| Ana | Friday |",
            "Stars \\* and 1. kept",
        ] {
            assert!(back.contains(line), "missing {line:?}\nin {back}");
        }
        assert_eq!(plain_text(&doc_of(&back)), plain_text(&doc));
    }

    #[test]
    fn checklists_and_bullets_side_by_side_become_two_lists() {
        let h = html(&doc_of("- Venue booked\n- [ ] Send the kit\n- [x] Book the hall\n- Coffee"));
        assert_eq!(
            h,
            "<ul><li><p>Venue booked</p>\n</li>\n</ul>\n<ul class=\"tasks\"><li><input type=\"checkbox\" disabled><div><p>Send the kit</p>\n</div></li>\n\
             <li><input type=\"checkbox\" disabled checked><div><p>Book the hall</p>\n</div></li>\n</ul>\n<ul><li><p>Coffee</p>\n</li>\n</ul>\n"
        );
    }

    #[test]
    fn odd_markdown_stays_text() {
        let doc = doc_of("2 * 3 * 4 = 24, snake_case_name, a ** b, unclosed *star and [bracket](\n\n1) one\n2) two\n\n#hashtag\n\n- ");
        let words = plain_text(&doc);
        assert!(words.starts_with("2 * 3 * 4 = 24, snake_case_name, a ** b, unclosed *star and [bracket]("), "{words}");
        assert!(words.contains("one\ntwo") && words.contains("#hashtag"), "{words}");
        assert!(html(&doc).contains("<ol><li><p>one</p>"));
        assert_eq!(read(&doc_of("")), "");
        assert_eq!(plain_text(&doc_of("日本 🎉 **émoji**")), "日本 🎉 émoji");
    }
}
