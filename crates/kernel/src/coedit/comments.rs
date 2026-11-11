//! Comment threads for a Word file: the caller's threads (the kernel keeps none), their anchors
//! (Yjs relative positions, as the editor stores them) resolved against the document, and
//! WordprocessingML's comments (`comments.xml`, replies and resolution in `commentsExtended.xml`).

use super::FRAGMENT;
use std::collections::HashMap;
use std::fmt::Write as _;
use yrs::branch::{Branch, BranchPtr};
use yrs::{ReadTxn, StickyIndex, Text, XmlFragment, XmlOut};

/// One comment of a thread, as its keeper has it.
#[derive(Debug, Clone, Default)]
pub struct Comment {
    pub author_email: String,
    /// The name shown; without it, the email's part before `@` (nothing more of it is written).
    pub author_name: Option<String>,
    pub at: Option<chrono::DateTime<chrono::Utc>>,
    /// Its words (at most 4000 characters are written).
    pub body: String,
}

/// A thread of comments on the document's text: the first comment and its replies, in order.
#[derive(Debug, Clone, Default)]
pub struct CommentThread {
    /// `{"from": …, "to": …}`: Yjs relative positions as JSON (`Y.relativePositionToJSON`). None: on
    /// the document as a whole (at its start).
    pub anchor: Option<serde_json::Value>,
    /// The text it was made on (at most 500 characters), shown with it when that text is gone.
    pub quote: Option<String>,
    pub resolved: bool,
    pub comments: Vec<Comment>,
}

/// A shared type's branch, the key texts are found by.
pub(super) fn ptr<B: AsRef<Branch>>(b: &B) -> BranchPtr {
    BranchPtr::from(b.as_ref())
}

/// Where a thread's range starts or ends, in one text of the document.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Edge {
    /// Ends come first where one range ends and another starts…
    End,
    Start,
    /// …but a range with nothing in it starts before it ends.
    EmptyEnd,
}

/// The threads placed in the document: per text (its branch), `(offset, edge, thread)` in order.
#[derive(Default)]
pub(super) struct Placed {
    pub marks: HashMap<BranchPtr, Vec<(u32, Edge, usize)>>,
    /// Threads whose text was removed: counted, and their quote shown in their first comment.
    pub removed: Vec<usize>,
    /// Threads with no place in the text (on the document as a whole, or their paragraph is gone):
    /// at the document's start.
    pub at_start: Vec<usize>,
}

/// The texts of the body in order, and each place between an element's children mapped to the
/// text that follows it (else the end of the last one): where a position between blocks lands.
struct Texts {
    order: HashMap<BranchPtr, usize>,
    between: HashMap<(BranchPtr, u32), (BranchPtr, u32)>,
}

fn texts<T: ReadTxn>(txn: &T) -> Texts {
    fn walk<T: ReadTxn>(node: XmlOut, txn: &T, t: &mut Texts, pending: &mut Vec<(BranchPtr, u32)>, last: &mut Option<(BranchPtr, u32)>) {
        match node {
            XmlOut::Text(x) => {
                let b = ptr(&x);
                t.order.insert(b, t.order.len());
                for p in pending.drain(..) {
                    t.between.insert(p, (b, 0));
                }
                *last = Some((b, x.len(txn)));
            }
            XmlOut::Element(e) => {
                let b = ptr(&e);
                let mut n = 0;
                for (i, c) in e.children(txn).enumerate() {
                    pending.push((b, i as u32));
                    walk(c, txn, t, pending, last);
                    n = i as u32 + 1;
                }
                pending.push((b, n));
            }
            XmlOut::Fragment(f) => {
                let b = ptr(&f);
                let mut n = 0;
                for (i, c) in f.children(txn).enumerate() {
                    pending.push((b, i as u32));
                    walk(c, txn, t, pending, last);
                    n = i as u32 + 1;
                }
                pending.push((b, n));
            }
        }
    }
    let mut t = Texts { order: HashMap::new(), between: HashMap::new() };
    let (mut pending, mut last) = (Vec::new(), None);
    if let Some(f) = txn.get_xml_fragment(FRAGMENT) {
        walk(XmlOut::Fragment(f), txn, &mut t, &mut pending, &mut last);
    }
    if let Some(end) = last {
        for p in pending {
            t.between.insert(p, end);
        }
    }
    t
}

