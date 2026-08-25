use anyhow::Result;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

struct Host;

fn main() -> Result<()> {
    let mut cfg = Config::new();
    cfg.wasm_component_model(true);
    let engine = Engine::new(&cfg)?;
    let path = std::env::args().nth(1).unwrap();

    let build = || {
        let component = Component::from_file(&engine, &path).unwrap();
        let mut linker: Linker<Host> = Linker::new(&engine);
        linker
            .instance("example:signed/host-api")
            .unwrap()
            .func_wrap("give-signed", |_c, (): ()| {
                println!("      [host returning -7 to the guest]");
                Ok((-7i64,))
            })
            .unwrap();
        let mut store = Store::new(&engine, Host);
        let instance = linker.instantiate(&mut store, &component).unwrap();
        (store, instance)
    };

    let (mut s, i) = build();
    let f = i.get_typed_func::<(i64,), ()>(&mut s, "sink-s64")?;
    println!("sink-s64(-1)       [lift export param      ] = {}",
        match f.call(&mut s, (-1,)) { Ok(_) => "ok".into(), Err(_) => "TRAP".to_string() });

    let (mut s, i) = build();
    let f = i.get_typed_func::<(), ()>(&mut s, "pull-from-import")?;
    println!("pull-from-import() [lift import return     ] = {}",
        match f.call(&mut s, ()) { Ok(_) => "ok".into(), Err(_) => "TRAP".to_string() });

    let (mut s, i) = build();
    let f = i.get_typed_func::<(), (i64,)>(&mut s, "source-s64")?;
    println!("source-s64()       [lower export return    ] = {}",
        match f.call(&mut s, ()) { Ok((v,)) => format!("{v}"), Err(_) => "TRAP".to_string() });

    let (mut s, i) = build();
    let f = i.get_typed_func::<(), (i64,)>(&mut s, "pull-and-return")?;
    println!("pull-and-return()  [lift then lower        ] = {}",
        match f.call(&mut s, ()) { Ok((v,)) => format!("{v}"), Err(_) => "TRAP".to_string() });
    let (mut s, i) = build();
    let f = i.get_typed_func::<(), (bool,)>(&mut s, "lift-matches")?;
    println!("lift-matches()     [lift import VALUE      ] = {}",
        match f.call(&mut s, ()) {
            Ok((true,)) => "ok".to_string(),
            Ok((false,)) => "WRONG VALUE (lifted as unsigned)".to_string(),
            Err(_) => "TRAP".to_string(),
        });

    let (mut s, i) = build();
    let f = i.get_typed_func::<(u64,), ()>(&mut s, "sink-u64")?;
    println!("sink-u64(2^63)     [control: bit 63 set    ] = {}",
        match f.call(&mut s, (1u64 << 63,)) { Ok(_) => "ok".into(), Err(_) => "TRAP".to_string() });
    Ok(())
}
