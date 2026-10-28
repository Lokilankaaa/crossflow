//! Read-only C ABI for native clients. It owns no session or mutable layout.
//! Layout parsing and crossing call the shared core, never a duplicated model.

use crossflow::arrangement::{Layout, Point};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{mem, panic::catch_unwind, ptr, slice, str};

pub const API_VERSION: u32 = 1;
pub const MAX_REQUEST: usize = 1_048_576;
pub const MAX_RESPONSE: usize = 1_048_576;
pub const OK: i32 = 0;
pub const BUFFER_TOO_SMALL: i32 = 1;
pub const INVALID_ARGUMENT: i32 = 2;
pub const REQUEST_TOO_LARGE: i32 = 3;
pub const INVALID_UTF8: i32 = 4;
pub const INTERNAL_ERROR: i32 = 5;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Environment {},
    LayoutValidate {
        layout: Value,
    },
    LayoutCrossing {
        layout: Value,
        from_screen: String,
        start: Point,
        end: Point,
    },
}

fn success(result: Value) -> Value {
    json!({"api_version":API_VERSION,"ok":true,"result":result})
}

fn failure(code: &'static str, message: &'static str) -> Value {
    // No submitted data, identifiers, paths, parser excerpts or panic payloads
    // are included in returned JSON errors. The process panic hook remains the
    // host's responsibility. The client may highlight its own local draft.
    json!({"api_version":API_VERSION,"ok":false,"error":{"code":code,"message":message}})
}

fn layout_from(value: Value) -> Result<Layout, Value> {
    serde_json::from_value(value).map_err(|_| {
        failure(
            "invalid_layout",
            "Layout requires 1 to 256 valid, uniquely identified screens with nonoverlapping interiors.",
        )
    })
}

