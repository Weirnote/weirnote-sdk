//! Account sync route and credential policy.
use crate::{
    engine::{Connection, Value},
    probe,
    store::{coordinate_library, Store, StoreError},
};
use std::{
    collections::{HashMap, HashSet},
    ffi::{CStr, CString},
    path::Path,
    sync::{Condvar, Mutex, OnceLock},
    time::{Duration, SystemTime},
};

const REMOTE: &str = "weirnote-account-sync";
const TRACKING: &str = "remotes/weirnote-account-sync/main";

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AccountKey {
    pub environment_id: String,
    pub user_id: String,
}
#[derive(Clone, Debug)]
pub struct Environment {
    pub id: String,
    pub sync_origin: String,
    pub allow_insecure_http: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub account: AccountKey,
    pub route_id: String,
    pub catalog_generation: i64,
}
#[derive(Clone)]
pub struct Credentials {
    pub user_id: String,
    pub access_token: String,
    pub expires_in: Duration,
}
#[derive(Clone)]
pub struct Reservation {
    pub id: String,
    pub secret: String,
    pub expires_at: SystemTime,
    pub catalog_generation: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeSyncError {
    pub category: i32,
    pub operation: i32,
    pub phase: i32,
    pub publication: i32,
    pub sqlite_code: i32,
    pub http_status: Option<i32>,
    pub retry_after_seconds: Option<i32>,
    pub message: String,
    pub request_id: Option<String>,
    pub intended_commit: Option<String>,
    pub remote_branch: Option<String>,
}
impl NativeSyncError {
    pub(crate) unsafe fn copy_from(r: &probe::SyncError) -> Self {
        unsafe fn copy(p: *const std::ffi::c_char) -> Option<String> {
            if p.is_null() {
                None
            } else {
                Some(CStr::from_ptr(p).to_string_lossy().into_owned())
            }
        }
        Self {
            category: r.category,
            operation: r.operation,
            phase: r.phase,
            publication: r.publication,
            sqlite_code: r.sqlite_code,
            http_status: (r.http_status != 0).then_some(r.http_status),
            retry_after_seconds: (r.retry_after >= 0).then_some(r.retry_after),
            message: copy(r.message).unwrap_or_default(),
            request_id: copy(r.request_id),
            intended_commit: copy(r.intended_commit),
            remote_branch: copy(r.remote_branch),
        }
    }
}
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("invalid sync environment")]
    InvalidEnvironment,
    #[error("invalid account")]
    InvalidAccount,
    #[error("invalid sync route")]
    InvalidRoute,
    #[error("session does not match account")]
    MismatchedSession,
    #[error("session expired or empty")]
    ExpiredSession,
    #[error("credential provider unavailable")]
    ProviderUnavailable,
    #[error("route not installed")]
    RouteNotInstalled,
    #[error("route not bound")]
    RouteNotBound,
    #[error("route draining or active")]
    RouteDraining,
    #[error("intended commit changed")]
    IntendedCommitChanged {
        expected: String,
        actual: Option<String>,
    },
    #[error("database operation failed: {0}")]
    Database(String),
    #[error("sync operation failed")]
    Native {
        native: NativeSyncError,
        intended_commit: Option<String>,
    },
    #[error("push publication outcome unknown")]
    PublicationUnknown {
        native: NativeSyncError,
        intended_commit: String,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushReconciliation {
    Equal,
    IntendedIsAncestor,
    RemoteIsAncestor,
    Unrelated,
    Unavailable,
}
type Result<T> = std::result::Result<T, SyncError>;

struct Snapshot {
    account: AccountKey,
    generation: i64,
    token: String,
    expires_at: SystemTime,
    reservation: Option<String>,
}
#[derive(Default)]
struct State {
    snapshots: HashMap<String, Snapshot>,
    active: HashSet<String>,
    draining: HashSet<String>,
    epochs: HashMap<AccountKey, u64>,
}
#[derive(Default)]
struct Registry {
    state: Mutex<State>,
    changed: Condvar,
}
static REGISTRY: OnceLock<Registry> = OnceLock::new();
fn registry() -> &'static Registry {
    REGISTRY.get_or_init(Registry::default)
}

