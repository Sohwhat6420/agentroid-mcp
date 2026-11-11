//! The selector matcher (task 2.2, item 1): one matcher, shared by live tools and replay.
//!
//! A [`Selector`] may carry several keys. They are tried in a fixed order, `ref` (a stable ref from
//! [`crate::refs`]), `id`, `text`, `desc`, `near_text`, and relative coordinates last; the first key that matches anything decides. The
//! answer is a [`Resolution`] that discloses which key matched, whether the match was unique,
//! disambiguated (and by what rule) or ambiguous, and every alternative that was passed over.

use crate::tree::{Bounds, Element, Tree};
use std::fmt;

/// The six selector keys, in matching order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Ref,
    Id,
    Text,
    Desc,
    NearText,
    Rel,
}

impl Kind {
    pub const ORDER: [Kind; 6] = [
        Kind::Ref,
        Kind::Id,
        Kind::Text,
        Kind::Desc,
        Kind::NearText,
        Kind::Rel,
    ];
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Ref => "ref",
            Kind::Id => "id",
            Kind::Text => "text",
            Kind::Desc => "desc",
            Kind::NearText => "near_text",
            Kind::Rel => "rel",
        }
    }
}

/// What to look for. Any subset of keys may be set; [`resolve`] tries them in [`Kind::ORDER`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Selector {
    /// A stable ref (`r3fa91c`, see [`crate::refs`]): names one element of the latest tree.
    pub ref_id: Option<String>,
    /// `resource-id`, either the full `pkg:id/name` or the short `name`.
    pub id: Option<String>,
    /// The element's own text (or a label borrowed for an unlabelled clickable).
    pub text: Option<String>,
    /// `content-desc`.
    pub desc: Option<String>,
    /// The interactive element nearest to the element showing this text.
    pub near_text: Option<String>,
    /// A point as fractions `(0..=1, 0..=1)` of the frame. Last resort; matches no element.
    pub rel: Option<(f64, f64)>,
}

impl Selector {
    pub fn ref_id(v: &str) -> Self {
        Selector {
            ref_id: Some(v.into()),
            ..Default::default()
        }
    }
    pub fn id(v: &str) -> Self {
        Selector {
            id: Some(v.into()),
            ..Default::default()
        }
    }
    pub fn text(v: &str) -> Self {
        Selector {
            text: Some(v.into()),
            ..Default::default()
        }
    }
    pub fn desc(v: &str) -> Self {
        Selector {
            desc: Some(v.into()),
            ..Default::default()
        }
    }
    pub fn near_text(v: &str) -> Self {
        Selector {
            near_text: Some(v.into()),
            ..Default::default()
        }
    }
    pub fn rel(fx: f64, fy: f64) -> Self {
        Selector {
            rel: Some((fx, fy)),
            ..Default::default()
        }
    }
    pub fn is_empty(&self) -> bool {
        self.ref_id.is_none()
            && self.id.is_none()
            && self.text.is_none()
            && self.desc.is_none()
            && self.near_text.is_none()
            && self.rel.is_none()
    }
    /// The keys that are set, in matching order, with their values rendered.
    pub fn keys(&self) -> Vec<(Kind, String)> {
        let mut v = Vec::new();
        if let Some(s) = &self.ref_id {
            v.push((Kind::Ref, s.clone()));
        }
        if let Some(s) = &self.id {
            v.push((Kind::Id, s.clone()));
        }
        if let Some(s) = &self.text {
            v.push((Kind::Text, s.clone()));
        }
        if let Some(s) = &self.desc {
            v.push((Kind::Desc, s.clone()));
        }
        if let Some(s) = &self.near_text {
            v.push((Kind::NearText, s.clone()));
        }
        if let Some((x, y)) = self.rel {
            v.push((Kind::Rel, format!("{x},{y}")));
        }
        v
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self
            .keys()
            .into_iter()
            .map(|(k, v)| format!("{}={:?}", k.as_str(), v))
            .collect();
        write!(f, "{}", parts.join(" | "))
    }
}

