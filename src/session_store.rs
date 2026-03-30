use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// How often we allow disk writes (debounce interval).
const SAVE_DEBOUNCE_SECS: u64 = 1;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PersistentSession {
    pub peer_id: String,
    pub name: String,
    pub session_id: u64,
    pub random_password: String,
    pub tfa: bool,
    /// Unix timestamp (seconds) of last recv time.
    pub last_recv_time: i64,
}

/// On-disk representation of a login-failure counter.
///
/// The tuple `(last_minute, minute_counter, total_counter)` mirrors the
/// in-memory `(i32, i32, i32)` used by `LOGIN_FAILURES` in connection.rs.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LoginFailureRecord {
    pub last_minute: i32,
    pub minute_counter: i32,
    pub total_counter: i32,
}

impl LoginFailureRecord {
    pub fn as_tuple(&self) -> (i32, i32, i32) {
        (self.last_minute, self.minute_counter, self.total_counter)
    }

    pub fn from_tuple(t: (i32, i32, i32)) -> Self {
        Self {
            last_minute: t.0,
            minute_counter: t.1,
            total_counter: t.2,
        }
    }
}

/// Top-level JSON envelope written to disk.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct StoreData {
    sessions: HashMap<String, PersistentSession>,
    /// Two independent failure maps, matching `LOGIN_FAILURES[0]` and `[1]`.
    failures: [HashMap<String, LoginFailureRecord>; 2],
}

/// Thread-safe, disk-backed session and failure store.
///
/// All public methods acquire the inner lock, mutate state, and (when dirty)
/// flush to disk at most once per `SAVE_DEBOUNCE_SECS`.
pub struct SessionStore {
    inner: Mutex<SessionStoreInner>,
}

struct SessionStoreInner {
    path: PathBuf,
    data: StoreData,
    last_save: Option<Instant>,
    dirty: bool,
}

/// Encode a `SessionKey`-equivalent triple into a single string key for the
/// JSON map.  Format: `peer_id\0name\0session_id`.
pub fn session_map_key(peer_id: &str, name: &str, session_id: u64) -> String {
    format!("{}\0{}\0{}", peer_id, name, session_id)
}

