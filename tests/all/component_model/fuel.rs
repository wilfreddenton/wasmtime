use super::{ApiStyle, config};
use crate::async_functions::CountPending;
use std::collections::HashMap;
use std::mem::size_of;
use std::pin::{Pin, pin};
use std::str::Utf8Error;
use std::task::{Context, Poll};
use wasmtime::component::{
    Component, ComponentType, Func, Lift, Linker, Source, StreamConsumer, StreamReader,
    StreamResult, TypedFunc, Val, WasmList,
};
use wasmtime::{Config, Engine, Result, Store, StoreContextMut, Trap};

#[derive(ComponentType, Lift, Clone, Debug, PartialEq)]
#[component(variant)]
enum ZeroSizedVariant {
    #[component(name = "only")]
    Only,
}

fn string_component(engine: &Engine) -> Result<Component> {
    Component::new(
        engine,
        r#"(component
            (core module $m
                (memory (export "memory") 1)
                (data (i32.const 0) "\08\00\00\00")
                (data (i32.const 8) "abcdefghijklmnop\ff")
                ;; Use the same guest instructions for empty, valid, and invalid strings.
                (func (export "run") (param $length i32) (result i32)
                    (i32.store (i32.const 4) (local.get $length))
                    (i32.const 0)))
            (core instance $i (instantiate $m))
            (func (export "run") (param "length" u32) (result string)
                (canon lift (core func $i "run")
                    (memory (core memory $i "memory")))))"#,
    )
}

fn string_fixture(config: &Config) -> Result<(Store<()>, Func, u64)> {
    let engine = Engine::new(config)?;
    let component = string_component(&engine)?;
    let mut store = Store::new(&engine, ());
    store.set_fuel(u64::MAX)?;
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_func(&mut store, "run").unwrap();
    let mut results = [Val::Bool(false)];
    store.set_fuel(10_000)?;
    run.call(&mut store, &[Val::U32(0)], &mut results)?;
    let instructions = 10_000 - store.get_fuel()?;
    store.set_fuel(10_000)?;
    Ok((store, run, instructions))
}

#[test]
fn lifting_fuel_is_disabled_by_default() -> Result<()> {
    let (mut store, run, instructions) = string_fixture(config().consume_fuel(true))?;
    let mut results = [Val::Bool(false)];
    store.set_fuel(instructions)?;

    run.call(&mut store, &[Val::U32(16)], &mut results)?;

    assert!(store.engine().get_consume_fuel());
    assert!(!store.engine().get_consume_lifting_fuel());
    assert!(matches!(&results[0], Val::String(text) if text == "abcdefghijklmnop"));
    assert_eq!(store.get_fuel()?, 0);
    Ok(())
}

#[test]
fn lifting_fuel_charges_when_enabled() -> Result<()> {
    let (mut store, run, instructions) =
        string_fixture(config().consume_fuel(true).consume_lifting_fuel(true))?;
    let mut results = [Val::Bool(false)];

    run.call(&mut store, &[Val::U32(16)], &mut results)?;

    assert!(store.engine().get_consume_lifting_fuel());
    assert!(matches!(&results[0], Val::String(text) if text == "abcdefghijklmnop"));
    assert_eq!(store.get_fuel()?, 10_000 - instructions - 16);

    store.set_fuel(instructions)?;
    let error = run
        .call(&mut store, &[Val::U32(16)], &mut results)
        .unwrap_err();

    assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
    assert_eq!(store.get_fuel()?, 0);
    Ok(())
}

#[test]
fn zero_sized_list_elements_charge_by_count_without_fuel() -> Result<()> {
    zero_sized_typed_list_elements_charge_by_count(&config())
}

#[test]
fn zero_sized_list_elements_charge_by_count_with_lifting_fuel() -> Result<()> {
    zero_sized_typed_list_elements_charge_by_count(
        config().consume_fuel(true).consume_lifting_fuel(true),
    )
}

