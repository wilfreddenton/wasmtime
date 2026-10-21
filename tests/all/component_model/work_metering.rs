//! Function lifting fuel, deferred access, and excluded transfer paths.

#![cfg(not(miri))]

use super::{ApiStyle, config};
use std::pin::Pin;
use std::task::{Context, Poll};
use wasmtime::component::{
    Component, Instance, Linker, Source, StreamConsumer, StreamReader, StreamResult, WasmStr,
};
use wasmtime::{Engine, LiftingPrices, Result, Store, StoreContextMut, Trap};

mod asynchronous;
mod synchronous;

const BUDGET: u64 = 1_000_000;

fn prices() -> LiftingPrices {
    LiftingPrices {
        string_byte: 7,
        lift_value: 11,
        metadata_byte: 17,
    }
}

fn engine(metered: bool) -> Result<Engine> {
    let mut config = config();
    config
        .consume_fuel(true)
        .consume_lifting_fuel(metered.then(prices));
    Engine::new(&config)
}

fn instantiate(engine: &Engine, wat: &str) -> Result<(Store<()>, Instance)> {
    let component = Component::new(engine, wat)?;
    let mut store = Store::new(engine, ());
    store.set_hostcall_fuel_enabled(false);
    store.set_fuel(BUDGET)?;
    let instance = Linker::new(engine).instantiate(&mut store, &component)?;
    store.set_fuel(BUDGET)?;
    Ok((store, instance))
}

#[test]
fn shared_store_decoding_requires_mutable_access_when_metered() -> Result<()> {
    let engine = engine(true)?;
    let (mut store, instance) = instantiate(
        &engine,
        r#"(component
        (core module $m
            (memory (export "memory") 1)
            (data (i32.const 0) "\10\00\00\00\00\04\00\00")
            (func (export "run") (result i32) (i32.const 0)))
        (core instance $i (instantiate $m))
        (func (export "run") (result string)
            (canon lift (core func $i "run") string-encoding=utf16
                (memory (core memory $i "memory")))))"#,
    )?;
    let run = instance.get_typed_func::<(), (WasmStr,)>(&mut store, "run")?;
    let (value,) = run.call(&mut store, ())?;
    store.set_fuel(0)?;
    let error = value.to_str(&store).unwrap_err();
    assert!(error.to_string().contains("to_str_metered"));
    assert_eq!(store.get_fuel()?, 0);
    Ok(())
}

struct NestedConsumer;

impl StreamConsumer<bool> for NestedConsumer {
    type Item = Vec<String>;

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: StoreContextMut<'_, bool>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        let fuel = store.get_fuel()?;
        store.set_fuel(0)?;
        let mut value = None;
        source.read(&mut store, &mut value)?;
        assert_eq!(value.unwrap(), ["abc", "def"]);
        assert_eq!(store.get_fuel()?, 0);
        assert_eq!(source.remaining(&mut store)?, 0);
        *store.data_mut() = true;
        store.set_fuel(fuel)?;
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

#[tokio::test]
async fn nested_stream_payloads_do_not_activate_shared_lifting_charges() -> Result<()> {
    let mut config = ApiStyle::Async.config();
    config
        .consume_fuel(true)
        .consume_lifting_fuel(Some(prices()));
    let engine = Engine::new(&config)?;
    let component = Component::new(
        &engine,
        r#"(component
        (core module $memory
            (memory (export "memory") 1)
            (data (i32.const 0) "\10\00\00\00\02\00\00\00")
            (data (i32.const 16) "\20\00\00\00\03\00\00\00\23\00\00\00\03\00\00\00")
            (data (i32.const 32) "abcdef"))
        (core instance $memory (instantiate $memory))
        (type $s (stream (list string)))
        (core func $new (canon stream.new $s))
        (core func $write (canon stream.write $s async
            (memory (core memory $memory "memory"))))
        (core module $m
            (import "" "new" (func $new (result i64)))
            (import "" "write" (func $write (param i32 i32 i32) (result i32)))
            (global $writer (mut i32) (i32.const 0))
            (func (export "start") (result i32)
                (local $pair i64)
                (local.set $pair (call $new))
                (global.set $writer (i32.wrap_i64
                    (i64.shr_u (local.get $pair) (i64.const 32))))
                (i32.wrap_i64 (local.get $pair)))
            (func (export "write") (result i32)
                (call $write (global.get $writer) (i32.const 0) (i32.const 1))))
        (core instance $i (instantiate $m (with "" (instance
            (export "new" (func $new)) (export "write" (func $write))))))
        (func (export "start") (result $s) (canon lift (core func $i "start")))
        (func (export "write") (result u32) (canon lift (core func $i "write"))))"#,
    )?;
    let mut store = Store::new(&engine, false);
    store.set_hostcall_fuel_enabled(false);
    store.set_fuel(BUDGET)?;
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    let start = instance.get_typed_func::<(), (StreamReader<Vec<String>>,)>(&mut store, "start")?;
    let write = instance.get_typed_func::<(), (u32,)>(&mut store, "write")?;
    let (reader,) = start.call_async(&mut store, ()).await?;
    reader.pipe(&mut store, NestedConsumer)?;
    assert_eq!(write.call_async(&mut store, ()).await?.0, 1 << 4);
    assert!(*store.data());
    Ok(())
}

#[test]
fn metered_lazy_decode_debits_before_each_conversion() -> Result<()> {
    for metered in [false, true] {
        let engine = engine(metered)?;
        let (mut store, instance) = instantiate(
            &engine,
            r#"(component
            (core module $m
                (memory (export "memory") 1)
                (data (i32.const 0) "\10\00\00\00\08\00\00\00")
                (func (export "run") (result i32) (i32.const 0))
                (func (export "invalidate")
                    (i32.store16 (i32.const 16) (i32.const 55296))))
            (core instance $i (instantiate $m))
            (func (export "run") (result string)
                (canon lift (core func $i "run") string-encoding=utf16
                    (memory (core memory $i "memory"))))
            (func (export "invalidate") (canon lift (core func $i "invalidate"))))"#,
        )?;
        let run = instance.get_typed_func::<(), (WasmStr,)>(&mut store, "run")?;
        let invalidate = instance.get_typed_func::<(), ()>(&mut store, "invalidate")?;
        let (value,) = run.call(&mut store, ())?;
        let price = if metered { 16 * 7 } else { 0 };
        store.set_fuel(2 * price)?;
        for remaining in [price, 0] {
            assert_eq!(value.to_str_metered(&mut store)?.len(), 8);
            assert_eq!(store.get_fuel()?, remaining);
        }
        if metered {
            let error = value.to_str_metered(&mut store).unwrap_err();
            assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
            store.set_fuel(price - 1)?;
            let error = value.to_str_metered(&mut store).unwrap_err();
            assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
            assert_eq!(store.get_fuel()?, 0);
        }
        store.set_fuel(BUDGET)?;
        invalidate.call(&mut store, ())?;
        store.set_fuel(price)?;
        let error = value.to_str_metered(&mut store).unwrap_err();
        assert!(error.downcast_ref::<Trap>().is_none(), "{error:?}");
        assert_eq!(store.get_fuel()?, 0);
    }
    Ok(())
}