/// A relative position now: the text and the offset in it, when it is in the body.
fn resolve<T: ReadTxn>(json: Option<&serde_json::Value>, txn: &T, t: &Texts) -> Option<(BranchPtr, u32)> {
    let sticky: StickyIndex = serde_json::from_value(json?.clone()).ok()?;
    let at = sticky.get_offset(txn)?;
    if at.branch.is_deleted() {
        return None;
    }
    if t.order.contains_key(&at.branch) { Some((at.branch, at.index)) } else { t.between.get(&(at.branch, at.index)).copied() }
}

/// Places each thread: its range where both ends still are; where its text was removed (the range
/// is empty though it quoted something), at the nearest place left; where an end is gone with its
/// paragraph, or cannot be read, at the document's start.
pub(super) fn place<T: ReadTxn>(threads: &[CommentThread], txn: &T) -> Placed {
    let t = texts(txn);
    let mut out = Placed::default();
    for (n, th) in threads.iter().enumerate() {
        if th.comments.is_empty() {
            continue;
        }
        let Some(anchor) = &th.anchor else {
            out.at_start.push(n);
            continue;
        };
        let (Some(from), Some(to)) = (resolve(anchor.get("from"), txn, &t), resolve(anchor.get("to"), txn, &t)) else {
            out.removed.push(n);
            out.at_start.push(n);
            continue;
        };
        let key = |(b, i): (BranchPtr, u32)| (t.order[&b], i);
        let quoted = th.quote.as_deref().is_some_and(|q| !q.trim().is_empty());
        if key(to) <= key(from) {
            if quoted {
                out.removed.push(n);
            }
            out.marks.entry(from.0).or_default().extend([(from.1, Edge::Start, n), (from.1, Edge::EmptyEnd, n)]);
            continue;
        }
        out.marks.entry(from.0).or_default().push((from.1, Edge::Start, n));
        out.marks.entry(to.0).or_default().push((to.1, Edge::End, n));
    }
    for m in out.marks.values_mut() {
        m.sort();
    }
    out
}

/// What the "not carried" list says of `n` threads whose text was removed.
pub(super) fn removed_words(n: usize) -> String {
    if n == 1 { "1 comment whose text was removed".into() } else { format!("{n} comments whose text was removed") }
}

/// The name shown for a comment's author: the name given, else the email's local part.
fn shown_name(c: &Comment) -> String {
    let name = c.author_name.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let local = c.author_email.split('@').next().unwrap_or_default().trim();
    let name = name.unwrap_or(if local.is_empty() { "Someone" } else { local });
    name.chars().filter(|c| !c.is_control()).take(100).collect()
}

/// Initials: the first letter of each word of the name (a local part's words split at `.`, `_`
/// and `-`), at most three.
fn initials(name: &str) -> String {
    name.split(|c: char| c.is_whitespace() || matches!(c, '.' | '_' | '-'))
        .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()))
        .flat_map(char::to_uppercase)
        .take(3)
        .collect()
}

/// Word ids of each comment, thread by thread: the first comment of thread `n` is `first[n]`.
pub(super) fn ids(threads: &[CommentThread]) -> Vec<usize> {
    let mut next = 0;
    threads
        .iter()
        .map(|t| {
            let first = next;
            next += t.comments.len();
            first
        })
        .collect()
}

/// A comment paragraph's id (`w14:paraId`), which `commentsExtended.xml` names threads by.
fn para_id(id: usize) -> String {
    format!("{:08X}", 0x1000_0000 + id)
}

