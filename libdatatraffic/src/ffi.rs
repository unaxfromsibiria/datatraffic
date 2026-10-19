//! C ABI for Python (ctypes) or any other FFI host; only pointers and integers cross the boundary.
//! The client lifecycle drives an async `TrafficClient` on one global tokio runtime via `block_on`.
//! All entry points must be called sequentially from a single thread.

use std::os::raw::c_void;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use tokio::runtime::Runtime;

use super::{
    DataFile, Settings, TrafficClient, FFI_ERR_BAD_INPUT, FFI_ERR_CLIENT_CREATE, FFI_ERR_SEND,
    FFI_OK,
};

/// Opaque byte buffer allocated by the crate. Free with `ffi_buffer_free`.
#[repr(C)]
pub struct FfiBuffer {
    pub ptr: *mut u8,
    pub len: usize,
    pub cap: usize,
}

const EMPTY_BUF: FfiBuffer = FfiBuffer {
    ptr: std::ptr::null_mut(),
    len: 0,
    cap: 0,
};

/// Received data file; each field is a heap buffer, release all of them via `ffi_datafile_free`.
#[repr(C)]
pub struct FfiDataFile {
    pub project: FfiBuffer,
    pub name: FfiBuffer,
    pub note: FfiBuffer,
    pub data: FfiBuffer,
}

static LAST_ERROR: AtomicI32 = AtomicI32::new(FFI_OK);

/// Global multi-threaded tokio runtime shared by all synchronous FFI entry points.
static RUNTIME: OnceLock<Runtime> = OnceLock::new();

fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| Runtime::new().expect("failed to start the global tokio runtime"))
}

#[inline]
fn set_error(code: i32) {
    LAST_ERROR.store(code, Ordering::Relaxed);
}

/// Read the last FFI error code (see constants in crate root). `FFI_OK` after a success.
#[unsafe(no_mangle)]
pub extern "C" fn ffi_last_error() -> i32 {
    LAST_ERROR.load(Ordering::Relaxed)
}

// --- Buffer helpers ----------------------------------------------------------

/// Copy bytes into a heap buffer owned by the caller (null/empty when `bytes` is empty).
fn to_ffi_buf(bytes: &[u8]) -> FfiBuffer {
    if bytes.is_empty() {
        return EMPTY_BUF;
    }
    let mut v = Vec::with_capacity(bytes.len());
    v.extend_from_slice(bytes);
    let ptr = v.as_mut_ptr();
    let len = v.len();
    let cap = v.capacity();
    std::mem::forget(v);
    FfiBuffer { ptr, len, cap }
}

/// Release a heap buffer produced by `to_ffi_buf`. Safe on null/empty.
fn drop_ffi_buf(buf: FfiBuffer) {
    if buf.ptr.is_null() || (buf.len == 0 && buf.cap == 0) {
        return;
    }
    unsafe { Vec::from_raw_parts(buf.ptr, buf.len, buf.cap.max(buf.len)) };
}

/// Read a length-delimited byte slice from the caller's memory. Null/zero -> empty.
unsafe fn read_bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(ptr, len) }
}

/// Read a length-delimited UTF-8 string. Fails on invalid UTF-8.
fn read_str(ptr: *const u8, len: usize) -> Result<String, ()> {
    let bytes = unsafe { read_bytes(ptr, len) };
    String::from_utf8(bytes.to_vec()).map_err(|_| ())
}

/// Release a buffer previously returned by the crate. Safe on null/empty.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ffi_buffer_free(ptr: *mut u8, len: usize, cap: usize) {
    drop_ffi_buf(FfiBuffer { ptr, len, cap });
}

// --- Client lifecycle --------------------------------------------------------

