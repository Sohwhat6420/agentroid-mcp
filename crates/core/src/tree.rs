//! UI-tree parser for `uiautomator dump` XML (task 2.2, item 5).
//!
//! The parser is std-only and borrows from the input: every label is a `Cow<'a, str>` that stays
//! `Borrowed` unless the text held an XML entity. Unlabelled clickables get a *borrowed label*
//! (a [`Hint`]) from a child or a sibling, the way phone-hands does for Compose screens.

use std::borrow::Cow;
use std::fmt;

/// `[l,t][r,b]` in device pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub l: i32,
    pub t: i32,
    pub r: i32,
    pub b: i32,
}

impl Bounds {
    pub fn centre(&self) -> (i32, i32) {
        (
            (self.l + self.r).div_euclid(2),
            (self.t + self.b).div_euclid(2),
        )
    }
    pub fn width(&self) -> i32 {
        self.r - self.l
    }
    pub fn height(&self) -> i32 {
        self.b - self.t
    }
    pub fn area(&self) -> i64 {
        i64::from(self.width()) * i64::from(self.height())
    }
    pub fn overlaps(&self, o: &Bounds) -> bool {
        self.l < o.r && o.l < self.r && self.t < o.b && o.t < self.b
    }
    /// Squared distance from a point to the rectangle (0 when inside; the last pixel is `r - 1`).
    pub fn dist2_to(&self, x: i32, y: i32) -> i64 {
        let dx = (self.l - x).max(x - (self.r - 1)).max(0);
        let dy = (self.t - y).max(y - (self.b - 1)).max(0);
        i64::from(dx) * i64::from(dx) + i64::from(dy) * i64::from(dy)
    }
    fn parse(s: &str) -> Option<Bounds> {
        // "[l,t][r,b]"
        let s = s.strip_prefix('[')?;
        let (a, rest) = s.split_once("][")?;
        let rest = rest.strip_suffix(']')?;
        let (l, t) = a.split_once(',')?;
        let (r, b) = rest.split_once(',')?;
        Some(Bounds {
            l: l.trim().parse().ok()?,
            t: t.trim().parse().ok()?,
            r: r.trim().parse().ok()?,
            b: b.trim().parse().ok()?,
        })
    }
}

/// Boolean node attributes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    pub clickable: bool,
    pub long_clickable: bool,
    pub scrollable: bool,
    pub checkable: bool,
    pub focused: bool,
    pub selected: bool,
    pub checked: bool,
    pub disabled: bool,
    pub password: bool,
}

impl Flags {
    pub fn interactive(&self) -> bool {
        self.clickable || self.long_clickable || self.scrollable || self.checkable
    }
    /// Flag names in phone-hands' order.
    pub fn names(&self) -> Vec<&'static str> {
        let all = [
            (self.clickable, "clickable"),
            (self.long_clickable, "long-clickable"),
            (self.scrollable, "scrollable"),
            (self.checkable, "checkable"),
            (self.focused, "focused"),
            (self.selected, "selected"),
            (self.checked, "checked"),
            (self.disabled, "disabled"),
            (self.password, "password"),
        ];
        all.iter().filter(|(on, _)| *on).map(|(_, n)| *n).collect()
    }
}

/// Where a borrowed label came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintSrc {
    Child,
    Sibling,
}

impl HintSrc {
    pub fn as_str(&self) -> &'static str {
        match self {
            HintSrc::Child => "child",
            HintSrc::Sibling => "sibling",
        }
    }
}

/// A label lent to an unlabelled clickable by its subtree or a sibling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint<'a> {
    pub text: Cow<'a, str>,
    pub src: HintSrc,
}

/// One `<node>` of the dump, kept in an arena so parent and sibling lookups are cheap.
#[derive(Debug, Clone)]
pub struct Node<'a> {
    pub class: Cow<'a, str>,
    pub text: Cow<'a, str>,
    pub desc: Cow<'a, str>,
    pub rid: Cow<'a, str>,
    pub package: Cow<'a, str>,
    pub bounds: Option<Bounds>,
    pub flags: Flags,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
}

/// A node that survived the filter, with its stable index (what `tap(index=N)` means).
#[derive(Debug, Clone)]
pub struct Element<'a> {
    pub index: usize,
    /// Position in [`Tree::nodes`].
    pub node: usize,
    /// Short class name (`Button`).
    pub class: String,
    pub text: Cow<'a, str>,
    pub desc: Cow<'a, str>,
    /// Full resource id, `pkg:id/name`.
    pub rid: Cow<'a, str>,
    pub bounds: Bounds,
    pub flags: Flags,
    pub hint: Option<Hint<'a>>,
}

