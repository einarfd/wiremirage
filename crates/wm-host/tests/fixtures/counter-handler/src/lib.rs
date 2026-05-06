// Counter fixture handler. Increments key "count" in the route-private
// store by 1 on each request and returns the new value as the body.
//
// Used to verify that handler state persists across requests when the
// storage backend is Valkey-backed (slice 2 onward).

wit_bindgen::generate!({
    path: "../../../../../wit",
    world: "handler",
});

struct Component;

impl Guest for Component {
    fn handle(
        _req: wiremirage::handler::http::Request,
        route_store: &wiremirage::handler::store::Bucket,
        _group_store: &wiremirage::handler::store::Bucket,
    ) -> wiremirage::handler::http::Response {
        let count = route_store.incr("count", 1);
        let body = format!("count={count}").into_bytes();
        wiremirage::handler::http::Response {
            status: 200,
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body,
        }
    }
}

export!(Component);
