//! The conversation as the client lists it, kept in the kernel and handed over as diffs: rows
//! (messages, and the steps a run took before its answer folded into one row) and, for each
//! message, its text and rendered blocks.
use crate::{
    Channel,
    markdown::{Block, Doc, Source},
};
use ryzhub_proto::{Item, item_detail};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Item kinds a run produces on its way to an answer.
const STEP_KINDS: &[&str] = &["tool", "reasoning", "plan", "file_change"];

/// Item kinds whose text is not Markdown.
const UNRENDERED: &[&str] = &[
    "tool",
    "file_change",
    "memory_notice",
    "memory_context",
    "conversation",
];

/// More rows than this changing at once replace the list.
const RESET_AFTER: usize = 64;

/// A row of the timeline: one item, or the steps of a run folded together.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Row {
    /// The item's id, or `steps:` and the first step's id.
    pub id: String,
    /// The item a message row shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    /// The items a steps row folds, in order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub run_id: String,
    /// Nothing of the run follows these steps yet.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub latest: bool,
}

/// A change to the rows, applied in order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RowDiff {
    Insert { index: usize, row: Row },
    Set { index: usize, row: Row },
    Remove { index: usize },
    Reset { rows: Vec<Row> },
}

/// What changed in an item the client shows.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ItemDiff {
    pub id: String,
    /// The whole item: first shown, or changed beyond its text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<Item>,
    /// Text that streamed onto the end of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub append: Option<String>,
    /// Its blocks from `from` on, replacing those the client has from there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocks: Option<Blocks>,
    /// The citation chips its blocks show, when they changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chips: Option<Vec<Vec<Source>>>,
    /// A running command's output so far, replacing what the client has (empty clears it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Output that streamed onto the end of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_append: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Blocks {
    pub from: usize,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ViewDiff {
    pub rows: Vec<RowDiff>,
    pub items: Vec<ItemDiff>,
    /// Items the client no longer needs.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
}

impl ViewDiff {
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.items.is_empty() && self.removed.is_empty()
    }
}

/// What the client has of an item.
#[derive(Default)]
struct Shown {
    revision: u64,
    text_len: usize,
    blocks: usize,
    doc: Option<Doc>,
    /// The revision and length of the command output the client has.
    output: Option<(u64, usize)>,
}

/// A channel's timeline as the client has it.
#[derive(Default)]
pub struct View {
    /// The client has the rows: what follows are changes to them.
    started: bool,
    rows: Vec<Row>,
    shown: HashMap<String, Shown>,
}

impl View {
    /// Brings the client up to date with `channel`: its rows, and the items in `touched`, or
    /// every item when None. The first update hands over everything.
    pub fn update(&mut self, channel: &Channel, touched: Option<&HashSet<String>>) -> ViewDiff {
        let mut diff = ViewDiff::default();
        let fresh = !std::mem::replace(&mut self.started, true);
        let rows = rows(&channel.items);
        diff.rows = if fresh {
            vec![RowDiff::Reset { rows: rows.clone() }]
        } else {
            diff_rows(&self.rows, &rows)
        };
        self.rows = rows;
        let removed: Vec<String> = self
            .shown
            .keys()
            .filter(|id| !channel.items.contains_key(*id))
            .cloned()
            .collect();
        for id in &removed {
            self.shown.remove(id);
        }
        diff.removed = removed;
        let ids: Vec<&String> = match touched {
            Some(ids) if !fresh => ids.iter().collect(),
            _ => channel.items.keys().collect(),
        };
        for id in ids {
            if let Some(item) = channel.items.get(id) {
                let shown = self.shown.entry(id.clone()).or_default();
                let output = channel
                    .outputs
                    .get(id)
                    .filter(|(revision, _)| *revision == item.revision)
                    .map(|(_, output)| output.as_str());
                if let Some(change) = item_diff(item, shown, output) {
                    diff.items.push(change);
                }
            }
        }
        diff.items.sort_by(|a, b| a.id.cmp(&b.id));
        diff
    }
}