fn zero_sized_typed_list_elements_charge_by_count(config: &Config) -> Result<()> {
    // A canonical variant tag still needs lifting despite having no native storage.
    assert_eq!(size_of::<ZeroSizedVariant>(), 0);
    assert_eq!(ZeroSizedVariant::SIZE32, 1);
    let engine = Engine::new(config)?;
    let instruction_fuel = engine.get_consume_fuel();
    let lifting_fuel = engine.get_consume_lifting_fuel();
    let component = Component::new(
        &engine,
        r#"(component
            (type $item' (variant (case "only")))
            (export $item "item" (type $item'))
            (core module $m
                (memory (export "memory") 1)
                (data (i32.const 0) "\10\00\00\00")
                (func (export "run") (param $length i32) (result i32)
                    (i32.store (i32.const 4) (local.get $length))
                    (i32.const 0)))
            (core instance $i (instantiate $m))
            (func (export "run") (param "length" u32) (result (list $item))
                (canon lift (core func $i "run")
                    (memory (core memory $i "memory")))))"#,
    )?;
    let mut store = Store::new(&engine, ());
    if instruction_fuel {
        store.set_fuel(u64::MAX)?;
    }
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(u32,), (Vec<ZeroSizedVariant>,)>(&mut store, "run")?;
    if instruction_fuel {
        store.set_fuel(10_000)?;
    }
    store.set_hostcall_fuel(0);
    assert!(run.call(&mut store, (0,))?.0.is_empty());
    let instructions = if instruction_fuel {
        10_000 - store.get_fuel()?
    } else {
        0
    };
    for length in [1, 3] {
        if instruction_fuel {
            store.set_fuel(10_000)?;
        }
        store.set_hostcall_fuel(length as usize);
        let (values,) = run.call(&mut store, (length,))?;
        assert_eq!(values, vec![ZeroSizedVariant::Only; length as usize]);
        if instruction_fuel {
            let lift_cost = if lifting_fuel { u64::from(length) } else { 0 };
            assert_eq!(store.get_fuel()?, 10_000 - instructions - lift_cost);
        }
    }

    if instruction_fuel {
        store.set_fuel(10_000)?;
    }
    store.set_hostcall_fuel(2);
    let error = run.call(&mut store, (3,)).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("fuel allocated for hostcalls has been exhausted"),
        "{error}"
    );
    assert_eq!(store.hostcall_fuel(), 2);
    if instruction_fuel {
        assert_eq!(store.get_fuel()?, 10_000 - instructions);
    }
    Ok(())
}

fn map_fixture(
    config: &Config,
) -> Result<(Store<()>, TypedFunc<(u32,), (HashMap<u32, u32>,)>, u64)> {
    let mut config = config.clone();
    config.wasm_component_model_map(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(
        &engine,
        r#"(component
            (core module $m
                (memory (export "memory") 1)
                (data (i32.const 0) "\08\00\00\00")
                ;; Map entries (1, 11) and (2, 22).
                (data (i32.const 8) "\01\00\00\00\0b\00\00\00")
                (data (i32.const 16) "\02\00\00\00\16\00\00\00")
                (func (export "run") (param $length i32) (result i32)
                    (i32.store (i32.const 4) (local.get $length))
                    (i32.const 0)))
            (core instance $i (instantiate $m))
            (func (export "run") (param "length" u32) (result (map u32 u32))
                (canon lift (core func $i "run")
                    (memory (core memory $i "memory")))))"#,
    )?;
    let mut store = Store::new(&engine, ());
    if engine.get_consume_fuel() {
        store.set_fuel(u64::MAX)?;
    }
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(u32,), (HashMap<u32, u32>,)>(&mut store, "run")?;
    let instructions = if engine.get_consume_fuel() {
        store.set_fuel(10_000)?;
        run.call(&mut store, (0,))?;
        let instructions = 10_000 - store.get_fuel()?;
        store.set_fuel(10_000)?;
        instructions
    } else {
        0
    };
    Ok((store, run, instructions))
}

#[test]
fn map_charges_each_call_without_fuel() -> Result<()> {
    map_charges_each_call(&config())
}

#[test]
fn map_charges_each_call_with_lifting_fuel() -> Result<()> {
    map_charges_each_call(config().consume_fuel(true).consume_lifting_fuel(true))
}

fn map_charges_each_call(config: &Config) -> Result<()> {
    let (mut store, run, instructions) = map_fixture(config)?;
    let instruction_fuel = store.engine().get_consume_fuel();
    let lifting_fuel = store.engine().get_consume_lifting_fuel();
    let cost = 2 * size_of::<(u32, u32)>();

    store.set_hostcall_fuel(0);
    let (empty,) = run.call(&mut store, (0,))?;
    store.set_hostcall_fuel(cost);
    let (first,) = run.call(&mut store, (2,))?;
    let (second,) = run.call(&mut store, (2,))?;

    assert!(empty.is_empty());
    assert_eq!(first, HashMap::from([(1, 11), (2, 22)]));
    assert_eq!(second, first);
    assert_eq!(store.hostcall_fuel(), cost);
    if instruction_fuel {
        let lift_cost = if lifting_fuel { cost as u64 } else { 0 };
        assert_eq!(store.get_fuel()?, 10_000 - 3 * instructions - 2 * lift_cost);
    }
    Ok(())
}

#[test]
fn map_rejects_insufficient_hostcall_allowance_without_fuel() -> Result<()> {
    map_rejects_insufficient_hostcall_allowance(&config())
}

#[test]
fn map_rejects_insufficient_hostcall_allowance_with_lifting_fuel() -> Result<()> {
    map_rejects_insufficient_hostcall_allowance(
        config().consume_fuel(true).consume_lifting_fuel(true),
    )
}

fn map_rejects_insufficient_hostcall_allowance(config: &Config) -> Result<()> {
    let (mut store, run, instructions) = map_fixture(config)?;
    let instruction_fuel = store.engine().get_consume_fuel();
    let allowance = 2 * size_of::<(u32, u32)>() - 1;
    store.set_hostcall_fuel(allowance);

    let error = run.call(&mut store, (2,)).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("fuel allocated for hostcalls has been exhausted"),
        "{error}"
    );
    assert_eq!(store.hostcall_fuel(), allowance);
    if instruction_fuel {
        assert_eq!(store.get_fuel()?, 10_000 - instructions);
    }
    Ok(())
}

