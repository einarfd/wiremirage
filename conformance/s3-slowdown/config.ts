// Companion config route for the injection mock. Writes the request body
// verbatim into GROUP state under "inject:rules", so the mock can be
// (re)configured at runtime via an ordinary mock request. WireMirage has no
// public state-write API, so a mock route is the channel a reusable,
// runtime-parameterized mock uses to receive its config.
//
// Mounted in the same group as inject.ts so they share group state.

export function handle(req, _routeStore, groupStore) {
  const bytes = req && req.body ? new Uint8Array(req.body) : new Uint8Array();
  groupStore.set("inject:rules", bytes);
  return {
    status: 200,
    headers: [["content-type", "application/json"]],
    body: new TextEncoder().encode('{"ok":true,"rules_bytes":' + bytes.length + "}"),
  };
}
