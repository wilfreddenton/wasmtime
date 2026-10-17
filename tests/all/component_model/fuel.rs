use super::{ApiStyle, config};
use crate::async_functions::CountPending;
use std::collections::HashMap;
use std::mem::size_of;
use std::pin::{Pin, pin};
use std::str::Utf8Error;
use std::task::{Context, Poll};
use wasmtime::component::{
    Component, ComponentType, Lift, Linker, Source, StreamConsumer, StreamReader, StreamResult,
    TypedFunc, Val, WasmList,
};
use wasmtime::{Engine, Result, Store, StoreContextMut, Trap};

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

#[test]
fn lifting_fuel_requires_instruction_fuel() -> Result<()> {
    let mut config = config();
    config.consume_lifting_fuel(true);
    for instruction_fuel in [None, Some(false)] {
        if let Some(enabled) = instruction_fuel {
            config.consume_fuel(enabled);
        }
        let error = Engine::new(&config).err().unwrap();
        assert!(
            error
                .to_string()
                .contains("consume_lifting_fuel requires consume_fuel")
        );
    }
    config.consume_fuel(true);
    assert!(Engine::new(&config)?.get_consume_lifting_fuel());
    config.consume_fuel(false).consume_lifting_fuel(false);
    assert!(!Engine::new(&config)?.get_consume_lifting_fuel());
    Ok(())
}

#[test]
fn lifting_fuel_is_opt_in() -> Result<()> {
    for lifting in [None, Some(false), Some(true)] {
        let mut config = config();
        config.consume_fuel(true);
        if let Some(enabled) = lifting {
            config.consume_lifting_fuel(enabled);
        }
        let engine = Engine::new(&config)?;
        assert!(engine.get_consume_fuel());
        assert_eq!(engine.get_consume_lifting_fuel(), lifting.unwrap_or(false));
        let component = string_component(&engine)?;
        let mut store = Store::new(&engine, ());
        let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
        let run = instance.get_func(&mut store, "run").unwrap();
        let mut results = [Val::Bool(false)];
        store.set_fuel(10_000)?;
        run.call(&mut store, &[Val::U32(0)], &mut results)?;
        let instructions = 10_000 - store.get_fuel()?;
        store.set_fuel(10_000)?;
        run.call(&mut store, &[Val::U32(16)], &mut results)?;
        assert!(matches!(&results[0], Val::String(text) if text == "abcdefghijklmnop"));
        let lift_cost = if lifting == Some(true) { 16 } else { 0 };
        assert_eq!(store.get_fuel()?, 10_000 - instructions - lift_cost);

        store.set_fuel(instructions)?;
        let result = run.call(&mut store, &[Val::U32(16)], &mut results);
        if lifting == Some(true) {
            assert_eq!(
                result.unwrap_err().downcast_ref::<Trap>(),
                Some(&Trap::OutOfFuel)
            );
        } else {
            result?;
            assert!(matches!(&results[0], Val::String(text) if text == "abcdefghijklmnop"));
        }
        assert_eq!(store.get_fuel()?, 0);
    }
    Ok(())
}

#[test]
fn zero_sized_typed_list_elements_charge_by_count() -> Result<()> {
    // A canonical variant tag still needs lifting despite having no native storage.
    assert_eq!(size_of::<ZeroSizedVariant>(), 0);
    assert_eq!(ZeroSizedVariant::SIZE32, 1);
    for (enabled, lifting) in [(false, false), (true, false), (true, true)] {
        let engine = Engine::new(config().consume_fuel(enabled).consume_lifting_fuel(lifting))?;
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
        let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
        let run = instance.get_typed_func::<(u32,), (Vec<ZeroSizedVariant>,)>(&mut store, "run")?;
        if enabled {
            store.set_fuel(10_000)?;
        }
        store.set_hostcall_fuel(0);
        assert!(run.call(&mut store, (0,))?.0.is_empty());
        let instructions = if enabled {
            10_000 - store.get_fuel()?
        } else {
            0
        };
        for length in [1, 3] {
            if enabled {
                store.set_fuel(10_000)?;
            }
            store.set_hostcall_fuel(length as usize);
            let (values,) = run.call(&mut store, (length,))?;
            assert_eq!(values, vec![ZeroSizedVariant::Only; length as usize]);
            if enabled {
                let lift_cost = if lifting { u64::from(length) } else { 0 };
                assert_eq!(store.get_fuel()?, 10_000 - instructions - lift_cost);
            }
        }

        if enabled {
            store.set_fuel(10_000)?;
        }
        store.set_hostcall_fuel(2);
        let error = run.call(&mut store, (3,)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("fuel allocated for hostcalls has been exhausted")
        );
        assert_eq!(store.hostcall_fuel(), 2);
        if enabled {
            assert_eq!(store.get_fuel()?, 10_000 - instructions);
        }
    }
    Ok(())
}

