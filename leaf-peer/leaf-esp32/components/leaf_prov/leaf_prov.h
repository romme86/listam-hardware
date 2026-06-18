#pragma once
/* C-ABI shim exposing a minimal NimBLE GATT "provisioning" peripheral to Rust.
 * Bound into Rust via esp-idf-sys `bindings_module = "leaf_prov"`. Keep this
 * header pure C so bindgen handles it.
 *
 * The leaf advertises a custom service with two characteristics:
 *   - config (write):   the central streams a framed config blob in
 *   - status (notify):  the leaf reports progress/result codes
 * Framing (BEGIN/CHUNK/COMMIT) + CRC16 verification happen in the .c side, so
 * Rust only ever sees a complete, CRC-checked payload. See
 * listam-packages/packages/provisioning/index.mjs for the wire contract. */
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Status codes — must match @listam/provisioning STATUS. */
enum {
    LEAF_PROV_STATUS_IDLE = 0,
    LEAF_PROV_STATUS_RECEIVING = 1,
    LEAF_PROV_STATUS_APPLYING = 2,
    LEAF_PROV_STATUS_OK = 3,
    LEAF_PROV_STATUS_ERR_CRC = 4,
    LEAF_PROV_STATUS_ERR_DECODE = 5,
    LEAF_PROV_STATUS_ERR_VALIDATE = 6,
    LEAF_PROV_STATUS_ERR_NVS = 7
};

/* Initialize NimBLE, register the GATT provisioning service, and start
 * advertising under `device_name`. Returns 0 on success, negative on error. */
int leaf_prov_start(const char *device_name);

/* Block until a complete, CRC-verified config payload arrives (or timeout).
 * Copies up to `max_len` bytes into `out`. Returns the payload length (>0),
 * 0 on timeout, or negative on error. `timeout_ms < 0` waits forever. */
int leaf_prov_wait_payload(uint8_t *out, int max_len, int timeout_ms);

/* Notify the connected central of a one-byte status code. No-op if no central
 * is connected or no subscription is active. */
void leaf_prov_notify_status(uint8_t code);

/* Stop advertising and tear down NimBLE + the BT controller, reclaiming its
 * RAM so the normal (provisioned) firmware path can run after esp_restart(). */
void leaf_prov_stop(void);

#ifdef __cplusplus
}
#endif
