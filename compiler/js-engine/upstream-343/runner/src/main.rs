use anyhow::Result;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

struct Host;

fn main() -> Result<()> {
    let mut cfg = Config::new();
    cfg.wasm_component_model(true);
    let engine = Engine::new(&cfg)?;
    let path = std::env::args().nth(1).unwrap();

    let fresh = |name: &str| {
        let component = Component::from_file(&engine, &path).unwrap();
        let linker: Linker<Host> = Linker::new(&engine);
        let mut store = Store::new(&engine, Host);
        let instance = linker.instantiate(&mut store, &component).unwrap();
        (store, instance, name.to_string())
    };

    let (mut s, i, n) = fresh("sink-s64");
    let f = i.get_typed_func::<(i64,), ()>(&mut s, &n)?;
    println!("sink-s64(-1)    [negative IN, no return ] = {}", match f.call(&mut s, (-1,)) { Ok(_) => "ok".into(), Err(_) => "TRAP".to_string() });

    let (mut s, i, n) = fresh("source-s64");
    let f = i.get_typed_func::<(), (i64,)>(&mut s, &n)?;
    println!("source-s64()    [negative OUT, no params] = {}", match f.call(&mut s, ()) { Ok((v,)) => format!("{v}"), Err(_) => "TRAP".to_string() });

    let (mut s, i, n) = fresh("sink-u64");
    let f = i.get_typed_func::<(u64,), ()>(&mut s, &n)?;
    println!("sink-u64(2^63)  [control: bit 63 set    ] = {}", match f.call(&mut s, (1u64 << 63,)) { Ok(_) => "ok".into(), Err(_) => "TRAP".to_string() });
    Ok(())
}
