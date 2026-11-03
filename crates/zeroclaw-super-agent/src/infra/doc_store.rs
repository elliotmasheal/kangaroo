//! The shared Markdown-document store: atomic writes (temp file + rename),
//! a per-store write mutex (single writer), an append-only JSONL revision
//! log with full-rendering snapshots (rollback), and external-edit handling
//! per document policy — restore for the Self document (I-S5), absorb for
//! the UserProfile document (I-U2).

use anyhow::{Context, Result, bail};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::marker::PhantomData;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use zeroclaw_log::{Action, Event, record};

use crate::application::ports::{CommitReceipt, ExternalEditHandling, LoadedDoc};
use crate::domain::{ChangeKind, Fingerprint, RevisionId, TimestampMs};
use crate::infra::document::{fingerprint, join_document, split_document};

/// What happens when the on-disk bytes no longer match the last revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalEditPolicy {
    /// Self: reject the edit and restore the last revision (I-S5).
    Restore,
    /// UserProfile: parse and absorb the edit as a `UserExplicitEdit`
    /// revision (I-U2).
    Absorb,
}

/// One store persists a `DocumentModel` of its own type.
pub(crate) trait DocumentModel: Serialize + DeserializeOwned + Clone + Send + Sync {
    const SCHEMA_VERSION: u32;
    const DOC_NAME: &'static str;
    const LOG_NAME: &'static str;

    fn front_matter(&self) -> Result<String>;
    /// Rebuilds a state from front matter plus the document body. The Self
    /// body is always regenerated, so `Self` ignores it; the UserProfile
    /// body is the user's notes and is preserved.
    fn from_parts(front_matter: &str, body: &str) -> Result<Self>;
    fn body(&self) -> String;
}

#[derive(Debug, Serialize, Deserialize)]
struct RevisionEntry {
    id: u64,
    at: TimestampMs,
    kind: ChangeKind,
    note: String,
    fingerprint: u64,
    bytes: String,
}

/// One revision-log entry is a full rendering snapshot, so rollback and
/// restore are byte-exact without operation replay.
pub(crate) struct MdDocumentStore<S: DocumentModel> {
    dir: Mutex<Dir>,
    policy: ExternalEditPolicy,
    _model: PhantomData<S>,
}