#[test]
fn map_rejects_insufficient_store_fuel() -> Result<()> {
    let mut config = config();
    config.consume_fuel(true).consume_lifting_fuel(true);
    let (mut store, run, instructions) = map_fixture(&config)?;
    let cost = 2 * size_of::<(u32, u32)>();
    store.set_hostcall_fuel(cost);
    store.set_fuel(instructions + cost as u64 - 1)?;

    let error = run.call(&mut store, (2,)).unwrap_err();

    assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
    assert_eq!(store.get_fuel()?, 0);
    assert_eq!(store.hostcall_fuel(), cost);
    Ok(())
}

fn lazy_list_fixture() -> Result<(Store<()>, TypedFunc<(u32,), (WasmList<String>,)>)> {
    let engine = Engine::new(config().consume_fuel(true).consume_lifting_fuel(true))?;
    let component = Component::new(
        &engine,
        r#"(component
            (core module $m
                (memory (export "memory") 1)
                (data (i32.const 0) "\08\00\00\00")
                ;; String elements (pointer, length): (24, 3) and (27, 3).
                (data (i32.const 8) "\18\00\00\00\03\00\00\00")
                (data (i32.const 16) "\1b\00\00\00\03\00\00\00")
                (data (i32.const 24) "abcdef")
                (func (export "run") (param $length i32) (result i32)
                    (i32.store (i32.const 4) (local.get $length))
                    (i32.const 0)))
            (core instance $i (instantiate $m))
            (func (export "run") (param "length" u32) (result (list string))
                (canon lift (core func $i "run")
                    (memory (core memory $i "memory")))))"#,
    )?;
    let mut store = Store::new(&engine, ());
    store.set_fuel(u64::MAX)?;
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(u32,), (WasmList<String>,)>(&mut store, "run")?;
    store.set_fuel(10_000)?;
    Ok((store, run))
}

#[test]
fn lazy_list_creation_charges_storage() -> Result<()> {
    let (mut store, run) = lazy_list_fixture()?;
    let (empty,) = run.call(&mut store, (0,))?;
    let instructions = 10_000 - store.get_fuel()?;
    store.set_fuel(10_000)?;

    let (list,) = run.call(&mut store, (2,))?;

    assert_eq!(empty.len(), 0);
    assert_eq!(list.len(), 2);
    assert_eq!(
        store.get_fuel()?,
        10_000 - instructions - 2 * size_of::<String>() as u64
    );
    Ok(())
}

#[test]
fn lazy_list_get_charges_fuel() -> Result<()> {
    let (mut store, run) = lazy_list_fixture()?;
    let (list,) = run.call(&mut store, (2,))?;
    store.set_fuel(10)?;

    let value = list.get(&mut store, 0).unwrap()?;
    let missing = list.get(&mut store, 2);

    assert_eq!(value, "abc");
    assert!(missing.is_none());
    assert_eq!(store.get_fuel()?, 7);
    Ok(())
}

