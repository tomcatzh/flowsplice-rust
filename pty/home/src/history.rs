//! Attachment-local immutable snapshots: live tmux history offsets move as output arrives.
use anyhow::{Context, Result, bail};
use flowsplice_pty_protocol::{
    MAX_HISTORY_LINES, MAX_HISTORY_PAGE_BYTES, MAX_HISTORY_PAGE_LINES, Reply,
};
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, SemaphorePermit};
use tokio::time::Instant;
use uuid::Uuid;

pub(crate) const MAX_CAPTURE_BYTES: usize = 64 * 1024 * 1024;
// Bound both the transient capture and retained snapshots across all service domains.
pub(crate) static CAPTURE_SLOT: Semaphore = Semaphore::const_new(1);
static CACHE_KIB: Semaphore = Semaphore::const_new(256 * 1024);
pub(crate) const SNAPSHOT_IDLE: std::time::Duration = std::time::Duration::from_secs(180);
pub(crate) fn connection_budget() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(64 * 1024))
}

#[derive(Debug)]
pub(crate) struct SnapshotExpired;
impl std::fmt::Display for SnapshotExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("history snapshot expired; start a new capture")
    }
}
impl std::error::Error for SnapshotExpired {}

pub(crate) struct Snapshot {
    pub id: Uuid,
    columns: u16,
    lines: Vec<String>,
    sizes: Vec<usize>,
    cache_budget: SemaphorePermit<'static>,
    connection_budget: Option<OwnedSemaphorePermit>,
    last_used: Mutex<Instant>,
}
impl Snapshot {
    pub fn parse(id: Uuid, capture: &str) -> Result<Self> {
        let (width, content) = capture
            .split_once('\n')
            .context("missing history dimensions")?;
        let columns: u16 = width.trim().parse()?;
        if !(1..=512).contains(&columns) {
            bail!("invalid history dimensions");
        }
        let mut style = Style::default();
        let mut lines = Vec::new();
        let mut sizes = Vec::new();
        let mut bytes = 0usize;
        for row in content.split_terminator('\n') {
            if lines.len() >= MAX_HISTORY_LINES as usize {
                bail!("too many captured history rows");
            }
            // tmux emits attribute differences across newlines. Each cached row must
            // start with its own style so pages can render in either direction.
            let line = format!("{}{row}", style.prefix());
            style.consume(row);
            let size = serde_json::to_vec(&line)?.len();
            if size + 2 > MAX_HISTORY_PAGE_BYTES {
                bail!("history row exceeds page budget");
            }
            bytes = bytes
                .checked_add(
                    line.capacity() + std::mem::size_of::<String>() + std::mem::size_of::<usize>(),
                )
                .context("history allocation overflow")?;
            if bytes > MAX_CAPTURE_BYTES {
                bail!("history snapshot exceeds memory budget");
            }
            lines.push(line);
            sizes.push(size);
        }
        lines.shrink_to_fit();
        sizes.shrink_to_fit();
        let permits = u32::try_from(bytes.div_ceil(1024).max(1))?;
        let budget = CACHE_KIB
            .try_acquire_many(permits)
            .context("history cache is busy; close unused terminal tabs and retry")?;
        Ok(Self {
            id,
            columns,
            lines,
            sizes,
            cache_budget: budget,
            connection_budget: None,
            last_used: Mutex::new(Instant::now()),
        })
    }
    pub fn parse_for_connection(id: Uuid, capture: &str, budget: Arc<Semaphore>) -> Result<Self> {
        let mut snapshot = Self::parse(id, capture)?;
        let permits = u32::try_from(snapshot.cache_budget.num_permits())?;
        snapshot.connection_budget = Some(budget.try_acquire_many_owned(permits).context(
            "this connection's history cache is full; close unused tabs or retry later",
        )?);
        Ok(snapshot)
    }
    pub fn expired_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(
            *self
                .last_used
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        ) >= SNAPSHOT_IDLE
    }
    pub fn page(&self, attachment_id: Uuid, before: Option<u32>) -> Result<Reply> {
        let now = Instant::now();
        if self.expired_at(now) {
            return Err(SnapshotExpired.into());
        }
        *self
            .last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = now;
        let end = before.map_or(self.lines.len(), |n| n as usize);
        if end > self.lines.len() {
            bail!("history cursor outside snapshot");
        }
        let mut start = end;
        let mut size = 2; // JSON array delimiters.
        while start > 0 && end - start < MAX_HISTORY_PAGE_LINES {
            let next = self.sizes[start - 1] + usize::from(start != end);
            if size + next > MAX_HISTORY_PAGE_BYTES {
                break;
            }
            size += next;
            start -= 1;
        }
        Ok(Reply::History {
            attachment_id,
            capture_id: self.id,
            total_lines: u32::try_from(self.lines.len())?,
            start: u32::try_from(start)?,
            columns: self.columns,
            lines: self.lines[start..end].to_vec(),
        })
    }
}