impl<S: DocumentModel> MdDocumentStore<S> {
    pub(crate) fn new(root: &Path, policy: ExternalEditPolicy) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("failed to create the document root {}", root.display()))?;
        let dir = Dir::open_ambient_dir(root, ambient_authority())?;
        Ok(Self {
            dir: Mutex::new(dir),
            policy,
            _model: PhantomData,
        })
    }

    fn lock_dir(&self) -> MutexGuard<'_, Dir> {
        // A poisoned mutex still guards consistent data for this store; the
        // poison comes from a panic in a prior writer, and recovery keeps the
        // single-writer contract intact without panicking here.
        self.dir
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn render(state: &S) -> Result<String> {
        Ok(join_document(&state.front_matter()?, &state.body()))
    }

    fn parse(raw: &str) -> Result<S> {
        let (front_matter, body) = split_document(raw)?;
        S::from_parts(&front_matter, &body)
    }

    fn read_doc(dir: &Dir) -> Result<Option<String>> {
        if !dir.exists(S::DOC_NAME) {
            return Ok(None);
        }
        let mut contents = String::new();
        dir.open(S::DOC_NAME)?
            .read_to_string(&mut contents)
            .with_context(|| format!("failed to read {}", S::DOC_NAME))?;
        Ok(Some(contents))
    }

    fn read_log(dir: &Dir) -> Result<Vec<RevisionEntry>> {
        if !dir.exists(S::LOG_NAME) {
            return Ok(Vec::new());
        }
        let mut contents = String::new();
        dir.open(S::LOG_NAME)?
            .read_to_string(&mut contents)
            .with_context(|| format!("failed to read {}", S::LOG_NAME))?;
        let mut entries = Vec::new();
        for (index, line) in contents.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: RevisionEntry = serde_json::from_str(line)
                .with_context(|| format!("corrupt revision log entry at line {}", index + 1))?;
            entries.push(entry);
        }
        Ok(entries)
    }

    fn append_log(dir: &Dir, entry: &RevisionEntry) -> Result<()> {
        use std::io::Write;
        let line = serde_json::to_string(entry)?;
        let mut file = dir.open_with(
            S::LOG_NAME,
            OpenOptions::new().write(true).create(true).append(true),
        )?;
        writeln!(file, "{line}")?;
        file.sync_all()?;
        Ok(())
    }

    /// Temp file in the same directory + rename: readers never see a partial
    /// document (invariants I-S3 / I-U1).
    fn atomic_write(dir: &Dir, bytes: &str) -> Result<()> {
        use std::io::Write;
        let tmp = format!("{}.tmp", S::DOC_NAME);
        let mut file = dir.open_with(
            &tmp,
            OpenOptions::new().write(true).create(true).truncate(true),
        )?;
        file.write_all(bytes.as_bytes())?;
        file.sync_all()?;
        drop(file);
        if let Err(error) = dir.rename(&tmp, dir, S::DOC_NAME) {
            let _ = dir.remove_file(&tmp);
            return Err(error).context("atomic rename failed");
        }
        Ok(())
    }

    fn next_id(log: &[RevisionEntry]) -> u64 {
        log.last().map_or(1, |entry| entry.id + 1)
    }

    fn load_impl(&self) -> Result<LoadedDoc<S>> {
        let dir = self.lock_dir();
        let log = Self::read_log(&dir)?;
        let file = Self::read_doc(&dir)?;
        match (file, log.last()) {
            (None, None) => Ok(LoadedDoc {
                state: None,
                fingerprint: Fingerprint(0),
                external_edit: None,
            }),
            (None, Some(last)) => {
                // The document was deleted externally; the revision history
                // is the source of truth, so both policies restore it.
                Self::atomic_write(&dir, &last.bytes)?;
                record!(
                    WARN,
                    Event::new(module_path!(), Action::Write).with_attrs(serde_json::json!({
                        "document": S::DOC_NAME,
                        "restored_from_revision": last.id,
                    })),
                    "document was deleted externally and restored from its revision history"
                );
                Ok(LoadedDoc {
                    state: Some(Self::parse(&last.bytes)?),
                    fingerprint: Fingerprint(last.fingerprint),
                    external_edit: Some(ExternalEditHandling::RestoredFromRevision),
                })
            }
            (Some(bytes), None) => match self.policy {
                ExternalEditPolicy::Restore => bail!(
                    "{} exists without revision history; external edits to this document are not adopted",
                    S::DOC_NAME
                ),
                ExternalEditPolicy::Absorb => {
                    let state = Self::parse(&bytes)?;
                    let fp = fingerprint(&bytes).0;
                    Self::append_log(
                        &dir,
                        &RevisionEntry {
                            id: 1,
                            at: 0,
                            kind: ChangeKind::UserExplicitEdit,
                            note: "adopted a pre-existing document without history".into(),
                            fingerprint: fp,
                            bytes,
                        },
                    )?;
                    Ok(LoadedDoc {
                        state: Some(state),
                        fingerprint: Fingerprint(fp),
                        external_edit: Some(ExternalEditHandling::AbsorbedAsUserEdit),
                    })
                }
            },
            (Some(bytes), Some(last)) => {
                let fp_now = fingerprint(&bytes).0;
                if fp_now == last.fingerprint {
                    return Ok(LoadedDoc {
                        state: Some(Self::parse(&bytes)?),
                        fingerprint: Fingerprint(fp_now),
                        external_edit: None,
                    });
                }
                match self.policy {
                    ExternalEditPolicy::Restore => {
                        Self::atomic_write(&dir, &last.bytes)?;
                        record!(
                            WARN,
                            Event::new(module_path!(), Action::Write).with_attrs(
                                serde_json::json!({
                                    "document": S::DOC_NAME,
                                    "restored_from_revision": last.id,
                                })
                            ),
                            "external edit detected and restored from the last revision"
                        );
                        Ok(LoadedDoc {
                            state: Some(Self::parse(&last.bytes)?),
                            fingerprint: Fingerprint(last.fingerprint),
                            external_edit: Some(ExternalEditHandling::RestoredFromRevision),
                        })
                    }
                    ExternalEditPolicy::Absorb => {
                        let state = Self::parse(&bytes).map_err(|error| {
                            anyhow::Error::msg(format!(
                                "the user-edited {} does not parse; fix or remove the file before continuing: {error}",
                                S::DOC_NAME
                            ))
                        })?;
                        let id = Self::next_id(&log);
                        Self::append_log(
                            &dir,
                            &RevisionEntry {
                                id,
                                at: 0,
                                kind: ChangeKind::UserExplicitEdit,
                                note: "external edit absorbed".into(),
                                fingerprint: fp_now,
                                bytes,
                            },
                        )?;
                        record!(
                            INFO,
                            Event::new(module_path!(), Action::Write).with_attrs(
                                serde_json::json!({
                                    "document": S::DOC_NAME,
                                    "revision": id,
                                })
                            ),
                            "external user edit absorbed as a revision"
                        );
                        Ok(LoadedDoc {
                            state: Some(state),
                            fingerprint: Fingerprint(fp_now),
                            external_edit: Some(ExternalEditHandling::AbsorbedAsUserEdit),
                        })
                    }
                }
            }
        }
    }

    fn commit_impl(
        &self,
        expected: Fingerprint,
        state: &S,
        kind: ChangeKind,
        note: &str,
        at: TimestampMs,
    ) -> Result<CommitReceipt> {
        let bytes = Self::render(state)?;
        let fp_new = fingerprint(&bytes).0;
        let dir = self.lock_dir();
        let current_fp = Self::read_doc(&dir)?.map_or(0, |bytes| fingerprint(&bytes).0);
        if current_fp != expected.0 {
            bail!(
                "document changed since load (expected fingerprint {}, found {}); reload and retry",
                expected.0,
                current_fp
            );
        }
        let log = Self::read_log(&dir)?;
        let id = Self::next_id(&log);
        Self::atomic_write(&dir, &bytes)?;
        Self::append_log(
            &dir,
            &RevisionEntry {
                id,
                at,
                kind,
                note: note.to_owned(),
                fingerprint: fp_new,
                bytes,
            },
        )?;
        record!(
            INFO,
            Event::new(module_path!(), Action::Write).with_attrs(serde_json::json!({
                "document": S::DOC_NAME,
                "revision": id,
            })),
            "document revision committed"
        );
        Ok(CommitReceipt {
            revision: RevisionId(id),
            fingerprint: Fingerprint(fp_new),
        })
    }

    fn rollback_impl(
        &self,
        expected: Fingerprint,
        to: RevisionId,
        at: TimestampMs,
    ) -> Result<CommitReceipt> {
        let dir = self.lock_dir();
        let log = Self::read_log(&dir)?;
        let target = log
            .iter()
            .find(|entry| entry.id == to.0)
            .ok_or_else(|| anyhow::Error::msg(format!("revision {} not found in the log", to.0)))?;
        let current_fp = Self::read_doc(&dir)?.map_or(0, |bytes| fingerprint(&bytes).0);
        if current_fp != expected.0 {
            bail!(
                "document changed since load (expected fingerprint {}, found {}); reload and retry",
                expected.0,
                current_fp
            );
        }
        let id = Self::next_id(&log);
        Self::atomic_write(&dir, &target.bytes)?;
        Self::append_log(
            &dir,
            &RevisionEntry {
                id,
                at,
                kind: ChangeKind::Rollback,
                note: format!("rolled back to revision {}", to.0),
                fingerprint: target.fingerprint,
                bytes: target.bytes.clone(),
            },
        )?;
        record!(
            INFO,
            Event::new(module_path!(), Action::Write).with_attrs(serde_json::json!({
                "document": S::DOC_NAME,
                "revision": id,
                "rolled_back_to": to.0,
            })),
            "document rolled back to an earlier revision"
        );
        Ok(CommitReceipt {
            revision: RevisionId(id),
            fingerprint: Fingerprint(target.fingerprint),
        })
    }
}

