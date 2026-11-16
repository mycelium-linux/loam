//! The work board: tracked items arranged by status, the way a surface draws
//! them.
//!
//! The tracker owns the vocabulary — Marbles reports `blocked`, `review`,
//! `in_progress`, `open`, `deferred` and `closed` — and this module owns only
//! the arrangement every view has to agree on: the statuses that ask for a
//! person come first, each column carries a total and a page of items, and a
//! status nobody has seen before is appended rather than dropped.
//!
//! The total is the fact that survives paging. A wall with room for three cards
//! a column still has to say "416 in progress", or the person reading it cannot
//! tell a busy board from an empty one.

use soil_marbles::Issue;

/// Column order, most urgent first.
///
/// Ordering is the host's job for the same reason `observe::agents` orders
/// agents that need a person first: a surface with room for one row should not
/// have to know what the tracker means. `open` is the backlog and `closed` is
/// the archive, so a view that wants them collapsed keeps that decision; it
/// just no longer has to guess which end of the list to look at.
pub const STATUS_ORDER: &[&str] = &[
    "blocked",
    "review",
    "in_progress",
    "open",
    "deferred",
    "closed",
];

/// One unit of tracked work, with the fields a board draws.
///
/// `assignee` is the tracker's actor name: the same string an agent's
/// `owner.actor` carries, so a view can join work to an agent without a second
/// key. It is `None` for unclaimed work, which is most of a backlog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItem {
    pub id: String,
    pub title: String,
    pub status: String,
    pub assignee: Option<String>,
    pub labels: Vec<String>,
    pub priority: i64,
    pub project: String,
    pub updated_at: u64,
}

/// One status, its full count, and the newest page of its items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkColumn {
    pub status: String,
    /// Everything in the tracker with this status, not just `items.len()`.
    pub total: usize,
    pub items: Vec<WorkItem>,
}

/// The board: every status the tracker reported, in [`STATUS_ORDER`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkBoard {
    pub columns: Vec<WorkColumn>,
}

impl WorkBoard {
    /// Arrange `issues` into columns.
    ///
    /// `per_column` caps the page each column carries; `0` keeps the counts and
    /// drops the items, which is what a collapsed column needs. Closed items
    /// appear only when the caller asked the tracker for them — this function
    /// neither knows nor cares how the list was filtered.
    pub fn project(issues: &[Issue], per_column: usize) -> Self {
        // Group first, page second: a page cut before the sort is the first
        // `per_column` rows the tracker happened to emit, not the newest ones.
        let mut statuses: Vec<String> = Vec::new();
        let mut groups: Vec<Vec<WorkItem>> = Vec::new();

        for issue in issues {
            let status = if issue.status.is_empty() {
                "unknown".to_string()
            } else {
                issue.status.clone()
            };
            let slot = match statuses.iter().position(|known| *known == status) {
                Some(slot) => slot,
                None => {
                    statuses.push(status.clone());
                    groups.push(Vec::new());
                    statuses.len() - 1
                }
            };
            groups[slot].push(WorkItem {
                id: issue.id.clone(),
                title: issue.title.clone(),
                status,
                assignee: issue.assignee.clone(),
                labels: issue.labels.clone(),
                priority: issue.priority,
                project: issue.project.clone(),
                updated_at: u64::try_from(issue.updated_at).unwrap_or_default(),
            });
        }

        // Newest first inside a column, id as the tiebreak so two items with the
        // same timestamp cannot swap between runs.
        for group in groups.iter_mut() {
            group.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
        }

        let take = |status: &str, mut items: Vec<WorkItem>| WorkColumn {
            status: status.to_string(),
            total: items.len(),
            items: {
                items.truncate(per_column);
                items
            },
        };

        let mut slots: Vec<usize> = (0..statuses.len()).collect();
        slots.sort_by_key(|slot| {
            STATUS_ORDER
                .iter()
                .position(|known| *known == statuses[*slot])
                .unwrap_or(usize::MAX)
        });

        Self {
            columns: slots
                .into_iter()
                .map(|slot| take(&statuses[slot].clone(), std::mem::take(&mut groups[slot])))
                .collect(),
        }
    }

