/* Real C client for the public ABI; no Rust test-only entry points. */
#include "crossflow.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define CHECK(condition) do { if (!(condition)) { \
    fprintf(stderr, "C ABI check failed at line %d\n", __LINE__); exit(1); \
} } while (0)

static char *request(const char *json) {
    size_t required = 0;
    int32_t status = crossflow_bridge_request_json((const uint8_t *)json,
        strlen(json), NULL, 0, &required);
    CHECK(status == CROSSFLOW_BRIDGE_BUFFER_TOO_SMALL);
    CHECK(required > 0 && required <= CROSSFLOW_BRIDGE_MAX_RESPONSE);
    uint8_t *bytes = (uint8_t *)malloc(required + 1);
    CHECK(bytes != NULL);
    memset(bytes, 0xA5, required + 1);
    size_t needed = 0;
    status = crossflow_bridge_request_json((const uint8_t *)json, strlen(json),
        bytes, required - 1, &needed);
    CHECK(status == CROSSFLOW_BRIDGE_BUFFER_TOO_SMALL && needed == required);
    for (size_t i = 0; i <= required; ++i) CHECK(bytes[i] == 0xA5);
    status = crossflow_bridge_request_json((const uint8_t *)json, strlen(json),
        bytes, required, &needed);
    CHECK(status == CROSSFLOW_BRIDGE_OK && needed == required);
    CHECK(bytes[required] == 0xA5); /* No hidden terminator/overwrite. */
    bytes[required] = 0; /* C caller owns adding a terminator, if desired. */
    return (char *)bytes;
}

#define SCREEN_A "{\"id\":\"a\",\"device_id\":\"windows\",\"x\":0,\"y\":0,\"width\":100,\"height\":100}"
#define SCREEN_B "{\"id\":\"b\",\"device_id\":\"mac\",\"x\":100,\"y\":0,\"width\":100,\"height\":100}"
#define SCREEN_GAP "{\"id\":\"b\",\"device_id\":\"mac\",\"x\":101,\"y\":0,\"width\":100,\"height\":100}"
#define SCREEN_OVERLAP "{\"id\":\"b\",\"device_id\":\"mac\",\"x\":99,\"y\":0,\"width\":100,\"height\":100}"
#define TRACE_PREFIX "{\"op\":\"layout_crossing\",\"layout\":{\"screens\":[" SCREEN_A ","
#define TRACE_SUFFIX "]},\"from_screen\":\"a\",\"start\":{\"x\":50,\"y\":50},\"end\":{\"x\":150,\"y\":50}}"

int main(void) {
    CHECK(crossflow_bridge_api_version() == CROSSFLOW_BRIDGE_API_VERSION);
    char *environment = request("{\"op\":\"environment\"}");
    CHECK(strstr(environment, "\"session_status\":\"not_exposed\"") != NULL);
    CHECK(strstr(environment, "\"input_injection\":false") != NULL);
    free(environment);

    char *first = request(TRACE_PREFIX SCREEN_B TRACE_SUFFIX);
    CHECK(strstr(first, "\"target_screen_id\":\"b\"") != NULL);
    char *gap = request(TRACE_PREFIX SCREEN_GAP TRACE_SUFFIX);
    CHECK(strstr(gap, "\"crossing\":null") != NULL);
    free(gap);
    char *overlap = request(TRACE_PREFIX SCREEN_OVERLAP TRACE_SUFFIX);
    CHECK(strstr(overlap, "\"code\":\"invalid_layout\"") != NULL);
    free(overlap);
    char *again = request(TRACE_PREFIX SCREEN_B TRACE_SUFFIX);
    CHECK(strcmp(first, again) == 0); /* No layout state leaked across calls. */
    free(first);
    free(again);

    char *valid = request("{\"op\":\"layout_validate\",\"layout\":{\"screens\":[" SCREEN_A "]}}");
    CHECK(strstr(valid, "\"screen_count\":1") != NULL);
    CHECK(strstr(valid, "\"applied_to_session\":false") != NULL);
    free(valid);
    char *invalid = request("invalid-json CONFIDENTIAL-FIXTURE");
    CHECK(strstr(invalid, "\"code\":\"invalid_request\"") != NULL);
    CHECK(strstr(invalid, "CONFIDENTIAL-FIXTURE") == NULL);
    free(invalid);

    const uint8_t bad_utf8[] = {0xFF};
    size_t needed = 42;
    CHECK(crossflow_bridge_request_json(bad_utf8, 1, NULL, 0, &needed) == CROSSFLOW_BRIDGE_INVALID_UTF8);
    CHECK(needed == 0);
    CHECK(crossflow_bridge_request_json(NULL, 1, NULL, 0, &needed) == CROSSFLOW_BRIDGE_INVALID_ARGUMENT);
    CHECK(crossflow_bridge_request_json(bad_utf8, 1, NULL, 0, NULL) == CROSSFLOW_BRIDGE_INVALID_ARGUMENT);
    uint8_t *large = (uint8_t *)malloc(CROSSFLOW_BRIDGE_MAX_REQUEST + 1);
    CHECK(large != NULL);
    CHECK(crossflow_bridge_request_json(large, CROSSFLOW_BRIDGE_MAX_REQUEST + 1,
        NULL, 0, &needed) == CROSSFLOW_BRIDGE_REQUEST_TOO_LARGE);
    free(large);

    uint8_t *aliased = (uint8_t *)malloc(64);
    CHECK(aliased != NULL);
    memcpy(aliased, "{\"op\":\"environment\"}", 20);
    CHECK(crossflow_bridge_request_json(aliased, 20, NULL, 0,
        (size_t *)aliased) == CROSSFLOW_BRIDGE_INVALID_ARGUMENT);
    CHECK(crossflow_bridge_request_json(aliased, 20, aliased, 64,
        &needed) == CROSSFLOW_BRIDGE_INVALID_ARGUMENT);
    free(aliased);
    puts("Crossflow C ABI smoke passed: bounds, ownership, validation, crossing, isolation.");
    return 0;
}