/// The rows and items of `items` for a client that has none of them: a past stretch of a
/// conversation, shown apart from its live timeline.
pub fn snapshot(items: Vec<Item>) -> (Vec<Row>, Vec<ItemDiff>) {
    let channel = Channel {
        items: items.into_iter().map(|i| (i.id.clone(), i)).collect(),
        ..Default::default()
    };
    let diff = View::default().update(&channel, None);
    let rows = match diff.rows.into_iter().next() {
        Some(RowDiff::Reset { rows }) => rows,
        _ => vec![],
    };
    (rows, diff.items)
}

fn item_diff(item: &Item, shown: &mut Shown, output: Option<&str>) -> Option<ItemDiff> {
    let first = shown.revision == 0 && shown.text_len == 0 && shown.doc.is_none();
    let mut diff = ItemDiff {
        id: item.id.clone(),
        item: None,
        append: None,
        blocks: None,
        chips: None,
        output: None,
        output_append: None,
    };
    if first || item.revision != shown.revision || item.text.len() < shown.text_len {
        diff.item = Some(item.clone());
    } else if item.text.len() > shown.text_len && item.text.is_char_boundary(shown.text_len) {
        diff.append = Some(item.text[shown.text_len..].to_owned());
    } else if item.text.len() > shown.text_len {
        diff.item = Some(item.clone());
    }
    shown.revision = item.revision;
    shown.text_len = item.text.len();
    if !UNRENDERED.contains(&item.kind.as_str()) {
        let doc = shown.doc.get_or_insert_with(Doc::default);
        let splice = doc.update(&item.text, &cited(item), item.state == "streaming");
        let from = if first { 0 } else { splice.from };
        if first || from < shown.blocks || from < doc.len() {
            diff.blocks = Some(Blocks {
                from,
                blocks: doc.blocks().skip(from).cloned().collect(),
            });
        }
        if first || splice.chips_changed {
            diff.chips = Some(doc.chips().cloned().collect());
        }
        shown.blocks = doc.len();
    }
    match (output, shown.output) {
        (Some(now), Some((revision, len)))
            if revision == item.revision && now.len() >= len && now.is_char_boundary(len) =>
        {
            if now.len() > len {
                diff.output_append = Some(now[len..].to_owned());
            }
        }
        (Some(now), _) => diff.output = Some(now.to_owned()),
        (None, Some(_)) => diff.output = Some(String::new()),
        (None, None) => {}
    }
    shown.output = output.map(|o| (item.revision, o.len()));
    let unchanged = diff.item.is_none()
        && diff.append.is_none()
        && diff.blocks.is_none()
        && diff.output.is_none()
        && diff.output_append.is_none();
    (!unchanged || diff.chips.is_some()).then_some(diff)
}

/// The pages an assistant reply cites.
fn cited(item: &Item) -> Vec<String> {
    match item.detail.as_ref().and_then(|d| d.kind.as_ref()) {
        Some(item_detail::Kind::Answer(answer)) => {
            answer.citations.iter().map(|c| c.url.clone()).collect()
        }
        _ => vec![],
    }
}

