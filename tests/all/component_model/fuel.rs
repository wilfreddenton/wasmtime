use super::{ApiStyle, config};
use crate::async_functions::CountPending;
use std::mem::size_of;
use std::pin::{Pin, pin};
use std::str::Utf8Error;
use std::task::{Context, Poll};
use wasmtime::component::{
    Component, ComponentType, Lift, Linker, Source, StreamConsumer, StreamReader, StreamResult, Val,
};
use wasmtime::{Engine, Result, Store, StoreContextMut, Trap};

#[derive(ComponentType, Lift, Clone, Debug, PartialEq)]
#[component(variant)]
enum ZeroSizedVariant {
    #[component(name = "only")]
    Only,
}

fn component(engine: &Engine) -> Result<Component> {
    Component::new(
        engine,
        r#"(component
            (core module $m
                (memory (export "memory") 1)
                (data (i32.const 0) "\20\00\00\00\10\00\00\00")
                (data (i32.const 16) "\30\00\00\00\10\00\00\00")
                (data (i32.const 32) "abcdefghijklmnop")
                (data (i32.const 63) "\ff")
                (func (export "valid") (result i32) i32.const 0)
                (func (export "empty") (result i32) i32.const 8)
                (func (export "invalid") (result i32) i32.const 16))
            (core instance $i (instantiate $m))
            (func (export "valid") (result string)
                (canon lift (core func $i "valid") (memory (core memory $i "memory"))))
            (func (export "empty") (result string)
                (canon lift (core func $i "empty") (memory (core memory $i "memory"))))
            (func (export "invalid") (result string)
                (canon lift (core func $i "invalid") (memory (core memory $i "memory")))))"#,
    )
}

