// wasmtime component bindings for the WireMirage handler world.
//
// `with` maps the WIT `bucket` resource to our concrete `MemBucket` so the
// generated HostBucket trait methods take `Resource<MemBucket>` directly. In
// slice 2 this will switch to a Valkey-backed concrete type.
//
// `imports: { default: trappable }` makes all generated host-import method
// signatures return `wasmtime::Result<T>` so we can trap the guest on errors
// (e.g. WRONGTYPE bucket access, non-integer incr).

wasmtime::component::bindgen!({
    path: "../../wit",
    world: "handler",
    with: {
        "wiremirage:handler/store.bucket": crate::store::MemBucket,
    },
    imports: { default: trappable },
});
