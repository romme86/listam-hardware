/* NimBLE GATT provisioning peripheral for the listam leaf. See leaf_prov.h.
 *
 * Advertises a custom 128-bit service with a config-write characteristic and a
 * status-notify characteristic. The central streams the config blob as a short
 * sequence of frames (BEGIN/CHUNK/COMMIT) which this file reassembles and
 * CRC16-verifies; a complete, valid payload is handed to Rust via
 * leaf_prov_wait_payload(). The wire contract mirrors @listam/provisioning. */

#include "leaf_prov.h"

#include <string.h>

#include "freertos/FreeRTOS.h"
#include "freertos/semphr.h"
#include "esp_log.h"

#include "nimble/nimble_port.h"
#include "nimble/nimble_port_freertos.h"
#include "host/ble_hs.h"
#include "host/util/util.h"
#include "services/gap/ble_svc_gap.h"
#include "services/gatt/ble_svc_gatt.h"

static const char *TAG = "leaf_prov";

/* Frame opcodes — must match @listam/provisioning. */
#define FRAME_BEGIN 0x01
#define FRAME_CHUNK 0x02
#define FRAME_COMMIT 0x03

#define MAX_PAYLOAD 1024

/* Custom 128-bit UUIDs (little-endian byte order for BLE_UUID128_INIT). The
 * first ASCII bytes spell "listam-LEAF"; the trailing byte is 01=service,
 * 02=config char, 03=status char. */
static const ble_uuid128_t SVC_UUID = BLE_UUID128_INIT(
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46, 0x41,
    0x45, 0x4c, 0x6d, 0x61, 0x74, 0x73, 0x69, 0x6c);
static const ble_uuid128_t CHR_CONFIG_UUID = BLE_UUID128_INIT(
    0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46, 0x41,
    0x45, 0x4c, 0x6d, 0x61, 0x74, 0x73, 0x69, 0x6c);
static const ble_uuid128_t CHR_STATUS_UUID = BLE_UUID128_INIT(
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46, 0x41,
    0x45, 0x4c, 0x6d, 0x61, 0x74, 0x73, 0x69, 0x6c);

static uint8_t s_own_addr_type;
static uint16_t s_conn_handle = BLE_HS_CONN_HANDLE_NONE;
static uint16_t s_status_val_handle;
static SemaphoreHandle_t s_payload_sem;

/* Reassembly state. */
static uint8_t s_reasm[MAX_PAYLOAD];
static int s_expected_len;
static int s_high_water; /* highest offset+len written, to detect completeness */
static int s_in_progress;

/* Delivered payload (copied out under the semaphore). */
static uint8_t s_payload[MAX_PAYLOAD];
static int s_payload_len;

static void start_advertising(void);

/* CRC16/CCITT-FALSE (poly 0x1021, init 0xFFFF) — matches the JS implementation. */
static uint16_t crc16_ccitt(const uint8_t *data, int len)
{
    uint16_t crc = 0xFFFF;
    for (int i = 0; i < len; i++) {
        crc ^= (uint16_t)data[i] << 8;
        for (int b = 0; b < 8; b++) {
            crc = (crc & 0x8000) ? (uint16_t)((crc << 1) ^ 0x1021) : (uint16_t)(crc << 1);
        }
    }
    return crc;
}

static void reset_reasm(void)
{
    s_expected_len = 0;
    s_high_water = 0;
    s_in_progress = 0;
}

void leaf_prov_notify_status(uint8_t code)
{
    if (s_conn_handle == BLE_HS_CONN_HANDLE_NONE) {
        return;
    }
    struct os_mbuf *om = ble_hs_mbuf_from_flat(&code, 1);
    if (om == NULL) {
        return;
    }
    ble_gatts_notify_custom(s_conn_handle, s_status_val_handle, om);
}