#[test]
fn lazy_list_get_exhausts_fuel() -> Result<()> {
    let (mut store, run) = lazy_list_fixture()?;
    let (list,) = run.call(&mut store, (2,))?;
    store.set_fuel(5)?;

    let first = list.get(&mut store, 0).unwrap()?;
    let error = list.get(&mut store, 1).unwrap().unwrap_err();

    assert_eq!(first, "abc");
    assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
    assert_eq!(store.get_fuel()?, 0);
    Ok(())
}

#[test]
fn lazy_list_iter_charges_fuel() -> Result<()> {
    let (mut store, run) = lazy_list_fixture()?;
    let (list,) = run.call(&mut store, (2,))?;
    store.set_fuel(10)?;

    let values = list.iter(&mut store)?.collect::<Result<Vec<_>>>()?;

    assert_eq!(values, ["abc", "def"]);
    assert_eq!(store.get_fuel()?, 4);
    Ok(())
}

#[test]
fn lazy_list_iter_exhausts_fuel() -> Result<()> {
    let (mut store, run) = lazy_list_fixture()?;
    let (list,) = run.call(&mut store, (2,))?;
    store.set_fuel(5)?;

    let (first, error, finished) = {
        let mut values = list.iter(&mut store)?;
        let first = values.next().unwrap()?;
        let error = values.next().unwrap().unwrap_err();
        let finished = values.next().is_none();
        (first, error, finished)
    };

    assert_eq!(first, "abc");
    assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
    assert!(finished);
    assert_eq!(store.get_fuel()?, 0);
    Ok(())
}

#[test]
fn failed_lifting_retains_fuel_charges() -> Result<()> {
    let engine = Engine::new(config().consume_fuel(true).consume_lifting_fuel(true))?;
    let component = string_component(&engine)?;
    let mut store = Store::new(&engine, ());
    store.set_fuel(u64::MAX)?;
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_func(&mut store, "run").unwrap();
    let mut results = [Val::Bool(false)];
    store.set_fuel(10_000)?;
    run.call(&mut store, &[Val::U32(0)], &mut results)?;
    let instructions = 10_000 - store.get_fuel()?;
    store.set_fuel(instructions + 17)?;
    let error = run
        .call(&mut store, &[Val::U32(17)], &mut results)
        .unwrap_err();
    assert!(error.downcast_ref::<Utf8Error>().is_some());
    assert_eq!(store.get_fuel()?, 0);
    Ok(())
}

#[tokio::test]
async fn typed_results_charge_fuel_sync() -> Result<()> {
    typed_results_charge_fuel(ApiStyle::Sync).await
}

#[tokio::test]
async fn typed_results_charge_fuel_async() -> Result<()> {
    typed_results_charge_fuel(ApiStyle::Async).await
}

#[tokio::test]
async fn typed_results_charge_fuel_async_not_concurrent() -> Result<()> {
    typed_results_charge_fuel(ApiStyle::AsyncNotConcurrent).await
}

#[tokio::test]
async fn typed_results_charge_fuel_concurrent() -> Result<()> {
    typed_results_charge_fuel(ApiStyle::Concurrent).await
}

async fn typed_results_charge_fuel(style: ApiStyle) -> Result<()> {
    let mut config = style.config();
    config.consume_fuel(true).consume_lifting_fuel(true);
    let engine = Engine::new(&config)?;
    let component = string_component(&engine)?;
    let mut store = Store::new(&engine, ());
    store.set_fuel(u64::MAX)?;
    let instance = style
        .instantiate(&mut store, &Linker::new(&engine), &component)
        .await?;
    let run = instance.get_typed_func::<(u32,), (String,)>(&mut store, "run")?;
    store.set_fuel(10_000)?;
    style.call(&mut store, run, (0,)).await?;
    let cost = 10_000 - store.get_fuel()? + 16;
    store.set_fuel(2 * cost)?;
    assert_eq!(
        style.call(&mut store, run, (16,)).await?.0,
        "abcdefghijklmnop"
    );
    assert_eq!(store.get_fuel()?, cost);
    style.call(&mut store, run, (16,)).await?;
    assert_eq!(store.get_fuel()?, 0);
    Ok(())
}

#[tokio::test]
async fn lifting_advances_next_yield_async() -> Result<()> {
    lifting_advances_next_yield(ApiStyle::Async).await
}

#[tokio::test]
async fn lifting_advances_next_yield_async_not_concurrent() -> Result<()> {
    lifting_advances_next_yield(ApiStyle::AsyncNotConcurrent).await
}

