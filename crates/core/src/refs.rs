//! Stable refs (sprint 25 Oct, step 5): a short name for an element that survives a scroll or a
//! re-layout, so the model can say `tap(ref="r3fa91c")` after the list moved, and ask for only what
//! changed since a snapshot (`since`).
//!
//! A ref is a hash of the element's identity, never of where it sits on screen:
//! `(class, resource-id, own label, ancestor path, row label, occurrence)`. This is the content-identity
//! scheme MobileMCP uses, with one addition for repeated rows: an element inside a row of a list (an
//! ancestor with identical siblings) also hashes the row's first label, so the "Delete" button of the
//! row "Item 7" keeps its ref whichever other rows are on screen.
//!
//! * `class` is the short class name; `resource-id` is the full id.
//! * The own label is `text`, else `content-desc`, else the label borrowed for an unlabelled clickable.
//! * The ancestor path is the `(class, resource-id)` of every ancestor that has a resource id, root
//!   first (an anonymous wrapper layout is skipped). Bounds and tree indices are never hashed.
//! * The row label is the first label in the subtree of the nearest ancestor-or-self that has an
//!   identical sibling (same class and id).
//! * The occurrence number separates what is still identical, in document order.
//!
//! A ref is `r` plus six hex digits (24 bits), computed on the full parse of the dump (never the
//! `interactive_only` view). Twins hash their document-order number and how many twins there are, so a
//! twin never inherits the ref of one that scrolled off. If two different identities collide inside one
//! tree the ref is ambiguous and [`find`] refuses it: a ref names exactly one element of a tree or none.

use crate::tree::{Element, Tree};
use std::collections::{HashMap, HashSet};

/// How many snapshots a device keeps for `since`.
/// (A `require` or `verify` poll reads the tree too, and each read is a snapshot, so a few acting
/// calls must not push out the one the caller is holding.)
pub const KEEP_SNAPSHOTS: usize = 12;