/// How sure the matcher is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Exactly one element matched.
    Unique,
    /// Several matched and one was strictly better; the string says by which rule.
    Disambiguated(String),
    /// Several matched and none was strictly better. Nothing was chosen; callers must refuse or ask.
    Ambiguous,
}

/// A candidate element, kept after the tree is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alt {
    pub index: usize,
    pub label: String,
    pub bounds: Bounds,
    /// How it matched ("exact text", "text contains", "label borrowed from child", ...).
    pub how: String,
}

/// What was picked.
#[derive(Debug, Clone, PartialEq)]
pub enum Chosen {
    Element(Alt),
    /// Relative coordinates; resolve them against a frame with [`crate::frame::Frame::map_rel`].
    Point {
        fx: f64,
        fy: f64,
    },
}

/// A key that was tried and found nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Miss {
    pub kind: Kind,
    pub value: String,
}

/// The disclosure that every resolution carries.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    /// The key that decided the match.
    pub matched_by: Kind,
    pub matched_value: String,
    pub status: Status,
    /// `None` only when `status` is [`Status::Ambiguous`].
    pub chosen: Option<Chosen>,
    /// Every other candidate (for `Ambiguous`, all of them), best first.
    pub alternatives: Vec<Alt>,
    /// Earlier keys that found nothing before `matched_by` did.
    pub tried: Vec<Miss>,
}