    pub fn column(&self, status: &str) -> Option<&WorkColumn> {
        self.columns.iter().find(|column| column.status == status)
    }

    /// Every item the tracker holds, including the ones not on a page.
    pub fn total(&self) -> usize {
        self.columns.iter().map(|column| column.total).sum()
    }

    /// Work somebody is holding: claimed, in progress, or waiting on review.
    pub fn in_flight(&self) -> usize {
        self.columns
            .iter()
            .filter(|column| matches!(column.status.as_str(), "in_progress" | "review"))
            .map(|column| column.total)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(id: &str, status: &str, assignee: Option<&str>, updated_at: i64) -> Issue {
        Issue {
            id: id.into(),
            title: format!("work {id}"),
            status: status.into(),
            assignee: assignee.map(str::to_string),
            updated_at,
            ..Issue::default()
        }
    }

    #[test]
    fn columns_follow_the_urgency_order_not_the_input_order() {
        let issues = vec![
            issue("a", "open", None, 1),
            issue("b", "review", None, 2),
            issue("c", "blocked", None, 3),
            issue("d", "closed", None, 4),
        ];
        let board = WorkBoard::project(&issues, 5);
        let order: Vec<&str> = board.columns.iter().map(|c| c.status.as_str()).collect();
        assert_eq!(order, vec!["blocked", "review", "open", "closed"]);
    }

    #[test]
    fn a_page_is_the_newest_and_the_total_is_everything() {
        let issues = vec![
            issue("old", "open", None, 10),
            issue("new", "open", None, 30),
            issue("mid", "open", None, 20),
        ];
        let board = WorkBoard::project(&issues, 2);
        let column = board.column("open").expect("open column");
        assert_eq!(column.total, 3);
        assert_eq!(
            column.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["new", "mid"]
        );
    }

    #[test]
    fn zero_per_column_keeps_the_counts_and_drops_the_cards() {
        let issues = vec![issue("a", "closed", None, 1), issue("b", "closed", None, 2)];
        let board = WorkBoard::project(&issues, 0);
        let column = board.column("closed").expect("closed column");
        assert_eq!(column.total, 2);
        assert!(column.items.is_empty());
        assert_eq!(board.total(), 2);
    }

    #[test]
    fn an_unknown_status_is_kept_after_the_known_ones() {
        let issues = vec![
            issue("a", "triage", None, 1),
            issue("b", "open", None, 2),
            issue("c", "triage", None, 3),
        ];
        let board = WorkBoard::project(&issues, 5);
        let order: Vec<&str> = board.columns.iter().map(|c| c.status.as_str()).collect();
        assert_eq!(order, vec!["open", "triage"]);
        assert_eq!(board.column("triage").map(|c| c.total), Some(2));
    }

    #[test]
    fn an_empty_status_is_grouped_rather_than_panicking() {
        let issues = vec![issue("a", "", None, 1)];
        let board = WorkBoard::project(&issues, 5);
        assert_eq!(board.column("unknown").map(|c| c.total), Some(1));
    }

    #[test]
    fn work_in_flight_counts_what_somebody_holds() {
        let issues = vec![
            issue("a", "in_progress", Some("ajmwagar"), 1),
            issue("b", "review", None, 2),
            issue("c", "open", None, 3),
        ];
        let board = WorkBoard::project(&issues, 5);
        assert_eq!(board.in_flight(), 2);
        assert_eq!(board.total(), 3);
    }

    #[test]
    fn an_assignee_survives_so_a_view_can_join_it_to_an_agent() {
        let issues = vec![issue("a", "in_progress", Some("agent-1"), 1)];
        let board = WorkBoard::project(&issues, 5);
        let item = &board.columns[0].items[0];
        assert_eq!(item.assignee.as_deref(), Some("agent-1"));
    }
}
