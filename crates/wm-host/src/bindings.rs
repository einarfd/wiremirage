// wasmtime component bindings for the WireMirage handler world.
//
// `with` maps the WIT `bucket` resource to our `Bucket` enum so the
// generated HostBucket trait methods take `Resource<Bucket>` directly. The
// enum dispatches at the op level to the appropriate backend (in-memory or
// Valkey) per `crate::store`.
//
// `imports: { default: trappable }` makes all generated host-import method
// signatures return `wasmtime::Result<T>` so we can trap the guest on errors
// (e.g. WRONGTYPE bucket access, non-integer incr).

wasmtime::component::bindgen!({
    path: "../../wit",
    world: "handler",
    with: {
        "wiremirage:handler/store.bucket": crate::store::Bucket,
    },
    imports: { default: trappable },
});
