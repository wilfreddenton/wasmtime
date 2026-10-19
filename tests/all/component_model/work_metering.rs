//! Function lifting fuel, deferred access, and excluded transfer paths.

use super::{ApiStyle, config};
use std::io::Read;
use std::pin::Pin;
use std::task::{Context, Poll};
use wasmtime::component::{
    Component, Instance, Lift, Linker, Source, StreamConsumer, StreamReader, StreamResult, WasmStr,
};
use wasmtime::{AsContextMut, Engine, LiftingPrices, Result, Store, StoreContextMut, Trap};

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
    for _ in 0..4 {
        let error = value.to_str(&store).unwrap_err();
        assert!(error.to_string().contains("to_str_metered"));
        assert_eq!(store.get_fuel()?, 0);
    }
    Ok(())
}

#[derive(Default)]
struct StreamState {
    balances: Vec<(u64, u64)>,
    count: usize,
}

struct ByteConsumer {
    direct: bool,
}

impl StreamConsumer<StreamState> for ByteConsumer {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<'_, StreamState>,
        mut source: Source<'_, u8>,
        _finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let count = source.remaining(&mut store)?;
        let before = store.get_fuel()?;
        let values = if self.direct {
            let mut values = vec![0xff; count];
            let copied = source.as_direct(store.as_context_mut()).read(&mut values)?;
            assert_eq!(copied, count);
            values
        } else {
            let mut values = Vec::with_capacity(count);
            source.read(&mut store, &mut values)?;
            values
        };
        assert_eq!(values, vec![0; count]);
        let after = store.get_fuel()?;
        store.data_mut().balances.push((before, after));
        store.data_mut().count += count;
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

struct UnitConsumer;

impl StreamConsumer<StreamState> for UnitConsumer {
    type Item = ();

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<'_, StreamState>,
        mut source: Source<'_, ()>,
        _finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let count = source.remaining(&mut store)?;
        let before = store.get_fuel()?;
        let mut values = Vec::with_capacity(count);
        source.read(&mut store, &mut values)?;
        assert_eq!(values.len(), count);
        let after = store.get_fuel()?;
        store.data_mut().balances.push((before, after));
        store.data_mut().count += count;
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

struct NestedConsumer;

impl StreamConsumer<StreamState> for NestedConsumer {
    type Item = Vec<String>;

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: StoreContextMut<'_, StreamState>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        store.set_fuel(0)?;
        let mut value = None;
        source.read(&mut store, &mut value)?;
        assert_eq!(value.unwrap(), ["abc", "def"]);
        assert_eq!(store.get_fuel()?, 0);
        assert_eq!(source.remaining(&mut store)?, 0);
        store.data_mut().balances.push((0, 0));
        store.data_mut().count += 1;
        store.set_fuel(BUDGET)?;
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

#[tokio::test]
async fn nested_stream_payloads_do_not_activate_shared_lifting_charges() -> Result<()> {
    assert_eq!(
        stream_probe_data(
            "(list string)",
            NestedConsumer,
            1,
            r#"(data (i32.const 0) "\10\00\00\00\02\00\00\00")
            (data (i32.const 16) "\20\00\00\00\03\00\00\00\23\00\00\00\03\00\00\00")
            (data (i32.const 32) "abcdef")"#,
        )
        .await?,
        0
    );
    Ok(())
}

async fn stream_probe<C: StreamConsumer<StreamState>>(
    payload: &str,
    consumer: C,
    count: u32,
) -> Result<u64>
where
    C::Item: Lift + Send + Sync + 'static,
{
    stream_probe_data(payload, consumer, count, "").await
}

async fn stream_probe_data<C: StreamConsumer<StreamState>>(
    payload: &str,
    consumer: C,
    count: u32,
    data: &str,
) -> Result<u64>
where
    C::Item: Lift + Send + Sync + 'static,
{
    let mut config = ApiStyle::Async.config();
    config
        .consume_fuel(true)
        .consume_lifting_fuel(Some(prices()));
    let engine = Engine::new(&config)?;
    let component = Component::new(
        &engine,
        format!(
            r#"(component
        (core module $memory (memory (export "memory") 1) {data})
        (core instance $memory (instantiate $memory))
        (type $s (stream {payload}))
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
            (func (export "write") (param $n i32) (result i32)
                (call $write (global.get $writer) (i32.const 0) (local.get $n))))
        (core instance $i (instantiate $m (with "" (instance
            (export "new" (func $new)) (export "write" (func $write))))))
        (func (export "start") (result $s) (canon lift (core func $i "start")))
        (func (export "write") (param "n" u32) (result u32)
            (canon lift (core func $i "write"))))"#
        ),
    )?;
    let mut store = Store::new(&engine, StreamState::default());
    store.set_hostcall_fuel_enabled(false);
    store.set_fuel(BUDGET)?;
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    let start = instance.get_typed_func::<(), (StreamReader<C::Item>,)>(&mut store, "start")?;
    let write = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "write")?;
    let (reader,) = start.call_async(&mut store, ()).await?;
    reader.pipe(&mut store, consumer)?;
    assert_eq!(write.call_async(&mut store, (count,)).await?.0, count << 4);
    assert_eq!(store.data().count, count as usize);
    assert!(!store.data().balances.is_empty());
    Ok(store.data().balances.iter().map(|(a, b)| a - b).sum())
}