impl Element<'_> {
    /// The id without its package part.
    pub fn short_id(&self) -> &str {
        self.rid.rsplit('/').next().unwrap_or("")
    }
    pub fn centre(&self) -> (i32, i32) {
        self.bounds.centre()
    }
    /// Own text, else desc, else borrowed hint, else id.
    pub fn label(&self) -> &str {
        if !self.text.is_empty() {
            &self.text
        } else if !self.desc.is_empty() {
            &self.desc
        } else if let Some(h) = &self.hint {
            &h.text
        } else {
            self.short_id()
        }
    }
    /// phone-hands' one-line rendering.
    pub fn line(&self) -> String {
        let own: &str = if !self.text.is_empty() {
            &self.text
        } else if !self.desc.is_empty() {
            &self.desc
        } else if self.hint.is_some() {
            ""
        } else {
            self.short_id()
        };
        let mut bits = vec![format!("[{}] {}", self.index, self.class)];
        if !own.is_empty() {
            bits.push(format!("{:?}", trunc(own, 80)));
        } else if let Some(h) = &self.hint {
            bits.push(format!(
                "{:?} [label from {}]",
                trunc(&h.text, 80),
                h.src.as_str()
            ));
        }
        if !self.short_id().is_empty() && self.short_id() != own {
            bits.push(format!("id={}", self.short_id()));
        }
        if !self.text.is_empty() && !self.desc.is_empty() && self.desc != self.text {
            bits.push(format!("desc={:?}", trunc(&self.desc, 60)));
        }
        let f = self.flags.names();
        if !f.is_empty() {
            bits.push(f.join(","));
        }
        let (x, y) = self.centre();
        bits.push(format!("@({x},{y})"));
        bits.join(" ")
    }
}

fn trunc(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// The parsed dump.
#[derive(Debug, Clone)]
pub struct Tree<'a> {
    pub rotation: Option<u8>,
    pub nodes: Vec<Node<'a>>,
    pub elements: Vec<Element<'a>>,
}

impl Tree<'_> {
    pub fn element(&self, index: usize) -> Option<&Element<'_>> {
        self.elements.get(index)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// No `<hierarchy>` in the text (a sleeping screen gives "null root node" instead).
    NoHierarchy(String),
    Malformed(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::NoHierarchy(s) => write!(f, "no hierarchy in the dump: {s}"),
            ParseError::Malformed(s) => write!(f, "malformed dump: {s}"),
        }
    }
}

impl std::error::Error for ParseError {}

// ------------------------------------------------------------------ XML scanning