// URL representation is deliberately restricted to ASCII canonical origins and
// route paths. Prefix matching is safe only after rejecting all URL modifiers.
fn origin_parts(s: &str) -> Option<(&str, &str)> {
    if !s.is_ascii()
        || s.bytes().any(|c| c <= b' ' || c == 127)
        || s.contains(['%', '?', '#', '@', '\\', '\''])
    {
        return None;
    }
    let (scheme, rest) = s.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.is_empty()
        || authority.contains('/')
        || authority.contains(':')
            && authority.rsplit_once(':').map_or(true, |(_, p)| {
                p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) || p.parse::<u16>().is_err()
            })
    {
        return None;
    }
    let host = authority.split(':').next()?;
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return None;
    }
    Some((scheme, authority))
}
fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control)
}
fn valid_route_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn native_request(base: &str, method: &str, url: &str) -> bool {
    if url.contains(['?', '#', '@', '%', '\\']) {
        return false;
    }
    let Some(suffix) = url.strip_prefix(base).filter(|s| s.starts_with('/')) else {
        return false;
    };
    match (method, suffix) {
        ("GET", "/root" | "/refs")
        | ("POST", "/has-chunks" | "/get-chunks" | "/chunks" | "/commit")
        | ("PUT", "/refs" | "/refs-if") => true,
        ("GET", s) if s.starts_with("/chunk/") => {
            let h = &s[7..];
            h.len() == 64
                && h.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        _ => false,
    }
}
/// Map a request to headers while retaining the registry lock until the hook
/// finishes consuming them, preserving drain and credential-revocation ordering.
pub(crate) fn with_request_headers<R>(
    method: &str,
    url: &str,
    consume: impl FnOnce(Vec<(&'static str, String)>) -> R,
) -> R {
    let state = registry().state.lock().unwrap_or_else(|e| e.into_inner());
    let mut headers = Vec::new();
    for (base, snapshot) in &state.snapshots {
        if snapshot.expires_at <= SystemTime::now() || !native_request(base, method, url) {
            continue;
        }
        headers.push(("Authorization", format!("Bearer {}", snapshot.token)));
        if let Some(secret) = &snapshot.reservation {
            headers.push(("X-Weirnote-Bootstrap-Reservation", secret.clone()));
        }
        break;
    }
    consume(headers)
}
struct Lease(String);
impl Drop for Lease {
    fn drop(&mut self) {
        let r = registry();
        let mut s = r.state.lock().unwrap_or_else(|e| e.into_inner());
        s.active.remove(&self.0);
        r.changed.notify_all();
    }
}
fn acquire(url: &str, route: &Route) -> Result<Lease> {
    let mut s = registry().state.lock().unwrap_or_else(|e| e.into_inner());
    let snapshot = s.snapshots.get(url).ok_or(SyncError::RouteNotInstalled)?;
    if snapshot.account != route.account || snapshot.generation != route.catalog_generation {
        return Err(SyncError::RouteNotInstalled);
    }
    if snapshot.expires_at <= SystemTime::now() {
        return Err(SyncError::ExpiredSession);
    }
    if s.draining.contains(url) || s.active.contains(url) {
        return Err(SyncError::RouteDraining);
    }
    s.active.insert(url.to_owned());
    Ok(Lease(url.to_owned()))
}
struct Binding {
    route: Route,
    path: std::path::PathBuf,
}
pub struct Transport {
    environment: Environment,
    binding: Option<Binding>,
}
impl Transport {
    pub fn new(mut environment: Environment) -> Result<Self> {
        let (scheme, authority) =
            origin_parts(&environment.sync_origin).ok_or(SyncError::InvalidEnvironment)?;
        let host = authority.split(':').next().unwrap_or("");
        if !valid_id(&environment.id)
            || (scheme == "http"
                && !(environment.allow_insecure_http
                    && (host == "localhost" || host == "127.0.0.1" || host.ends_with(".ts.net"))))
        {
            return Err(SyncError::InvalidEnvironment);
        }
        environment.sync_origin = environment.sync_origin.trim_end_matches('/').to_owned();
        if !crate::header_hook::register_once() {
            return Err(SyncError::ProviderUnavailable);
        }
        Ok(Self {
            environment,
            binding: None,
        })
    }
    fn url(&self, route: &Route) -> Result<String> {
        if !valid_id(&route.account.environment_id) || !valid_id(&route.account.user_id) {
            return Err(SyncError::InvalidAccount);
        }
        if route.account.environment_id != self.environment.id
            || route.catalog_generation < 0
            || !valid_route_id(&route.route_id)
        {
            return Err(SyncError::InvalidRoute);
        }
        Ok(format!(
            "{}/v1/account-sync/remotes/{}/notes",
            self.environment.sync_origin, route.route_id
        ))
    }
    pub fn install(
        &self,
        credentials: Credentials,
        account: &AccountKey,
        route: &Route,
        reservation: Option<Reservation>,
    ) -> Result<()> {
        let url = self.url(route)?;
        if &route.account != account {
            return Err(SyncError::InvalidRoute);
        }
        if credentials.user_id != account.user_id {
            return Err(SyncError::MismatchedSession);
        }
        if credentials.access_token.is_empty()
            || credentials.access_token.bytes().any(|b| b < 32 || b == 127)
            || credentials.expires_in.is_zero()
        {
            return Err(SyncError::ExpiredSession);
        }
        let expires_at = SystemTime::now()
            .checked_add(credentials.expires_in)
            .ok_or(SyncError::ExpiredSession)?;
        let secret = if let Some(r) = reservation {
            if r.id.is_empty()
                || r.secret.is_empty()
                || r.secret.bytes().any(|b| b < 32 || b == 127)
                || r.expires_at <= SystemTime::now()
                || r.catalog_generation != route.catalog_generation
            {
                return Err(SyncError::InvalidRoute);
            }
            Some(r.secret)
        } else {
            None
        };
        let reg = registry();
        let mut s = reg.state.lock().unwrap_or_else(|e| e.into_inner());
        let epoch = *s.epochs.get(account).unwrap_or(&0);
        while s.draining.contains(&url) {
            s = reg.changed.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        if *s.epochs.get(account).unwrap_or(&0) != epoch {
            return Err(SyncError::RouteDraining);
        }
        s.draining.insert(url.clone());
        while s.active.contains(&url) {
            s = reg.changed.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        if *s.epochs.get(account).unwrap_or(&0) != epoch {
            s.draining.remove(&url);
            reg.changed.notify_all();
            return Err(SyncError::RouteDraining);
        }
        s.snapshots.insert(
            url.clone(),
            Snapshot {
                account: account.clone(),
                generation: route.catalog_generation,
                token: credentials.access_token,
                expires_at,
                reservation: secret,
            },
        );
        s.draining.remove(&url);
        reg.changed.notify_all();
        Ok(())
    }
    pub fn drain_and_invalidate(&mut self, account: &AccountKey) {
        let reg = registry();
        let mut s = reg.state.lock().unwrap_or_else(|e| e.into_inner());
        *s.epochs.entry(account.clone()).or_default() += 1;
        let keys: Vec<String> = s
            .snapshots
            .iter()
            .filter(|(_, v)| &v.account == account)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &keys {
            s.draining.insert(k.clone());
        }
        while keys.iter().any(|k| s.active.contains(k)) {
            s = reg.changed.wait(s).unwrap_or_else(|e| e.into_inner());
        }
        for k in &keys {
            s.snapshots.remove(k);
            s.draining.remove(k);
        }
        reg.changed.notify_all();
        if self
            .binding
            .as_ref()
            .is_some_and(|b| &b.route.account == account)
        {
            self.binding = None;
        }
    }
    pub fn unbind(&mut self) {
        self.binding = None;
    }
    pub fn bind(&mut self, path: impl AsRef<Path>, route: &Route) -> Result<()> {
        let url = self.url(route)?;
        let _lease = acquire(&url, route)?;
        if !path.as_ref().is_absolute() {
            return Err(SyncError::RouteNotBound);
        }
        let path = path.as_ref().to_path_buf();
        // A normal rebind only needs to inspect the already-configured
        // current library. A writable connection can take a graph lock even
        // on close, so do not open one for this read-only case.
        let existing = if path.is_file() {
            let reader = Store {
                db: Connection::open_readonly(&path).map_err(db)?,
                path: path.clone(),
            };
            if reader.schema_version()? == 3 {
                reader.validate_schema()?;
                first(
                    &reader.db,
                    "SELECT url FROM dolt_remotes WHERE name = ?",
                    &[Value::from(REMOTE)],
                )?
            } else {
                None
            }
        } else {
            None
        };
        if existing.as_deref() != Some(url.as_str()) {
            // Migration is separately coordinated by Store::open. Retain the
            // same gate for the whole mutation, including writable close.
            let writable = Store::open(&path)?;
            let _gate = coordinate_library(&path)?;
            let store = writable;
            let existing = first(
                &store.db,
                "SELECT url FROM dolt_remotes WHERE name = ?",
                &[Value::from(REMOTE)],
            )?;
            if let Some(old) = existing {
                if old != url {
                    let (old_origin, old_path) =
                        split_route_url(&old).ok_or(SyncError::RouteNotBound)?;
                    let (new_origin, new_path) =
                        split_route_url(&url).ok_or(SyncError::RouteNotBound)?;
                    let _old_lease = {
                        let mut state = registry().state.lock().unwrap_or_else(|e| e.into_inner());
                        if state.active.contains(&old) || state.draining.contains(&old) {
                            return Err(SyncError::RouteDraining);
                        }
                        state.active.insert(old.clone());
                        Lease(old.clone())
                    };
                    if old_path != new_path
                        || origin_parts(old_origin).map(|x| x.0)
                            != origin_parts(new_origin).map(|x| x.0)
                    {
                        return Err(SyncError::RouteNotBound);
                    }
                    store
                        .db
                        .execute(
                            "SELECT dolt_remote('set-url', ?, ?, ?)",
                            &[REMOTE.into(), url.clone().into(), old.into()],
                        )
                        .map_err(db)?;
                }
            } else {
                store
                    .db
                    .execute(
                        "SELECT dolt_remote('add', ?, ?)",
                        &[REMOTE.into(), url.into()],
                    )
                    .map_err(db)?;
            }
        }
        self.binding = Some(Binding {
            route: route.clone(),
            path,
        });
        Ok(())
    }
    fn bound(&self, route: &Route) -> Result<Store> {
        let b = self
            .binding
            .as_ref()
            .filter(|b| &b.route == route)
            .ok_or(SyncError::RouteNotBound)?;
        if !b.path.is_file() {
            return Err(SyncError::RouteNotBound);
        }
        let store = Store::open(&b.path)?;
        let configured = first(
            &store.db,
            "SELECT url FROM dolt_remotes WHERE name = ?",
            &[REMOTE.into()],
        )?;
        if configured.as_deref() != Some(self.url(route)?.as_str()) {
            return Err(SyncError::RouteNotBound);
        }
        Ok(store)
    }
    pub fn remote_head(&self, route: &Route) -> Result<Option<String>> {
        let url = self.url(route)?;
        let _lease = acquire(&url, route)?;
        let url = CString::new(url).map_err(|_| SyncError::InvalidRoute)?;
        let mut out: probe::RemoteHead = unsafe { std::mem::zeroed() };
        out.size = std::mem::size_of_val(&out) as i32;
        let rc = unsafe {
            probe::sqlite3_doltlite_remote_head(url.as_ptr(), c"main".as_ptr(), 30000, &mut out)
        };
        if rc != 0 {
            return Err(SyncError::Native {
                native: unsafe { NativeSyncError::copy_from(&out.error) },
                intended_commit: None,
            });
        }
        if out.found == 0 {
            Ok(None)
        } else {
            Ok(Some(
                unsafe { CStr::from_ptr(out.hash.as_ptr()) }
                    .to_string_lossy()
                    .into_owned(),
            ))
        }
    }
    pub fn clone_into(&self, path: impl AsRef<Path>, route: &Route) -> Result<()> {
        let url = self.url(route)?;
        let _lease = acquire(&url, route)?;
        let path = path.as_ref();
        if !path.is_absolute() {
            return Err(SyncError::RouteNotBound);
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|_| SyncError::RouteNotBound)?;
        let result = (|| {
            let db = Connection::open(path).map_err(db)?;
            execute_sync(&db, "SELECT dolt_clone(?)", &[url.into()], None)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(path);
            for suffix in ["-wal", "-shm", "-journal"] {
                let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
            }
        }
        result
    }
    pub fn pull(&self, route: &Route) -> Result<()> {
        self.pull_with_merge(route, Store::merge_from)
    }
    pub(crate) fn pull_clean(&self, route: &Route) -> Result<bool> {
        match self.pull_with_merge(route, Store::merge_clean_from) {
            Ok(()) => Ok(true),
            Err(SyncError::Store(StoreError::PendingChanges)) => Ok(false),
            Err(error) => Err(error),
        }
    }
    fn pull_with_merge(
        &self,
        route: &Route,
        merge: impl FnOnce(&Store, &str) -> std::result::Result<(), StoreError>,
    ) -> Result<()> {
        let url = self.url(route)?;
        let _lease = acquire(&url, route)?;
        let store = self.bound(route)?;
        execute_sync(
            &store.db,
            "SELECT dolt_fetch(?, 'main')",
            &[REMOTE.into()],
            None,
        )?;
        merge(&store, TRACKING)?;
        Ok(())
    }
    pub fn push(&self, route: &Route, intended_commit: &str) -> Result<()> {
        let url = self.url(route)?;
        let _lease = acquire(&url, route)?;
        let store = self.bound(route)?;
        let actual = first(&store.db, "SELECT commit_hash FROM dolt_log LIMIT 1", &[])?;
        if !hash(intended_commit) || actual.as_deref() != Some(intended_commit) {
            return Err(SyncError::IntendedCommitChanged {
                expected: intended_commit.into(),
                actual,
            });
        }
        execute_sync(
            &store.db,
            "SELECT dolt_push(?, 'main')",
            &[REMOTE.into()],
            Some(intended_commit),
        )
    }
    pub fn reconcile_push(
        &self,
        route: &Route,
        intended_commit: &str,
    ) -> Result<PushReconciliation> {
        let url = self.url(route)?;
        let _lease = acquire(&url, route)?;
        let store = self.bound(route)?;
        if !hash(intended_commit) {
            return Err(SyncError::IntendedCommitChanged {
                expected: intended_commit.into(),
                actual: None,
            });
        }
        match execute_sync(
            &store.db,
            "SELECT dolt_fetch(?, 'main')",
            &[REMOTE.into()],
            Some(intended_commit),
        ) {
            Err(SyncError::Native { native, .. }) if native.category == 1 => {
                return Ok(PushReconciliation::Unavailable)
            }
            Err(e) => return Err(e),
            _ => (),
        }
        let Some(remote) = first(
            &store.db,
            "SELECT hash FROM dolt_remote_branches WHERE name = ?",
            &[TRACKING.into()],
        )?
        .filter(|s| hash(s)) else {
            return Ok(PushReconciliation::Unavailable);
        };
        if remote == intended_commit {
            return Ok(PushReconciliation::Equal);
        }
        let base = first(
            &store.db,
            "SELECT dolt_merge_base(?, ?)",
            &[intended_commit.into(), remote.clone().into()],
        )?;
        Ok(if base.as_deref() == Some(intended_commit) {
            PushReconciliation::IntendedIsAncestor
        } else if base.as_deref() == Some(&remote) {
            PushReconciliation::RemoteIsAncestor
        } else {
            PushReconciliation::Unrelated
        })
    }
}
fn split_route_url(s: &str) -> Option<(&str, &str)> {
    let i = s.find("://")? + 3;
    let j = s[i..].find('/')? + i;
    origin_parts(&s[..j])?;
    Some((&s[..j], &s[j..]))
}
fn hash(s: &str) -> bool {
    (4..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn db(e: crate::engine::Error) -> SyncError {
    SyncError::Database(e.to_string())
}
fn first(db_: &Connection, sql: &str, args: &[Value]) -> Result<Option<String>> {
    Ok(db_
        .query_row(sql, args)
        .map_err(db)?
        .and_then(|r| r.get(0).and_then(Value::as_str).map(str::to_owned)))
}
fn execute_sync(db_: &Connection, sql: &str, args: &[Value], intended: Option<&str>) -> Result<()> {
    let mut stmt = db_.prepare(sql).map_err(db)?;
    for (i, arg) in args.iter().enumerate() {
        stmt.bind(i + 1, arg).map_err(db)?;
    }
    loop {
        match stmt.step() {
            Ok(true) => (),
            Ok(false) => return Ok(()),
            Err(e) => {
                if let Some(native) = stmt.sync_error() {
                    return Err(if native.publication == 2 && intended.is_some() {
                        SyncError::PublicationUnknown {
                            native,
                            intended_commit: intended.unwrap().to_owned(),
                        }
                    } else {
                        SyncError::Native {
                            native,
                            intended_commit: intended.map(str::to_owned),
                        }
                    });
                }
                return Err(db(e));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::native_request;
    #[test]
    fn exact_native_routes_only() {
        let base = "https://sync.example/v1/account-sync/remotes/abc/notes";
        for (method, path) in [
            ("GET", "/root"),
            ("GET", "/refs"),
            ("POST", "/has-chunks"),
            ("POST", "/get-chunks"),
            ("POST", "/chunks"),
            ("PUT", "/refs"),
            ("PUT", "/refs-if"),
            ("POST", "/commit"),
        ] {
            assert!(native_request(base, method, &format!("{base}{path}")));
            assert!(!native_request(base, "DELETE", &format!("{base}{path}")));
            for suffix in ["?x=1", "#fragment", "/extra"] {
                assert!(!native_request(
                    base,
                    method,
                    &format!("{base}{path}{suffix}")
                ));
            }
        }
        let hash = "a".repeat(64);
        assert!(native_request(base, "GET", &format!("{base}/chunk/{hash}")));
        for h in ["a".repeat(63), "a".repeat(65), "A".repeat(64)] {
            assert!(!native_request(base, "GET", &format!("{base}/chunk/{h}")));
        }
        for u in [
            format!("{base}.evil/refs"),
            format!("{base}/refs?x=1"),
            format!("{base}/refs#x"),
            "https://user@sync.example/v1/account-sync/remotes/abc/notes/refs".into(),
            "https://other.example/v1/account-sync/remotes/abc/notes/refs".into(),
            "https://sync.example:443/v1/account-sync/remotes/abc/notes/refs".into(),
        ] {
            assert!(!native_request(base, "GET", &u));
        }
    }
    fn setup(port: u16) -> (super::Transport, super::AccountKey, super::Route) {
        use super::*;
        let account = AccountKey {
            environment_id: format!("test-{port}"),
            user_id: "alice".into(),
        };
        let route = Route {
            account: account.clone(),
            route_id: "route".into(),
            catalog_generation: 1,
        };
        let transport = Transport::new(Environment {
            id: account.environment_id.clone(),
            sync_origin: format!("http://127.0.0.1:{port}"),
            allow_insecure_http: true,
        })
        .unwrap();
        transport
            .install(
                Credentials {
                    user_id: "alice".into(),
                    access_token: "secret".into(),
                    expires_in: Duration::from_secs(60),
                },
                &account,
                &route,
                None,
            )
            .unwrap();
        (transport, account, route)
    }
    #[test]
    fn expired_snapshot_never_reaches_native_request() {
        use super::*;
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let (transport, _, route) = setup(listener.local_addr().unwrap().port());
        let url = transport.url(&route).unwrap();
        registry()
            .state
            .lock()
            .unwrap()
            .snapshots
            .get_mut(&url)
            .unwrap()
            .expires_at = SystemTime::UNIX_EPOCH;
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 1024];
            while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            assert!(!String::from_utf8_lossy(&bytes)
                .to_ascii_lowercase()
                .contains("authorization:"));
            stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        assert!(matches!(
            transport.remote_head(&route),
            Err(SyncError::ExpiredSession)
        ));
        let mut out: probe::RemoteHead = unsafe { std::mem::zeroed() };
        out.size = std::mem::size_of_val(&out) as i32;
        let url = CString::new(url).unwrap();
        unsafe {
            probe::sqlite3_doltlite_remote_head(url.as_ptr(), c"main".as_ptr(), 3000, &mut out);
        }
        worker.join().unwrap();
    }
    #[test]
    fn draining_cancels_waiting_replacement_and_waits_for_lease() {
        use super::*;
        use std::{net::TcpListener, sync::mpsc, thread};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let (mut transport, account, route) = setup(listener.local_addr().unwrap().port());
        let url = transport.url(&route).unwrap();
        let lease = acquire(&url, &route).unwrap();
        let replacement = Transport::new(transport.environment.clone()).unwrap();
        let a = account.clone();
        let r = route.clone();
        let (send, recv) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = replacement.install(
                Credentials {
                    user_id: "alice".into(),
                    access_token: "new".into(),
                    expires_in: Duration::from_secs(60),
                },
                &a,
                &r,
                None,
            );
            send.send(result).unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !registry().state.lock().unwrap().draining.contains(&url) {
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
        assert!(recv.try_recv().is_err());
        let (done_tx, done_rx) = mpsc::channel();
        let a = account.clone();
        let drain = thread::spawn(move || {
            transport.drain_and_invalidate(&a);
            done_tx.send(()).unwrap();
        });
        while *registry()
            .state
            .lock()
            .unwrap()
            .epochs
            .get(&account)
            .unwrap_or(&0)
            == 0
        {
            assert!(std::time::Instant::now() < deadline);
            thread::yield_now();
        }
        assert!(done_rx.try_recv().is_err());
        drop(lease);
        assert!(matches!(
            recv.recv_timeout(Duration::from_secs(3)).unwrap(),
            Err(SyncError::RouteDraining)
        ));
        done_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        worker.join().unwrap();
        drain.join().unwrap();
        assert!(!registry()
            .state
            .lock()
            .unwrap()
            .snapshots
            .contains_key(&url));
    }
    #[test]
    fn clone_refuses_existing_destination() {
        use super::*;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let (transport, _, route) = setup(listener.local_addr().unwrap().port());
        let path = std::env::temp_dir().join(format!("weirnote-clone-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"existing").unwrap();
        assert!(matches!(
            transport.clone_into(&path, &route),
            Err(SyncError::RouteNotBound)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"existing");
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn bind_refuses_active_route_without_creating_database() {
        use super::*;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (mut transport, _, route) = setup(listener.local_addr().unwrap().port());
        let url = transport.url(&route).unwrap();
        let lease = acquire(&url, &route).unwrap();
        let path = std::env::temp_dir().join(format!("weirnote-bind-{}", uuid::Uuid::new_v4()));
        assert!(matches!(
            transport.bind(&path, &route),
            Err(SyncError::RouteDraining)
        ));
        assert!(!path.exists());
        drop(lease);
    }
    #[test]
    fn installed_snapshot_requires_matching_account_and_generation() {
        use super::*;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (transport, _, route) = setup(listener.local_addr().unwrap().port());
        let mut other_account = route.clone();
        other_account.account.user_id = "bob".into();
        assert!(matches!(
            transport.remote_head(&other_account),
            Err(SyncError::RouteNotInstalled)
        ));
        let mut other_generation = route.clone();
        other_generation.catalog_generation += 1;
        assert!(matches!(
            transport.remote_head(&other_generation),
            Err(SyncError::RouteNotInstalled)
        ));
    }
}
