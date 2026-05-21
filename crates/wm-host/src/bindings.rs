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

// Engine-world bindings (ADR-0020). Same shape as the handler world
// but with the extra `engine-host` import that delivers per-route
// source to `js-engine.wasm` at request time. Generated alongside
// the handler bindings so the host can use whichever shape matches
// the component it's instantiating.
pub mod engine_bindings {
    // The engine bindgen generates its own copies of Request /
    // Response — bindgen's `with` doesn't easily share types that
    // are `use`'d (rather than imported) by the target world. The
    // dispatch path converts between the two with `From` impls
    // below. Sharing the Bucket resource via `with` IS load-bearing
    // though — both worlds must use the same resource-table backing
    // type so the host can push one bucket and pass it to either.
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "engine",
        with: {
            "wiremirage:handler/store.bucket": crate::store::Bucket,
        },
        imports: { default: trappable },
    });
}

/// Convert the engine-world's Request type to the handler-world's
/// Request. Same field shape; only the module path differs.
pub fn engine_request_to_handler(
    req: engine_bindings::wiremirage::handler::http::Request,
) -> wiremirage::handler::http::Request {
    wiremirage::handler::http::Request {
        method: req.method,
        path: req.path,
        matched_pattern: req.matched_pattern,
        path_params: req.path_params,
        query: req.query,
        headers: req.headers,
        body: req.body,
    }
}

/// Convert the handler-world Request to the engine-world Request.
/// The dispatcher builds a single `wiremirage:handler::http::Request`
/// from the incoming axum request, then translates it for the
/// engine call here.
pub fn handler_request_to_engine(
    req: wiremirage::handler::http::Request,
) -> engine_bindings::wiremirage::handler::http::Request {
    engine_bindings::wiremirage::handler::http::Request {
        method: req.method,
        path: req.path,
        matched_pattern: req.matched_pattern,
        path_params: req.path_params,
        query: req.query,
        headers: req.headers,
        body: req.body,
    }
}

/// Convert the engine-world Response back to the handler-world
/// Response so the dispatcher's existing serializer / journal-
/// writer / axum-response builder picks it up.
pub fn engine_response_to_handler(
    resp: engine_bindings::wiremirage::handler::http::Response,
) -> wiremirage::handler::http::Response {
    wiremirage::handler::http::Response {
        status: resp.status,
        headers: resp.headers,
        body: resp.body,
    }
}