use crate::domain::profile::UserProfile;
use crate::domain::selfhood::SelfState;

impl crate::application::ports::SelfDocumentStore for MdDocumentStore<SelfState> {
    fn load(&self) -> Result<LoadedDoc<SelfState>> {
        self.load_impl()
    }

    fn commit(
        &self,
        expected: Fingerprint,
        state: &SelfState,
        kind: ChangeKind,
        note: &str,
        at: TimestampMs,
    ) -> Result<CommitReceipt> {
        self.commit_impl(expected, state, kind, note, at)
    }

    fn rollback(
        &self,
        expected: Fingerprint,
        to: RevisionId,
        at: TimestampMs,
    ) -> Result<CommitReceipt> {
        self.rollback_impl(expected, to, at)
    }
}

impl crate::application::ports::ProfileDocumentStore for MdDocumentStore<UserProfile> {
    fn load(&self) -> Result<LoadedDoc<UserProfile>> {
        self.load_impl()
    }

    fn commit(
        &self,
        expected: Fingerprint,
        state: &UserProfile,
        kind: ChangeKind,
        note: &str,
        at: TimestampMs,
    ) -> Result<CommitReceipt> {
        self.commit_impl(expected, state, kind, note, at)
    }

    fn rollback(
        &self,
        expected: Fingerprint,
        to: RevisionId,
        at: TimestampMs,
    ) -> Result<CommitReceipt> {
        self.rollback_impl(expected, to, at)
    }
}