/// `comments.xml` and `commentsExtended.xml`: every comment with its author, initials and date, a
/// reply naming its thread's first comment, a resolved thread's comments done.
pub(super) fn parts(threads: &[CommentThread], removed: &[usize], esc: impl Fn(&str) -> String) -> (String, String) {
    const NS: &str = "xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
                      xmlns:w14=\"http://schemas.microsoft.com/office/word/2010/wordml\" \
                      xmlns:w15=\"http://schemas.microsoft.com/office/word/2012/wordml\" \
                      xmlns:mc=\"http://schemas.openxmlformats.org/markup-compatibility/2006\" mc:Ignorable=\"w14 w15\"";
    let mut comments = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w:comments {NS}>");
    let mut extended = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<w15:commentsEx {NS}>");
    let first = ids(threads);
    for (n, th) in threads.iter().enumerate() {
        for (i, c) in th.comments.iter().enumerate() {
            let id = first[n] + i;
            let name = shown_name(c);
            let date = c.at.map(|d| format!(" w:date=\"{}\"", d.format("%Y-%m-%dT%H:%M:%SZ"))).unwrap_or_default();
            let _ = write!(comments, "<w:comment w:id=\"{id}\" w:author=\"{}\" w:initials=\"{}\"{date}>", esc(&name), esc(&initials(&name)));
            let mut body: String = c.body.chars().take(4000).collect();
            if i == 0 && removed.contains(&n) {
                let quote: String = th.quote.as_deref().unwrap_or_default().chars().take(500).collect();
                if !quote.trim().is_empty() {
                    let _ = write!(body, "\n\n[On text since removed: \u{201C}{}\u{201D}]", quote.trim());
                }
            }
            let lines: Vec<&str> = body.split('\n').collect();
            for (l, line) in lines.iter().enumerate() {
                let last = l + 1 == lines.len();
                let pid = if last { format!(" w14:paraId=\"{}\" w14:textId=\"77777777\"", para_id(id)) } else { String::new() };
                let _ = write!(comments, "<w:p{pid}><w:pPr><w:pStyle w:val=\"CommentText\"/></w:pPr>");
                if l == 0 {
                    comments.push_str("<w:r><w:rPr><w:rStyle w:val=\"CommentReference\"/></w:rPr><w:annotationRef/></w:r>");
                }
                if !line.is_empty() {
                    let _ = write!(comments, "<w:r><w:t xml:space=\"preserve\">{}</w:t></w:r>", esc(line.trim_end_matches('\r')));
                }
                comments.push_str("</w:p>");
            }
            comments.push_str("</w:comment>");
            let parent = if i > 0 { format!(" w15:paraIdParent=\"{}\"", para_id(first[n])) } else { String::new() };
            let _ = write!(extended, "<w15:commentEx w15:paraId=\"{}\"{parent} w15:done=\"{}\"/>", para_id(id), u8::from(th.resolved));
        }
    }
    comments.push_str("</w:comments>");
    extended.push_str("</w15:commentsEx>");
    (comments, extended)
}

/// A thread's range starting here: one range per comment (Word ties each comment to a range).
pub(super) fn starts(threads: &[CommentThread], first: &[usize], n: usize, out: &mut String) {
    for id in first[n]..first[n] + threads[n].comments.len() {
        let _ = write!(out, "<w:commentRangeStart w:id=\"{id}\"/>");
    }
}

/// A thread's range ending here, each comment's reference after it.
pub(super) fn ends(threads: &[CommentThread], first: &[usize], n: usize, out: &mut String) {
    for id in first[n]..first[n] + threads[n].comments.len() {
        let _ = write!(
            out,
            "<w:commentRangeEnd w:id=\"{id}\"/><w:r><w:rPr><w:rStyle w:val=\"CommentReference\"/></w:rPr><w:commentReference w:id=\"{id}\"/></w:r>"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_initials_say_no_more_than_given() {
        let c = |email: &str, name: Option<&str>| Comment { author_email: email.into(), author_name: name.map(Into::into), ..Comment::default() };
        assert_eq!(shown_name(&c("priya.nair@demo.test", Some("Priya Nair"))), "Priya Nair");
        assert_eq!(shown_name(&c("priya.nair@demo.test", None)), "priya.nair", "the local part, never the domain");
        assert_eq!(shown_name(&c("priya.nair@demo.test", Some("  "))), "priya.nair");
        assert_eq!(shown_name(&c("", None)), "Someone");
        assert_eq!(initials("Priya Nair"), "PN");
        assert_eq!(initials("priya.nair"), "PN");
        assert_eq!(initials("omar_al-haddad x y"), "OAH");
        assert_eq!(removed_words(1), "1 comment whose text was removed");
        assert_eq!(removed_words(3), "3 comments whose text was removed");
    }
}