#[test]
fn zero_sized_typed_list_elements_charge_by_count() -> Result<()> {
    // A canonical variant tag still needs lifting despite having no native storage.
    assert_eq!(size_of::<ZeroSizedVariant>(), 0);
    assert_eq!(ZeroSizedVariant::SIZE32, 1);
    for enabled in [false, true] {
        let engine = Engine::new(config().consume_fuel(enabled))?;
        let component = Component::new(
            &engine,
            r#"(component
                (type $item' (variant (case "only")))
                (export $item "item" (type $item'))
                (core module $m
                    (memory (export "memory") 1)
                    (data (i32.const 0) "\10\00\00\00")
                    (func (export "run") (param i32) (result i32)
                        (i32.store (i32.const 4) (local.get 0))
                        (i32.const 0)))
                (core instance $i (instantiate $m))
                (func (export "run") (param "length" u32) (result (list $item))
                    (canon lift (core func $i "run")
                        (memory (core memory $i "memory")))))"#,
        )?;
        let mut store = Store::new(&engine, ());
        let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
        let run = instance.get_typed_func::<(u32,), (Vec<ZeroSizedVariant>,)>(&mut store, "run")?;
        let mut instructions = 0;
        for length in [0, 1, 3] {
            if enabled {
                store.set_fuel(10_000)?;
            }
            store.set_hostcall_fuel(length as usize);
            let (values,) = run.call(&mut store, (length,))?;
            assert_eq!(values, vec![ZeroSizedVariant::Only; length as usize]);
            if enabled {
                let consumed = 10_000 - store.get_fuel()?;
                if length == 0 {
                    instructions = consumed;
                }
                assert_eq!(consumed, instructions + u64::from(length));
            }
        }
    }
    Ok(())
}

#[test]
fn dynamic_results_share_instruction_fuel_and_retain_failed_work() -> Result<()> {
    let engine = Engine::new(config().consume_fuel(true))?;
    let component = component(&engine)?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let empty = instance.get_func(&mut store, "empty").unwrap();
    let valid = instance.get_func(&mut store, "valid").unwrap();
    let invalid = instance.get_func(&mut store, "invalid").unwrap();
    let mut results = [Val::Bool(false)];
    store.set_fuel(10_000)?;
    empty.call(&mut store, &[], &mut results)?;
    let instructions = 10_000 - store.get_fuel()?;
    let cost = instructions + 16;
    store.set_fuel(2 * cost)?;
    valid.call(&mut store, &[], &mut results)?;
    assert!(matches!(&results[0], Val::String(text) if text == "abcdefghijklmnop"));
    assert_eq!(store.get_fuel()?, cost);
    valid.call(&mut store, &[], &mut results)?;
    assert_eq!(store.get_fuel()?, 0);

    store.set_fuel(cost)?;
    let error = invalid.call(&mut store, &[], &mut results).unwrap_err();
    assert!(error.downcast_ref::<Utf8Error>().is_some());
    assert_eq!(store.get_fuel()?, 0);

    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let valid = instance.get_func(&mut store, "valid").unwrap();
    store.set_fuel(cost - 1)?;
    let error = valid.call(&mut store, &[], &mut results).unwrap_err();
    assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
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
        config.consume_fuel(true);
        let engine = Engine::new(&config)?;
        let component = component(&engine)?;
        let mut store = Store::new(&engine, ());
        let instance = style
            .instantiate(&mut store, &Linker::new(&engine), &component)
            .await?;
        let empty = instance.get_typed_func::<(), (String,)>(&mut store, "empty")?;
        let valid = instance.get_typed_func::<(), (String,)>(&mut store, "valid")?;
        store.set_fuel(10_000)?;
        style.call(&mut store, empty, ()).await?;
        let cost = 10_000 - store.get_fuel()? + 16;
        store.set_fuel(2 * cost)?;
        assert_eq!(
            style.call(&mut store, valid, ()).await?.0,
            "abcdefghijklmnop"
        );
        assert_eq!(store.get_fuel()?, cost);
        style.call(&mut store, valid, ()).await?;
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
        config.consume_fuel(true);
        let engine = Engine::new(&config)?;
        let component = component(&engine)?;
        let mut store = Store::new(&engine, ());
        let instance = style
            .instantiate(&mut store, &Linker::new(&engine), &component)
            .await?;
        let empty = instance.get_typed_func::<(), (String,)>(&mut store, "empty")?;
        let valid = instance.get_typed_func::<(), (String,)>(&mut store, "valid")?;
        store.set_fuel(10_000)?;
        style.call(&mut store, empty, ()).await?;
        let instructions = 10_000 - store.get_fuel()?;
        // Leave enough active fuel for another empty call, but not the string lift.
        assert!(instructions < 8);
        store.fuel_async_yield_interval(Some(instructions + 8))?;

        for (first, bytes) in [(empty, 0), (valid, 16)] {
            store.set_fuel(10_000)?;
            let (result, pending) =
                CountPending::new(pin!(style.call(&mut store, first, ()))).await;
            assert_eq!(result?.0.len(), bytes);
            assert_eq!(pending, 0, "lifting should complete without yielding");
            assert_eq!(store.get_fuel()?, 10_000 - instructions - bytes as u64);
            let (result, pending) =
                CountPending::new(pin!(style.call(&mut store, empty, ()))).await;
            assert_eq!(result?.0, "");
            assert_eq!(
                pending,
                usize::from(bytes != 0),
                "only the string lift should advance the next yield"
            );
            assert_eq!(store.get_fuel()?, 10_000 - 2 * instructions - bytes as u64);
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
        while source.remaining(&mut store) > 0 {
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

#[tokio::test]
async fn stream_lifting_exhaustion_retains_accepted_items_and_charges() -> Result<()> {
    for style in [ApiStyle::Async, ApiStyle::Concurrent] {
        let mut config = style.config();
        config.consume_fuel(true);
        let engine = Engine::new(&config)?;
        let component = Component::new(
            &engine,
            r#"(component
                (core module $memory
                    (memory (export "memory") 1)
                    (data (i32.const 0) "\20\00\00\00\08\00\00\00\28\00\00\00\08\00\00\00")
                    (data (i32.const 32) "abcdefghijklmnop"))
                (core instance $memory (instantiate $memory))
                (type $s (stream string))
                (core func $new (canon stream.new $s))
                (core func $write (canon stream.write $s async (memory (core memory $memory "memory"))))
                (core module $guest
                    (import "" "new" (func $new (result i64)))
                    (import "" "write" (func $write (param i32 i32 i32) (result i32)))
                    (global $writer (mut i32) (i32.const 0))
                    (func (export "start") (result i32)
                        (local $pair i64)
                        (local.set $pair (call $new))
                        (global.set $writer (i32.wrap_i64 (i64.shr_u (local.get $pair) (i64.const 32))))
                        (i32.wrap_i64 (local.get $pair)))
                    (func (export "write") (result i32)
                        (call $write (global.get $writer) (i32.const 0) (i32.const 2))))
                (core instance $guest (instantiate $guest
                    (with "" (instance
                        (export "new" (func $new))
                        (export "write" (func $write))))))
                (func (export "start") (result $s) (canon lift (core func $guest "start")))
                (func (export "write") (result u32) (canon lift (core func $guest "write"))))"#,
        )?;
        // Streams charge the host item representation as well as its string bytes.
        let item_cost = size_of::<String>() as u64 + 8;
        let mut before_host = 0;
        for available in [10_000, item_cost - 1, 2 * item_cost - 1] {
            let mut store = Store::new(&engine, StreamFuelState::default());
            store.set_fuel(10_000)?;
            let instance = style
                .instantiate(&mut store, &Linker::new(&engine), &component)
                .await?;
            let start =
                instance.get_typed_func::<(), (StreamReader<String>,)>(&mut store, "start")?;
            let write = instance.get_typed_func::<(), (u32,)>(&mut store, "write")?;
            let (reader,) = style.call(&mut store, start, ()).await?;
            reader.pipe(&mut store, StringConsumer)?;
            store.set_fuel(before_host + available)?;
            let result = style.call(&mut store, write, ()).await;
            let state = store.data();
            if available == 10_000 {
                assert_eq!(result?.0, 2 << 4);
                assert_eq!(state.values, ["abcdefgh", "ijklmnop"]);
                let before = state.balances[0];
                assert_eq!(
                    state.balances,
                    [before, before - item_cost, before - 2 * item_cost]
                );
                before_host = available - before;
            } else {
                assert_eq!(
                    result.unwrap_err().downcast_ref::<Trap>(),
                    Some(&Trap::OutOfFuel)
                );
                assert_eq!(store.get_fuel()?, 0);
                if available < item_cost {
                    assert!(state.values.is_empty());
                    assert_eq!(state.balances, [available, 0]);
                } else {
                    assert_eq!(state.values, ["abcdefgh"]);
                    assert_eq!(state.balances, [available, item_cost - 1, 0]);
                }
            }
        }
    }
    Ok(())
}

#[test]
fn per_lift_allowance_applies_with_or_without_store_fuel() -> Result<()> {
    for enabled in [false, true] {
        let engine = Engine::new(config().consume_fuel(enabled))?;
        let component = component(&engine)?;
        for allowance in [15, 16] {
            let mut store = Store::new(&engine, ());
            if enabled {
                store.set_fuel(10_000)?;
            }
            let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
            let empty = instance.get_func(&mut store, "empty").unwrap();
            let valid = instance.get_func(&mut store, "valid").unwrap();
            let mut results = [Val::Bool(false)];
            empty.call(&mut store, &[], &mut results)?;
            let instructions = if enabled {
                10_000 - store.get_fuel()?
            } else {
                0
            };
            if enabled {
                store.set_fuel(10_000)?;
            }
            store.set_hostcall_fuel(allowance);
            let result = valid.call(&mut store, &[], &mut results);
            if allowance == 15 {
                let error = result.unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("fuel allocated for hostcalls has been exhausted")
                );
                if enabled {
                    assert_eq!(store.get_fuel()?, 10_000 - instructions);
                }
            } else {
                result?;
                valid.call(&mut store, &[], &mut results)?;
                if enabled {
                    assert_eq!(store.get_fuel()?, 10_000 - 2 * (instructions + 16));
                }
            }
            assert_eq!(store.hostcall_fuel(), allowance);
        }
    }
    Ok(())
}

#[test]
fn imported_arguments_are_charged_before_entering_the_host() -> Result<()> {
    let engine = Engine::new(config().consume_fuel(true))?;
    let component = Component::new(
        &engine,
        r#"(component
        (import "accept" (func $accept (param "text" string)))
        (core module $memory
            (memory (export "memory") 1)
            (data (i32.const 0) "abcdefghijklmnop"))
        (core instance $memory (instantiate $memory))
        (core func $accept (canon lower (func $accept) (memory (core memory $memory "memory"))))
        (core module $guest
            (import "" "accept" (func $accept (param i32 i32)))
            (func (export "empty") i32.const 0 i32.const 0 call $accept)
            (func (export "valid") i32.const 0 i32.const 16 call $accept))
        (core instance $guest (instantiate $guest
            (with "" (instance (export "accept" (func $accept))))))
        (func (export "empty") (canon lift (core func $guest "empty")))
        (func (export "valid") (canon lift (core func $guest "valid"))))"#,
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
        let empty = instance.get_typed_func::<(), ()>(&mut store, "empty")?;
        let valid = instance.get_typed_func::<(), ()>(&mut store, "valid")?;
        store.set_fuel(10_000)?;
        empty.call(&mut store, ())?;
        let before_host = 10_000 - store.data()[0];
        store.set_fuel(10_000)?;
        valid.call(&mut store, ())?;
        assert_eq!(10_000 - store.data()[1], before_host + 16);
        store.set_fuel(before_host + 15)?;
        let error = valid.call(&mut store, ()).unwrap_err();
        assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
        assert_eq!(store.data().len(), 2);
        assert_eq!(store.get_fuel()?, 0);
    }
    Ok(())
}