/// The conversation as displayed. A run's answer is its last reply when nothing else of the
/// run follows it; the work before it (tool calls, reasoning, interim replies) becomes one
/// steps row. What a subagent did shows under its call, not as rows of its own. Memory context
/// and what a retried model call discarded are not shown.
pub fn rows(items: &BTreeMap<String, Item>) -> Vec<Row> {
    let mut visible: Vec<&Item> = items
        .values()
        .filter(|i| i.kind != "memory_context" && i.state != "discarded")
        .filter(|i| i.parent_item_id.is_empty() || !items.contains_key(&i.parent_item_id))
        .collect();
    visible.sort_by_key(|i| i.first_seq);
    let of_run = |i: &Item| {
        !i.run_id.is_empty() && (i.kind == "assistant" || STEP_KINDS.contains(&i.kind.as_str()))
    };
    let mut last: HashMap<&str, &str> = HashMap::new();
    for item in visible.iter().filter(|i| of_run(i)) {
        last.insert(&item.run_id, &item.id);
    }
    let mut rows: Vec<Row> = Vec::new();
    for item in visible {
        let run = item.run_id.as_str();
        if !of_run(item) || item.kind == "assistant" && last.get(run) == Some(&item.id.as_str()) {
            rows.push(Row {
                id: item.id.clone(),
                item: Some(item.id.clone()),
                steps: vec![],
                run_id: item.run_id.clone(),
                latest: false,
            });
            continue;
        }
        // A step that only called tools leaves an empty reply.
        if item.kind == "assistant" && item.text.trim().is_empty() {
            continue;
        }
        match rows.last_mut() {
            Some(group) if group.item.is_none() && group.run_id == run => {
                group.steps.push(item.id.clone());
            }
            _ => rows.push(Row {
                id: format!("steps:{}", item.id),
                item: None,
                steps: vec![item.id.clone()],
                run_id: item.run_id.clone(),
                latest: false,
            }),
        }
    }
    for row in rows.iter_mut().filter(|r| r.item.is_none()) {
        row.latest = row.steps.last().map(String::as_str) == last.get(row.run_id.as_str()).copied();
    }
    rows
}

/// Changes from `old` to `new`: rows of both ends kept by id, the middle replaced.
fn diff_rows(old: &[Row], new: &[Row]) -> Vec<RowDiff> {
    let prefix = old
        .iter()
        .zip(new)
        .take_while(|(a, b)| a.id == b.id)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a.id == b.id)
        .count();
    let (removed, inserted) = (old.len() - prefix - suffix, new.len() - prefix - suffix);
    if removed + inserted > RESET_AFTER && removed + inserted > new.len() / 2 {
        return vec![RowDiff::Reset { rows: new.to_vec() }];
    }
    let mut out = Vec::new();
    for i in 0..prefix {
        if old[i] != new[i] {
            out.push(RowDiff::Set {
                index: i,
                row: new[i].clone(),
            });
        }
    }
    for index in (prefix..prefix + removed).rev() {
        out.push(RowDiff::Remove { index });
    }
    for (k, row) in new[prefix..prefix + inserted].iter().enumerate() {
        out.push(RowDiff::Insert {
            index: prefix + k,
            row: row.clone(),
        });
    }
    for j in 0..suffix {
        let (o, n) = (old.len() - suffix + j, new.len() - suffix + j);
        if old[o] != new[n] {
            out.push(RowDiff::Set {
                index: n,
                row: new[n].clone(),
            });
        }
    }
    out
}