fn fnv(parts: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for b in p.as_bytes().iter().chain(std::iter::once(&0x1f_u8)) {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

fn short_class(full: &str) -> &str {
    full.rsplit('.').next().unwrap_or(full)
}

fn own_label<'a>(e: &'a Element<'_>) -> &'a str {
    if !e.text.is_empty() {
        &e.text
    } else if !e.desc.is_empty() {
        &e.desc
    } else if let (Some(h), false) = (&e.hint, e.flags.scrollable) {
        // a scrollable container borrows whatever rows are visible: that changes on every scroll
        &h.text
    } else {
        ""
    }
}

fn node_label<'a>(n: &'a crate::tree::Node<'_>) -> &'a str {
    if !n.text.is_empty() {
        &n.text
    } else {
        &n.desc
    }
}

/// The first label in the subtree of `root` (pre-order, `root` included), or "".
fn first_label<'a>(tree: &'a Tree<'_>, root: usize) -> &'a str {
    let mut stack = vec![root];
    while let Some(i) = stack.pop() {
        let n = &tree.nodes[i];
        let l = node_label(n);
        if !l.is_empty() {
            return l;
        }
        stack.extend(n.children.iter().rev());
    }
    ""
}

/// The row that holds node `n`: the nearest ancestor-or-self with an identical sibling.
fn row_of(tree: &Tree<'_>, n: usize) -> Option<usize> {
    let mut cur = Some(n);
    while let Some(c) = cur {
        let node = &tree.nodes[c];
        if let Some(p) = node.parent {
            let twins = tree.nodes[p]
                .children
                .iter()
                .filter(|&&s| {
                    s != c && tree.nodes[s].class == node.class && tree.nodes[s].rid == node.rid
                })
                .count();
            if twins > 0 {
                return Some(c);
            }
        }
        cur = node.parent;
    }
    None
}

/// One ref per element of `tree`, in element order.
pub fn compute(tree: &Tree<'_>) -> Vec<String> {
    let mut row_labels: HashMap<usize, String> = HashMap::new();
    let mut bases: Vec<u64> = Vec::with_capacity(tree.elements.len());
    for e in &tree.elements {
        let mut path = String::new();
        let mut anc = tree.nodes[e.node].parent;
        let mut chain = Vec::new();
        while let Some(a) = anc {
            chain.push(a);
            anc = tree.nodes[a].parent;
        }
        for &a in chain.iter().rev() {
            let n = &tree.nodes[a];
            if n.rid.is_empty() {
                continue; // an anonymous wrapper is layout noise: adding or removing one must not change a ref
            }
            path.push_str(short_class(&n.class));
            path.push('#');
            path.push_str(&n.rid);
            path.push('/');
        }
        let row = match row_of(tree, e.node) {
            Some(r) => row_labels
                .entry(r)
                .or_insert_with(|| first_label(tree, r).to_string())
                .clone(),
            None => String::new(),
        };
        bases.push(fnv(&[
            short_class(&tree.nodes[e.node].class),
            &e.rid,
            own_label(e),
            &path,
            &row,
        ]));
    }
    // Twins (the same identity more than once) are told apart by document order AND by how many
    // there are: when one of a pair scrolls off, the other must not inherit its ref.
    let mut total: HashMap<u64, u32> = HashMap::new();
    for b in &bases {
        *total.entry(*b).or_insert(0) += 1;
    }
    let mut seen: HashMap<u64, u32> = HashMap::new();
    bases
        .iter()
        .map(|b| {
            let occ = seen.entry(*b).or_insert(0);
            let n = *occ;
            *occ += 1;
            let h = fnv(&[&format!("{b:016x}"), &n.to_string(), &total[b].to_string()]);
            format!("r{:06x}", h & 0xff_ffff)
        })
        .collect()
}

/// The index of the element carrying `want`. `None` when no element has it, and also when two
/// different elements of this tree share it (a 24-bit collision): a ref names exactly one element or none.
pub fn find(tree: &Tree<'_>, want: &str) -> Option<usize> {
    let want = want.trim();
    let refs = compute(tree);
    let mut hits = refs
        .iter()
        .enumerate()
        .filter(|(_, r)| r.as_str() == want)
        .map(|(i, _)| i);
    match (hits.next(), hits.next()) {
        (Some(i), None) => Some(i),
        _ => None,
    }
}

/// A hash of the screen's structure and content: every element's class, id, label and flags in order,
/// never where it sits. A tap that changed nothing leaves it as it was (`verify` fails on that); a
/// scroll, a toggled switch, a field that gained text or a new screen changes it.
pub fn fingerprint(tree: &Tree<'_>) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325_u64;
    for e in &tree.elements {
        let f = fnv(&[
            &e.class,
            &e.rid,
            &e.text,
            &e.desc,
            &e.flags.names().join(","),
        ]);
        h ^= f;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// What an element looks like to `since`: everything that is not its position.
fn signature(e: &Element<'_>) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}",
        e.class,
        e.rid,
        e.text,
        e.desc,
        own_label(e),
        e.flags.names().join(",")
    )
}

/// One element of a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub ref_id: String,
    pub sig: String,
    /// The tree listing line, without its trailing ref.
    pub line: String,
    pub interactive: bool,
}

/// The tree as it was at one read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub id: String,
    pub interactive_only: bool,
    pub rows: Vec<Row>,
}

impl Snapshot {
    /// `tree` is the view that was listed; `full` is the same dump parsed without `interactive_only`
    /// (or `None` when `tree` already is that). Refs are always computed on the full parse, so an
    /// element has the same ref in either view, and `tap(ref)` (which reads the full tree) finds it.
    pub fn of(
        id: String,
        interactive_only: bool,
        tree: &Tree<'_>,
        full: Option<&Tree<'_>>,
    ) -> Snapshot {
        let full = full.unwrap_or(tree);
        let by_node: HashMap<usize, String> = full
            .elements
            .iter()
            .map(|e| e.node)
            .zip(compute(full))
            .collect();
        let rows = tree
            .elements
            .iter()
            .map(|e| Row {
                ref_id: by_node.get(&e.node).cloned().unwrap_or_default(),
                sig: signature(e),
                line: e.line(),
                interactive: e.flags.interactive(),
            })
            .collect();
        Snapshot {
            id,
            interactive_only,
            rows,
        }
    }
}

/// A listing line: the element's line, then its ref when it can be a target (an interactive element).
pub fn line_with_ref(row: &Row) -> String {
    if row.interactive {
        format!("{} {}", row.line, row.ref_id)
    } else {
        row.line.clone()
    }
}

/// The full listing of a snapshot.
pub fn listing(s: &Snapshot) -> String {
    s.rows
        .iter()
        .map(line_with_ref)
        .collect::<Vec<_>>()
        .join("\n")
}

/// What changed between two snapshots, by ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub added: usize,
    pub changed: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub text: String,
}