/// Create a traffic client; returns an opaque handle (null on error, see `ffi_last_error`).
/// String args are length-delimited; empty means "not provided" and falls back to env/defaults.
/// `watch_projects` is ';'-separated; the send target project is passed per message via `ffi_client_send`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ffi_client_new(
    redis_url: *const u8,
    redis_url_len: usize,
    key_hex: *const u8,
    key_hex_len: usize,
    client_name: *const u8,
    client_name_len: usize,
    password: *const u8,
    password_len: usize,
    watch_projects: *const u8,
    watch_projects_len: usize,
) -> *mut c_void {
    let redis_url = match read_str(redis_url, redis_url_len) {
        Ok(s) => s,
        Err(_) => return fail_null(FFI_ERR_BAD_INPUT),
    };
    let key_hex = match read_str(key_hex, key_hex_len) {
        Ok(s) => s,
        Err(_) => return fail_null(FFI_ERR_BAD_INPUT),
    };
    let client_name = match read_str(client_name, client_name_len) {
        Ok(s) => s,
        Err(_) => return fail_null(FFI_ERR_BAD_INPUT),
    };
    let password = match read_str(password, password_len) {
        Ok(s) => s,
        Err(_) => return fail_null(FFI_ERR_BAD_INPUT),
    };
    let watch_raw = match read_str(watch_projects, watch_projects_len) {
        Ok(s) => s,
        Err(_) => return fail_null(FFI_ERR_BAD_INPUT),
    };

    // Empty strings mean "not provided" -> fall back to env / defaults.
    let settings = match Settings::resolve(
        if redis_url.is_empty() { None } else { Some(&redis_url) },
        if key_hex.is_empty() { None } else { Some(&key_hex) },
        if client_name.is_empty() { None } else { Some(&client_name) },
        if password.is_empty() { None } else { Some(&password) },
    ) {
        Ok(s) => s,
        Err(e) => {
            log::error!("ffi_client_new: {e}");
            return fail_null(FFI_ERR_CLIENT_CREATE);
        }
    };

    let watch = watch_raw.split(';').map(str::trim).filter(
        |s| !s.is_empty()
    ).map(String::from).collect::<Vec<_>>();

    match runtime().block_on(TrafficClient::new(settings, watch)) {
        Ok(client) => {
            set_error(FFI_OK);
            Box::into_raw(Box::new(client)) as *mut c_void
        }
        Err(e) => {
            log::error!("ffi_client_new: failed to create client: {e}");
            fail_null(FFI_ERR_CLIENT_CREATE)
        }
    }
}

/// Send a data file to the given project through the client's outbox. Returns an FFI error code.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ffi_client_send(
    handle: *mut c_void,
    project: *const u8,
    project_len: usize,
    name: *const u8,
    name_len: usize,
    note: *const u8,
    note_len: usize,
    data: *const u8,
    data_len: usize,
) -> i32 {
    if handle.is_null() {
        return fail_code(FFI_ERR_BAD_INPUT);
    }
    let project = match read_str(project, project_len) {
        Ok(s) => s,
        Err(_) => return fail_code(FFI_ERR_BAD_INPUT),
    };
    let name = match read_str(name, name_len) {
        Ok(s) => s,
        Err(_) => return fail_code(FFI_ERR_BAD_INPUT),
    };
    let note = match read_str(note, note_len) {
        Ok(s) => s,
        Err(_) => return fail_code(FFI_ERR_BAD_INPUT),
    };
    let data_bytes = unsafe { read_bytes(data, data_len) }.to_vec();

    // Single-threaded sequential access contract: no other live borrow aliases this handle.
    let client = unsafe { &*(handle as *const TrafficClient) };
    let file = DataFile { name, note, data: bytes::Bytes::from(data_bytes) };
    match runtime().block_on(client.outbox().send((project, file))) {
        Ok(()) => {
            set_error(FFI_OK);
            FFI_OK
        }
        Err(_) => fail_code(FFI_ERR_SEND),
    }
}

/// Receive a data file, waiting up to `timeout_ms`. Returns null on timeout (not an error).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ffi_client_recv(handle: *mut c_void, timeout_ms: u32) -> *mut FfiDataFile {
    if handle.is_null() {
        set_error(FFI_ERR_BAD_INPUT);
        return std::ptr::null_mut();
    }
    // Sequential single-threaded access contract (see `ffi_client_send`): a unique &mut is sound.
    let client = unsafe { &mut *(handle as *mut TrafficClient) };
    let timeout = Duration::from_millis(timeout_ms as u64);
    // The timeout future must be built inside the runtime context; hence the async block wrapper.
    match runtime().block_on(async move {
        tokio::time::timeout(timeout, client.inbox().recv()).await
    }) {
        Ok(Some((project, file))) => {
            set_error(FFI_OK);
            Box::into_raw(Box::new(FfiDataFile {
                project: to_ffi_buf(project.as_bytes()),
                name: to_ffi_buf(file.name.as_bytes()),
                note: to_ffi_buf(file.note.as_bytes()),
                data: to_ffi_buf(file.data.as_ref()),
            }))
        }
        _ => {
            set_error(FFI_OK); // timeout (or closed channel) is not an error
            std::ptr::null_mut()
        }
    }
}

