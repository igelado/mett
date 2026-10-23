//! Sets of `UID`s, as the ranges they mostly are (`1:5000,5002:9000`).

use std::fmt;

/// A set of `UID`s, kept as sorted ranges that don't touch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct UidSet(Vec<(u32, u32)>);

impl UidSet {
    /// The set of `uids`, in any order.
    pub fn of(uids: impl IntoIterator<Item = u32>) -> Self {
        let mut sorted: Vec<u32> = uids.into_iter().collect();
        sorted.sort_unstable();
        sorted.dedup();
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        for uid in sorted {
            match ranges.last_mut() {
                Some((_, to)) if *to + 1 == uid => *to = uid,
                _ => ranges.push((uid, uid)),
            }
        }
        Self(ranges)
    }

    /// How many there are.
    pub fn len(&self) -> usize {
        self.0
            .iter()
            .map(|(from, to)| (to - from) as usize + 1)
            .sum()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, uid: u32) -> bool {
        self.0
            .binary_search_by(|(from, to)| {
                if uid < *from {
                    std::cmp::Ordering::Greater
                } else if uid > *to {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    /// Every one, newest (highest) first.
    pub fn newest_first(&self) -> impl Iterator<Item = u32> + '_ {
        self.0
            .iter()
            .rev()
            .flat_map(|(from, to)| (*from..=*to).rev())
    }

    /// The `count` highest.
    pub fn newest(&self, count: usize) -> Vec<u32> {
        self.newest_first().take(count).collect()
    }

    /// This set and `more`.
    #[must_use]
    pub fn with(&self, more: impl IntoIterator<Item = u32>) -> Self {
        Self::of(self.newest_first().chain(more))
    }

    /// This set less `gone`.
    #[must_use]
    pub fn without(&self, gone: impl IntoIterator<Item = u32>) -> Self {
        let gone = Self::of(gone);
        Self::of(self.newest_first().filter(|uid| !gone.contains(*uid)))
    }

    /// Those of this set that `other` doesn't have.
    pub fn minus(&self, other: &Self) -> Vec<u32> {
        self.newest_first()
            .filter(|uid| !other.contains(*uid))
            .collect()
    }

    /// The set written as `UID` commands take it, or `None` if it is empty.
    pub fn parse(text: &str) -> Option<Self> {
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        for part in text.split(',').filter(|p| !p.is_empty()) {
            let (from, to) = match part.split_once(':') {
                Some((from, to)) => (from.parse().ok()?, to.parse().ok()?),
                None => {
                    let one = part.parse().ok()?;
                    (one, one)
                }
            };
            if from > to || ranges.last().is_some_and(|(_, last)| from <= *last + 1) {
                return None;
            }
            ranges.push((from, to));
        }
        Some(Self(ranges))
    }
}

impl fmt::Display for UidSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (at, (from, to)) in self.0.iter().enumerate() {
            if at > 0 {
                f.write_str(",")?;
            }
            if from == to {
                write!(f, "{from}")?;
            } else {
                write!(f, "{from}:{to}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_are_ranges() {
        let set = UidSet::of([5, 1, 2, 3, 9, 3]);
        assert_eq!(set.to_string(), "1:3,5,9");
        assert_eq!(set.len(), 5);
        assert!(set.contains(2) && set.contains(9) && !set.contains(4) && !set.contains(10));
    }

    #[test]
    fn writing_and_reading_agree() {
        for text in ["", "7", "1:3,5,9:12"] {
            let set = UidSet::parse(text).unwrap();
            assert_eq!(set.to_string(), text);
        }
        for bad in ["a", "3:1", "1:3,2:5", "5,1", "1:3,4"] {
            assert!(UidSet::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn sets_are_added_and_taken_from() {
        let set = UidSet::of([1, 2, 3, 7]);
        assert_eq!(set.with([4, 8]).to_string(), "1:4,7:8");
        assert_eq!(set.without([2, 7]).to_string(), "1,3");
        assert_eq!(set.minus(&UidSet::of([2, 3])), [7, 1]);
        assert_eq!(set.newest(3), [7, 3, 2]);
        assert!(UidSet::default().is_empty());
    }
}