/// Applies `diffs` to `rows`, as the client does.
pub fn apply_rows(rows: &mut Vec<Row>, diffs: &[RowDiff]) {
    for diff in diffs {
        match diff {
            RowDiff::Insert { index, row } => rows.insert(*index, row.clone()),
            RowDiff::Set { index, row } => rows[*index] = row.clone(),
            RowDiff::Remove { index } => {
                rows.remove(*index);
            }
            RowDiff::Reset { rows: all } => *rows = all.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, kind: &str, run: &str, seq: u64, text: &str) -> Item {
        Item {
            id: id.into(),
            revision: 1,
            kind: kind.into(),
            state: "completed".into(),
            text: text.into(),
            first_seq: seq,
            last_seq: seq,
            run_id: run.into(),
            ..Default::default()
        }
    }

    fn channel(items: Vec<Item>) -> Channel {
        Channel {
            items: items.into_iter().map(|i| (i.id.clone(), i)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn steps_fold_before_the_answer() {
        let rows = rows(
            &channel(vec![
                item("u", "user", "", 1, "hi"),
                item("r", "reasoning", "run", 2, "think"),
                item("t", "tool", "run", 3, ""),
                item("e", "assistant", "run", 4, " "),
                item("i", "assistant", "run", 5, "interim"),
                item("t2", "tool", "run", 6, ""),
                item("a", "assistant", "run", 7, "answer"),
                item("m", "memory_context", "", 8, ""),
            ])
            .items,
        );
        let ids: Vec<_> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["u", "steps:r", "a"]);
        assert_eq!(rows[1].steps, ["r", "t", "i", "t2"]);
        assert!(!rows[1].latest);
        // A subagent's work stays with its call.
        let mut child = item("c", "assistant", "run", 9, "subagent text");
        child.parent_item_id = "t".into();
        let nested = super::rows(
            &channel(vec![
                item("u", "user", "", 1, "hi"),
                item("t", "tool", "run", 2, "Task"),
                child,
                item("a", "assistant", "run", 10, "answer"),
            ])
            .items,
        );
        assert_eq!(nested[1].steps, ["t"]);
        assert_eq!(
            nested[2].id, "a",
            "the subagent's reply is not the run's answer"
        );
        // While the run works, its last step is the latest of it.
        let working = super::rows(
            &channel(vec![
                item("u", "user", "", 1, "hi"),
                item("t", "tool", "run", 2, ""),
            ])
            .items,
        );
        assert!(working[1].latest);
    }

    #[test]
    fn rows_diff_by_position() {
        let row = |id: &str| Row {
            id: id.into(),
            item: Some(id.into()),
            steps: vec![],
            run_id: String::new(),
            latest: false,
        };
        let old = vec![row("a"), row("b"), row("c")];
        for new in [
            vec![row("a"), row("b"), row("c"), row("d")],
            vec![row("z"), row("a"), row("b"), row("c")],
            vec![row("a"), row("c")],
            vec![row("a"), row("x"), row("c")],
            vec![],
        ] {
            let diffs = diff_rows(&old, &new);
            let mut rows = old.clone();
            apply_rows(&mut rows, &diffs);
            assert_eq!(rows, new, "{diffs:?}");
        }
        assert_eq!(
            diff_rows(&old, &[row("a"), row("b"), row("c"), row("d")]),
            vec![RowDiff::Insert {
                index: 3,
                row: row("d")
            }]
        );
    }

    #[test]
    fn streaming_items_send_what_is_new() {
        let mut view = View::default();
        let mut reply = item("a", "assistant", "run", 1, "first paragraph\n\nsec");
        reply.state = "streaming".into();
        let first = view.update(&channel(vec![reply.clone()]), None);
        assert!(matches!(first.rows[0], RowDiff::Reset { .. }));
        assert!(first.items[0].item.is_some());
        reply.text.push_str("ond line\nmore");
        let touched: HashSet<String> = ["a".to_owned()].into();
        let next = view.update(&channel(vec![reply.clone()]), Some(&touched));
        assert!(next.rows.is_empty());
        let change = &next.items[0];
        assert_eq!(change.item, None);
        assert_eq!(change.append.as_deref(), Some("ond line\nmore"));
        assert_eq!(
            change.blocks.as_ref().unwrap().from,
            1,
            "the first paragraph stays"
        );
        // Completed, the item comes whole again.
        reply.revision = 2;
        reply.state = "completed".into();
        let done = view.update(&channel(vec![reply]), Some(&touched));
        assert!(done.items[0].item.is_some());
        // An item gone from the channel is removed.
        let gone = view.update(&channel(vec![]), Some(&touched));
        assert_eq!(gone.removed, ["a"]);
        assert_eq!(gone.rows, vec![RowDiff::Remove { index: 0 }]);
    }

    #[test]
    fn command_output_streams_beside_its_item() {
        let mut view = View::default();
        // A running command's output streams beside its item.
        let mut command = item("t", "tool", "run", 2, "ls");
        command.state = "running".into();
        let mut running = channel(vec![command.clone()]);
        running.outputs.insert("t".into(), (1, "a\n".into()));
        view.update(&channel(vec![]), None);
        let shown = view.update(&running, Some(&["t".to_owned()].into()));
        let change = shown.items.iter().find(|i| i.id == "t").unwrap();
        assert_eq!(change.output.as_deref(), Some("a\n"));
        running.outputs.insert("t".into(), (1, "a\nb\n".into()));
        let more = view.update(&running, Some(&["t".to_owned()].into()));
        assert_eq!(more.items[0].output_append.as_deref(), Some("b\n"));
    }
}