fn decode(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        match rest.find(';') {
            Some(j) if j <= 10 => {
                let ent = &rest[1..j];
                let ch = match ent {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    _ => ent
                        .strip_prefix("#x")
                        .or_else(|| ent.strip_prefix("#X"))
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                        .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                        .and_then(char::from_u32),
                };
                match ch {
                    Some(c) => {
                        out.push(c);
                        rest = &rest[j + 1..];
                    }
                    None => {
                        out.push('&');
                        rest = &rest[1..];
                    }
                }
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Attributes of one tag, values still undecoded and borrowed from the input.
fn attrs(tag: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let b = tag.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let ns = i;
        while i < b.len() && b[i] != b'=' && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let name = &tag[ns..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || (b[i] != b'"' && b[i] != b'\'') {
            continue;
        }
        let q = b[i];
        i += 1;
        let vs = i;
        while i < b.len() && b[i] != q {
            i += 1;
        }
        out.push((name, &tag[vs..i.min(b.len())]));
        i += 1;
    }
    out
}

/// End of the tag that starts at `from` (the index of its `>`), honouring quoted values.
fn tag_end(s: &str, from: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut q: Option<u8> = None;
    for (i, &c) in b.iter().enumerate().skip(from) {
        match q {
            Some(qc) if c == qc => q = None,
            Some(_) => {}
            None if c == b'"' || c == b'\'' => q = Some(c),
            None if c == b'>' => return Some(i),
            None => {}
        }
    }
    None
}

/// Parse a dump. Text before `<hierarchy` (`UI hierchary dumped to: /dev/tty`) and after
/// `</hierarchy>` is ignored. With `interactive_only`, plain labels are dropped too.
pub fn parse(xml: &str, interactive_only: bool) -> Result<Tree<'_>, ParseError> {
    let start = xml
        .find("<hierarchy")
        .ok_or_else(|| ParseError::NoHierarchy(trunc(xml, 300).to_string()))?;
    let end = xml
        .rfind("</hierarchy>")
        .ok_or_else(|| ParseError::NoHierarchy(trunc(xml, 300).to_string()))?;
    let body = &xml[start..end + "</hierarchy>".len()];

    let mut nodes: Vec<Node<'_>> = Vec::new();
    let mut stack: Vec<Option<usize>> = Vec::new(); // None = <hierarchy> or an unknown wrapper
    let mut rotation = None;
    let mut pos = 0;
    while let Some(off) = body[pos..].find('<') {
        let lt = pos + off;
        if body[lt..].starts_with("<!--") {
            let e = body[lt..]
                .find("-->")
                .ok_or_else(|| ParseError::Malformed("unterminated comment".into()))?;
            pos = lt + e + 3;
            continue;
        }
        let gt = tag_end(body, lt + 1)
            .ok_or_else(|| ParseError::Malformed("unterminated tag".into()))?;
        let inner = &body[lt + 1..gt];
        pos = gt + 1;
        if inner.starts_with('?') || inner.starts_with('!') {
            continue;
        }
        if inner.starts_with('/') {
            if stack.pop().is_none() {
                return Err(ParseError::Malformed("unbalanced closing tag".into()));
            }
            continue;
        }
        let self_closing = inner.ends_with('/');
        let name_end = inner
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(inner.len());
        let name = &inner[..name_end];
        let attr_list = attrs(&inner[name_end..]);
        if name == "node" {
            let get = |k: &str| {
                attr_list
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| decode(v))
            };
            let flag = |k: &str| get(k).is_some_and(|v| v == "true");
            let idx = nodes.len();
            let parent = stack.iter().rev().find_map(|p| *p);
            nodes.push(Node {
                class: get("class").unwrap_or(Cow::Borrowed("")),
                text: get("text").unwrap_or(Cow::Borrowed("")),
                desc: get("content-desc").unwrap_or(Cow::Borrowed("")),
                rid: get("resource-id").unwrap_or(Cow::Borrowed("")),
                package: get("package").unwrap_or(Cow::Borrowed("")),
                bounds: get("bounds").and_then(|b| Bounds::parse(&b)),
                flags: Flags {
                    clickable: flag("clickable"),
                    long_clickable: flag("long-clickable"),
                    scrollable: flag("scrollable"),
                    checkable: flag("checkable"),
                    focused: flag("focused"),
                    selected: flag("selected"),
                    checked: flag("checked"),
                    disabled: get("enabled").is_some_and(|v| v == "false"),
                    password: flag("password"),
                },
                parent,
                children: Vec::new(),
            });
            if let Some(p) = parent {
                nodes[p].children.push(idx);
            }
            if !self_closing {
                stack.push(Some(idx));
            }
        } else if name == "hierarchy" {
            rotation = attr_list
                .iter()
                .find(|(n, _)| *n == "rotation")
                .and_then(|(_, v)| v.parse().ok());
            if !self_closing {
                stack.push(None);
            }
        } else if !self_closing {
            stack.push(None); // unknown wrapper element: keep balance
        }
    }
    if !stack.is_empty() {
        return Err(ParseError::Malformed("unclosed element".into()));
    }

    let mut elements: Vec<Element<'_>> = Vec::new();
    for (i, n) in nodes.iter().enumerate() {
        let Some(bounds) = n.bounds else { continue };
        if bounds.r <= bounds.l || bounds.b <= bounds.t {
            continue;
        }
        let interactive = n.flags.interactive();
        let labelled = !n.text.is_empty() || !n.desc.is_empty();
        if !interactive && !labelled {
            continue;
        }
        if interactive_only && !interactive {
            continue;
        }
        let hint = if interactive && !labelled {
            borrowed_label(&nodes, i)
        } else {
            None
        };
        elements.push(Element {
            index: elements.len(),
            node: i,
            class: n.class.rsplit('.').next().unwrap_or("").to_string(),
            text: n.text.clone(),
            desc: n.desc.clone(),
            rid: n.rid.clone(),
            bounds,
            flags: n.flags,
            hint,
        });
    }
    Ok(Tree {
        rotation,
        nodes,
        elements,
    })
}

fn own_label<'a>(n: &Node<'a>) -> Option<Cow<'a, str>> {
    let raw = if !n.text.is_empty() { &n.text } else { &n.desc };
    let t = match raw {
        Cow::Borrowed(s) => Cow::Borrowed(s.trim()),
        Cow::Owned(s) => Cow::Owned(s.trim().to_string()),
    };
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// Up to `limit` distinct labels in the subtree below `at` (not `at` itself), document order.
fn subtree_labels<'a>(nodes: &[Node<'a>], at: usize, limit: usize) -> Vec<Cow<'a, str>> {
    let mut out: Vec<Cow<'a, str>> = Vec::new();
    let mut stack: Vec<usize> = nodes[at].children.iter().rev().copied().collect();
    while let Some(i) = stack.pop() {
        if let Some(l) = own_label(&nodes[i]) {
            if !out.contains(&l) {
                out.push(l);
                if out.len() >= limit {
                    break;
                }
            }
        }
        stack.extend(nodes[i].children.iter().rev().copied());
    }
    out
}

