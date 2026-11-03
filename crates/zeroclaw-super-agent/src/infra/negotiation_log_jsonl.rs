//! Append-only JSONL event log for Given negotiation (I-N4). Independent
//! from both document revision logs; a rejected or withdrawn negotiation
//! produces no document revision (I-N3).

use anyhow::{Context, Result};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use crate::application::ports::NegotiationEventLog;
use crate::domain::selfhood::negotiation::GivenAmendmentEvent;

pub(crate) const NEGOTIATION_LOG_NAME: &str = "given_negotiation.jsonl";

pub(crate) struct JsonlNegotiationLog {
    dir: Mutex<Dir>,
}

impl JsonlNegotiationLog {
    pub(crate) fn new(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("failed to create the document root {}", root.display()))?;
        let dir = Dir::open_ambient_dir(root, ambient_authority())?;
        Ok(Self {
            dir: Mutex::new(dir),
        })
    }

    fn lock_dir(&self) -> MutexGuard<'_, Dir> {
        self.dir
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl NegotiationEventLog for JsonlNegotiationLog {
    fn append(&self, event: &GivenAmendmentEvent) -> Result<()> {
        let line = serde_json::to_string(event)?;
        let dir = self.lock_dir();
        let mut file = dir.open_with(
            NEGOTIATION_LOG_NAME,
            OpenOptions::new().write(true).create(true).append(true),
        )?;
        writeln!(file, "{line}")?;
        file.sync_all()?;
        Ok(())
    }

    fn load_all(&self) -> Result<Vec<GivenAmendmentEvent>> {
        let dir = self.lock_dir();
        if !dir.exists(NEGOTIATION_LOG_NAME) {
            return Ok(Vec::new());
        }
        let mut contents = String::new();
        dir.open(NEGOTIATION_LOG_NAME)?
            .read_to_string(&mut contents)
            .with_context(|| format!("failed to read {NEGOTIATION_LOG_NAME}"))?;
        let mut events = Vec::new();
        for (index, line) in contents.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            events.push(
                serde_json::from_str(line).with_context(|| {
                    format!("corrupt negotiation log entry at line {}", index + 1)
                })?,
            );
        }
        Ok(events)
    }
}
