// Echo fixture handler. Mirrors the request method and path back in the body.
// Used by tier-2 integration tests in wm-host to exercise the wasmtime + WIT
// path end-to-end without depending on the TS compiler sidecar.

wit_bindgen::generate!({
    path: "../../../../../wit",
    world: "handler",
});

struct Component;

impl Guest for Component {
    fn handle(
        req: wiremirage::handler::http::Request,
        _route_store: &wiremirage::handler::store::Bucket,
        _group_store: &wiremirage::handler::store::Bucket,
    ) -> wiremirage::handler::http::Response {
        let body = format!("echo: {} {}", req.method, req.path).into_bytes();
        wiremirage::handler::http::Response {
            status: 200,
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body,
        }
    }
}

export!(Component);
