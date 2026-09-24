//! Minimal WinHTTP client: blocking GETs for the HTTP ES-module loader and the upgrade handshake
//! behind the native `WebSocket`. WinHTTP rather than a Rust HTTP stack so https dev servers use
//! the system TLS/certificate store with no extra dependencies.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable,
    WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
    WinHttpSetOption, WinHttpSetTimeouts, WinHttpWebSocketCompleteUpgrade,
    WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_DISABLE_COOKIES, WINHTTP_FLAG_REFRESH,
    WINHTTP_FLAG_SECURE, WINHTTP_OPEN_REQUEST_FLAGS, WINHTTP_OPTION_DISABLE_FEATURE,
    WINHTTP_OPTION_UPGRADE_TO_WEB_SOCKET, WINHTTP_QUERY_CONTENT_TYPE, WINHTTP_QUERY_FLAG_NUMBER,
    WINHTTP_QUERY_STATUS_CODE,
};

/// A WinHTTP handle, closed on drop.
pub(crate) struct Handle(*mut c_void);

// WinHTTP handles are thread-safe (the session is shared; request handles move to the thread
// that drives them).
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

impl Handle {
    pub(crate) fn raw(&self) -> *mut c_void {
        self.0
    }
    fn new(raw: *mut c_void, what: &str) -> Result<Self, String> {
        if raw.is_null() {
            Err(format!("{what} failed: {}", windows::core::Error::from_thread().message()))
        } else {
            Ok(Handle(raw))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

fn session() -> Result<&'static Handle, String> {
    static SESSION: OnceLock<Result<Handle, String>> = OnceLock::new();
    SESSION
        .get_or_init(|| {
            let raw = unsafe {
                WinHttpOpen(
                    &HSTRING::from("NativeScript-Windows"),
                    WINHTTP_ACCESS_TYPE_NO_PROXY,
                    PCWSTR::null(),
                    PCWSTR::null(),
                    0,
                )
            };
            let h = Handle::new(raw, "WinHttpOpen")?;
            // Connect/send/receive timeouts: a dev server that stops answering must surface as an
            // import error, not a UI thread blocked forever.
            unsafe {
                let _ = WinHttpSetTimeouts(h.raw(), 5_000, 5_000, 15_000, 30_000);
            }
            Ok(h)
        })
        .as_ref()
        .map_err(|e| e.clone())
}

pub(crate) struct Response {
    pub status: u32,
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Open a GET request (connected, not yet sent) for `url` with `extra_headers` (CRLF-separated).
fn open_get(url: &str) -> Result<(Handle, Handle), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid URL '{url}': {e}"))?;
    let secure = match parsed.scheme() {
        "http" | "ws" => false,
        "https" | "wss" => true,
        other => return Err(format!("unsupported URL scheme '{other}' in {url}")),
    };
    let host = parsed.host_str().ok_or_else(|| format!("URL has no host: {url}"))?;
    let port = parsed.port_or_known_default().unwrap_or(if secure { 443 } else { 80 });
    let mut object = parsed.path().to_string();
    if let Some(q) = parsed.query() {
        object.push('?');
        object.push_str(q);
    }
    let connect = Handle::new(
        unsafe { WinHttpConnect(session()?.raw(), &HSTRING::from(host), port, 0) },
        "WinHttpConnect",
    )?;
    let flags = WINHTTP_OPEN_REQUEST_FLAGS(
        WINHTTP_FLAG_REFRESH.0 | if secure { WINHTTP_FLAG_SECURE.0 } else { 0 },
    );
    let request = Handle::new(
        unsafe {
            WinHttpOpenRequest(
                connect.raw(),
                &HSTRING::from("GET"),
                &HSTRING::from(object.as_str()),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                flags,
            )
        },
        "WinHttpOpenRequest",
    )?;
    let no_cookies = WINHTTP_DISABLE_COOKIES.to_le_bytes();
    unsafe {
        let _ = WinHttpSetOption(Some(request.raw()), WINHTTP_OPTION_DISABLE_FEATURE, Some(&no_cookies));
    }
    Ok((connect, request))
}

fn send_and_receive(request: &Handle, headers: &str) -> Result<(), String> {
    let wide: Vec<u16> = headers.encode_utf16().collect();
    unsafe {
        WinHttpSendRequest(
            request.raw(),
            if wide.is_empty() { None } else { Some(&wide) },
            None,
            0,
            0,
            0,
        )
        .map_err(|e| format!("request failed: {}", e.message()))?;
        WinHttpReceiveResponse(request.raw(), std::ptr::null_mut())
            .map_err(|e| format!("no response: {}", e.message()))
    }
}

fn status_code(request: &Handle) -> u32 {
    let mut status = 0u32;
    let mut len = std::mem::size_of::<u32>() as u32;
    let mut index = 0u32;
    unsafe {
        let _ = WinHttpQueryHeaders(
            request.raw(),
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut c_void),
            &mut len,
            &mut index,
        );
    }
    status
}

fn content_type(request: &Handle) -> String {
    let mut buf = [0u16; 256];
    let mut len = std::mem::size_of_val(&buf) as u32;
    let mut index = 0u32;
    let ok = unsafe {
        WinHttpQueryHeaders(
            request.raw(),
            WINHTTP_QUERY_CONTENT_TYPE,
            PCWSTR::null(),
            Some(buf.as_mut_ptr() as *mut c_void),
            &mut len,
            &mut index,
        )
        .is_ok()
    };
    if ok {
        String::from_utf16_lossy(&buf[..(len as usize / 2).min(buf.len())])
    } else {
        String::new()
    }
}

/// Blocking GET. `headers` are extra request headers, CRLF-separated.
pub(crate) fn get(url: &str, headers: &str) -> Result<Response, String> {
    let (_connect, request) = open_get(url)?;
    send_and_receive(&request, headers)?;
    let status = status_code(&request);
    let content_type = content_type(&request);
    let mut body = Vec::new();
    loop {
        let mut available = 0u32;
        unsafe { WinHttpQueryDataAvailable(request.raw(), &mut available) }
            .map_err(|e| format!("reading body failed: {}", e.message()))?;
        if available == 0 {
            break;
        }
        let start = body.len();
        body.resize(start + available as usize, 0);
        let mut read = 0u32;
        unsafe {
            WinHttpReadData(
                request.raw(),
                body[start..].as_mut_ptr() as *mut c_void,
                available,
                &mut read,
            )
        }
        .map_err(|e| format!("reading body failed: {}", e.message()))?;
        body.truncate(start + read as usize);
        if read == 0 {
            break;
        }
    }
    Ok(Response { status, content_type, body })
}

/// Perform the WebSocket upgrade handshake for a `ws://`/`wss://` URL and return the socket
/// handle (plus the connection handle, which must outlive it).
pub(crate) fn websocket_connect(url: &str, protocols: &[String]) -> Result<(Handle, Handle), String> {
    let (connect, request) = open_get(url)?;
    unsafe { WinHttpSetOption(Some(request.raw()), WINHTTP_OPTION_UPGRADE_TO_WEB_SOCKET, None) }
        .map_err(|e| format!("WebSocket upgrade option failed: {}", e.message()))?;
    let headers = if protocols.is_empty() {
        String::new()
    } else {
        format!("Sec-WebSocket-Protocol: {}\r\n", protocols.join(", "))
    };
    send_and_receive(&request, &headers)?;
    let status = status_code(&request);
    if status != 101 {
        return Err(format!("WebSocket handshake failed: HTTP {status}"));
    }
    let socket = Handle::new(
        unsafe { WinHttpWebSocketCompleteUpgrade(request.raw(), None) },
        "WinHttpWebSocketCompleteUpgrade",
    )?;
    drop(request);
    Ok((socket, connect))
}
