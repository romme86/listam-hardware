# leaf_prov: a minimal NimBLE GATT "provisioning" peripheral, exposed to Rust
# via a pure-C C-ABI (leaf_prov.h, bound through esp-idf-sys bindings_module).
# Requires the `bt` component (NimBLE host + controller); enable NimBLE in
# sdkconfig.defaults (CONFIG_BT_ENABLED + CONFIG_BT_NIMBLE_ENABLED).
idf_component_register(
    SRCS "leaf_prov.c"
    INCLUDE_DIRS "."
    REQUIRES bt
)
