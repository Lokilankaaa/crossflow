#ifndef CROSSFLOW_BRIDGE_H
#define CROSSFLOW_BRIDGE_H

#include <stddef.h>
#include <stdint.h>

#if defined(_WIN32) && !defined(CROSSFLOW_BRIDGE_STATIC)
#define CROSSFLOW_BRIDGE_API __declspec(dllimport)
#else
#define CROSSFLOW_BRIDGE_API
#endif

#if defined(_WIN32)
#define CROSSFLOW_BRIDGE_CALL __cdecl
#else
#define CROSSFLOW_BRIDGE_CALL
#endif

#ifdef __cplusplus
extern "C" {
#endif

#define CROSSFLOW_BRIDGE_API_VERSION UINT32_C(1)
#define CROSSFLOW_BRIDGE_MAX_REQUEST ((size_t)1048576)
#define CROSSFLOW_BRIDGE_MAX_RESPONSE ((size_t)1048576)

enum CrossflowBridgeStatus {
    CROSSFLOW_BRIDGE_OK = 0,
    CROSSFLOW_BRIDGE_BUFFER_TOO_SMALL = 1,
    CROSSFLOW_BRIDGE_INVALID_ARGUMENT = 2,
    CROSSFLOW_BRIDGE_REQUEST_TOO_LARGE = 3,
    CROSSFLOW_BRIDGE_INVALID_UTF8 = 4,
    CROSSFLOW_BRIDGE_INTERNAL_ERROR = 5
};

/* No allocation, thread creation, device discovery or system input. */
CROSSFLOW_BRIDGE_API uint32_t CROSSFLOW_BRIDGE_CALL
crossflow_bridge_api_version(void);

/* Stateless, read-only request. All buffers belong to the caller; no Rust
 * allocation crosses the boundary and no free function is needed.
 *
 * request: non-null pointer to request_len readable UTF-8 bytes; no NUL needed.
 * output: output_capacity writable bytes, or NULL only with capacity 0.
 * output_required: non-null, aligned writable size_t, distinct from both buffers.
 * The request/output memory regions must not overlap and must remain valid and
 * unmodified by other threads during the call. Invalid non-null pointers are a
 * caller error: no C ABI can safely validate arbitrary address accessibility.
 *
 * Required length excludes a terminator. No NUL is written. Probe with output
 * NULL/capacity 0, allocate output_required bytes, then repeat the same request.
 * BUFFER_TOO_SMALL sets output_required and leaves output bytes unchanged.
 * Other ABI errors set output_required=0 after numeric pointer/overlap checks.
 * Null, misaligned, overlapping or overflowing regions are rejected without
 * writing output_required. Accessibility of non-null addresses remains the
 * caller's responsibility; it cannot be established by these numeric checks.
 * OK means a complete JSON response was written; inspect its "ok" member for
 * application validation failures. A malformed JSON/layout is not an ABI error.
 *
 * Requests (unknown fields/operations are rejected):
 * {"op":"environment"}
 * {"op":"layout_validate","layout":{"screens":[...]}}
 * {"op":"layout_crossing","layout":{"screens":[...]},
 *  "from_screen":"screen-id","start":{"x":1,"y":2},"end":{"x":3,"y":4}}
 *
 * Responses:
 * {"api_version":1,"ok":true,"result":{...}}
 * {"api_version":1,"ok":false,"error":{"code":"...","message":"..."}}
 *
 * This API cannot start capture/injection, pair devices, apply a layout to a
 * running session, or report connection state. Parallel calls are independent.
 * Rust unwinding is caught; process aborts/OOM and invalid pointers cannot be
 * converted into recoverable errors. Build the bridge with panic=unwind.
 */
CROSSFLOW_BRIDGE_API int32_t CROSSFLOW_BRIDGE_CALL
crossflow_bridge_request_json(const uint8_t *request, size_t request_len,
                             uint8_t *output, size_t output_capacity,
                             size_t *output_required);

#ifdef __cplusplus
}
#endif
#endif
