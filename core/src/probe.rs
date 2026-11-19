use crate::engine::ffi::{Sqlite3, Stmt};
use std::ffi::{c_char, c_int, c_void};
#[repr(C)]
pub struct SyncError {
    pub version: c_int,
    pub size: c_int,
    pub category: c_int,
    pub operation: c_int,
    pub phase: c_int,
    pub publication: c_int,
    pub sqlite_code: c_int,
    pub http_status: c_int,
    pub retry_after: c_int,
    pub message: *const c_char,
    pub request_id: *const c_char,
    pub intended_commit: *const c_char,
    pub remote_branch: *const c_char,
}
#[repr(C)]
pub struct RemoteHead {
    pub size: c_int,
    pub found: c_int,
    pub hash: [c_char; 41],
    pub error: SyncError,
    pub request_id: [c_char; 44],
}
extern "C" {
    pub fn sqlite3_doltlite_remote_head(
        url: *const c_char,
        branch: *const c_char,
        timeout: c_int,
        out: *mut RemoteHead,
    ) -> c_int;
    pub fn sqlite3_doltlite_sync_error_get(stmt: *mut Stmt, out: *mut SyncError) -> c_int;
    pub fn sqlite3_open(path: *const c_char, db: *mut *mut Sqlite3) -> c_int;
    pub fn sqlite3_close(db: *mut Sqlite3) -> c_int;
    pub fn sqlite3_exec(
        db: *mut Sqlite3,
        sql: *const c_char,
        cb: *mut c_void,
        arg: *mut c_void,
        err: *mut *mut c_char,
    ) -> c_int;
}