/// `+` lines are new, `~` lines changed (same ref, different label or flags), `-` lines are gone.
/// An element that only moved on screen is unchanged and not listed. Lines carry the new tree's index,
/// so `tap(index)` works for what is listed; for anything else use the ref.
pub fn diff(old: &Snapshot, new: &Snapshot) -> Diff {
    let before: HashMap<&str, &Row> = old.rows.iter().map(|r| (r.ref_id.as_str(), r)).collect();
    let now: HashSet<&str> = new.rows.iter().map(|r| r.ref_id.as_str()).collect();
    let (mut added, mut changed, mut unchanged) = (0, 0, 0);
    let mut lines = Vec::new();
    for r in &new.rows {
        match before.get(r.ref_id.as_str()) {
            None => {
                added += 1;
                lines.push(format!("+ {} {}", r.line, r.ref_id));
            }
            Some(o) if o.sig != r.sig => {
                changed += 1;
                lines.push(format!("~ {} {}", r.line, r.ref_id));
            }
            Some(_) => unchanged += 1,
        }
    }
    let mut removed = 0;
    for r in &old.rows {
        if !now.contains(r.ref_id.as_str()) {
            removed += 1;
            let label = r
                .line
                .split_once("] ")
                .map_or(r.line.as_str(), |(_, rest)| rest);
            lines.push(format!(
                "- {} {}",
                label.split(" @(").next().unwrap_or(label),
                r.ref_id
            ));
        }
    }
    let head = format!("snapshot {} since {}: {added} added, {changed} changed, {removed} removed, {unchanged} unchanged", new.id, old.id);
    let text = if lines.is_empty() {
        head
    } else {
        format!("{head}\n{}", lines.join("\n"))
    };
    Diff {
        added,
        changed,
        removed,
        unchanged,
        text,
    }
}

/// A device's recent snapshots.
#[derive(Debug, Default, Clone)]
pub struct Snapshots {
    next: u32,
    kept: std::collections::VecDeque<Snapshot>,
}

