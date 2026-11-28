//! Durable append-only JSONL event log for mesh synchronization.

use super::{SyncEvent, VectorClock};
use anyhow::{Context, Result};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use tracing::warn;

/// Append-only JSONL log of `SyncEvent`s with bounded in-memory retention
/// and atomic compaction.
pub struct SyncLog {
    path: PathBuf,
    max_entries: usize,
    events: Vec<SyncEvent>,
    disk_lines: usize,
}

impl SyncLog {
    /// Load an existing JSONL log from `path`, or initialize an empty log if it
    /// does not exist yet.
    pub fn load(path: PathBuf, max_entries: usize) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                path,
                max_entries,
                events: Vec::new(),
                disk_lines: 0,
            });
        }

        restrict_to_owner(&path)?;

        let file = File::open(&path)
            .with_context(|| format!("Failed to open sync log at {}", path.display()))?;
        let reader = BufReader::new(file);

        let mut events = Vec::new();
        let mut seen_ids = HashSet::new();
        let mut disk_lines = 0usize;

        for (idx, line_res) in reader.lines().enumerate() {
            let line = match line_res {
                Ok(l) => l,
                Err(e) => {
                    warn!(
                        line = idx + 1,
                        error = %e,
                        "Skipping unreadable line in sync log"
                    );
                    continue;
                }
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            disk_lines += 1;

            match serde_json::from_str::<SyncEvent>(trimmed) {
                Ok(event) => {
                    if seen_ids.insert(event.id.clone()) {
                        events.push(event);
                    }
                }
                Err(e) => {
                    warn!(
                        line = idx + 1,
                        error = %e,
                        "Skipping corrupt JSONL line in sync log"
                    );
                }
            }
        }

        sort_and_trim(&mut events, max_entries);

        Ok(Self {
            path,
            max_entries,
            events,
            disk_lines,
        })
    }

    #[allow(dead_code)]
    pub fn events(&self) -> &[SyncEvent] {
        &self.events
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Reconstruct the merged `VectorClock` across all retained events.
    pub fn reconstructed_clock(&self) -> VectorClock {
        let mut clock = VectorClock::default();
        for event in &self.events {
            clock.merge(&event.clock);
        }
        clock
    }

    pub fn contains(&self, event_id: &str) -> bool {
        self.events.iter().any(|e| e.id == event_id)
    }

    /// Append a new `SyncEvent` to the log. Returns `Ok(false)` if an event with
    /// the same `id` is already present.
    pub fn append(&mut self, event: SyncEvent) -> Result<bool> {
        if self.contains(&event.id) {
            return Ok(false);
        }

        let serialized =
            serde_json::to_string(&event).context("Failed to serialize SyncEvent to JSON")?;

        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("Failed to create sync log directory {}", parent.display())
                })?;
            }
        }

        let mut file = open_append_owner_only(&self.path)?;
        writeln!(file, "{serialized}").context("Failed to write event to sync log")?;
        file.flush().context("Failed to flush sync log")?;

        self.events.push(event);
        sort_and_trim(&mut self.events, self.max_entries);
        self.disk_lines += 1;

        if self.max_entries > 0 && self.disk_lines > self.max_entries.saturating_mul(2) {
            self.compact()?;
        }

        Ok(true)
    }

    /// Atomically rewrite the on-disk log to match the retained in-memory events.
    pub fn compact(&mut self) -> Result<()> {
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create sync log directory {}", parent.display()))?;

        let tmp_name = format!(
            ".{}.tmp.{}",
            self.path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("sync.jsonl"),
            uuid::Uuid::new_v4()
        );
        let tmp_path = parent.join(tmp_name);

        let write_res = (|| -> Result<()> {
            let mut tmp_file = create_new_owner_only(&tmp_path)?;
            for event in &self.events {
                let line = serde_json::to_string(event)
                    .context("Failed to serialize SyncEvent during compaction")?;
                writeln!(tmp_file, "{line}").context("Failed to write compacted event")?;
            }
            tmp_file
                .sync_all()
                .context("Failed to fsync compacted sync log")?;
            drop(tmp_file);
            std::fs::rename(&tmp_path, &self.path)
                .context("Failed to atomically replace sync log")?;
            Ok(())
        })();

        if let Err(err) = write_res {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }

        self.disk_lines = self.events.len();
        Ok(())
    }

    /// Return retained events in causal order that `peer_clock` has not yet seen.
    pub fn events_missing_for(&self, peer_clock: &VectorClock) -> Vec<SyncEvent> {
        self.events
            .iter()
            .filter(|e| {
                if let Some(&seq) = e.clock.map.get(&e.device_id) {
                    seq > peer_clock.map.get(&e.device_id).copied().unwrap_or(0)
                } else {
                    e.clock.map.is_empty()
                        || e.clock
                            .map
                            .iter()
                            .any(|(k, &v)| v > peer_clock.map.get(k).copied().unwrap_or(0))
                }
            })
            .cloned()
            .collect()
    }
}