impl SessionStore {
    /// Load (or create) a persistent store at the given path.
    pub fn load(path: PathBuf) -> Self {
        let data = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(contents) => serde_json::from_str::<StoreData>(&contents).unwrap_or_else(|e| {
                    hbb_common::log::warn!("session_store: failed to parse {:?}: {}", path, e);
                    StoreData::default()
                }),
                Err(e) => {
                    hbb_common::log::warn!("session_store: failed to read {:?}: {}", path, e);
                    StoreData::default()
                }
            }
        } else {
            StoreData::default()
        };

        SessionStore {
            inner: Mutex::new(SessionStoreInner {
                path,
                data,
                last_save: None,
                dirty: false,
            }),
        }
    }

    // ── sessions ──────────────────────────────────────────────────────

    /// Insert or update a session.  Mirrors `update_or_insert_session`.
    pub fn upsert_session(
        &self,
        key: String,
        peer_id: &str,
        name: &str,
        session_id: u64,
        password: Option<String>,
        tfa: Option<bool>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(existing) = inner.data.sessions.get_mut(&key) {
            if let Some(pw) = password {
                existing.random_password = pw;
            }
            if let Some(t) = tfa {
                existing.tfa = t;
            }
            existing.last_recv_time = now_unix();
        } else {
            inner.data.sessions.insert(
                key,
                PersistentSession {
                    peer_id: peer_id.to_owned(),
                    name: name.to_owned(),
                    session_id,
                    random_password: password.unwrap_or_default(),
                    tfa: tfa.unwrap_or_default(),
                    last_recv_time: now_unix(),
                },
            );
        }
        inner.dirty = true;
        Self::maybe_save(&mut inner);
    }

    /// Update the last_recv_time for an existing session.
    pub fn touch_session(&self, key: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(s) = inner.data.sessions.get_mut(key) {
            s.last_recv_time = now_unix();
            inner.dirty = true;
            Self::maybe_save(&mut inner);
        }
    }

    /// Retrieve a session by key.
    pub fn get_session(&self, key: &str) -> Option<PersistentSession> {
        self.inner.lock().unwrap().data.sessions.get(key).cloned()
    }

    /// Remove a session by key.
    pub fn remove_session(&self, key: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.data.sessions.remove(key).is_some() {
            inner.dirty = true;
            Self::maybe_save(&mut inner);
        }
    }

    /// Remove sessions whose `last_recv_time` is older than `timeout_secs`
    /// seconds ago.
    pub fn cleanup_expired(&self, timeout_secs: i64) {
        let cutoff = now_unix() - timeout_secs;
        let mut inner = self.inner.lock().unwrap();
        let before = inner.data.sessions.len();
        inner
            .data
            .sessions
            .retain(|_, s| s.last_recv_time >= cutoff);
        if inner.data.sessions.len() != before {
            inner.dirty = true;
            Self::maybe_save(&mut inner);
        }
    }

    // ── login failures ────────────────────────────────────────────────

    /// Read-modify-write a failure entry.  `map_idx` is 0 or 1.
    /// Returns the *new* tuple `(last_minute, minute_count, total_count)`.
    pub fn get_failure(&self, map_idx: usize, ip: &str) -> (i32, i32, i32) {
        let inner = self.inner.lock().unwrap();
        inner.data.failures[map_idx]
            .get(ip)
            .map(|r| r.as_tuple())
            .unwrap_or((0, 0, 0))
    }

    /// Insert or overwrite a failure tuple.
    pub fn set_failure(&self, map_idx: usize, ip: &str, val: (i32, i32, i32)) {
        let mut inner = self.inner.lock().unwrap();
        inner.data.failures[map_idx]
            .insert(ip.to_owned(), LoginFailureRecord::from_tuple(val));
        inner.dirty = true;
        Self::maybe_save(&mut inner);
    }

    /// Remove a failure entry.
    pub fn remove_failure(&self, map_idx: usize, ip: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.data.failures[map_idx].remove(ip).is_some() {
            inner.dirty = true;
            Self::maybe_save(&mut inner);
        }
    }

    /// Get all failure keys for a given map index (needed for batch ops on
    /// IPv6 prefixes).
    pub fn get_all_failure_keys(&self, map_idx: usize) -> Vec<String> {
        self.inner.lock().unwrap().data.failures[map_idx]
            .keys()
            .cloned()
            .collect()
    }

    // ── persistence ───────────────────────────────────────────────────

    /// Force an immediate save regardless of debounce.
    pub fn force_save(&self) {
        let mut inner = self.inner.lock().unwrap();
        Self::do_save(&mut inner);
    }

    /// Save to disk if dirty and debounce interval has elapsed.
    fn maybe_save(inner: &mut SessionStoreInner) {
        if !inner.dirty {
            return;
        }
        let dominated = inner
            .last_save
            .map(|t| t.elapsed().as_secs() < SAVE_DEBOUNCE_SECS)
            .unwrap_or(false);
        if dominated {
            return;
        }
        Self::do_save(inner);
    }

    fn do_save(inner: &mut SessionStoreInner) {
        match serde_json::to_string_pretty(&inner.data) {
            Ok(json) => {
                // Atomic write: write to temp, then rename.
                let tmp = inner.path.with_extension("json.tmp");
                let result = (|| -> std::io::Result<()> {
                    let mut f = std::fs::File::create(&tmp)?;
                    f.write_all(json.as_bytes())?;
                    f.sync_all()?;
                    std::fs::rename(&tmp, &inner.path)?;
                    Ok(())
                })();
                if let Err(e) = result {
                    hbb_common::log::error!("session_store: failed to save {:?}: {}", inner.path, e);
                }
            }
            Err(e) => {
                hbb_common::log::error!("session_store: failed to serialize: {}", e);
            }
        }
        inner.last_save = Some(Instant::now());
        inner.dirty = false;
    }

    /// Retrieve the path this store writes to (for testing / diagnostics).
    pub fn path(&self) -> PathBuf {
        self.inner.lock().unwrap().path.clone()
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ── global singleton ─────────────────────────────────────────────────

lazy_static::lazy_static! {
    static ref GLOBAL_STORE: Arc<SessionStore> = {
        let mut path = hbb_common::config::Config::path("");
        // Config::path("") returns the config dir with an empty filename;
        // pop the empty component to get the directory itself.
        if path.file_name().map(|f| f.is_empty()).unwrap_or(true) {
            path.pop();
        }
        path.push("sessions.json");
        Arc::new(SessionStore::load(path))
    };
}

/// Get a reference to the global persistent session store.
pub fn global_store() -> &'static Arc<SessionStore> {
    &GLOBAL_STORE
}