#[tokio::test]
async fn direct_and_typed_byte_stream_payloads_are_unmetered() -> Result<()> {
    for count in [8, 1024] {
        assert_eq!(
            stream_probe("u8", ByteConsumer { direct: true }, count).await?,
            0
        );
        assert_eq!(
            stream_probe("u8", ByteConsumer { direct: false }, count).await?,
            0
        );
    }
    Ok(())
}

#[tokio::test]
async fn unit_stream_payloads_are_unmetered() -> Result<()> {
    for count in [8, 1024] {
        assert_eq!(stream_probe("", UnitConsumer, count).await?, 0);
    }
    Ok(())
}

async fn flat_stream_cost(count: u32, metered: bool) -> Result<u64> {
    let mut config = ApiStyle::Async.config();
    config
        .consume_fuel(true)
        .consume_lifting_fuel(metered.then(prices));
    let engine = Engine::new(&config)?;
    let source = "A".repeat(1024);
    let destination = "B".repeat(1025);
    let component = Component::new(
        &engine,
        format!(
            r#"(component
        (core module $memory
            (memory (export "memory") 1)
            (data (i32.const 16) "{source}")
            (data (i32.const 8192) "{destination}"))
        (core instance $memory (instantiate $memory))
        (type $s (stream u8))
        (core func $new (canon stream.new $s))
        (core func $read (canon stream.read $s async
            (memory (core memory $memory "memory"))))
        (core func $write (canon stream.write $s async
            (memory (core memory $memory "memory"))))
        (core module $m
            (import "" "memory" (memory 1))
            (import "" "new" (func $new (result i64)))
            (import "" "read" (func $read (param i32 i32 i32) (result i32)))
            (import "" "write" (func $write (param i32 i32 i32) (result i32)))
            (func (export "run") (param $n i32) (result i32)
                (local $pair i64) (local $result i32)
                (local.set $pair (call $new))
                (if (i32.ne
                    (call $read (i32.wrap_i64 (local.get $pair))
                        (i32.const 8192) (local.get $n))
                    (i32.const -1)) (then unreachable))
                (local.set $result (call $write
                    (i32.wrap_i64 (i64.shr_u (local.get $pair) (i64.const 32)))
                    (i32.const 16) (local.get $n)))
                (if (i32.ne (i32.load8_u (i32.const 8192)) (i32.const 65))
                    (then unreachable))
                (if (i32.ne
                    (i32.load8_u (i32.add (i32.const 8191) (local.get $n)))
                    (i32.const 65)) (then unreachable))
                (if (i32.ne
                    (i32.load8_u (i32.add (i32.const 8192) (local.get $n)))
                    (i32.const 66)) (then unreachable))
                (local.get $result)))
        (core instance $i (instantiate $m (with "" (instance
            (export "memory" (memory $memory "memory"))
            (export "new" (func $new))
            (export "read" (func $read))
            (export "write" (func $write))))))
        (func (export "run") (param "n" u32) (result u32)
            (canon lift (core func $i "run"))))"#
        ),
    )?;
    let mut store = Store::new(&engine, ());
    store.set_fuel(BUDGET)?;
    store.set_hostcall_fuel_enabled(false);
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    let run = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "run")?;
    store.set_fuel(BUDGET)?;
    assert_eq!(run.call_async(&mut store, (count,)).await?.0, count << 4);
    Ok(BUDGET - store.get_fuel()?)
}

#[tokio::test]
async fn guest_stream_copies_are_outside_function_lifting() -> Result<()> {
    let small = flat_stream_cost(8, true).await?;
    let large = flat_stream_cost(1024, true).await?;
    assert_eq!(small - flat_stream_cost(8, false).await?, 2 * 11);
    assert_eq!(large - flat_stream_cost(1024, false).await?, 2 * 11);
    assert_eq!(small, large);
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