impl Snapshots {
    /// Record a new snapshot of `tree`; returns its id (`s1`, `s2`, ...).
    pub fn push(
        &mut self,
        interactive_only: bool,
        tree: &Tree<'_>,
        full: Option<&Tree<'_>>,
    ) -> &Snapshot {
        self.next += 1;
        self.kept.push_back(Snapshot::of(
            format!("s{}", self.next),
            interactive_only,
            tree,
            full,
        ));
        while self.kept.len() > KEEP_SNAPSHOTS {
            self.kept.pop_front();
        }
        self.kept.back().expect("just pushed")
    }
    pub fn get(&self, id: &str) -> Option<&Snapshot> {
        self.kept.iter().find(|s| s.id == id.trim())
    }
    pub fn latest(&self) -> Option<&Snapshot> {
        self.kept.back()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::parse;

    const DUMP: &str = include_str!("../fixtures/settings_dump.xml");

    /// A list screen: a toolbar and `rows` rows from `first`, each a container with a title and a
    /// "Delete" button, laid out at `offset` pixels. `skew` adds a wrapper layout level.
    fn list(first: usize, rows: usize, offset: i32, skew: bool) -> String {
        let mut x = String::from("<hierarchy rotation=\"0\"><node class=\"android.widget.FrameLayout\" bounds=\"[0,0][1080,2400]\">");
        x += "<node class=\"android.widget.TextView\" text=\"Inbox\" resource-id=\"app:id/title\" bounds=\"[0,0][1080,150]\"/>";
        if skew {
            x += "<node class=\"android.widget.LinearLayout\" bounds=\"[0,150][1080,2400]\">";
        }
        x += "<node class=\"androidx.recyclerview.widget.RecyclerView\" resource-id=\"app:id/list\" scrollable=\"true\" bounds=\"[0,150][1080,2400]\">";
        for k in first..first + rows {
            let top = 150 + offset + ((k - first) as i32) * 200;
            x += &format!(
                "<node class=\"android.widget.LinearLayout\" resource-id=\"app:id/row\" clickable=\"true\" bounds=\"[0,{top}][1080,{}]\">\
                 <node class=\"android.widget.TextView\" text=\"Item {k}\" resource-id=\"app:id/name\" bounds=\"[10,{top}][700,{}]\"/>\
                 <node class=\"android.widget.Button\" text=\"Delete\" resource-id=\"app:id/del\" clickable=\"true\" bounds=\"[800,{top}][1000,{}]\"/>\
                 </node>",
                top + 190,
                top + 90,
                top + 90
            );
        }
        x += "</node>";
        if skew {
            x += "</node>";
        }
        x + "</node></hierarchy>"
    }

    fn ref_of(xml: &str, label: &str, class: &str, row: &str) -> Option<String> {
        let t = parse(xml, false).unwrap();
        let refs = compute(&t);
        // the element with this class and label inside the row titled `row`
        t.elements.iter().zip(refs).find_map(|(e, r)| {
            let in_row = {
                let mut n = Some(e.node);
                let mut hit = false;
                while let Some(i) = n {
                    let node = &t.nodes[i];
                    if node.children.iter().any(|&c| t.nodes[c].text == row) || node.text == row {
                        hit = true;
                        break;
                    }
                    n = node.parent;
                }
                hit
            };
            (e.class == class && e.text == label && in_row).then_some(r)
        })
    }

    #[test]
    fn a_ref_is_r_and_six_hex_digits_and_unique_in_a_tree() {
        let t = parse(DUMP, false).unwrap();
        let refs = compute(&t);
        assert_eq!(refs.len(), t.elements.len());
        for r in &refs {
            assert!(
                r.len() == 7 && r.starts_with('r') && r[1..].chars().all(|c| c.is_ascii_hexdigit()),
                "{r}"
            );
        }
        let set: HashSet<&String> = refs.iter().collect();
        assert_eq!(set.len(), refs.len(), "refs are unique inside one tree");
        // a second computation gives the same refs
        assert_eq!(refs, compute(&parse(DUMP, false).unwrap()));
    }

    #[test]
    fn find_returns_the_element_a_ref_names() {
        let t = parse(DUMP, false).unwrap();
        let refs = compute(&t);
        for (i, r) in refs.iter().enumerate() {
            assert_eq!(find(&t, r), Some(i));
        }
        assert_eq!(find(&t, "r000000"), None);
    }

    #[test]
    fn the_same_row_keeps_its_ref_across_200_scrolls_and_relayouts_with_a_wrapper_added_or_not() {
        // the reference screen: rows 10..=19 at offset 0
        let base = list(10, 10, 0, false);
        let tracked: Vec<usize> = (10..20).collect();
        let want: Vec<(String, String)> = tracked
            .iter()
            .map(|k| {
                (
                    ref_of(&base, "Delete", "Button", &format!("Item {k}")).unwrap(),
                    ref_of(
                        &base,
                        &format!("Item {k}"),
                        "TextView",
                        &format!("Item {k}"),
                    )
                    .unwrap(),
                )
            })
            .collect();
        // a small deterministic generator: no rand dependency, same layouts every run
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move |m: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % m
        };
        let (mut kept, mut total) = (0u32, 0u32);
        for _ in 0..200 {
            let first = 6 + next(8) as usize; // 6..=13: 4 rows scroll off one end, up to 3 new ones on
            let rows = 8 + next(5) as usize; // 8..=12 rows visible
            let offset = -(next(180) as i32); // partly scrolled
            let skew = next(2) == 1; // an extra layout level, as after a re-layout
            let xml = list(first, rows, offset, skew);
            for (n, k) in tracked.iter().enumerate() {
                if *k < first || *k >= first + rows {
                    continue; // scrolled off: nothing to compare
                }
                let del = ref_of(&xml, "Delete", "Button", &format!("Item {k}"));
                let name = ref_of(&xml, &format!("Item {k}"), "TextView", &format!("Item {k}"));
                total += 2;
                kept += u32::from(del.as_deref() == Some(want[n].0.as_str()))
                    + u32::from(name.as_deref() == Some(want[n].1.as_str()));
            }
        }
        assert!(total > 1000, "the fixture exercised {total} comparisons");
        let pct = f64::from(kept) / f64::from(total) * 100.0;
        eprintln!("stable refs: kept {kept} of {total} = {pct:.1}%");
        assert!(pct >= 95.0, "kept {kept} of {total} = {pct:.1}%");
    }

