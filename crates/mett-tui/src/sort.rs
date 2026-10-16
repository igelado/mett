//! Ordering the index: mutt's `sort` and `sort_aux`, and threads
//! (`docs/MUTTRC.md` §3).
//!
//! Threads come from the backend's conversations: messages of the same
//! conversation form one thread, and each message's path in it (Outlook's
//! conversation index) says which message it answers. A message whose
//! parent isn't in the folder hangs from the thread's first message, so a
//! conversation stays together.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::hash_map::Entry;

use chrono::{DateTime, Utc};
use mett_core::{MessageId, MessageSummary};

/// What messages are ordered by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// When it was sent (`date`, `date-sent`).
    Date,
    /// When it reached the mailbox (`date-received`).
    Received,
    /// The sender (`from`).
    From,
    /// The subject, without `Re:` and the like (`subject`).
    Subject,
    /// The first recipient (`to`).
    To,
    /// The order the folder was listed in (`unsorted`).
    Unsorted,
    /// Threads (`threads`), only for `sort`.
    Threads,
}

/// A `sort` or `sort_aux` value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    /// What to order by.
    pub method: Method,
    /// `reverse-`: the other way round.
    pub reverse: bool,
    /// `last-` (for `sort_aux`, with threads): order threads by their
    /// latest message rather than their first.
    pub last: bool,
}

impl Sort {
    /// Parses a `sort` (`threads` allowed) or `sort_aux` value.
    pub fn parse(text: &str, threads: bool) -> Result<Self, String> {
        let mut rest = text.trim().to_ascii_lowercase();
        let mut sort = Sort {
            method: Method::Date,
            reverse: false,
            last: false,
        };
        loop {
            if let Some(after) = rest.strip_prefix("reverse-") {
                sort.reverse = true;
                rest = after.to_owned();
            } else if let Some(after) = rest.strip_prefix("last-") {
                sort.last = true;
                rest = after.to_owned();
            } else {
                break;
            }
        }
        sort.method = match rest.as_str() {
            "date" | "date-sent" => Method::Date,
            "date-received" => Method::Received,
            "from" => Method::From,
            "subject" => Method::Subject,
            "to" => Method::To,
            "unsorted" | "mailbox-order" => Method::Unsorted,
            "threads" if threads => Method::Threads,
            _ => return Err(format!("{text}: not a sort order mett knows")),
        };
        Ok(sort)
    }

    /// The value as mutt writes it, such as `reverse-threads`.
    pub fn name(self) -> String {
        let method = match self.method {
            Method::Date => "date",
            Method::Received => "date-received",
            Method::From => "from",
            Method::Subject => "subject",
            Method::To => "to",
            Method::Unsorted => "unsorted",
            Method::Threads => "threads",
        };
        let reverse = if self.reverse { "reverse-" } else { "" };
        let last = if self.last { "last-" } else { "" };
        format!("{reverse}{last}{method}")
    }

    /// Compares two messages by what this orders by (`a` and `b`, their
    /// [`Value`]s for it), with ties going to where each was listed (the
    /// order for `unsorted`).
    fn compare(self, a: &Value, b: &Value, a_at: usize, b_at: usize) -> Ordering {
        self.directed(a.cmp(b).then(a_at.cmp(&b_at)))
    }

    /// `order`, the other way round for `reverse-`.
    fn directed(self, order: Ordering) -> Ordering {
        if self.reverse { order.reverse() } else { order }
    }
}

/// What a method orders a message by, worked out once rather than at
/// every comparison. Two messages ordered by one method have the same
/// kind of value.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Value {
    /// When it was sent or received.
    Date(DateTime<Utc>),
    /// A name or subject, lower-cased.
    Text(String),
    /// Nothing: `unsorted` goes by where messages were listed alone.
    Nothing,
}

impl Value {
    /// What `method` alone orders `message` by.
    fn of(method: Method, message: &MessageSummary) -> Self {
        match method {
            Method::Date | Method::Threads => Value::Date(message.received),
            Method::Received => Value::Date(message.delivered.unwrap_or(message.received)),
            Method::From => Value::Text(key(message.from.as_ref())),
            Method::Subject => Value::Text(base_subject(&message.subject).to_lowercase()),
            Method::To => Value::Text(key(message.to.first())),
            Method::Unsorted => Value::Nothing,
        }
    }
}

