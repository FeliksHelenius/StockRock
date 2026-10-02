//! Minimal synchronous HTTPS GET on top of WinHTTP.
//!
//! WinHTTP ships with Windows, so there is no TLS library or certificate bundle to carry around,
//! and it honours the system proxy settings and certificate store.

use std::ffi::c_void;

use windows::Win32::Networking::WinHttp::*;
use windows::core::{Error, PCWSTR, w};

/// Upper bound on a response body; quote payloads are a couple of kilobytes.
const MAX_BODY: usize = 4 << 20;

pub struct Response {
    pub status: u32,
    pub body: Vec<u8>,
}

/// Owns a WinHTTP handle and closes it on drop.
struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void, what: &str) -> Result<Self, String> {
        if raw.is_null() {
            Err(describe(what, &Error::from_thread()))
        } else {
            Ok(Self(raw))
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

/// A WinHTTP session. TCP/TLS connections are pooled per session, so repeated requests to the
/// same host reuse one connection.
pub struct Http {
    session: Handle,
}

impl Http {
    pub fn new(user_agent: &str) -> Result<Self, String> {
        let agent = wide(user_agent);
        let raw = unsafe {
            WinHttpOpen(
                PCWSTR(agent.as_ptr()),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            )
        };
        let session = Handle::new(raw, "WinHttpOpen")?;
        // resolve, connect, send, receive (milliseconds)
        unsafe { WinHttpSetTimeouts(session.0, 5_000, 5_000, 10_000, 15_000) }
            .map_err(|e| describe("WinHttpSetTimeouts", &e))?;
        Ok(Self { session })
    }

    /// Performs `GET https://{host}{path}` and returns the status code and body.
    pub fn get(&self, host: &str, path: &str) -> Result<Response, String> {
        let host = wide(host);
        let path = wide(path);
        let headers = wide("Accept: application/json\r\n");
        // The slice passed to WinHTTP must not include the terminating NUL.
        let headers = &headers[..headers.len() - 1];

        unsafe {
            let connect = Handle::new(
                WinHttpConnect(
                    self.session.0,
                    PCWSTR(host.as_ptr()),
                    INTERNET_DEFAULT_HTTPS_PORT,
                    0,
                ),
                "WinHttpConnect",
            )?;
            let request = Handle::new(
                WinHttpOpenRequest(
                    connect.0,
                    w!("GET"),
                    PCWSTR(path.as_ptr()),
                    PCWSTR::null(),
                    PCWSTR::null(),
                    std::ptr::null(),
                    WINHTTP_FLAG_SECURE,
                ),
                "WinHttpOpenRequest",
            )?;

            WinHttpSendRequest(request.0, Some(headers), None, 0, 0, 0)
                .map_err(|e| describe("request", &e))?;
            WinHttpReceiveResponse(request.0, std::ptr::null_mut())
                .map_err(|e| describe("response", &e))?;

            let mut status = 0u32;
            let mut size = size_of::<u32>() as u32;
            WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some(&mut status as *mut u32 as *mut c_void),
                &mut size,
                std::ptr::null_mut(),
            )
            .map_err(|e| describe("status", &e))?;

            let mut body = Vec::new();
            loop {
                let mut available = 0u32;
                WinHttpQueryDataAvailable(request.0, &mut available)
                    .map_err(|e| describe("read", &e))?;
                if available == 0 || body.len() + available as usize > MAX_BODY {
                    break;
                }
                let start = body.len();
                body.resize(start + available as usize, 0);
                let mut read = 0u32;
                WinHttpReadData(
                    request.0,
                    body.as_mut_ptr().add(start) as *mut c_void,
                    available,
                    &mut read,
                )
                .map_err(|e| describe("read", &e))?;
                body.truncate(start + read as usize);
                if read == 0 {
                    break;
                }
            }
            Ok(Response { status, body })
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Turns a WinHTTP failure into a short human-readable reason.
fn describe(what: &str, error: &Error) -> String {
    let hresult = error.code().0 as u32;
    match hresult & 0xFFFF {
        12002 => "timed out".into(),
        12007 => "can't resolve host (offline?)".into(),
        12029 => "can't connect".into(),
        12030 | 12031 => "connection dropped".into(),
        12157 | 12175 => "TLS error".into(),
        code => format!("{what} failed (error {code})"),
    }
}
