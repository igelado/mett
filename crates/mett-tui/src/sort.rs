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

    /// Compares two messages, `position` being where each was listed (the
    /// tie-breaker, and the order for `unsorted`).
    fn compare(
        self,
        a: &MessageSummary,
        b: &MessageSummary,
        position: &dyn Fn(&MessageId) -> usize,
    ) -> Ordering {
        let order = primary(self.method, a, b).then_with(|| position(&a.id).cmp(&position(&b.id)));
        if self.reverse { order.reverse() } else { order }
    }
}

/// How `method` alone orders two messages.
fn primary(method: Method, a: &MessageSummary, b: &MessageSummary) -> Ordering {
    match method {
        Method::Date | Method::Threads => a.received.cmp(&b.received),
        Method::Received => {
            let at = |m: &MessageSummary| m.delivered.unwrap_or(m.received);
            at(a).cmp(&at(b))
        }
        Method::From => key(a.from.as_ref()).cmp(&key(b.from.as_ref())),
        Method::Subject => base_subject(&a.subject)
            .to_lowercase()
            .cmp(&base_subject(&b.subject).to_lowercase()),
        Method::To => key(a.to.first()).cmp(&key(b.to.first())),
        Method::Unsorted => Ordering::Equal,
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
        let lower = subject.to_ascii_lowercase();
        let stripped = ["re:", "fwd:", "fw:", "aw:", "sv:"]
            .iter()
            .find(|prefix| lower.starts_with(*prefix))
            .map(|prefix| subject[prefix.len()..].trim_start());
        match stripped {
            Some(rest) => subject = rest,
            None => return subject,
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

/// Orders `messages` by `sort` (with `aux` breaking ties, or ordering
/// each thread's messages); `position` says where each was listed.
pub fn arrange(
    messages: &[&MessageSummary],
    sort: Sort,
    aux: Sort,
    position: &dyn Fn(&MessageId) -> usize,
    ascii: bool,
) -> Vec<Node> {
    if sort.method != Method::Threads {
        let mut order: Vec<usize> = (0..messages.len()).collect();
        order.sort_by(|&a, &b| {
            let (a, b) = (messages[a], messages[b]);
            // Ties go to `sort_aux`, then to where they were listed; with
            // `unsorted`, only where they were listed counts.
            if sort.method == Method::Unsorted {
                return sort.compare(a, b, position);
            }
            let first = primary(sort.method, a, b);
            let first = if sort.reverse { first.reverse() } else { first };
            first.then_with(|| aux.compare(a, b, position))
        });
        return order
            .into_iter()
            .map(|index| Node {
                index,
                depth: 0,
                tree: String::new(),
                same_subject: false,
                root: index,
            })
            .collect();
    }
    threads(messages, sort, aux, position, ascii)
}

/// [`arrange`] by threads.
fn threads(
    messages: &[&MessageSummary],
    sort: Sort,
    aux: Sort,
    position: &dyn Fn(&MessageId) -> usize,
    ascii: bool,
) -> Vec<Node> {
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
    let compare = |a: &usize, b: &usize| {
        let children = Sort { last: false, ..aux };
        children.compare(messages[*a], messages[*b], position)
    };
    // Each conversation's tree: children of each message, and its root.
    let mut trees: Vec<(usize, HashMap<usize, Vec<usize>>)> = Vec::new();
    for group in &groups {
        let path = |i: usize| {
            messages[i]
                .thread
                .as_ref()
                .map_or(&[][..], |t| t.path.as_slice())
        };
        let mut by_path: HashMap<&[Vec<u8>], usize> = HashMap::new();
        for &i in group {
            if !path(i).is_empty() {
                // Two messages with one path: the first listed stands for
                // it.
                by_path.entry(path(i)).or_insert(i);
            }
        }
        // Each message's nearest ancestor in the folder, if any.
        let parent_of = |i: usize| {
            let path = path(i);
            (1..path.len())
                .rev()
                .find_map(|len| by_path.get(&path[..len]).copied())
                .filter(|&p| p != i)
        };
        let orphans: Vec<usize> = group
            .iter()
            .copied()
            .filter(|&i| parent_of(i).is_none())
            .collect();
        // The thread starts at the orphan nearest the conversation's start,
        // or else the earliest.
        let root = *orphans
            .iter()
            .min_by(|a, b| {
                let depth = |i: usize| match path(i).len() {
                    0 => usize::MAX,
                    n => n,
                };
                depth(**a)
                    .cmp(&depth(**b))
                    .then_with(|| messages[**a].received.cmp(&messages[**b].received))
                    .then_with(|| position(&messages[**a].id).cmp(&position(&messages[**b].id)))
            })
            .expect("a conversation has a message without a parent among its own");
        let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
        for &i in group {
            if i == root {
                continue;
            }
            let parent = parent_of(i).unwrap_or(root);
            children.entry(parent).or_default().push(i);
        }
        for list in children.values_mut() {
            list.sort_by(compare);
        }
        trees.push((root, children));
    }
    // Threads in order: by their first message, or with `last-`, their
    // latest.
    // The latest by date, whichever way the threads are then ordered.
    let latest = |tree: &(usize, HashMap<usize, Vec<usize>>)| {
        std::iter::once(tree.0)
            .chain(tree.1.values().flatten().copied())
            .max_by(|a, b| {
                messages[*a]
                    .received
                    .cmp(&messages[*b].received)
                    .then_with(|| position(&messages[*a].id).cmp(&position(&messages[*b].id)))
            })
            .expect("a thread has a message")
    };
    let representative: Vec<usize> = trees
        .iter()
        .map(|tree| {
            // `last-threads` or `sort_aux=last-...`.
            if aux.last || sort.last {
                latest(tree)
            } else {
                tree.0
            }
        })
        .collect();
    let mut order: Vec<usize> = (0..trees.len()).collect();
    order.sort_by(|&a, &b| {
        let by_aux = compare(&representative[a], &representative[b]);
        if sort.reverse {
            by_aux.reverse()
        } else {
            by_aux
        }
    });
    let (branch, last, line, arrow) = if ascii {
        ("|-", "`-", "| ", ">")
    } else {
        ("├─", "└─", "│ ", ">")
    };
    let mut nodes = Vec::with_capacity(messages.len());
    for t in order {
        let (root, children) = &trees[t];
        // Depth first: the message, whether each ancestor (below the root)
        // has more children after this branch, and whether it is the last
        // of its siblings.
        let mut stack: Vec<(usize, Vec<bool>, bool, Option<usize>)> =
            vec![(*root, Vec::new(), true, None)];
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
                root: *root,
            });
            if let Some(kids) = children.get(&i) {
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
    nodes
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
        );
        assert!(!nodes[0].same_subject);
        assert!(nodes[1].same_subject);
        assert_eq!(nodes[1].tree, "└─>");
        assert_eq!(nodes[1].root, 0);
    }
}
