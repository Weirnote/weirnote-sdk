//! Sole owner of DoltLite's process-wide HTTP header-provider ABI.
use std::{
    ffi::{c_char, c_int, c_void, CStr, CString},
    sync::OnceLock,
};

#[repr(C)]
struct Request {
    version: c_int,
    size: c_int,
    method: *const c_char,
    url: *const c_char,
}
extern "C" {
    fn sqlite3_doltlite_http_header_provider_register(
        cb: Option<unsafe extern "C" fn(*mut c_void, *const Request, *mut c_void) -> c_int>,
        context: *mut c_void,
    ) -> c_int;
    fn sqlite3_doltlite_http_header_set(
        headers: *mut c_void,
        name: *const c_char,
        value: *const c_char,
    ) -> c_int;
}
static PROVIDER: OnceLock<bool> = OnceLock::new();

pub(crate) fn register_once() -> bool {
    *PROVIDER.get_or_init(|| unsafe {
        sqlite3_doltlite_http_header_provider_register(Some(provider), std::ptr::null_mut()) == 0
    })
}
unsafe extern "C" fn provider(
    _: *mut c_void,
    request: *const Request,
    headers: *mut c_void,
) -> c_int {
    if request.is_null() || headers.is_null() {
        return 0;
    }
    let request = &*request;
    if request.version < 1
        || request.size < std::mem::size_of::<Request>() as i32
        || request.method.is_null()
        || request.url.is_null()
    {
        return 0;
    }
    let (Ok(method), Ok(url)) = (
        CStr::from_ptr(request.method).to_str(),
        CStr::from_ptr(request.url).to_str(),
    ) else {
        return 0;
    };
    crate::sync::with_request_headers(method, url, |values| {
        for (name, value) in values {
            let (Ok(name), Ok(value)) = (CString::new(name), CString::new(value)) else {
                return 1;
            };
            let rc = sqlite3_doltlite_http_header_set(headers, name.as_ptr(), value.as_ptr());
            if rc != 0 {
                return rc;
            }
        }
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ffi::{sqlite3_finalize, sqlite3_prepare_v2, sqlite3_step};
    use crate::probe::*;
    use std::{
        ffi::{c_void, CString},
        io::{Read, Write},
        net::TcpListener,
        ptr,
        sync::atomic::{AtomicUsize, Ordering},
        thread,
    };
    #[test]
    fn invalid_request_is_ignored_before_accessing_headers() {
        unsafe {
            let headers = std::ptr::NonNull::<u8>::dangling().as_ptr().cast();
            assert_eq!(provider(ptr::null_mut(), ptr::null(), headers), 0);
            let mut request = Request {
                version: 0,
                size: std::mem::size_of::<Request>() as i32,
                method: c"GET".as_ptr(),
                url: c"https://example.invalid/refs".as_ptr(),
            };
            assert_eq!(provider(ptr::null_mut(), &request, headers), 0);
            request.version = 1;
            request.size = 0;
            assert_eq!(provider(ptr::null_mut(), &request, headers), 0);
            request.size = std::mem::size_of::<Request>() as i32;
            request.method = ptr::null();
            assert_eq!(provider(ptr::null_mut(), &request, headers), 0);
            assert_eq!(provider(ptr::null_mut(), &request, ptr::null_mut()), 0);
        }
    }
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn headers(_: *mut c_void, _: *const Request, h: *mut c_void) -> i32 {
        CALLS.fetch_add(1, Ordering::SeqCst);
        sqlite3_doltlite_http_header_set(h, c"X-Spike".as_ptr(), c"present".as_ptr())
    }
    #[test]
    fn custom_calls() {
        if std::env::var_os("WEIRNOTE_SPIKE_CHILD").is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "header_hook::tests::custom_calls"])
                .env("WEIRNOTE_SPIKE_CHILD", "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        unsafe {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url =
                CString::new(format!("http://{}/repo", listener.local_addr().unwrap())).unwrap();
            let server = thread::spawn(move || {
                for stream in listener.incoming().take(2) {
                    let mut stream = stream.unwrap();
                    let mut request = Vec::new();
                    let mut buf = [0; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = stream.read(&mut buf).unwrap();
                        assert_ne!(n, 0);
                        request.extend_from_slice(&buf[..n]);
                    }
                    assert!(String::from_utf8_lossy(&request)
                        .to_lowercase()
                        .contains("x-spike: present"));
                    stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                }
            });
            assert_eq!(
                sqlite3_doltlite_http_header_provider_register(Some(headers), ptr::null_mut()),
                0
            );
            let mut head: RemoteHead = std::mem::zeroed();
            head.size = std::mem::size_of::<RemoteHead>() as i32;
            assert_ne!(
                sqlite3_doltlite_remote_head(url.as_ptr(), c"main".as_ptr(), 1000, &mut head),
                0
            );
            assert_eq!(
                (
                    head.error.category,
                    head.error.operation,
                    head.error.http_status
                ),
                (2, 5, 503)
            );
            let mut db = ptr::null_mut();
            assert_eq!(sqlite3_open(c":memory:".as_ptr(), &mut db), 0);
            let sql = CString::new(format!(
                "SELECT dolt_remote('add','origin','{}')",
                url.to_str().unwrap()
            ))
            .unwrap();
            assert_eq!(
                sqlite3_exec(
                    db,
                    sql.as_ptr(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut()
                ),
                0
            );
            let mut stmt = ptr::null_mut();
            assert_eq!(
                sqlite3_prepare_v2(
                    db,
                    c"SELECT dolt_fetch('origin','main')".as_ptr(),
                    -1,
                    &mut stmt,
                    ptr::null_mut()
                ),
                0
            );
            assert_ne!(sqlite3_step(stmt), 100);
            let mut error: SyncError = std::mem::zeroed();
            error.size = std::mem::size_of::<SyncError>() as i32;
            assert_eq!(sqlite3_doltlite_sync_error_get(stmt, &mut error), 0);
            assert_eq!(
                (error.category, error.operation, error.http_status),
                (2, 1, 503)
            );
            sqlite3_finalize(stmt);
            sqlite3_close(db);
            server.join().unwrap();
            assert_eq!(CALLS.load(Ordering::SeqCst), 2);
        }
    }
}
