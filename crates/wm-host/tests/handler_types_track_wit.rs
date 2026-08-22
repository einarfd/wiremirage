//! The shipped handler `.d.ts` must describe the store the WIT actually
//! exposes (ADR-0038).
//!
//! `types/wiremirage-handler.d.ts` is hand-written, not generated: the
//! handler surface is a JavaScript-ergonomics layer over the WIT (camelCased
//! names, a `host` global, accessor helpers), so WIT-generated bindings would
//! be the wrong shape. That leaves a sync obligation, and the ADR is explicit
//! that a drifted `.d.ts` is worse than none — it type-checks a contract the
//! host does not implement. This test is the mitigation: it fails when the
//! two disagree in either direction.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `list-range` -> `listRange`
fn camel(kebab: &str) -> String {
    let mut out = String::new();
    let mut upper = false;
    for c in kebab.chars() {
        if c == '-' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Function names declared inside the WIT `interface bucket { ... }` block.
fn wit_bucket_functions(wit: &str) -> Vec<String> {
    let start = wit
        .find("resource bucket {")
        .expect("wit declares the `bucket` resource");
    let body = &wit[start..];
    let end = body.find("\n  }").expect("bucket block is closed");
    body[..end]
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with("//") {
                return None;
            }
            let (name, rest) = line.split_once(':')?;
            rest.trim_start()
                .starts_with("func(")
                .then(|| name.trim().to_string())
        })
        .collect()
}

/// Method names declared on the `.d.ts`'s store interface.
fn dts_store_methods(dts: &str) -> Vec<String> {
    let start = dts
        .find("export interface WireMirageStore {")
        .expect(".d.ts declares WireMirageStore");
    let body = &dts[start..];
    let end = body.find("\n}").expect("interface is closed");
    body[..end]
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with("//") || line.starts_with('*') || line.starts_with("/*") {
                return None;
            }
            let (name, rest) = line.split_once('(')?;
            (!name.is_empty() && rest.contains(')')).then(|| name.trim().to_string())
        })
        .collect()
}

#[test]
fn every_wit_store_function_is_typed() {
    let root = repo_root();
    let wit = std::fs::read_to_string(root.join("wit/wiremirage.wit")).expect("read wit");
    let dts =
        std::fs::read_to_string(root.join("types/wiremirage-handler.d.ts")).expect("read .d.ts");

    let missing: Vec<String> = wit_bucket_functions(&wit)
        .into_iter()
        .map(|f| camel(&f))
        .filter(|m| !dts.contains(&format!("{m}(")))
        .collect();

    assert!(
        missing.is_empty(),
        "wit/wiremirage.wit exposes store functions the handler .d.ts doesn't type: {missing:?}\n\
         Add them to types/wiremirage-handler.d.ts (camelCased, s64/u64 as bigint)."
    );
}

#[test]
fn the_dts_invents_nothing() {
    let root = repo_root();
    let wit = std::fs::read_to_string(root.join("wit/wiremirage.wit")).expect("read wit");
    let dts =
        std::fs::read_to_string(root.join("types/wiremirage-handler.d.ts")).expect("read .d.ts");

    let wit_camel: Vec<String> = wit_bucket_functions(&wit)
        .iter()
        .map(|f| camel(f))
        .collect();
    let invented: Vec<String> = dts_store_methods(&dts)
        .into_iter()
        .filter(|m| !wit_camel.contains(m))
        .collect();

    assert!(
        invented.is_empty(),
        "the handler .d.ts types store methods the WIT does not expose: {invented:?}\n\
         A handler written against those would compile and then fail at runtime."
    );
}