/// How an address sorts: by name, or by address without one.
fn key(address: Option<&mett_core::Address>) -> String {
    address
        .map(|a| crate::expando::display_name(a).to_lowercase())
        .unwrap_or_default()
}

/// A subject without the `Re:`, `Fwd:` and `Fw:` that replies and
/// forwards add.
pub fn base_subject(subject: &str) -> &str {
    let mut subject = subject.trim_start();
    loop {
        let stripped = ["re:", "fwd:", "fw:", "aw:", "sv:"].iter().find(|prefix| {
            subject
                .as_bytes()
                .get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(prefix.as_bytes()))
        });
        match stripped {
            Some(prefix) => subject = subject[prefix.len()..].trim_start(),
            None => return subject,
        }
    }
}

/// What a message is ordered by, for [`compare`]: worked out once, so
/// that sorting doesn't work it out again at every comparison.
#[derive(Clone, Debug)]
pub struct Key {
    /// What `sort` orders it by.
    sort: Value,
    /// What `sort_aux` orders it by.
    aux: Value,
    /// Where it was listed.
    position: usize,
}

impl Key {
    /// `message`'s key for `sort` and `aux`, listed at `position`.
    pub fn new(message: &MessageSummary, sort: Sort, aux: Sort, position: usize) -> Self {
        Self {
            sort: Value::of(sort.method, message),
            aux: Value::of(aux.method, message),
            position,
        }
    }
}

/// A message's place in the ordered index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// Which message (an index into what was ordered).
    pub index: usize,
    /// How deep in its thread: 0 for a thread's first message.
    pub depth: usize,
    /// The thread's lines that lead to it (`├─>`), empty at depth 0.
    pub tree: String,
    /// Whether its subject is its parent's, which mutt doesn't repeat.
    pub same_subject: bool,
    /// Its thread's first message (an index into what was ordered).
    pub root: usize,
}

/// The ordered index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arranged {
    /// Each message's place, in order.
    pub nodes: Vec<Node>,
    /// Whether the order, and the threads, depend only on the messages,
    /// not on the order they were given in: no two were listed at one
    /// position, and no two of a conversation have one path. Arranging
    /// the messages again in another order then gives the same.
    pub settled: bool,
}

/// Orders `messages` by `sort` (with `aux` breaking ties, or ordering
/// each thread's messages); `position` says where each was listed.
pub fn arrange(
    messages: &[&MessageSummary],
    sort: Sort,
    aux: Sort,
    position: &dyn Fn(&MessageId) -> usize,
    ascii: bool,
) -> Arranged {
    // Where each was listed, looked up once.
    let at: Vec<usize> = messages.iter().map(|m| position(&m.id)).collect();
    let distinct = distinct(&at);
    if sort.method != Method::Threads {
        let keys: Vec<Key> = messages
            .iter()
            .zip(&at)
            .map(|(m, &at)| Key::new(m, sort, aux, at))
            .collect();
        let mut order: Vec<usize> = (0..messages.len()).collect();
        order.sort_by(|&a, &b| compare(&keys[a], &keys[b], sort, aux));
        let nodes = order
            .into_iter()
            .map(|index| Node {
                index,
                depth: 0,
                tree: String::new(),
                same_subject: false,
                root: index,
            })
            .collect();
        return Arranged {
            nodes,
            settled: distinct,
        };
    }
    let (nodes, single_paths) = threads(messages, &at, sort, aux, ascii);
    Arranged {
        nodes,
        settled: distinct && single_paths,
    }
}

/// Whether no two of `positions` are the same.
fn distinct(positions: &[usize]) -> bool {
    // Positions count from 0, so they usually fit a table as long as
    // there are messages; otherwise they are sorted.
    let mut seen = vec![false; positions.len()];
    for &at in positions {
        match seen.get_mut(at) {
            Some(true) => return false,
            Some(slot) => *slot = true,
            None => {
                let mut sorted = positions.to_vec();
                sorted.sort_unstable();
                return sorted.windows(2).all(|pair| pair[0] != pair[1]);
            }
        }
    }
    true
}

/// How [`arrange`] orders two messages, by their [`Key`]s for `sort` and
/// `aux`, when not by threads: by `sort`, with ties going to `sort_aux`,
/// then to where they were listed (with `unsorted`, only where they were
/// listed counts). No two messages are equal, so merging sorted lists
/// gives what sorting them all would.
pub fn compare(a: &Key, b: &Key, sort: Sort, aux: Sort) -> Ordering {
    if sort.method == Method::Unsorted {
        return sort.compare(&a.sort, &b.sort, a.position, b.position);
    }
    let first = sort.directed(a.sort.cmp(&b.sort));
    first.then_with(|| aux.compare(&a.aux, &b.aux, a.position, b.position))
}