// ══════════════════════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    /// Create a store backed by a temp file.
    fn temp_store() -> (SessionStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let path = dir.path().join("sessions.json");
        (SessionStore::load(path), dir)
    }

    // ── save/load round-trip ──────────────────────────────────────────

    #[test]
    fn round_trip_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");

        {
            let store = SessionStore::load(path.clone());
            let key = session_map_key("peer1", "name1", 42);
            store.upsert_session(
                key.clone(),
                "peer1",
                "name1",
                42,
                Some("secret".into()),
                Some(true),
            );
            store.force_save();
        }

        // Reload from disk.
        let store2 = SessionStore::load(path);
        let key = session_map_key("peer1", "name1", 42);
        let s = store2.get_session(&key).expect("session should persist");
        assert_eq!(s.peer_id, "peer1");
        assert_eq!(s.random_password, "secret");
        assert!(s.tfa);
    }

    #[test]
    fn round_trip_failures() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");

        {
            let store = SessionStore::load(path.clone());
            store.set_failure(0, "1.2.3.4", (100, 3, 15));
            store.set_failure(1, "5.6.7.8", (200, 1, 5));
            store.force_save();
        }

        let store2 = SessionStore::load(path);
        assert_eq!(store2.get_failure(0, "1.2.3.4"), (100, 3, 15));
        assert_eq!(store2.get_failure(1, "5.6.7.8"), (200, 1, 5));
        assert_eq!(store2.get_failure(0, "unknown"), (0, 0, 0));
    }

    // ── session expiry cleanup ────────────────────────────────────────

    #[test]
    fn cleanup_expired_removes_old_sessions() {
        let (store, _dir) = temp_store();
        let key_fresh = session_map_key("fresh", "n", 1);
        let key_stale = session_map_key("stale", "n", 2);

        store.upsert_session(
            key_fresh.clone(),
            "fresh",
            "n",
            1,
            Some("pw".into()),
            None,
        );

        // Insert a session then manually backdate it.
        store.upsert_session(
            key_stale.clone(),
            "stale",
            "n",
            2,
            Some("pw".into()),
            None,
        );
        {
            let mut inner = store.inner.lock().unwrap();
            inner
                .data
                .sessions
                .get_mut(&key_stale)
                .unwrap()
                .last_recv_time = now_unix() - 120; // 2 minutes ago
        }

        store.cleanup_expired(30); // 30-second timeout

        assert!(store.get_session(&key_fresh).is_some());
        assert!(store.get_session(&key_stale).is_none());
    }

    // ── failure counter persists across reload ────────────────────────

    #[test]
    fn failure_counter_persists_across_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");

        let store = SessionStore::load(path.clone());
        store.set_failure(0, "10.0.0.1", (50, 5, 20));
        store.force_save();
        drop(store);

        let store2 = SessionStore::load(path);
        assert_eq!(store2.get_failure(0, "10.0.0.1"), (50, 5, 20));

        // Mutate and reload again.
        store2.set_failure(0, "10.0.0.1", (51, 1, 21));
        store2.force_save();
        drop(store2);

        let store3 = SessionStore::load(dir.path().join("sessions.json"));
        assert_eq!(store3.get_failure(0, "10.0.0.1"), (51, 1, 21));
    }

    // ── concurrent access safety ──────────────────────────────────────

    #[test]
    fn concurrent_access_is_safe() {
        let (store, _dir) = temp_store();
        let store = Arc::new(store);

        let mut handles = Vec::new();
        for i in 0..10 {
            let store = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                let key = session_map_key(&format!("peer{}", i), "n", i as u64);
                store.upsert_session(
                    key.clone(),
                    &format!("peer{}", i),
                    "n",
                    i as u64,
                    Some(format!("pw{}", i)),
                    Some(i % 2 == 0),
                );
                // Also exercise failures concurrently.
                store.set_failure(0, &format!("10.0.0.{}", i), (1, 1, i as i32));
                store.get_session(&key);
                store.get_failure(0, &format!("10.0.0.{}", i));
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }

        // All 10 sessions should exist.
        for i in 0..10 {
            let key = session_map_key(&format!("peer{}", i), "n", i as u64);
            assert!(store.get_session(&key).is_some(), "session {} missing", i);
        }
    }

    // ── upsert updates existing session fields selectively ────────────

    #[test]
    fn upsert_updates_selectively() {
        let (store, _dir) = temp_store();
        let key = session_map_key("p", "n", 1);

        store.upsert_session(key.clone(), "p", "n", 1, Some("old_pw".into()), Some(false));
        store.upsert_session(key.clone(), "p", "n", 1, Some("new_pw".into()), None);

        let s = store.get_session(&key).unwrap();
        assert_eq!(s.random_password, "new_pw");
        assert!(!s.tfa, "tfa should remain unchanged when None passed");

        store.upsert_session(key.clone(), "p", "n", 1, None, Some(true));
        let s = store.get_session(&key).unwrap();
        assert_eq!(s.random_password, "new_pw", "password should remain unchanged");
        assert!(s.tfa);
    }

    // ── remove_session ────────────────────────────────────────────────

    #[test]
    fn remove_session_works() {
        let (store, _dir) = temp_store();
        let key = session_map_key("p", "n", 1);
        store.upsert_session(key.clone(), "p", "n", 1, Some("pw".into()), None);
        assert!(store.get_session(&key).is_some());
        store.remove_session(&key);
        assert!(store.get_session(&key).is_none());
    }

    // ── remove_failure ────────────────────────────────────────────────

    #[test]
    fn remove_failure_works() {
        let (store, _dir) = temp_store();
        store.set_failure(0, "1.2.3.4", (1, 1, 1));
        assert_ne!(store.get_failure(0, "1.2.3.4"), (0, 0, 0));
        store.remove_failure(0, "1.2.3.4");
        assert_eq!(store.get_failure(0, "1.2.3.4"), (0, 0, 0));
    }

    // ── load from missing file creates empty store ────────────────────

    #[test]
    fn load_missing_file_creates_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let store = SessionStore::load(path);
        assert_eq!(store.get_failure(0, "any"), (0, 0, 0));
        assert!(store.get_session("any").is_none());
    }

    // ── load from corrupt file falls back to empty ────────────────────

    #[test]
    fn load_corrupt_file_falls_back_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        std::fs::write(&path, "not valid json!!!").unwrap();
        let store = SessionStore::load(path);
        assert!(store.get_session("any").is_none());
    }

    // ── atomic save does not corrupt on normal operation ──────────────

    #[test]
    fn atomic_save_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let store = SessionStore::load(path.clone());
        store.set_failure(0, "x", (1, 1, 1));
        store.force_save();

        assert!(path.exists(), "sessions.json should exist");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "tmp file should be renamed away"
        );
    }
}