async fn lifting_advances_next_yield(style: ApiStyle) -> Result<()> {
    let mut config = style.config();
    config.consume_fuel(true).consume_lifting_fuel(true);
    let engine = Engine::new(&config)?;
    let component = string_component(&engine)?;
    let mut store = Store::new(&engine, ());
    store.set_fuel(u64::MAX)?;
    let instance = style
        .instantiate(&mut store, &Linker::new(&engine), &component)
        .await?;
    let run = instance.get_typed_func::<(u32,), (String,)>(&mut store, "run")?;
    store.set_fuel(10_000)?;
    style.call(&mut store, run, (0,)).await?;
    let instructions = 10_000 - store.get_fuel()?;
    // Leave enough active fuel for another empty call, but not the string lift.
    assert!(instructions < 8);
    store.fuel_async_yield_interval(Some(instructions + 8))?;

    for (length, expected_yields) in [(0, 0), (16, 1)] {
        store.set_fuel(10_000)?;
        let (result, pending) =
            CountPending::new(pin!(style.call(&mut store, run, (length,)))).await;
        assert_eq!(result?.0.len(), length as usize);
        assert_eq!(pending, 0, "lifting should complete without yielding");
        assert_eq!(store.get_fuel()?, 10_000 - instructions - u64::from(length));
        let (result, pending) = CountPending::new(pin!(style.call(&mut store, run, (0,)))).await;
        assert_eq!(result?.0, "");
        assert_eq!(
            pending, expected_yields,
            "only the string lift should advance the next yield"
        );
        assert_eq!(
            store.get_fuel()?,
            10_000 - 2 * instructions - u64::from(length)
        );
    }
    Ok(())
}

#[derive(Default)]
struct StreamFuelState {
    values: Vec<String>,
    balances: Vec<u64>,
}

struct StringConsumer;