    #[test]
    fn scroll_and_offset_alone_keep_the_ref_in_at_least_95_percent_of_200_layouts() {
        let base = list(10, 10, 0, false);
        let tracked: Vec<usize> = (10..20).collect();
        let want: Vec<(String, String)> = tracked
            .iter()
            .map(|k| {
                (
                    ref_of(&base, "Delete", "Button", &format!("Item {k}")).unwrap(),
                    ref_of(
                        &base,
                        &format!("Item {k}"),
                        "TextView",
                        &format!("Item {k}"),
                    )
                    .unwrap(),
                )
            })
            .collect();
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move |m: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % m
        };
        let (mut kept, mut total) = (0u32, 0u32);
        for _ in 0..200 {
            let first = 6 + next(8) as usize;
            let rows = 8 + next(5) as usize;
            let offset = -(next(180) as i32);
            let xml = list(first, rows, offset, false);
            for (n, k) in tracked.iter().enumerate() {
                if *k < first || *k >= first + rows {
                    continue;
                }
                let del = ref_of(&xml, "Delete", "Button", &format!("Item {k}"));
                let name = ref_of(&xml, &format!("Item {k}"), "TextView", &format!("Item {k}"));
                total += 2;
                kept += u32::from(del.as_deref() == Some(want[n].0.as_str()))
                    + u32::from(name.as_deref() == Some(want[n].1.as_str()));
            }
        }
        let pct = f64::from(kept) / f64::from(total) * 100.0;
        eprintln!("stable refs (no wrapper): kept {kept} of {total} = {pct:.1}%");
        assert!(
            total > 1000 && pct >= 95.0,
            "kept {kept} of {total} = {pct:.1}%"
        );
    }

    #[test]
    fn rows_with_different_titles_get_different_refs_for_their_identical_buttons() {
        let xml = list(1, 3, 0, false);
        let t = parse(&xml, false).unwrap();
        let refs = compute(&t);
        let dels: Vec<&String> = t
            .elements
            .iter()
            .zip(&refs)
            .filter(|(e, _)| e.text == "Delete")
            .map(|(_, r)| r)
            .collect();
        assert_eq!(dels.len(), 3);
        assert!(
            dels[0] != dels[1] && dels[1] != dels[2] && dels[0] != dels[2],
            "{dels:?}"
        );
    }

    #[test]
    fn the_fingerprint_follows_content_and_flags_and_ignores_position() {
        let fp = |xml: &str| fingerprint(&parse(xml, false).unwrap());
        let a = list(1, 3, 0, false);
        assert_eq!(
            fp(&a),
            fp(&list(1, 3, -90, false)),
            "a move alone is not a change"
        );
        assert_ne!(
            fp(&a),
            fp(&list(2, 3, 0, false)),
            "a scroll that brings new rows is"
        );
        assert_ne!(
            fp(&a),
            fp(&a.replacen("text=\"Delete\"", "text=\"Delete\" checked=\"true\"", 1)),
            "a flag is"
        );
        assert_ne!(fp(&a), fp(&a.replace("Item 1", "Item one")), "a label is");
        // the order matters: swapping two rows is a change
        assert_ne!(
            fp(&list(1, 2, 0, false)),
            fp(&list(1, 2, 0, false)
                .replace("Item 1", "X")
                .replace("Item 2", "Item 1")
                .replace("X", "Item 2"))
        );
    }

    #[test]
    fn the_bounds_are_not_part_of_the_identity() {
        let a = compute(&parse(&list(1, 3, 0, false), false).unwrap());
        let b = compute(&parse(&list(1, 3, -90, false), false).unwrap());
        assert_eq!(a, b);
    }

    #[test]
    fn a_changed_label_is_a_new_identity() {
        let a = list(1, 2, 0, false);
        let b = a.replace("Item 1", "Item one");
        assert_ne!(
            compute(&parse(&a, false).unwrap()),
            compute(&parse(&b, false).unwrap())
        );
    }

    fn snap(id: &str, xml: &str) -> Snapshot {
        Snapshot::of(id.into(), false, &parse(xml, false).unwrap(), None)
    }

    #[test]
    fn a_since_diff_of_a_one_row_scroll_is_at_most_30_percent_of_the_full_tree() {
        let a = snap("s1", &list(1, 30, 0, false));
        let b = snap("s2", &list(2, 30, 0, false)); // scrolled one row: one row off the top, one on at the bottom
        let d = diff(&a, &b);
        let full = listing(&b);
        assert_eq!(
            (d.added, d.removed),
            (3, 3),
            "a row is a container, a title and a button: {}",
            d.text
        );
        assert_eq!(d.changed, 0);
        assert!(d.unchanged >= 90 - 3, "{}", d.text);
        let ratio = d.text.len() as f64 / full.len() as f64;
        assert!(
            ratio <= 0.30,
            "diff {} chars vs full {} chars = {:.0}%",
            d.text.len(),
            full.len(),
            ratio * 100.0
        );
    }

    #[test]
    fn a_since_diff_reports_a_changed_label_and_names_removed_elements() {
        let a = snap("s1", &list(1, 4, 0, false));
        let b = snap("s2", &list(1, 4, 0, false).replace("Item 2", "Item two"));
        let d = diff(&a, &b);
        assert!(d.text.starts_with("snapshot s2 since s1:"), "{}", d.text);
        // the title changed identity (its text is part of it) and so did its row: removed plus added
        assert!(d.added >= 1 && d.removed >= 1, "{}", d.text);
        assert!(
            d.text
                .lines()
                .any(|l| l.starts_with("- ") && l.contains("Item 2")),
            "{}",
            d.text
        );
        assert!(
            d.text
                .lines()
                .any(|l| l.starts_with("+ ") && l.contains("Item two")),
            "{}",
            d.text
        );
        // an identical screen is an all-unchanged diff with no body
        let same = diff(&a, &a);
        assert_eq!((same.added, same.changed, same.removed), (0, 0, 0));
        assert!(!same.text.contains('\n'), "{}", same.text);
    }

    #[test]
    fn a_flag_change_on_the_same_element_is_a_changed_row() {
        let a = snap("s1", &list(1, 2, 0, false));
        let b = snap(
            "s2",
            &list(1, 2, 0, false).replacen(
                "scrollable=\"true\"",
                "scrollable=\"true\" focused=\"true\"",
                1,
            ),
        );
        // the list container is not an element of its own (not labelled or interactive), so nothing is listed
        assert_eq!(diff(&a, &b).added, 0);
        let c = snap(
            "s2",
            &list(1, 2, 0, false).replacen(
                "text=\"Delete\"",
                "text=\"Delete\" checked=\"true\"",
                1,
            ),
        );
        let d = diff(&a, &c);
        assert_eq!((d.changed, d.added, d.removed), (1, 0, 0), "{}", d.text);
        assert!(
            d.text
                .lines()
                .any(|l| l.starts_with("~ ") && l.contains("checked")),
            "{}",
            d.text
        );
    }

    #[test]
    fn snapshots_are_kept_a_fixed_number_deep_and_found_by_id() {
        let t = parse(DUMP, false).unwrap();
        let mut s = Snapshots::default();
        let n = KEEP_SNAPSHOTS + 3;
        for _ in 0..n {
            s.push(false, &t, None);
        }
        assert!(
            s.get("s1").is_none() && s.get("s3").is_none(),
            "the oldest are dropped"
        );
        assert!(s.get("s4").is_some() && s.get(&format!("s{n}")).is_some());
        assert_eq!(
            s.latest().map(|x| x.id.as_str()),
            Some(format!("s{n}").as_str())
        );
        assert_eq!(s.kept.len(), KEEP_SNAPSHOTS);
    }

    /// Two buttons with exactly the same identity: same class, id, label, ancestors, no row.
    fn twins(both: bool) -> String {
        let one = "<node class=\"android.widget.Button\" text=\"Like\" resource-id=\"app:id/like\" clickable=\"true\" bounds=\"[0,{T}][500,{B}]\"/>";
        let a = one.replace("{T}", "0").replace("{B}", "100");
        let b = one.replace("{T}", "200").replace("{B}", "300");
        let body = if both { format!("{a}{b}") } else { b };
        format!("<hierarchy rotation=\"0\"><node class=\"android.widget.FrameLayout\" bounds=\"[0,0][1000,2000]\">{body}</node></hierarchy>")
    }

    #[test]
    fn a_twin_never_inherits_the_ref_of_the_twin_that_scrolled_off() {
        let (xml_before, xml_after) = (twins(true), twins(false));
        let before = parse(&xml_before, false).unwrap();
        let before_refs = compute(&before);
        assert_ne!(
            before_refs[0], before_refs[1],
            "twins are told apart while both are on screen"
        );
        // the first one scrolls off: the second is alone now
        let after = parse(&xml_after, false).unwrap();
        let after_refs = compute(&after);
        assert_eq!(after_refs.len(), 1);
        assert!(!before_refs.contains(&after_refs[0]), "the survivor has a new ref, so an old ref taps nothing: {before_refs:?} -> {after_refs:?}");
        assert_eq!(find(&after, &before_refs[0]), None);
        assert_eq!(find(&after, &before_refs[1]), None);
    }

    #[test]
    fn a_ref_shared_by_two_different_elements_is_refused_not_re_hashed() {
        // find two labels whose refs collide (24 bits: 20000 labels collide a dozen times over)
        let one = |label: &str| {
            format!("<hierarchy rotation=\"0\"><node class=\"android.widget.Button\" text=\"{label}\" clickable=\"true\" bounds=\"[0,0][10,10]\"/></hierarchy>")
        };
        let mut seen: HashMap<String, String> = HashMap::new();
        let mut pair = None;
        for i in 0..40_000 {
            let label = format!("L{i}");
            let r = compute(&parse(&one(&label), false).unwrap()).remove(0);
            if let Some(other) = seen.insert(r.clone(), label.clone()) {
                pair = Some((other, label, r));
                break;
            }
        }
        let (a, b, r) = pair.expect("a 24-bit collision among 40000 labels");
        let two = format!(
            "<hierarchy rotation=\"0\"><node class=\"android.widget.Button\" text=\"{a}\" clickable=\"true\" bounds=\"[0,0][10,10]\"/><node class=\"android.widget.Button\" text=\"{b}\" clickable=\"true\" bounds=\"[0,20][10,30]\"/></hierarchy>"
        );
        let t = parse(&two, false).unwrap();
        assert_eq!(compute(&t), vec![r.clone(), r.clone()]);
        assert_eq!(
            find(&t, &r),
            None,
            "two elements carry it: it names neither"
        );
        // alone, either one is found by it
        assert_eq!(find(&parse(&one(&a), false).unwrap(), &r), Some(0));
    }

    #[test]
    fn an_element_has_the_same_ref_in_the_interactive_only_view_and_the_full_one() {
        // a plain title and a clickable view with the same class, id and text: in the full tree the title is
        // the first of two twins, in the interactive-only tree the button is the only one
        let xml = "<hierarchy rotation=\"0\"><node class=\"android.widget.FrameLayout\" bounds=\"[0,0][1000,2000]\">\
            <node class=\"android.widget.TextView\" text=\"Wi-Fi\" resource-id=\"a:id/t\" bounds=\"[0,0][500,100]\"/>\
            <node class=\"android.widget.TextView\" text=\"Wi-Fi\" resource-id=\"a:id/t\" clickable=\"true\" bounds=\"[0,200][500,300]\"/>\
            </node></hierarchy>";
        let full = parse(xml, false).unwrap();
        let inter = parse(xml, true).unwrap();
        assert_eq!((full.elements.len(), inter.elements.len()), (2, 1));
        let s = Snapshot::of("s1".into(), true, &inter, Some(&full));
        let listed = s.rows[0].ref_id.clone();
        assert_eq!(
            find(&full, &listed),
            Some(1),
            "the ref in the interactive listing finds the clickable one in the full tree"
        );
        assert!(listing(&s).ends_with(&listed));
    }
}