/* Process one inbound frame on the config characteristic. */
static void handle_frame(const uint8_t *f, int n)
{
    if (n < 1) {
        return;
    }
    switch (f[0]) {
    case FRAME_BEGIN: {
        if (n < 3) {
            return;
        }
        int len = f[1] | (f[2] << 8);
        if (len <= 0 || len > MAX_PAYLOAD) {
            reset_reasm();
            leaf_prov_notify_status(LEAF_PROV_STATUS_ERR_DECODE);
            return;
        }
        memset(s_reasm, 0, sizeof(s_reasm));
        s_expected_len = len;
        s_high_water = 0;
        s_in_progress = 1;
        leaf_prov_notify_status(LEAF_PROV_STATUS_RECEIVING);
        break;
    }
    case FRAME_CHUNK: {
        if (n < 3 || !s_in_progress) {
            return;
        }
        int off = f[1] | (f[2] << 8);
        int data_len = n - 3;
        if (off < 0 || off + data_len > s_expected_len) {
            return; /* out of range; ignore */
        }
        memcpy(s_reasm + off, f + 3, data_len);
        if (off + data_len > s_high_water) {
            s_high_water = off + data_len;
        }
        break;
    }
    case FRAME_COMMIT: {
        if (n < 3 || !s_in_progress) {
            return;
        }
        uint16_t want = (uint16_t)(f[1] | (f[2] << 8));
        if (s_high_water != s_expected_len) {
            reset_reasm();
            leaf_prov_notify_status(LEAF_PROV_STATUS_ERR_DECODE);
            return;
        }
        if (crc16_ccitt(s_reasm, s_expected_len) != want) {
            reset_reasm();
            leaf_prov_notify_status(LEAF_PROV_STATUS_ERR_CRC);
            return;
        }
        /* Hand the verified payload to Rust. */
        s_payload_len = s_expected_len;
        memcpy(s_payload, s_reasm, s_payload_len);
        reset_reasm();
        xSemaphoreGive(s_payload_sem);
        break;
    }
    default:
        break;
    }
}

static int gatt_config_access(uint16_t conn_handle, uint16_t attr_handle,
                              struct ble_gatt_access_ctxt *ctxt, void *arg)
{
    (void)conn_handle;
    (void)attr_handle;
    (void)arg;
    if (ctxt->op != BLE_GATT_ACCESS_OP_WRITE_CHR) {
        return BLE_ATT_ERR_UNLIKELY;
    }
    uint8_t buf[256];
    uint16_t copied = 0;
    int rc = ble_hs_mbuf_to_flat(ctxt->om, buf, sizeof(buf), &copied);
    if (rc != 0) {
        return BLE_ATT_ERR_UNLIKELY;
    }
    handle_frame(buf, copied);
    return 0;
}

/* Notify-only: a read should never happen, but NimBLE requires a callback. */
static int gatt_status_access(uint16_t conn_handle, uint16_t attr_handle,
                              struct ble_gatt_access_ctxt *ctxt, void *arg)
{
    (void)conn_handle;
    (void)attr_handle;
    (void)ctxt;
    (void)arg;
    return 0;
}

static const struct ble_gatt_svc_def gatt_svcs[] = {
    {
        .type = BLE_GATT_SVC_TYPE_PRIMARY,
        .uuid = &SVC_UUID.u,
        .characteristics = (struct ble_gatt_chr_def[]){
            {
                .uuid = &CHR_CONFIG_UUID.u,
                .access_cb = gatt_config_access,
                .flags = BLE_GATT_CHR_F_WRITE | BLE_GATT_CHR_F_WRITE_NO_RSP,
            },
            {
                .uuid = &CHR_STATUS_UUID.u,
                .access_cb = gatt_status_access,
                .flags = BLE_GATT_CHR_F_NOTIFY,
                .val_handle = &s_status_val_handle,
            },
            {0},
        },
    },
    {0},
};

static int gap_event(struct ble_gap_event *event, void *arg)
{
    (void)arg;
    switch (event->type) {
    case BLE_GAP_EVENT_CONNECT:
        if (event->connect.status == 0) {
            s_conn_handle = event->connect.conn_handle;
            ESP_LOGI(TAG, "central connected");
        } else {
            start_advertising();
        }
        return 0;
    case BLE_GAP_EVENT_DISCONNECT:
        ESP_LOGI(TAG, "central disconnected (reason %d)", event->disconnect.reason);
        s_conn_handle = BLE_HS_CONN_HANDLE_NONE;
        reset_reasm();
        start_advertising();
        return 0;
    case BLE_GAP_EVENT_ADV_COMPLETE:
        start_advertising();
        return 0;
    default:
        return 0;
    }
}