impl StreamConsumer<StreamFuelState> for StringConsumer {
    type Item = String;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        mut store: StoreContextMut<'_, StreamFuelState>,
        mut source: Source<'_, String>,
        _finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let before = store.get_fuel()?;
        store.data_mut().balances.push(before);
        while source.remaining(&mut store)? > 0 {
            let mut value = None;
            let result = source.read(&mut store, &mut value);
            let remaining = store.get_fuel()?;
            store.data_mut().balances.push(remaining);
            result?;
            store.data_mut().values.push(value.unwrap());
        }
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

async fn stream_writer(
    style: ApiStyle,
    component: &Component,
) -> Result<(Store<StreamFuelState>, TypedFunc<(), (u32,)>)> {
    let mut store = Store::new(component.engine(), StreamFuelState::default());
    store.set_fuel(u64::MAX)?;
    let instance = style
        .instantiate(&mut store, &Linker::new(component.engine()), component)
        .await?;
    let start = instance.get_typed_func::<(), (StreamReader<String>,)>(&mut store, "start")?;
    let write = instance.get_typed_func::<(), (u32,)>(&mut store, "write")?;
    let (reader,) = style.call(&mut store, start, ()).await?;
    reader.pipe(&mut store, StringConsumer)?;
    Ok((store, write))
}

#[tokio::test]
async fn stream_lifting_exhaustion_async() -> Result<()> {
    stream_lifting_exhaustion(ApiStyle::Async).await
}

async fn stream_lifting_exhaustion(style: ApiStyle) -> Result<()> {
    let mut config = style.config();
    config.consume_fuel(true).consume_lifting_fuel(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(
        &engine,
        r#"(component
            (core module $memory
                (memory (export "memory") 1)
                ;; String elements (pointer, length): (16, 8) and (24, 8).
                (data (i32.const 0) "\10\00\00\00\08\00\00\00")
                (data (i32.const 8) "\18\00\00\00\08\00\00\00")
                (data (i32.const 16) "abcdefghijklmnop"))
            (core instance $memory (instantiate $memory))
            (type $s (stream string))
            (core func $new (canon stream.new $s))
            (core func $write (canon stream.write $s async
                (memory (core memory $memory "memory"))))
            (core module $guest
                (import "" "new" (func $new (result i64)))
                (import "" "write" (func $write (param i32 i32 i32) (result i32)))
                (global $writer (mut i32) (i32.const 0))
                (func (export "start") (result i32)
                    (local $pair i64)
                    (local.set $pair (call $new))
                    (global.set $writer
                        (i32.wrap_i64 (i64.shr_u (local.get $pair) (i64.const 32))))
                    (i32.wrap_i64 (local.get $pair)))
                (func (export "write") (result i32)
                    (call $write (global.get $writer) (i32.const 0) (i32.const 2))))
            (core instance $guest (instantiate $guest
                (with "" (instance
                    (export "new" (func $new))
                    (export "write" (func $write))))))
            (func (export "start") (result $s)
                (canon lift (core func $guest "start")))
            (func (export "write") (result u32)
                (canon lift (core func $guest "write"))))"#,
    )?;
    // Streams charge the host item representation as well as its string bytes.
    let item_cost = size_of::<String>() as u64 + 8;
    let (mut store, write) = stream_writer(style, &component).await?;
    store.set_fuel(10_000)?;
    assert_eq!(style.call(&mut store, write, ()).await?.0, 2 << 4); // COMPLETED(2)
    let state = store.data();
    assert_eq!(state.values, ["abcdefgh", "ijklmnop"]);
    let before = state.balances[0];
    assert_eq!(
        state.balances,
        [before, before - item_cost, before - 2 * item_cost]
    );
    let before_host = 10_000 - before;

    for (available, values, balances) in [
        (item_cost - 1, &[][..], &[item_cost - 1, 0][..]),
        (
            2 * item_cost - 1,
            &["abcdefgh"][..],
            &[2 * item_cost - 1, item_cost - 1, 0][..],
        ),
    ] {
        let (mut store, write) = stream_writer(style, &component).await?;
        store.set_fuel(before_host + available)?;
        let error = style.call(&mut store, write, ()).await.unwrap_err();
        assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
        assert_eq!(store.get_fuel()?, 0);
        assert_eq!(store.data().values, values);
        assert_eq!(store.data().balances, balances);
    }
    Ok(())
}

#[test]
fn imported_arguments_are_charged_before_host_typed() -> Result<()> {
    imported_arguments_are_charged_before_entering_the_host(false)
}

#[test]
fn imported_arguments_are_charged_before_host_dynamic() -> Result<()> {
    imported_arguments_are_charged_before_entering_the_host(true)
}

fn imported_arguments_are_charged_before_entering_the_host(dynamic: bool) -> Result<()> {
    let engine = Engine::new(config().consume_fuel(true).consume_lifting_fuel(true))?;
    let component = Component::new(
        &engine,
        r#"(component
            (import "accept" (func $accept (param "text" string)))
            (core module $memory
                (memory (export "memory") 1)
                (data (i32.const 0) "abcdefghijklmnop"))
            (core instance $memory (instantiate $memory))
            (core func $accept (canon lower (func $accept)
                (memory (core memory $memory "memory"))))
            (core module $guest
                (import "" "accept" (func $accept (param i32 i32)))
                (func (export "run") (param $length i32)
                    (call $accept (i32.const 0) (local.get $length))))
            (core instance $guest (instantiate $guest
                (with "" (instance (export "accept" (func $accept))))))
            (func (export "run") (param "length" u32)
                (canon lift (core func $guest "run"))))"#,
    )?;
    let mut linker = Linker::new(&engine);
    if dynamic {
        linker.root().func_new(
            "accept",
            |mut store: StoreContextMut<'_, Vec<u64>>, _, _, _| {
                let remaining = store.get_fuel()?;
                store.data_mut().push(remaining);
                Ok(())
            },
        )?;
    } else {
        linker.root().func_wrap(
            "accept",
            |mut store: StoreContextMut<'_, Vec<u64>>, (_text,): (String,)| {
                let remaining = store.get_fuel()?;
                store.data_mut().push(remaining);
                Ok(())
            },
        )?;
    }
    let mut store = Store::new(&engine, Vec::new());
    store.set_fuel(u64::MAX)?;
    let instance = linker.instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(u32,), ()>(&mut store, "run")?;
    store.set_fuel(10_000)?;
    run.call(&mut store, (0,))?;
    let before_host = 10_000 - store.data()[0];
    store.set_fuel(10_000)?;
    run.call(&mut store, (16,))?;
    assert_eq!(10_000 - store.data()[1], before_host + 16);
    store.set_fuel(before_host + 15)?;
    let error = run.call(&mut store, (16,)).unwrap_err();
    assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
    assert_eq!(store.data().len(), 2);
    assert_eq!(store.get_fuel()?, 0);
    Ok(())
}