/// Release a `FfiDataFile` returned by `ffi_client_recv`. Safe on null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ffi_datafile_free(ptr: *mut FfiDataFile) {
    if ptr.is_null() {
        return;
    }
    let file = unsafe { Box::from_raw(ptr) };
    drop_ffi_buf(file.project);
    drop_ffi_buf(file.name);
    drop_ffi_buf(file.note);
    drop_ffi_buf(file.data);
}

/// Release a client handle returned by `ffi_client_new`. Best-effort presence cleanup, then drop.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ffi_client_free(handle: *mut c_void) {
    if handle.is_null() {
        return;
    }
    let client = unsafe { Box::from_raw(handle as *mut TrafficClient) };
    runtime().block_on(client.shutdown());
}

// --- Error helpers -----------------------------------------------------------

fn fail_code(code: i32) -> i32 {
    set_error(code);
    code
}

fn fail_null(code: i32) -> *mut c_void {
    set_error(code);
    std::ptr::null_mut()
}

// --- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed 32-byte key (0xa5 x 32) as a 64-char hex string.
    fn key_hex() -> String {
        "a5".repeat(32)
    }

    fn c_bytes(s: &str) -> (*const u8, usize) {
        (s.as_ptr(), s.len())
    }

    /// Redis URL used by the tests: `REDIS_URL` env or localhost default.
    fn test_redis_url() -> String {
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into())
    }

    /// Optional Redis password from `REDIS_PASSWORD` (empty/absent -> None).
    fn test_redis_password() -> Option<String> {
        std::env::var("REDIS_PASSWORD").ok().filter(|s| !s.is_empty())
    }

    /// True when Redis at `url` answers PING with the given password; a real command is issued
    /// because a bare connect can succeed even on a NOAUTH server.
    fn redis_available_at(url: &str, password: Option<&str>) -> bool {
        let url = crate::settings::authed_url(url, password);
        runtime().block_on(async move {
            let client = match redis::Client::open(url.as_str()) {
                Ok(c) => c,
                Err(_) => return false,
            };
            let mut conn = match tokio::time::timeout(Duration::from_secs(2), client.get_multiplexed_async_connection()).await {
                Ok(Ok(c)) => c,
                _ => return false,
            };
            // PING with a typed binding so the response type is inferred as `()`.
            let pong: Result<(), redis::RedisError> = match tokio::time::timeout(Duration::from_secs(2), redis::cmd("PING").query_async(&mut conn)).await {
                Ok(r) => r,
                Err(_) => return false,
            };
            pong.is_ok()
        })
    }

    /// (ptr, len) for an optional string: null/0 when `None`.
    fn opt_bytes(s: &Option<String>) -> (*const u8, usize) {
        match s {
            Some(v) => c_bytes(v),
            None => (std::ptr::null(), 0),
        }
    }

    // --- Null-handle safety (no Redis needed) ---------------------------------

    #[test]
    fn datafile_free_null_is_safe() {
        unsafe { ffi_datafile_free(std::ptr::null_mut()) };
    }

    #[test]
    fn client_free_null_is_safe() {
        unsafe { ffi_client_free(std::ptr::null_mut()) };
    }

    #[test]
    fn send_null_handle_reports_bad_input() {
        let (n, nl) = c_bytes("x.txt");
        let code = unsafe {
            ffi_client_send(
                std::ptr::null_mut(),
                std::ptr::null(), 0, // project
                n, nl,
                std::ptr::null(), 0,
                std::ptr::null(), 0,
            )
        };
        assert_eq!(code, FFI_ERR_BAD_INPUT);
        assert_eq!(ffi_last_error(), FFI_ERR_BAD_INPUT);
    }

    #[test]
    fn recv_null_handle_returns_null() {
        let ptr = unsafe { ffi_client_recv(std::ptr::null_mut(), 10) };
        assert!(ptr.is_null());
        assert_eq!(ffi_last_error(), FFI_ERR_BAD_INPUT);
    }

    // --- Client creation error (no Redis: a bad key fails before connecting) --

    #[test]
    fn new_bad_key_reports_client_create() {
        let (ru, rul) = c_bytes("redis://localhost:6379");
        let (kh, khl) = c_bytes("abcd"); // 2 bytes -> wrong length for AES-256
        let handle = unsafe {
            ffi_client_new(
                ru, rul, kh, khl,
                std::ptr::null(), 0,
                std::ptr::null(), 0,
                std::ptr::null(), 0,
            )
        };
        assert!(handle.is_null());
        assert_eq!(ffi_last_error(), FFI_ERR_CLIENT_CREATE);
    }

    // --- Redis-dependent (skipped when no Redis is reachable) -----------------

    #[test]
    fn recv_timeout_returns_null() {
        let url = test_redis_url();
        let pw = test_redis_password();
        if !redis_available_at(&url, pw.as_deref()) {
            eprintln!("skip: no usable redis at {url}");
            return;
        }
        let (ru, rul) = c_bytes(&url);
        let kh = key_hex();
        let (kh_, khl) = c_bytes(&kh);
        let (nb, nbl) = c_bytes("ffi-timeout-client");
        let (pw_ptr, pw_len) = opt_bytes(&pw);
        let handle = unsafe {
            ffi_client_new(
                ru, rul, kh_, khl, nb, nbl,
                pw_ptr, pw_len,
                std::ptr::null(), 0,
            )
        };
        assert!(!handle.is_null(), "create failed: {}", ffi_last_error());
        // No one is publishing -> recv must time out to null (not an error).
        let ptr = unsafe { ffi_client_recv(handle, 200) };
        assert!(ptr.is_null());
        assert_eq!(ffi_last_error(), FFI_OK);
        unsafe { ffi_client_free(handle) };
    }

    #[test]
    fn roundtrip_send_recv() {
        let url = test_redis_url();
        let pw = test_redis_password();
        if !redis_available_at(&url, pw.as_deref()) {
            eprintln!("skip: no usable redis at {url}");
            return;
        }
        // Receiver B watches the project; sender A publishes to it. Distinct names so
        // self-filtering does not drop the message.
        let (ru, rul) = c_bytes(&url);
        let kh = key_hex();
        let (kh_, khl) = c_bytes(&kh);
        let (nb, nbl) = c_bytes("ffi-b");
        let (na, nal) = c_bytes("ffi-a");
        let (wp, wpl) = c_bytes("ffi_roundtrip_proj");
        let (pw_ptr, pw_len) = opt_bytes(&pw);

        let b = unsafe { ffi_client_new(ru, rul, kh_, khl, nb, nbl, pw_ptr, pw_len, wp, wpl) };
        assert!(!b.is_null(), "receiver create failed: {}", ffi_last_error());
        let a = unsafe { ffi_client_new(ru, rul, kh_, khl, na, nal, pw_ptr, pw_len, std::ptr::null(), 0) };
        assert!(!a.is_null(), "sender create failed: {}", ffi_last_error());

        // Let subscriptions settle on the Redis side before publishing.
        std::thread::sleep(Duration::from_millis(200));

        let (fn_, fnl) = c_bytes("hello.txt");
        let (no, nol) = c_bytes("a note");
        let (da, dal) = c_bytes("payload-bytes-1234567890");
        let code = unsafe { ffi_client_send(a, wp, wpl, fn_, fnl, no, nol, da, dal) };
        assert_eq!(code, FFI_OK);

        // B should receive A's message.
        let got = unsafe { ffi_client_recv(b, 3000) };
        assert!(!got.is_null(), "recv timed out; last_error={}", ffi_last_error());
        if !got.is_null() {
            let f = unsafe { &*got };
            let project = unsafe { std::slice::from_raw_parts(f.project.ptr, f.project.len) };
            let name = unsafe { std::slice::from_raw_parts(f.name.ptr, f.name.len) };
            let note = unsafe { std::slice::from_raw_parts(f.note.ptr, f.note.len) };
            let data = unsafe { std::slice::from_raw_parts(f.data.ptr, f.data.len) };
            assert_eq!(std::str::from_utf8(project).unwrap(), "ffi_roundtrip_proj");
            assert_eq!(std::str::from_utf8(name).unwrap(), "hello.txt");
            assert_eq!(std::str::from_utf8(note).unwrap(), "a note");
            assert_eq!(data, b"payload-bytes-1234567890");
            unsafe { ffi_datafile_free(got) };
        }

        unsafe { ffi_client_free(a) };
        unsafe { ffi_client_free(b) };
    }
}