fn respond(request: &str) -> Value {
    let request = match serde_json::from_str::<Request>(request) {
        Ok(request) => request,
        Err(_) => return failure("invalid_request", "Request JSON or operation is invalid."),
    };
    match request {
        Request::Environment {} => success(json!({
            "bridge_version":env!("CARGO_PKG_VERSION"),
            "os":std::env::consts::OS,
            "arch":std::env::consts::ARCH,
            "wire_version":crossflow::protocol::WIRE_VERSION,
            "environment_scope":"process_and_bridge",
            "session_status":"not_exposed",
            "capabilities":{
                "layout_validate":true,
                "layout_crossing":true,
                "session_control":false,
                "input_capture":false,
                "input_injection":false,
                "display_discovery":false,
                "permission_inspection":false
            }
        })),
        Request::LayoutValidate { layout } => match layout_from(layout) {
            Ok(layout) => success(json!({
                "valid":true,"screen_count":layout.screens().len(),
                "layout":layout,"applied_to_session":false
            })),
            Err(error) => error,
        },
        Request::LayoutCrossing {
            layout,
            from_screen,
            start,
            end,
        } => {
            let layout = match layout_from(layout) {
                Ok(layout) => layout,
                Err(error) => return error,
            };
            match layout.crossing(&from_screen, start, end) {
                Ok(crossing) => success(json!({
                    "crossing":crossing,"system_input_injected":false
                })),
                Err(_) => failure(
                    "invalid_crossing",
                    "Crossing requires a known source screen and finite movement starting on that screen.",
                ),
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn crossflow_bridge_api_version() -> u32 {
    API_VERSION
}

fn overlaps(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1 && a.0 != a.1 && b.0 != b.1
}

fn panic_guard(operation: impl FnOnce() -> i32 + std::panic::UnwindSafe) -> i32 {
    catch_unwind(operation).unwrap_or(INTERNAL_ERROR)
}

/// Read-only C ABI. See `include/crossflow.h` for buffer ownership and statuses.
///
/// # Safety
/// Every non-null pointer must identify a valid region for the documented length
/// and access; `output_required` must be aligned/writable. These regions cannot
/// overlap or be concurrently mutated. A null output is allowed only at size 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn crossflow_bridge_request_json(
    request: *const u8,
    request_len: usize,
    output: *mut u8,
    output_capacity: usize,
    output_required: *mut usize,
) -> i32 {
    if output_required.is_null() || (output_required as usize) % mem::align_of::<usize>() != 0 {
        return INVALID_ARGUMENT;
    }
    if request.is_null() || request_len == 0 || (output.is_null() && output_capacity != 0) {
        return INVALID_ARGUMENT;
    }
    let request_start = request as usize;
    let output_start = output as usize;
    let required_start = output_required as usize;
    let (Some(request_end), Some(output_end), Some(required_end)) = (
        request_start.checked_add(request_len),
        output_start.checked_add(output_capacity),
        required_start.checked_add(mem::size_of::<usize>()),
    ) else {
        return INVALID_ARGUMENT;
    };
    let request_range = (request_start, request_end);
    let output_range = (output_start, output_end);
    let required_range = (required_start, required_end);
    if overlaps(request_range, output_range)
        || overlaps(request_range, required_range)
        || overlaps(output_range, required_range)
    {
        return INVALID_ARGUMENT;
    }
    // SAFETY: numeric checks exclude null/alignment/aliasing mistakes; address
    // accessibility and lifetime are still the caller's responsibility.
    unsafe { output_required.write(0) };
    if request_len > MAX_REQUEST {
        return REQUEST_TOO_LARGE;
    }
    // The bounded view cannot exceed isize::MAX on supported 32/64-bit targets.
    // Invalid non-null pointers violate the caller's contract and are not caught.
    let status = panic_guard(|| {
        // SAFETY: request is non-null, bounded and caller-owned readable memory.
        let input = unsafe { slice::from_raw_parts(request, request_len) };
        let input = match str::from_utf8(input) {
            Ok(input) => input,
            Err(_) => return INVALID_UTF8,
        };
        let response = match serde_json::to_vec(&respond(input)) {
            Ok(response) if response.len() <= MAX_RESPONSE => response,
            _ => return INTERNAL_ERROR,
        };
        // SAFETY: required points to a valid caller-owned size_t.
        unsafe { output_required.write(response.len()) };
        if output_capacity < response.len() {
            return BUFFER_TOO_SMALL;
        }
        // A complete response is always nonempty, so a null output took the
        // capacity=0 probe path above. No terminator is copied.
        // SAFETY: caller provides enough nonoverlapping writable output bytes.
        unsafe { ptr::copy_nonoverlapping(response.as_ptr(), output, response.len()) };
        OK
    });
    if status == INTERNAL_ERROR {
        // Do not expose a panic payload across Swift/WinUI's C ABI boundary.
        unsafe { output_required.write(0) };
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invoke(request: &[u8]) -> (i32, Vec<u8>, usize) {
        let mut required = 999;
        let status = unsafe {
            crossflow_bridge_request_json(
                request.as_ptr(),
                request.len(),
                ptr::null_mut(),
                0,
                &mut required,
            )
        };
        if status != BUFFER_TOO_SMALL {
            return (status, vec![], required);
        }
        let mut output = vec![0xCC; required + 1];
        let status = unsafe {
            crossflow_bridge_request_json(
                request.as_ptr(),
                request.len(),
                output.as_mut_ptr(),
                required,
                &mut required,
            )
        };
        assert_eq!(output[required], 0xCC, "must not write a hidden NUL byte");
        output.truncate(required);
        (status, output, required)
    }

    #[test]
    fn probe_short_buffer_and_exact_write_follow_c_contract() {
        let request = br#"{"op":"environment"}"#;
        let (status, bytes, required) = invoke(request);
        assert_eq!(status, OK);
        assert_eq!(required, bytes.len());
        let response: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response["result"]["session_status"], "not_exposed");
        assert_eq!(response["result"]["capabilities"]["input_injection"], false);
        let mut output = vec![0xA5; required];
        let mut needed = 0;
        let status = unsafe {
            crossflow_bridge_request_json(
                request.as_ptr(),
                request.len(),
                output.as_mut_ptr(),
                required - 1,
                &mut needed,
            )
        };
        assert_eq!(status, BUFFER_TOO_SMALL);
        assert_eq!(needed, required);
        assert!(output.iter().all(|&byte| byte == 0xA5));
    }

    #[test]
    fn malformed_requests_have_fixed_redacted_errors() {
        for input in [
            br#"{"op":"SECRET-TEXT"}"#.as_slice(),
            b"not-json SECRET-TEXT",
            br#"{"op":"environment","secret":"SECRET-TEXT"}"#,
        ] {
            let (status, response, _) = invoke(input);
            assert_eq!(status, OK);
            let text = String::from_utf8(response).unwrap();
            assert!(!text.contains("SECRET-TEXT"));
            let response: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(response["error"]["code"], "invalid_request");
        }
    }

    #[test]
    fn ffi_rejects_null_utf8_and_oversized_input_before_read() {
        let mut needed = 99;
        let status = unsafe {
            crossflow_bridge_request_json(ptr::null(), 1, ptr::null_mut(), 0, &mut needed)
        };
        assert_eq!((status, needed), (INVALID_ARGUMENT, 99));
        assert_eq!(
            unsafe {
                crossflow_bridge_request_json(b"x".as_ptr(), 1, ptr::null_mut(), 0, ptr::null_mut())
            },
            INVALID_ARGUMENT
        );
        let oversized = vec![b'x'; MAX_REQUEST + 1];
        assert_eq!(
            unsafe {
                crossflow_bridge_request_json(
                    oversized.as_ptr(),
                    oversized.len(),
                    ptr::null_mut(),
                    0,
                    &mut needed,
                )
            },
            REQUEST_TOO_LARGE
        );
        assert_eq!(needed, 0);
        assert_eq!(invoke(&[0xFF]).0, INVALID_UTF8);
    }

    #[test]
    fn overlapping_regions_are_rejected_without_mutating_request() {
        let mut storage = [0usize; 16];
        let request = br#"{"op":"environment"}"#;
        let buffer = storage.as_mut_ptr().cast::<u8>();
        unsafe { ptr::copy_nonoverlapping(request.as_ptr(), buffer, request.len()) };
        let before = storage;
        assert_eq!(
            unsafe {
                crossflow_bridge_request_json(
                    buffer,
                    request.len(),
                    ptr::null_mut(),
                    0,
                    buffer.cast(),
                )
            },
            INVALID_ARGUMENT
        );
        assert_eq!(storage, before);
        let mut needed = 999;
        assert_eq!(
            unsafe {
                crossflow_bridge_request_json(buffer, request.len(), buffer, 32, &mut needed)
            },
            INVALID_ARGUMENT
        );
        assert_eq!(needed, 999);
        assert_eq!(storage, before);
        assert_eq!(
            unsafe {
                crossflow_bridge_request_json(
                    request.as_ptr(),
                    request.len(),
                    buffer,
                    32,
                    buffer.cast(),
                )
            },
            INVALID_ARGUMENT
        );
        assert_eq!(storage, before);
    }

    #[test]
    fn panic_payload_never_crosses_the_result_boundary() {
        assert_eq!(
            panic_guard(|| panic!("synthetic confidential fixture")),
            INTERNAL_ERROR
        );
    }

    fn layout(gap: f64) -> Value {
        json!({"screens":[
            {"id":"a","device_id":"windows","x":0,"y":0,"width":100,"height":100},
            {"id":"b","device_id":"mac","x":100.0+gap,"y":0,"width":100,"height":100}
        ]})
    }

    fn crossing(layout: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"op":"layout_crossing","layout":layout,"from_screen":"a","start":{"x":50,"y":50},"end":{"x":150,"y":50}})).unwrap()
    }

    #[test]
    fn abi_uses_core_layout_validation_and_crossings_without_shared_state() {
        let valid = crossing(layout(0.0));
        let first = invoke(&valid).1;
        let response: Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(response["result"]["crossing"]["target_screen_id"], "b");
        let response: Value = serde_json::from_slice(&invoke(&crossing(layout(1.0))).1).unwrap();
        assert!(response["result"]["crossing"].is_null());
        let response: Value = serde_json::from_slice(&invoke(&crossing(layout(-1.0))).1).unwrap();
        assert_eq!(response["error"]["code"], "invalid_layout");
        assert_eq!(invoke(&valid).1, first);
    }

    #[test]
    fn abi_preserves_one_ulp_crossings_and_layout_boundaries() {
        // Do not construct these coordinates through serde_json::Value first:
        // the regression was decimal input rounding onto the adjacent edge.
        let request = |target_x: &str, end_x: &str| {
            format!(
                r#"{{"op":"layout_crossing","layout":{{"screens":[{{"id":"a","device_id":"win","x":0,"y":0,"width":1920,"height":1080}},{{"id":"b","device_id":"mac","x":{target_x},"y":0,"width":1920,"height":1080}}]}},"from_screen":"a","start":{{"x":960,"y":540}},"end":{{"x":{end_x},"y":540}}}}"#
            )
        };
        let response = |target_x: &str, end_x: &str| {
            let (status, bytes, _) = invoke(request(target_x, end_x).as_bytes());
            assert_eq!(status, OK);
            serde_json::from_slice::<Value>(&bytes).unwrap()
        };
        let crossed = response("1920", "1920.0000000000002");
        assert_eq!(crossed["result"]["crossing"]["target_screen_id"], "b");
        let fraction = crossed["result"]["crossing"]["fraction"].as_f64().unwrap();
        assert!(fraction > 0.0 && fraction < 1.0);
        assert!(response("1920", "1920")["result"]["crossing"].is_null());
        assert!(response("1920.0000000000002", "1921")["result"]["crossing"].is_null());
        assert_eq!(
            response("1919.9999999999998", "1921")["error"]["code"],
            "invalid_layout"
        );
    }
}