impl Resolution {
    pub fn element_index(&self) -> Option<usize> {
        match &self.chosen {
            Some(Chosen::Element(a)) => Some(a.index),
            _ => None,
        }
    }
    /// One line for a tool reply.
    pub fn describe(&self) -> String {
        let mut s = format!(
            "matched by {}={:?}",
            self.matched_by.as_str(),
            self.matched_value
        );
        match (&self.status, &self.chosen) {
            (Status::Unique, Some(Chosen::Element(a))) => {
                s += &format!(": unique, [{}] {:?}", a.index, a.label)
            }
            (Status::Disambiguated(why), Some(Chosen::Element(a))) => {
                s += &format!(
                    ": [{}] {:?} chosen over {} other(s) ({why})",
                    a.index,
                    a.label,
                    self.alternatives.len()
                );
            }
            (Status::Ambiguous, _) => {
                s += &format!(
                    ": AMBIGUOUS, {} candidates, nothing chosen",
                    self.alternatives.len()
                )
            }
            (_, Some(Chosen::Point { fx, fy })) => {
                s += &format!(": relative point ({fx:.3},{fy:.3})")
            }
            _ => {}
        }
        if !self.tried.is_empty() {
            let t: Vec<String> = self
                .tried
                .iter()
                .map(|m| format!("{}={:?}", m.kind.as_str(), m.value))
                .collect();
            s += &format!("; earlier keys with no match: {}", t.join(", "));
        }
        s
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolveError {
    /// The selector has no key at all.
    Empty,
    /// `rel` outside 0..=1 or not finite.
    BadRel(f64, f64),
    /// Nothing matched; lists every key tried.
    NoMatch(Vec<Miss>),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::Empty => write!(f, "selector has no id, text, desc, near_text or rel"),
            ResolveError::BadRel(x, y) => {
                write!(f, "rel ({x},{y}) must be two fractions between 0 and 1")
            }
            ResolveError::NoMatch(m) => {
                let t: Vec<String> = m
                    .iter()
                    .map(|m| format!("{}={:?}", m.kind.as_str(), m.value))
                    .collect();
                write!(f, "no element matched: {}", t.join(", "))
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Ranking key: lower is better. `(quality, not_interactive, disabled, distance, area)`.
type Key = (u8, u8, u8, i64, i64);

struct Cand {
    index: usize,
    key: Key,
    how: String,
}

/// 0 exact, 1 equal ignoring case, 2 contains ignoring case.
fn quality(hay: &str, needle: &str) -> Option<u8> {
    let (h, n) = (hay.trim(), needle.trim());
    if n.is_empty() || h.is_empty() {
        return None;
    }
    if h == n {
        return Some(0);
    }
    let (hl, nl) = (h.to_lowercase(), n.to_lowercase());
    if hl == nl {
        Some(1)
    } else if hl.contains(&nl) {
        Some(2)
    } else {
        None
    }
}

const QUALITY_NAMES: [&str; 3] = ["exact", "case-insensitive", "contains"];

fn base_key(e: &Element<'_>, q: u8, dist: i64, area: i64) -> Key {
    (
        q,
        u8::from(!e.flags.interactive()),
        u8::from(e.flags.disabled),
        dist,
        area,
    )
}

fn by_ref(tree: &Tree<'_>, v: &str) -> Vec<Cand> {
    match crate::refs::find(tree, v) {
        Some(i) => vec![Cand {
            index: i,
            key: base_key(&tree.elements[i], 0, 0, 0),
            how: "ref".into(),
        }],
        None => vec![],
    }
}

fn by_id(tree: &Tree<'_>, v: &str) -> Vec<Cand> {
    let v = v.trim();
    tree.elements
        .iter()
        .filter(|e| !v.is_empty() && (e.rid == v || e.short_id() == v))
        .map(|e| Cand {
            index: e.index,
            key: base_key(e, 0, 0, 0),
            how: "id".into(),
        })
        .collect()
}

/// Own text (or desc, per `use_desc`) first; for text, a borrowed label counts at a lower rank.
fn by_label(tree: &Tree<'_>, v: &str, use_desc: bool) -> Vec<Cand> {
    let mut out = Vec::new();
    for e in &tree.elements {
        let own: &str = if use_desc { &e.desc } else { &e.text };
        if let Some(q) = quality(own, v) {
            let what = if use_desc { "desc" } else { "text" };
            out.push(Cand {
                index: e.index,
                key: base_key(e, q, 0, 0),
                how: format!("{} {what}", QUALITY_NAMES[q as usize]),
            });
        } else if !use_desc && own.is_empty() {
            if let Some(h) = &e.hint {
                if let Some(q) = quality(&h.text, v) {
                    out.push(Cand {
                        index: e.index,
                        key: base_key(e, 3 + q, 0, 0),
                        how: format!(
                            "{} label borrowed from {}",
                            QUALITY_NAMES[q as usize],
                            h.src.as_str()
                        ),
                    });
                }
            }
        }
    }
    out
}

/// For every element showing `v`, the interactive element nearest to it; one candidate per target.
fn by_near(tree: &Tree<'_>, v: &str) -> Vec<Cand> {
    let mut best: Vec<Cand> = Vec::new();
    for anchor in &tree.elements {
        let q = [
            quality(&anchor.text, v),
            quality(&anchor.desc, v),
            anchor
                .hint
                .as_ref()
                .and_then(|h| quality(&h.text, v))
                .map(|q| q + 3),
        ]
        .into_iter()
        .flatten()
        .min();
        let Some(q) = q else { continue };
        let (ax, ay) = anchor.centre();
        let nearest = tree
            .elements
            .iter()
            .filter(|e| e.flags.interactive())
            .min_by_key(|e| {
                (
                    u8::from(e.flags.disabled),
                    e.bounds.dist2_to(ax, ay),
                    e.bounds.area(),
                    e.index,
                )
            });
        let Some(t) = nearest else { continue };
        let key = base_key(t, q.min(2), t.bounds.dist2_to(ax, ay), t.bounds.area());
        let how = format!(
            "nearest interactive element to {:?} (element [{}])",
            anchor.label(),
            anchor.index
        );
        match best.iter_mut().find(|c| c.index == t.index) {
            Some(c) if c.key <= key => {}
            Some(c) => {
                c.key = key;
                c.how = how;
            }
            None => best.push(Cand {
                index: t.index,
                key,
                how,
            }),
        }
    }
    best
}

fn rule_between(a: &Key, b: &Key) -> &'static str {
    if a.0 != b.0 {
        "better match quality"
    } else if a.1 != b.1 {
        "clickable preferred over non-interactive"
    } else if a.2 != b.2 {
        "enabled preferred over disabled"
    } else if a.3 != b.3 {
        "nearest to the anchor text"
    } else {
        "smaller area (more specific)"
    }
}

fn to_alt(tree: &Tree<'_>, c: &Cand) -> Alt {
    let e = &tree.elements[c.index];
    Alt {
        index: e.index,
        label: e.label().to_string(),
        bounds: e.bounds,
        how: c.how.clone(),
    }
}

fn decide(
    tree: &Tree<'_>,
    kind: Kind,
    value: &str,
    mut cands: Vec<Cand>,
    tried: Vec<Miss>,
) -> Resolution {
    cands.sort_by_key(|c| (c.key, c.index));
    let alts: Vec<Alt> = cands.iter().map(|c| to_alt(tree, c)).collect();
    let (status, chosen, alternatives) = if cands.len() == 1 {
        (
            Status::Unique,
            Some(Chosen::Element(alts[0].clone())),
            vec![],
        )
    } else if cands[0].key < cands[1].key {
        let why = rule_between(&cands[0].key, &cands[1].key).to_string();
        (
            Status::Disambiguated(why),
            Some(Chosen::Element(alts[0].clone())),
            alts[1..].to_vec(),
        )
    } else {
        (Status::Ambiguous, None, alts)
    };
    Resolution {
        matched_by: kind,
        matched_value: value.to_string(),
        status,
        chosen,
        alternatives,
        tried,
    }
}

/// Resolve a selector against a tree. See the module docs for the order and the disclosure.
pub fn resolve(tree: &Tree<'_>, sel: &Selector) -> Result<Resolution, ResolveError> {
    if sel.is_empty() {
        return Err(ResolveError::Empty);
    }
    let mut tried: Vec<Miss> = Vec::new();
    for (kind, value) in sel.keys() {
        let cands = match kind {
            Kind::Ref => by_ref(tree, &value),
            Kind::Id => by_id(tree, &value),
            Kind::Text => by_label(tree, &value, false),
            Kind::Desc => by_label(tree, &value, true),
            Kind::NearText => by_near(tree, &value),
            Kind::Rel => {
                let (fx, fy) = sel.rel.unwrap_or((f64::NAN, f64::NAN));
                if !(fx.is_finite()
                    && fy.is_finite()
                    && (0.0..=1.0).contains(&fx)
                    && (0.0..=1.0).contains(&fy))
                {
                    return Err(ResolveError::BadRel(fx, fy));
                }
                return Ok(Resolution {
                    matched_by: Kind::Rel,
                    matched_value: value,
                    status: Status::Unique,
                    chosen: Some(Chosen::Point { fx, fy }),
                    alternatives: vec![],
                    tried,
                });
            }
        };
        if cands.is_empty() {
            tried.push(Miss { kind, value });
            if kind == Kind::Ref {
                // a ref names one exact element: when it is gone, a looser key must not pick another
                return Err(ResolveError::NoMatch(tried));
            }
        } else {
            return Ok(decide(tree, kind, &value, cands, tried));
        }
    }
    Err(ResolveError::NoMatch(tried))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::parse;

    const DUMP: &str = include_str!("../fixtures/settings_dump.xml");
    const COMPOSE: &str = include_str!("../fixtures/compose_dump.xml");

    #[test]
    fn a_ref_that_misses_does_not_fall_through_to_a_looser_key() {
        let t = parse(DUMP, false).unwrap();
        let refs = crate::refs::compute(&t);
        let wifi = t.elements.iter().position(|e| e.desc == "Wi-Fi").unwrap();
        // the ref is on screen: it decides, whatever the other keys say
        let mut s = Selector::ref_id(&refs[wifi]);
        s.text = Some("Battery & performance".into());
        let r = resolve(&t, &s).unwrap();
        assert_eq!((r.matched_by, r.element_index()), (Kind::Ref, Some(wifi)));
        // the ref is gone: the text must not pick some other element
        let mut gone = Selector::ref_id("r000000");
        gone.text = Some("Battery & performance".into());
        match resolve(&t, &gone) {
            Err(ResolveError::NoMatch(tried)) => {
                assert_eq!(tried.len(), 1, "only the ref was tried: {tried:?}")
            }
            other => panic!("expected NoMatch, got {other:?}"),
        }
    }

    #[test]
    fn unique_match_by_id() {
        let t = parse(DUMP, false).unwrap();
        let r = resolve(&t, &Selector::id("wifi")).unwrap();
        assert_eq!(r.matched_by, Kind::Id);
        assert_eq!(r.status, Status::Unique);
        assert!(r.alternatives.is_empty() && r.tried.is_empty());
        let idx = r.element_index().unwrap();
        assert_eq!(t.elements[idx].desc, "Wi-Fi");
        // the full id resolves to the same element
        let r2 = resolve(&t, &Selector::id("com.android.settings:id/wifi")).unwrap();
        assert_eq!(r2.element_index(), Some(idx));
        assert!(r.describe().contains("unique"));
    }

    #[test]
    fn disambiguated_match_lists_alternatives() {
        let t = parse(DUMP, false).unwrap();
        // "title" is the id of the Settings heading (static) and of the Battery row (clickable).
        let r = resolve(&t, &Selector::id("title")).unwrap();
        match &r.status {
            Status::Disambiguated(why) => assert!(why.contains("clickable"), "{why}"),
            s => panic!("expected disambiguated, got {s:?}"),
        }
        let chosen = &t.elements[r.element_index().unwrap()];
        assert_eq!(chosen.text, "Battery & performance");
        assert_eq!(r.alternatives.len(), 1);
        assert_eq!(r.alternatives[0].label, "Settings");
        assert!(r.describe().contains("chosen over 1 other"));
    }

    #[test]
    fn text_quality_orders_and_ties_are_ambiguous() {
        let t = parse(DUMP, false).unwrap();
        // The heading matches ignoring case; the unlabelled switch also *borrowed* "Settings" from a
        // sibling, which ranks lower, so the heading wins and the switch is disclosed.
        let r = resolve(&t, &Selector::text("settings")).unwrap();
        assert_eq!(t.elements[r.element_index().unwrap()].text, "Settings");
        assert_eq!(
            r.status,
            Status::Disambiguated("better match quality".into())
        );
        assert_eq!(r.alternatives.len(), 1);
        assert!(
            r.alternatives[0].how.contains("borrowed from sibling"),
            "{}",
            r.alternatives[0].how
        );
        let r = resolve(&t, &Selector::text("Bluetooth")).unwrap();
        assert_eq!(r.status, Status::Unique);
        // Two clickable, enabled, contains-matches tie, so nothing is chosen.
        let amb = r#"<hierarchy rotation="0"><node class="a.B" text="Open mail" clickable="true" bounds="[0,0][10,10]"/><node class="a.B" text="Open maps" clickable="true" bounds="[0,20][10,30]"/></hierarchy>"#;
        let t2 = parse(amb, false).unwrap();
        let r = resolve(&t2, &Selector::text("open")).unwrap();
        assert_eq!(r.status, Status::Ambiguous);
        assert!(r.chosen.is_none());
        assert_eq!(r.alternatives.len(), 2);
        // An exact match wins over a contains match.
        let amb3 = r#"<hierarchy rotation="0"><node class="a.B" text="Open" clickable="true" bounds="[0,0][10,10]"/><node class="a.B" text="Open mail" clickable="true" bounds="[0,20][10,30]"/></hierarchy>"#;
        let t3 = parse(amb3, false).unwrap();
        let r = resolve(&t3, &Selector::text("Open")).unwrap();
        assert_eq!(
            r.status,
            Status::Disambiguated("better match quality".into())
        );
        assert_eq!(t3.elements[r.element_index().unwrap()].text, "Open");
        assert_eq!(r.alternatives.len(), 1);
        assert!(r.alternatives[0].how.contains("contains"));
    }

    #[test]
    fn keys_are_tried_in_order_and_misses_disclosed() {
        let t = parse(DUMP, false).unwrap();
        let sel = Selector {
            id: Some("no_such_id".into()),
            text: Some("nothing here".into()),
            desc: Some("Search".into()),
            rel: Some((0.5, 0.5)),
            ..Default::default()
        };
        let r = resolve(&t, &sel).unwrap();
        assert_eq!(r.matched_by, Kind::Desc);
        assert_eq!(
            r.tried.iter().map(|m| m.kind).collect::<Vec<_>>(),
            vec![Kind::Id, Kind::Text]
        );
        assert!(r.describe().contains("earlier keys with no match"));
        // rel is last: used only when everything before it missed.
        let sel = Selector {
            id: Some("no_such_id".into()),
            rel: Some((0.25, 0.75)),
            ..Default::default()
        };
        let r = resolve(&t, &sel).unwrap();
        assert_eq!(r.matched_by, Kind::Rel);
        assert_eq!(r.chosen, Some(Chosen::Point { fx: 0.25, fy: 0.75 }));
        assert_eq!(r.tried.len(), 1);
        // ... and does not pre-empt an earlier key that matches.
        let sel = Selector {
            rel: Some((0.1, 0.1)),
            id: Some("wifi".into()),
            ..Default::default()
        };
        assert_eq!(resolve(&t, &sel).unwrap().matched_by, Kind::Id);
    }

    #[test]
    fn near_text_finds_the_neighbouring_control() {
        let t = parse(DUMP, false).unwrap();
        // "Off" is a disabled, non-clickable summary under the Bluetooth row, which contains it.
        let r = resolve(&t, &Selector::near_text("Off")).unwrap();
        let e = &t.elements[r.element_index().unwrap()];
        assert_eq!(e.text, "Bluetooth");
        assert_eq!(r.matched_by, Kind::NearText);
        // The Compose card has no label of its own; "Shizuku" sits inside it.
        let c = parse(COMPOSE, false).unwrap();
        let r = resolve(&c, &Selector::near_text("Shizuku")).unwrap();
        let e = &c.elements[r.element_index().unwrap()];
        assert_eq!(
            e.bounds,
            Bounds {
                l: 0,
                t: 300,
                r: 1220,
                b: 600
            }
        );
    }

    #[test]
    fn borrowed_labels_are_matched_and_say_so() {
        let c = parse(COMPOSE, false).unwrap();
        // The label's own element wins on quality; the clickable that borrowed it is disclosed.
        let r = resolve(&c, &Selector::text("Search")).unwrap();
        let Some(Chosen::Element(a)) = &r.chosen else {
            panic!()
        };
        assert!(a.how.contains("exact text"), "{}", a.how);
        assert_eq!(r.alternatives.len(), 1);
        assert!(
            r.alternatives[0].how.contains("borrowed from child"),
            "{}",
            r.alternatives[0].how
        );
        // With only the borrowed label in the tree (the label node has no area), it is the match.
        let lone = r#"<hierarchy rotation="0"><node class="a.V" clickable="true" bounds="[0,0][50,50]"><node class="a.T" text="Go" bounds="[0,0][0,0]"/></node></hierarchy>"#;
        let t = parse(lone, false).unwrap();
        let r = resolve(&t, &Selector::text("Go")).unwrap();
        let Some(Chosen::Element(a)) = &r.chosen else {
            panic!()
        };
        assert!(a.how.contains("borrowed from child"), "{}", a.how);
        assert_eq!(r.status, Status::Unique);
    }

    #[test]
    fn errors() {
        let t = parse(DUMP, false).unwrap();
        assert_eq!(resolve(&t, &Selector::default()), Err(ResolveError::Empty));
        assert!(matches!(
            resolve(&t, &Selector::rel(1.5, 0.0)),
            Err(ResolveError::BadRel(..))
        ));
        assert!(matches!(
            resolve(&t, &Selector::rel(f64::NAN, 0.0)),
            Err(ResolveError::BadRel(..))
        ));
        match resolve(&t, &Selector::text("zzz")) {
            Err(ResolveError::NoMatch(m)) => assert_eq!(m.len(), 1),
            other => panic!("{other:?}"),
        }
    }
}