static void start_advertising(void)
{
    const char *name = ble_svc_gap_device_name();

    /* Advertising packet: flags + the 128-bit service UUID (already ~21 bytes,
     * so the name goes in the scan response to stay within 31 bytes). */
    struct ble_hs_adv_fields fields;
    memset(&fields, 0, sizeof(fields));
    fields.flags = BLE_HS_ADV_F_DISC_GEN | BLE_HS_ADV_F_BREDR_UNSUP;
    fields.uuids128 = (ble_uuid128_t *)&SVC_UUID;
    fields.num_uuids128 = 1;
    fields.uuids128_is_complete = 1;
    int rc = ble_gap_adv_set_fields(&fields);
    if (rc != 0) {
        ESP_LOGE(TAG, "adv_set_fields rc=%d", rc);
        return;
    }

    /* Scan response: the full device name. */
    struct ble_hs_adv_fields rsp;
    memset(&rsp, 0, sizeof(rsp));
    rsp.name = (uint8_t *)name;
    rsp.name_len = strlen(name);
    rsp.name_is_complete = 1;
    ble_gap_adv_rsp_set_fields(&rsp);

    struct ble_gap_adv_params adv_params;
    memset(&adv_params, 0, sizeof(adv_params));
    adv_params.conn_mode = BLE_GAP_CONN_MODE_UND;
    adv_params.disc_mode = BLE_GAP_DISC_MODE_GEN;
    rc = ble_gap_adv_start(s_own_addr_type, NULL, BLE_HS_FOREVER, &adv_params, gap_event, NULL);
    if (rc != 0) {
        ESP_LOGE(TAG, "adv_start rc=%d", rc);
    }
}

static void on_sync(void)
{
    int rc = ble_hs_id_infer_auto(0, &s_own_addr_type);
    if (rc != 0) {
        ESP_LOGE(TAG, "ble_hs_id_infer_auto rc=%d", rc);
        return;
    }
    start_advertising();
}

static void on_reset(int reason)
{
    ESP_LOGW(TAG, "nimble host reset (reason %d)", reason);
}

static void host_task(void *param)
{
    (void)param;
    nimble_port_run(); /* returns only after nimble_port_stop() */
    nimble_port_freertos_deinit();
}

int leaf_prov_start(const char *device_name)
{
    if (s_payload_sem == NULL) {
        s_payload_sem = xSemaphoreCreateBinary();
        if (s_payload_sem == NULL) {
            return -1;
        }
    }
    reset_reasm();
    s_conn_handle = BLE_HS_CONN_HANDLE_NONE;

    esp_err_t err = nimble_port_init();
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "nimble_port_init failed: %d", err);
        return -2;
    }

    ble_svc_gap_init();
    ble_svc_gatt_init();

    int rc = ble_gatts_count_cfg(gatt_svcs);
    if (rc != 0) {
        ESP_LOGE(TAG, "ble_gatts_count_cfg rc=%d", rc);
        return -3;
    }
    rc = ble_gatts_add_svcs(gatt_svcs);
    if (rc != 0) {
        ESP_LOGE(TAG, "ble_gatts_add_svcs rc=%d", rc);
        return -4;
    }

    rc = ble_svc_gap_device_name_set(device_name);
    if (rc != 0) {
        ESP_LOGW(TAG, "device_name_set rc=%d", rc);
    }

    ble_hs_cfg.sync_cb = on_sync;
    ble_hs_cfg.reset_cb = on_reset;

    nimble_port_freertos_init(host_task);
    return 0;
}

int leaf_prov_wait_payload(uint8_t *out, int max_len, int timeout_ms)
{
    if (s_payload_sem == NULL) {
        return -1;
    }
    TickType_t ticks = (timeout_ms < 0) ? portMAX_DELAY : pdMS_TO_TICKS(timeout_ms);
    if (xSemaphoreTake(s_payload_sem, ticks) != pdTRUE) {
        return 0;
    }
    int n = s_payload_len;
    if (n > max_len) {
        n = max_len;
    }
    memcpy(out, s_payload, n);
    return n;
}

void leaf_prov_stop(void)
{
    ble_gap_adv_stop();
    int rc = nimble_port_stop();
    if (rc == 0) {
        nimble_port_deinit();
    }
}