#[derive(Default)]
struct Style {
    slots: [Option<String>; 16],
}
impl Style {
    fn prefix(&self) -> String {
        let values: Vec<&str> = self.slots.iter().filter_map(|s| s.as_deref()).collect();
        if values.is_empty() {
            "\x1b[0m".into()
        } else {
            format!("\x1b[0;{}m", values.join(";"))
        }
    }
    fn consume(&mut self, text: &str) {
        let mut remaining = text;
        while let Some((_, tail)) = remaining.split_once("\x1b[") {
            let Some(end) = tail.find(|c: char| !c.is_ascii_digit() && c != ';' && c != ':') else {
                break;
            };
            if tail.as_bytes()[end] == b'm' {
                self.apply(&tail[..end]);
            }
            remaining = &tail[end..];
        }
    }
    #[allow(clippy::too_many_lines)] // Keep the SGR state transitions in one auditable table.
    fn apply(&mut self, sequence: &str) {
        let parts: Vec<&str> = sequence.split(';').collect();
        let mut i = 0;
        while i < parts.len() {
            let part = parts[i];
            let code = part
                .split(':')
                .next()
                .unwrap_or("")
                .parse::<u16>()
                .unwrap_or(0);
            let slot = match code {
                0 => {
                    self.slots.fill(None);
                    None
                }
                1 => Some(0),
                2 => Some(1),
                3 => Some(2),
                4 | 21 => Some(3),
                5 | 6 => Some(4),
                7 => Some(5),
                8 => Some(6),
                9 => Some(7),
                22 => {
                    self.slots[0] = None;
                    self.slots[1] = None;
                    None
                }
                23 => {
                    self.slots[2] = None;
                    None
                }
                24 => {
                    self.slots[3] = None;
                    None
                }
                25 => {
                    self.slots[4] = None;
                    None
                }
                27 => {
                    self.slots[5] = None;
                    None
                }
                28 => {
                    self.slots[6] = None;
                    None
                }
                29 => {
                    self.slots[7] = None;
                    None
                }
                30..=38 | 90..=97 => Some(8),
                39 => {
                    self.slots[8] = None;
                    None
                }
                40..=48 | 100..=107 => Some(9),
                49 => {
                    self.slots[9] = None;
                    None
                }
                51 | 52 => Some(10),
                53 => Some(11),
                54 => {
                    self.slots[10] = None;
                    None
                }
                55 => {
                    self.slots[11] = None;
                    None
                }
                58 => Some(12),
                59 => {
                    self.slots[12] = None;
                    None
                }
                10..=20 => Some(13),
                60..=64 => Some(14),
                65 => {
                    self.slots[14] = None;
                    None
                }
                73 | 74 => Some(15),
                75 => {
                    self.slots[15] = None;
                    None
                }
                _ => None,
            };
            let mut end = i + 1;
            if matches!(code, 38 | 48 | 58) && !part.contains(':') {
                let extra = match parts.get(i + 1) {
                    Some(&"2") => 4,
                    Some(&"5") => 2,
                    _ => 0,
                };
                end = (end + extra).min(parts.len());
            }
            if let Some(slot) = slot {
                self.slots[slot] = Some(parts[i..end].join(";"));
            }
            i = end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_preserve_inherited_attributes_and_boundaries() -> Result<()> {
        let capture =
            "80\n\x1b[1;38;2;12;34;56mred\n中文\n\x1b[22;39;48:2::1:2:3mbackground\nnext\n";
        let snapshot = Snapshot::parse(Uuid::new_v4(), capture)?;
        assert!(snapshot.lines[1].starts_with("\x1b[0;1;38;2;12;34;56m"));
        assert!(snapshot.lines[3].starts_with("\x1b[0;48:2::1:2:3m"));
        assert!(snapshot.page(Uuid::new_v4(), Some(5)).is_err());
        Ok(())
    }
}

#[cfg(test)]
mod cache_lifetime_tests {
    use super::*;

    #[test]
    fn connection_quota_is_released_and_snapshots_have_an_idle_expiry() -> Result<()> {
        let budget = Arc::new(Semaphore::new(1));
        let first =
            Snapshot::parse_for_connection(Uuid::new_v4(), "80\nrow\n", Arc::clone(&budget))?;
        assert_eq!(budget.available_permits(), 0);
        assert!(
            Snapshot::parse_for_connection(Uuid::new_v4(), "80\nnext\n", Arc::clone(&budget))
                .is_err()
        );
        assert!(!first.expired_at(Instant::now()));
        assert!(first.expired_at(Instant::now() + SNAPSHOT_IDLE));
        first.page(Uuid::new_v4(), None)?;
        assert!(!first.expired_at(Instant::now()));
        *first
            .last_used
            .lock()
            .map_err(|_| anyhow::anyhow!("snapshot timestamp lock poisoned"))? =
            Instant::now() - SNAPSHOT_IDLE;
        let error = first
            .page(Uuid::new_v4(), Some(1))
            .err()
            .context("expired snapshot unexpectedly returned a page")?;
        assert!(error.is::<SnapshotExpired>());
        drop(first);
        assert_eq!(budget.available_permits(), 1);
        assert!(Snapshot::parse_for_connection(Uuid::new_v4(), "80\nnext\n", budget).is_ok());
        Ok(())
    }
}