/// [`arrange`] by threads, with `at` where each message was listed; also
/// says whether no two messages of a conversation have one path.
fn threads(
    messages: &[&MessageSummary],
    at: &[usize],
    sort: Sort,
    aux: Sort,
    ascii: bool,
) -> (Vec<Node>, bool) {
    // The conversations, in the order first seen.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut by_conversation: HashMap<&str, usize> = HashMap::new();
    for (i, message) in messages.iter().enumerate() {
        match &message.thread {
            Some(thread) => {
                let group = *by_conversation
                    .entry(thread.conversation.as_str())
                    .or_insert_with(|| {
                        groups.push(Vec::new());
                        groups.len() - 1
                    });
                groups[group].push(i);
            }
            None => groups.push(vec![i]),
        }
    }
    // A thread's messages, and threads, go by `sort_aux`.
    let children_sort = Sort { last: false, ..aux };
    let values: Vec<Value> = messages
        .iter()
        .map(|m| Value::of(children_sort.method, m))
        .collect();
    let compare =
        |a: &usize, b: &usize| children_sort.compare(&values[*a], &values[*b], at[*a], at[*b]);
    let path = |i: usize| {
        messages[i]
            .thread
            .as_ref()
            .map_or(&[][..], |t| t.path.as_slice())
    };
    let mut single_paths = true;
    // Each conversation's first message, and the children of each
    // message.
    let mut roots: Vec<usize> = Vec::with_capacity(groups.len());
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); messages.len()];
    for group in &groups {
        // Two messages with one path: the first listed stands for it. A
        // small conversation is looked through rather than indexed.
        let small = group.len() <= 8;
        let mut by_path: HashMap<&[Vec<u8>], usize> = HashMap::new();
        for (k, &i) in group.iter().enumerate() {
            if path(i).is_empty() {
                continue;
            }
            let repeated = if small {
                group[..k].iter().any(|&j| path(j) == path(i))
            } else {
                match by_path.entry(path(i)) {
                    Entry::Occupied(_) => true,
                    Entry::Vacant(entry) => {
                        entry.insert(i);
                        false
                    }
                }
            };
            single_paths &= !repeated;
        }
        let stands_for = |prefix: &[Vec<u8>]| {
            if small {
                group.iter().copied().find(|&j| path(j) == prefix)
            } else {
                by_path.get(prefix).copied()
            }
        };
        // Each message's nearest ancestor in the folder, if any.
        let parents: Vec<Option<usize>> = group
            .iter()
            .map(|&i| {
                let path = path(i);
                (1..path.len())
                    .rev()
                    .find_map(|len| stands_for(&path[..len]))
                    .filter(|&p| p != i)
            })
            .collect();
        let orphans = || {
            group
                .iter()
                .zip(&parents)
                .filter(|(_, parent)| parent.is_none())
                .map(|(&i, _)| i)
        };
        // The thread starts at the orphan nearest the conversation's start,
        // or else the earliest. One without a place in the conversation
        // (no conversation index) goes by its date: it starts the thread
        // if it is older than that.
        let earlier = |a: usize, b: usize| {
            messages[a]
                .received
                .cmp(&messages[b].received)
                .then_with(|| at[a].cmp(&at[b]))
        };
        let placed = orphans().filter(|&i| !path(i).is_empty()).min_by(|&a, &b| {
            path(a)
                .len()
                .cmp(&path(b).len())
                .then_with(|| earlier(a, b))
        });
        let unplaced = orphans()
            .filter(|&i| path(i).is_empty())
            .min_by(|&a, &b| earlier(a, b));
        let root = match (placed, unplaced) {
            (Some(placed), Some(unplaced)) if earlier(unplaced, placed).is_lt() => unplaced,
            (Some(root), _) | (None, Some(root)) => root,
            (None, None) => {
                unreachable!("a conversation has a message without a parent among its own")
            }
        };
        for (&i, parent) in group.iter().zip(&parents) {
            if i != root {
                children[parent.unwrap_or(root)].push(i);
            }
        }
        for &i in group {
            children[i].sort_by(compare);
        }
        roots.push(root);
    }
    // Threads in order: by their first message, or with `last-`, their
    // latest.
    // The latest by date, whichever way the threads are then ordered.
    let latest = |t: usize| {
        let root = roots[t];
        std::iter::once(root)
            .chain(groups[t].iter().copied().filter(|&i| i != root))
            .max_by(|a, b| {
                // By the date `sort_aux` goes by.
                let date = |m: &MessageSummary| match aux.method {
                    Method::Received => m.delivered.unwrap_or(m.received),
                    _ => m.received,
                };
                date(messages[*a])
                    .cmp(&date(messages[*b]))
                    .then_with(|| at[*a].cmp(&at[*b]))
            })
            .expect("a thread has a message")
    };
    let representative: Vec<usize> = (0..roots.len())
        .map(|t| {
            // `last-threads` or `sort_aux=last-...`.
            if aux.last || sort.last {
                latest(t)
            } else {
                roots[t]
            }
        })
        .collect();
    let mut order: Vec<usize> = (0..roots.len()).collect();
    order.sort_by(|&a, &b| sort.directed(compare(&representative[a], &representative[b])));
    let (branch, last, line, arrow) = if ascii {
        ("|-", "`-", "| ", ">")
    } else {
        ("├─", "└─", "│ ", ">")
    };
    let mut nodes = Vec::with_capacity(messages.len());
    for t in order {
        let root = roots[t];
        // Depth first: the message, whether each ancestor (below the root)
        // has more children after this branch, and whether it is the last
        // of its siblings.
        let mut stack: Vec<(usize, Vec<bool>, bool, Option<usize>)> =
            vec![(root, Vec::new(), true, None)];
        while let Some((i, more, is_last, parent)) = stack.pop() {
            let depth = more.len() + usize::from(parent.is_some());
            let mut tree = String::new();
            if parent.is_some() {
                for &continues in &more {
                    tree.push_str(if continues { line } else { "  " });
                }
                tree.push_str(if is_last { last } else { branch });
                tree.push_str(arrow);
            }
            let same_subject = parent.is_some_and(|p| {
                base_subject(&messages[p].subject)
                    .eq_ignore_ascii_case(base_subject(&messages[i].subject))
            });
            nodes.push(Node {
                index: i,
                depth,
                tree,
                same_subject,
                root,
            });
            let kids = &children[i];
            if !kids.is_empty() {
                let mut below = more.clone();
                if parent.is_some() {
                    below.push(!is_last);
                }
                for (k, &kid) in kids.iter().enumerate().rev() {
                    stack.push((kid, below.clone(), k + 1 == kids.len(), Some(i)));
                }
            }
        }
    }
    (nodes, single_paths)
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeDelta};
    use mett_core::{Address, ThreadPosition, mock};

    use super::*;

    fn message(id: &str, minutes: i64, thread: Option<(&str, &[u8])>) -> MessageSummary {
        let mut m = mock::message(
            id,
            "in",
            &format!("Re: topic {}", thread.map_or(id, |t| t.0)),
            DateTime::UNIX_EPOCH + TimeDelta::minutes(minutes),
        )
        .summary;
        m.from = Some(Address::new(format!("{id}@x.org")));
        m.thread = thread.map(|(conversation, path)| ThreadPosition {
            conversation: conversation.into(),
            path: path.iter().map(|step| vec![*step]).collect(),
        });
        m
    }

    fn arranged(messages: &[MessageSummary], sort: &str, aux: &str) -> Vec<String> {
        let refs: Vec<&MessageSummary> = messages.iter().collect();
        let listed: HashMap<MessageId, usize> = messages
            .iter()
            .enumerate()
            .map(|(i, m)| (m.id.clone(), i))
            .collect();
        arrange(
            &refs,
            Sort::parse(sort, true).unwrap(),
            Sort::parse(aux, false).unwrap(),
            &|id| listed[id],
            true,
        )
        .nodes
        .into_iter()
        .map(|node| format!("{}{}", node.tree, messages[node.index].id))
        .collect()
    }

    #[test]
    fn parses_sort_orders() {
        let sort = Sort::parse("reverse-last-date-received", false).unwrap();
        assert_eq!(
            (sort.method, sort.reverse, sort.last),
            (Method::Received, true, true)
        );
        assert!(Sort::parse("threads", false).is_err(), "not for sort_aux");
        assert!(Sort::parse("size", true).is_err());
        assert_eq!(
            Sort::parse("date-received", false).unwrap().method,
            Method::Received
        );
        assert_eq!(base_subject("Re: FW:  re:Lunch"), "Lunch");
        assert_eq!(sort.name(), "reverse-last-date-received");
    }

    #[test]
    fn a_start_without_a_conversation_index_goes_by_its_date() {
        // s has no place in the conversation, but came first.
        let messages = vec![
            message("r", 2, Some(("c", &[0, 1]))),
            message("s", 1, Some(("c", &[]))),
        ];
        assert_eq!(arranged(&messages, "threads", "date"), ["s", "`->r"]);
        // A later one doesn't.
        let messages = vec![
            message("r", 2, Some(("c", &[0, 1]))),
            message("s", 3, Some(("c", &[]))),
        ];
        assert_eq!(arranged(&messages, "threads", "date"), ["r", "`->s"]);
    }

    #[test]
    fn sorts_with_ties_to_sort_aux() {
        let mut messages = vec![
            message("a", 3, None),
            message("b", 1, None),
            message("c", 2, None),
        ];
        assert_eq!(arranged(&messages, "date", "date"), ["b", "c", "a"]);
        assert_eq!(arranged(&messages, "reverse-date", "date"), ["a", "c", "b"]);
        assert_eq!(arranged(&messages, "unsorted", "date"), ["a", "b", "c"]);
        // Sent in one order, received in another.
        messages[1].delivered = Some(DateTime::UNIX_EPOCH + TimeDelta::minutes(9));
        assert_eq!(
            arranged(&messages, "date-received", "date"),
            ["c", "a", "b"]
        );
        messages[1].delivered = None;
        messages[0].from = Some(Address::new("z@x.org").with_name("Same"));
        messages[1].from = Some(Address::new("y@x.org").with_name("Same"));
        // Same sender: by date.
        assert_eq!(arranged(&messages, "from", "date"), ["c", "b", "a"]);
        assert_eq!(arranged(&messages, "from", "reverse-date")[1..], ["a", "b"]);
    }

    #[test]
    fn builds_threads_from_conversation_paths() {
        let messages = vec![
            message("root", 0, Some(("t", &[0]))),
            message("r2", 5, Some(("t", &[0, 2]))),
            message("r1", 3, Some(("t", &[0, 1]))),
            message("r11", 4, Some(("t", &[0, 1, 1]))),
            // Its parent isn't here: it hangs from the root.
            message("lost", 6, Some(("t", &[0, 9, 9]))),
            message("other", 1, None),
            message("u", 2, Some(("u", &[]))),
        ];
        assert_eq!(
            arranged(&messages, "threads", "date"),
            [
                "root", "|->r1", "| `->r11", "|->r2", "`->lost", "other", "u"
            ]
        );
        // Newest thread first, by its latest message.
        assert_eq!(
            arranged(&messages, "reverse-threads", "last-date")[..2],
            ["root".to_owned(), "|->r1".to_owned()]
        );
        assert_eq!(
            arranged(&messages, "reverse-threads", "date")[..3],
            ["u", "other", "root"]
        );
        // `last-` goes by the latest message whichever way it sorts: the
        // thread whose latest is newest first.
        assert_eq!(
            arranged(&messages, "threads", "last-reverse-date")[..2],
            ["root".to_owned(), "|->lost".to_owned()]
        );
        assert_eq!(arranged(&messages, "threads", "last-date")[0], "other");
        // By when mail was received, with `last-date-received`: "other"
        // was received last.
        let mut received = messages.clone();
        received[5].delivered = Some(DateTime::UNIX_EPOCH + TimeDelta::minutes(99));
        assert_eq!(
            arranged(&received, "reverse-threads", "last-date-received")[0],
            "other"
        );
        // The same with `last-threads`.
        assert_eq!(arranged(&messages, "last-threads", "date")[0], "other");
    }

    #[test]
    fn repeated_subjects_are_marked() {
        let messages = [
            message("root", 0, Some(("t", &[0]))),
            message("reply", 1, Some(("t", &[0, 1]))),
        ];
        let refs: Vec<&MessageSummary> = messages.iter().collect();
        let nodes = arrange(
            &refs,
            Sort::parse("threads", true).unwrap(),
            Sort::parse("date", false).unwrap(),
            &|_| 0,
            false,
        )
        .nodes;
        assert!(!nodes[0].same_subject);
        assert!(nodes[1].same_subject);
        assert_eq!(nodes[1].tree, "└─>");
        assert_eq!(nodes[1].root, 0);
    }
}