fn join(labels: Vec<Cow<'_, str>>) -> Cow<'_, str> {
    if labels.len() == 1 {
        labels.into_iter().next().unwrap_or(Cow::Borrowed(""))
    } else {
        Cow::Owned(
            labels
                .iter()
                .map(|c| c.as_ref())
                .collect::<Vec<_>>()
                .join(" / "),
        )
    }
}

/// A label for a clickable with no text or desc: its own subtree first, then the siblings drawn over
/// it (or, failing that, the nearest labelled non-clickable sibling).
fn borrowed_label<'a>(nodes: &[Node<'a>], at: usize) -> Option<Hint<'a>> {
    let kids = subtree_labels(nodes, at, 3);
    if !kids.is_empty() {
        return Some(Hint {
            text: join(kids),
            src: HintSrc::Child,
        });
    }
    let parent = nodes[at].parent?;
    let siblings = &nodes[parent].children;
    let me = siblings.iter().position(|&s| s == at)?;
    let mut cands: Vec<(u8, usize, Vec<Cow<'a, str>>)> = Vec::new();
    for (i, &s) in siblings.iter().enumerate() {
        if s == at || nodes[s].flags.clickable {
            continue;
        }
        let mut labs: Vec<Cow<'a, str>> = own_label(&nodes[s]).into_iter().collect();
        labs.extend(subtree_labels(nodes, s, 2));
        if labs.is_empty() {
            continue;
        }
        let over = match (nodes[s].bounds, nodes[at].bounds) {
            (Some(a), Some(b)) => a.overlaps(&b),
            _ => false,
        };
        cands.push((if over { 0 } else { 1 }, i.abs_diff(me), labs));
    }
    if cands.is_empty() {
        return None;
    }
    cands.sort_by_key(|c| (c.0, c.1)); // stable, like Python's sort
    let chosen: Vec<_> = if cands[0].0 == 0 {
        cands.iter().filter(|c| c.0 == 0).collect()
    } else {
        vec![&cands[0]]
    };
    let mut labs: Vec<Cow<'a, str>> = Vec::new();
    for c in chosen {
        for l in &c.2 {
            if !labs.contains(l) {
                labs.push(l.clone());
            }
        }
    }
    labs.truncate(3);
    Some(Hint {
        text: join(labs),
        src: HintSrc::Sibling,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = include_str!("../fixtures/settings_dump.xml");
    const COMPOSE: &str = include_str!("../fixtures/compose_dump.xml");
    const IDONLY: &str = include_str!("../fixtures/idonly_dump.xml");

    #[test]
    fn realistic_dump_parses() {
        let t = parse(DUMP, false).unwrap();
        assert_eq!(t.rotation, Some(0));
        for (i, e) in t.elements.iter().enumerate() {
            assert_eq!(e.index, i);
        }
        let title = t.elements.iter().find(|e| e.text == "Settings").unwrap();
        assert_eq!(title.class, "TextView");
        assert_eq!(title.short_id(), "title");
        assert_eq!(
            title.bounds,
            Bounds {
                l: 48,
                t: 200,
                r: 400,
                b: 280
            }
        );
        let wifi = t.elements.iter().find(|e| e.desc == "Wi-Fi").unwrap();
        assert!(wifi.flags.clickable);
        assert_eq!(wifi.rid, "com.android.settings:id/wifi");
        let sw = t
            .elements
            .iter()
            .find(|e| e.short_id() == "switch_widget")
            .unwrap();
        assert!(sw.flags.checked && sw.flags.checkable);
        assert_eq!(sw.centre(), (1090, 480));
        assert!(sw.line().contains("checkable,focused") || sw.line().contains("checkable,checked"));
        let search = t.elements.iter().find(|e| e.desc == "Search").unwrap();
        assert!(search.flags.focused);
        assert!(!search.flags.password);
        let off = t.elements.iter().find(|e| e.text == "Off").unwrap();
        assert!(off.flags.disabled);
        // Zero-area and empty non-interactive nodes are dropped.
        assert!(t.elements.iter().all(|e| e.bounds.area() > 0));
        assert!(t.elements.iter().all(|e| e.text != "ghost"));
        assert!(t.elements.len() < t.nodes.len());
    }

    #[test]
    fn entities_decode_and_plain_labels_stay_borrowed() {
        let t = parse(COMPOSE, false).unwrap();
        let dev = t
            .elements
            .iter()
            .find(|e| e.text.starts_with("Xingchen"))
            .unwrap();
        assert_eq!(dev.text, "Xingchen & Rikka");
        assert!(matches!(dev.text, Cow::Owned(_)));
        let inst = t.elements.iter().find(|e| e.text == "Install").unwrap();
        match &inst.text {
            Cow::Borrowed(s) => {
                let p = s.as_ptr() as usize;
                let base = COMPOSE.as_ptr() as usize;
                assert!(p >= base && p < base + COMPOSE.len());
            }
            Cow::Owned(_) => panic!("label was copied"),
        }
        assert_eq!(
            decode("a&#233;&#x6771;&lt;&bogus;"),
            "a\u{e9}\u{6771}<&bogus;"
        );
    }

    #[test]
    fn borrowed_labels() {
        let t = parse(COMPOSE, false).unwrap();
        let card = t
            .elements
            .iter()
            .find(|e| {
                e.bounds
                    == Bounds {
                        l: 0,
                        t: 300,
                        r: 1220,
                        b: 600,
                    }
                    && e.flags.clickable
            })
            .unwrap();
        let h = card.hint.as_ref().unwrap();
        assert_eq!(
            (h.text.as_ref(), h.src),
            ("Shizuku / Xingchen & Rikka", HintSrc::Sibling)
        );
        let tab = t
            .elements
            .iter()
            .find(|e| e.bounds.l == 600 && e.bounds.t == 2500)
            .unwrap();
        let h = tab.hint.as_ref().unwrap();
        assert_eq!((h.text.as_ref(), h.src), ("Search", HintSrc::Child));
        let inst = t.elements.iter().find(|e| e.text == "Install").unwrap();
        assert!(inst.hint.is_none());
        assert!(inst.line().contains("\"Install\""));
        let idonly = parse(IDONLY, false).unwrap();
        let me = &idonly.elements[0];
        assert_eq!(me.hint.as_ref().unwrap().text, "ME");
        assert!(me.line().contains("id=navBarMe"));
        assert!(me.line().contains("[label from child]"));
    }

    #[test]
    fn interactive_only_drops_plain_labels() {
        let all = parse(DUMP, false).unwrap().elements.len();
        let some = parse(DUMP, true).unwrap();
        assert!(some.elements.len() < all);
        assert!(some.elements.iter().all(|e| e.flags.interactive()));
    }

    #[test]
    fn errors() {
        assert!(matches!(
            parse(
                "ERROR: null root node returned by UiTestAutomationBridge.",
                false
            ),
            Err(ParseError::NoHierarchy(_))
        ));
        assert!(matches!(
            parse("<hierarchy><node bounds=\"[0,0][1,1]\"></hierarchy>", false),
            Err(ParseError::Malformed(_))
        ));
    }

    #[test]
    fn bounds_helpers() {
        let b = Bounds::parse("[10,20][30,60]").unwrap();
        assert_eq!(b.centre(), (20, 40));
        assert_eq!(b.dist2_to(20, 40), 0);
        assert_eq!(b.dist2_to(40, 40), 121); // the last pixel column is 29
        assert!(Bounds::parse("garbage").is_none());
    }
}