#[test]
fn typed_map_results_check_both_fuel_budgets() -> Result<()> {
    for (enabled, lifting) in [(false, false), (true, false), (true, true)] {
        let engine = Engine::new(
            config()
                .wasm_component_model_map(true)
                .consume_fuel(enabled)
                .consume_lifting_fuel(lifting),
        )?;
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
        let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
        let run = instance.get_typed_func::<(u32,), (HashMap<u32, u32>,)>(&mut store, "run")?;
        if enabled {
            store.set_fuel(10_000)?;
        }
        store.set_hostcall_fuel(0);
        assert!(run.call(&mut store, (0,))?.0.is_empty());
        let instructions = if enabled {
            10_000 - store.get_fuel()?
        } else {
            0
        };
        let cost = 2 * size_of::<(u32, u32)>();
        store.set_hostcall_fuel(cost);
        if enabled {
            store.set_fuel(10_000)?;
        }
        assert_eq!(
            run.call(&mut store, (2,))?.0,
            HashMap::from([(1, 11), (2, 22)])
        );
        // The hostcall allowance resets for each call, while Store fuel accumulates.
        assert_eq!(
            run.call(&mut store, (2,))?.0,
            HashMap::from([(1, 11), (2, 22)])
        );
        assert_eq!(store.hostcall_fuel(), cost);
        if enabled {
            let lift_cost = if lifting { cost as u64 } else { 0 };
            assert_eq!(store.get_fuel()?, 10_000 - 2 * (instructions + lift_cost));
            store.set_fuel(10_000)?;
        }

        store.set_hostcall_fuel(cost - 1);
        let error = run.call(&mut store, (2,)).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("fuel allocated for hostcalls has been exhausted")
        );
        assert_eq!(store.hostcall_fuel(), cost - 1);
        if enabled {
            assert_eq!(store.get_fuel()?, 10_000 - instructions);
        }

        if lifting {
            let mut store = Store::new(&engine, ());
            let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
            let run = instance.get_typed_func::<(u32,), (HashMap<u32, u32>,)>(&mut store, "run")?;
            store.set_hostcall_fuel(cost);
            store.set_fuel(instructions + cost as u64 - 1)?;
            let error = run.call(&mut store, (2,)).unwrap_err();
            assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
            assert_eq!(store.get_fuel()?, 0);
            assert_eq!(store.hostcall_fuel(), cost);
        }
    }
    Ok(())
}

#[test]
fn lazy_list_elements_charge_fuel_when_accessed() -> Result<()> {
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
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(u32,), (WasmList<String>,)>(&mut store, "run")?;
    store.set_fuel(10_000)?;
    assert_eq!(run.call(&mut store, (0,))?.0.len(), 0);
    let instructions = 10_000 - store.get_fuel()?;
    store.set_fuel(10_000)?;
    let (list,) = run.call(&mut store, (2,))?;
    let remaining = 10_000 - instructions - 2 * size_of::<String>() as u64;
    assert_eq!(store.get_fuel()?, remaining);
    assert_eq!(list.get(&mut store, 0).unwrap()?, "abc");
    assert!(list.get(&mut store, 2).is_none());
    assert_eq!(store.get_fuel()?, remaining - 3);
    assert_eq!(
        list.iter(&mut store)?.collect::<Result<Vec<_>>>()?,
        ["abc", "def"]
    );
    assert_eq!(store.get_fuel()?, remaining - 9);

    store.set_fuel(5)?;
    assert_eq!(list.get(&mut store, 0).unwrap()?, "abc");
    let error = list.get(&mut store, 1).unwrap().unwrap_err();
    assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
    assert_eq!(store.get_fuel()?, 0);

    store.set_fuel(5)?;
    let error = {
        let mut values = list.iter(&mut store)?;
        assert_eq!(values.next().unwrap()?, "abc");
        let error = values.next().unwrap().unwrap_err();
        assert!(values.next().is_none());
        error
    };
    assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
    assert_eq!(store.get_fuel()?, 0);
    Ok(())
}

#[test]
fn failed_lifting_retains_fuel_charges() -> Result<()> {
    let engine = Engine::new(config().consume_fuel(true).consume_lifting_fuel(true))?;
    let component = string_component(&engine)?;
    let mut store = Store::new(&engine, ());
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
async fn typed_results_charge_in_all_call_styles() -> Result<()> {
    for style in [
        ApiStyle::Sync,
        ApiStyle::Async,
        ApiStyle::AsyncNotConcurrent,
        ApiStyle::Concurrent,
    ] {
        let mut config = style.config();
        config.consume_fuel(true).consume_lifting_fuel(true);
        let engine = Engine::new(&config)?;
        let component = string_component(&engine)?;
        let mut store = Store::new(&engine, ());
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
    }
    Ok(())
}

#[tokio::test]
async fn lifting_exhausts_the_active_batch_before_the_next_guest_call() -> Result<()> {
    for style in [
        ApiStyle::Async,
        ApiStyle::AsyncNotConcurrent,
        ApiStyle::Concurrent,
    ] {
        let mut config = style.config();
        config.consume_fuel(true).consume_lifting_fuel(true);
        let engine = Engine::new(&config)?;
        let component = string_component(&engine)?;
        let mut store = Store::new(&engine, ());
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
            let (result, pending) =
                CountPending::new(pin!(style.call(&mut store, run, (0,)))).await;
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
    store.set_fuel(10_000)?;
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
async fn stream_lifting_exhaustion_retains_accepted_items_and_charges() -> Result<()> {
    for style in [ApiStyle::Async, ApiStyle::Concurrent] {
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
        assert_eq!(style.call(&mut store, write, ()).await?.0, 2 << 4);
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
    }
    Ok(())
}

#[test]
fn imported_arguments_are_charged_before_entering_the_host() -> Result<()> {
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
    for dynamic in [false, true] {
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
    }
    Ok(())
}
