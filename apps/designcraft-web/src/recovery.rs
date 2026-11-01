//! Crash recovery in the browser: while a document has unsaved changes a copy of it is kept in
//! IndexedDB (craft-libs' `craft-web-recovery`: one store per tab session, held by a Web Lock so a
//! second tab leaves a running tab's copies alone, a write counted only once IndexedDB has
//! committed it). The desktop keeps its copies in a recovery folder (`file.recovery.save` on a
//! timer, `designcraft_engine::recovery`), which the browser doesn't have.
//!
//! Startup reads the copies of tabs that are gone before the app is built and reopens them as
//! unsaved documents, as `file.recovery.open` does on the desktop. A copy goes when its document
//! is saved, closed or reverted to its saved state.

use std::collections::HashMap;

use craft_web_recovery::{Opened, Recovered, Snapshot, Store};
use designcraft_engine::{DocState, Session};

/// The least time between two rounds of copies while documents are being edited (milliseconds):
/// encoding runs on the page.
const EVERY_MS: f64 = 10_000.0;

/// What [`open`] found, for the app to be built with.
pub struct Startup {
    pub store: Store,
    pub recovered: Vec<Recovered>,
    pub warnings: Vec<String>,
}

pub async fn open() -> Startup {
    let Opened { store, recovered, warnings } = craft_web_recovery::open("designcraft").await;
    Startup { store, recovered, warnings: warnings.iter().map(ToString::to_string).collect() }
}

/// Reopen every copy in `session` as an unsaved document → the copies now open (document uid,
/// store key, revision) and a message for the status bar.
pub fn restore(session: &mut Session, recovered: &[Recovered]) -> (Vec<(u64, String, u64)>, Option<String>) {
    let mut adopted = Vec::new();
    let mut names = Vec::new();
    // Newest first: open the oldest first, so the newest ends up the active document.
    for r in recovered.iter().rev() {
        match designcraft_engine::cmd::from_bytes(&r.bytes) {
            Ok(doc) => {
                let mut st = DocState::new(doc, r.path.clone());
                // Unsaved: the file it was saved to (if any) is older than this copy.
                st.saved_doc = std::sync::Arc::new((*st.doc).clone());
                st.bump_revision();
                let (uid, revision) = (st.uid, st.revision);
                session.add_document(st);
                adopted.push((uid, r.key.clone(), revision));
                names.push(r.name.clone());
            }
            Err(e) => log::warn!("recovery copy “{}” could not be opened: {e}", r.name),
        }
    }
    let message = match names.as_slice() {
        [] => None,
        [one] => Some(format!("Recovered “{one}” from this browser. Save it to keep it.")),
        many => Some(format!("Recovered {} documents from this browser. Save them to keep them.", many.len())),
    };
    (adopted, message)
}

/// One open document's copy.
struct Entry {
    key: String,
    /// The revision last handed to the store.
    queued: Option<u64>,
}

/// The copies of the open documents, kept up to date once per frame and when the tab is hidden.
pub struct WebRecovery {
    store: Store,
    /// By document uid.
    entries: HashMap<u64, Entry>,
    last: f64,
}

impl WebRecovery {
    pub fn new(store: Store, adopted: Vec<(u64, String, u64)>) -> Self {
        let entries = adopted.into_iter().map(|(uid, key, revision)| (uid, Entry { key, queued: Some(revision) })).collect();
        Self { store, entries, last: f64::NEG_INFINITY }
    }

    /// Wake an idle page when a write finishes, so its outcome is picked up.
    pub fn set_waker(&self, ctx: &egui::Context) {
        let ctx = ctx.clone();
        self.store.set_waker(move || ctx.request_repaint());
    }

    /// Whether any open document has unsaved changes (New Document's preview isn't one yet).
    pub fn unsaved(session: &Session) -> bool {
        let preview = session.new_document_preview();
        session.documents().iter().any(|d| d.is_dirty() && preview != Some(d.uid))
    }

    /// Write what changed, at most once every [`EVERY_MS`]; `now` writes it at once (the tab is
    /// being hidden, the last moment the browser promises before it may discard the page).
    pub fn tick(&mut self, session: &Session, ctx: Option<&egui::Context>, now: bool) {
        for w in self.store.take_written() {
            if let Err(e) = w.result {
                log::warn!("recovery copy not written: {e}");
                // Write the same revision again at the next interval.
                if let Some(entry) = self.entries.values_mut().find(|entry| entry.key == w.key && entry.queued == Some(w.revision)) {
                    entry.queued = None;
                }
            }
        }
        let preview = session.new_document_preview();
        let docs = session.documents();
        // Saved, closed or back at its saved state: the copy goes.
        let gone: Vec<u64> =
            self.entries.keys().copied().filter(|uid| !docs.iter().any(|d| d.uid == *uid && d.is_dirty() && preview != Some(d.uid))).collect();
        for uid in gone {
            if let Some(entry) = self.entries.remove(&uid) {
                self.store.remove(&entry.key);
            }
        }
        let changed: Vec<&DocState> = docs
            .iter()
            .filter(|d| d.is_dirty() && preview != Some(d.uid) && self.entries.get(&d.uid).is_none_or(|e| e.queued != Some(d.revision)))
            .collect();
        if changed.is_empty() {
            return;
        }
        let clock = craft_web_recovery::now_ms();
        if !now && clock - self.last < EVERY_MS {
            if let Some(ctx) = ctx {
                ctx.request_repaint_after(std::time::Duration::from_secs_f64((EVERY_MS - (clock - self.last)) / 1000.0));
            }
            return;
        }
        self.last = clock;
        for d in changed {
            // A data merge preview is not the document: copy the template, as Save does.
            let bytes = match designcraft_format::save(d.preview_stash.as_deref().unwrap_or(d.doc.as_ref())) {
                Ok(b) => b,
                Err(e) => {
                    log::warn!("recovery copy of “{}” not encoded: {e}", d.title());
                    continue;
                }
            };
            let store = &self.store;
            let entry = self.entries.entry(d.uid).or_insert_with(|| Entry { key: store.key(d.uid), queued: None });
            let name = d.title();
            match self.store.put(&entry.key, d.revision, Snapshot { name: &name, path: d.path.as_deref(), meta: None, bytes: &bytes }) {
                Ok(()) => entry.queued = Some(d.revision),
                Err(e) => log::warn!("recovery copy of “{name}” not stored: {e}"),
            }
        }
    }
}