fn sort_and_trim(events: &mut Vec<SyncEvent>, max_entries: usize) {
    events.sort_by(|a, b| {
        let a_ahead = a.clock.is_ahead_of(&b.clock);
        let b_ahead = b.clock.is_ahead_of(&a.clock);
        match (a_ahead, b_ahead) {
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            _ => a.timestamp.cmp(&b.timestamp).then_with(|| a.id.cmp(&b.id)),
        }
    });

    if max_entries == 0 {
        events.clear();
    } else if events.len() > max_entries {
        let excess = events.len() - max_entries;
        events.drain(0..excess);
    }
}

fn open_append_owner_only(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .with_context(|| format!("Failed to open sync log for append at {}", path.display()))
}

fn create_new_owner_only(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .with_context(|| format!("Failed to create temp sync log at {}", path.display()))
}

fn restrict_to_owner(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("Failed to stat sync log at {}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            warn!(
                path = %path.display(),
                mode = format!("{:o}", mode & 0o777),
                "Sync log had permissive mode; restricting to 0600"
            );
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("Failed to restrict permissions on {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::SyncOperation;
    use chrono::Utc;
    use std::collections::HashMap;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mycel-sync-log-test-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_event(id: &str, device_id: &str, seq: u64) -> SyncEvent {
        let mut map = HashMap::new();
        map.insert(device_id.to_string(), seq);
        SyncEvent {
            id: id.to_string(),
            device_id: device_id.to_string(),
            timestamp: Utc::now(),
            clock: VectorClock { map },
            operation: SyncOperation::UpdatePreference {
                key: format!("k-{id}"),
                value: format!("v-{id}"),
            },
            signature: Vec::new(),
        }
    }

    #[test]
    fn test_load_append_reload_and_dedup() {
        let dir = temp_dir("reload");
        let log_path = dir.join("events.jsonl");

        let mut log = SyncLog::load(log_path.clone(), 10).unwrap();
        assert!(log.is_empty());

        let e1 = make_event("e1", "devA", 1);
        let e2 = make_event("e2", "devA", 2);
        assert!(log.append(e1.clone()).unwrap());
        assert!(!log.append(e1).unwrap());
        assert!(log.append(e2).unwrap());
        assert_eq!(log.len(), 2);

        let reloaded = SyncLog::load(log_path, 10).unwrap();
        assert_eq!(reloaded.len(), 2);
        assert!(reloaded.contains("e1"));
        assert!(reloaded.contains("e2"));
        assert_eq!(reloaded.reconstructed_clock().map.get("devA"), Some(&2));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_file_permissions_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("perms");
        let log_path = dir.join("events.jsonl");

        let mut log = SyncLog::load(log_path.clone(), 10).unwrap();
        log.append(make_event("e1", "devA", 1)).unwrap();

        let mode = std::fs::metadata(&log_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // Make permissive, then verify load restricts it back to 0600
        std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let _reloaded = SyncLog::load(log_path.clone(), 10).unwrap();
        let restricted = std::fs::metadata(&log_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(restricted, 0o600);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_compaction_when_exceeding_double_max_entries() {
        let dir = temp_dir("compact");
        let log_path = dir.join("events.jsonl");

        let max_entries = 3;
        let mut log = SyncLog::load(log_path.clone(), max_entries).unwrap();

        // Appending 7 events exceeds 2 * 3 = 6 disk lines and triggers compaction
        for i in 1..=7 {
            assert!(log
                .append(make_event(&format!("e{i}"), "devA", i as u64))
                .unwrap());
        }

        assert_eq!(log.len(), 3);
        let raw_lines = std::fs::read_to_string(&log_path)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        assert_eq!(raw_lines, 3);

        let reloaded = SyncLog::load(log_path, max_entries).unwrap();
        assert_eq!(reloaded.len(), 3);
        assert!(reloaded.contains("e5"));
        assert!(reloaded.contains("e6"));
        assert!(reloaded.contains("e7"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_corrupt_line_resilience() {
        let dir = temp_dir("corrupt");
        let log_path = dir.join("events.jsonl");

        let e1 = serde_json::to_string(&make_event("e1", "devA", 1)).unwrap();
        let e2 = serde_json::to_string(&make_event("e2", "devA", 2)).unwrap();
        let content = format!("{e1}\n{{\"partial_corrupt\":\n\n{e2}\n");
        std::fs::write(&log_path, content).unwrap();

        let log = SyncLog::load(log_path, 10).unwrap();
        assert_eq!(log.len(), 2);
        assert!(log.contains("e1"));
        assert!(log.contains("e2"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_events_missing_for_precision() {
        let dir = temp_dir("missing");
        let log_path = dir.join("events.jsonl");

        let mut log = SyncLog::load(log_path, 20).unwrap();
        log.append(make_event("a1", "devA", 1)).unwrap();
        log.append(make_event("a2", "devA", 2)).unwrap();
        log.append(make_event("b1", "devB", 1)).unwrap();

        let mut peer_clock = VectorClock::default();
        peer_clock.map.insert("devA".to_string(), 1);

        let missing = log.events_missing_for(&peer_clock);
        let ids: Vec<&str> = missing.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"a2"));
        assert!(ids.contains(&"b1"));

        let mut up_to_date = VectorClock::default();
        up_to_date.map.insert("devA".to_string(), 2);
        up_to_date.map.insert("devB".to_string(), 1);
        assert!(log.events_missing_for(&up_to_date).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
