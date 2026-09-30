use super::ApiStyle;
use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Config, Engine, Result, Store, StoreContextMut, Trap};

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
fn dynamic_results_share_instruction_fuel_and_retain_failed_work() -> Result<()> {
    let engine = Engine::new(Config::new().consume_fuel(true))?;
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
    assert!(error.downcast_ref::<Trap>().is_none());
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
        if !matches!(style, ApiStyle::Sync) {
            store.fuel_async_yield_interval(Some(10))?;
        }
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

#[test]
fn per_lift_allowance_applies_with_or_without_store_fuel() -> Result<()> {
    for enabled in [false, true] {
        let engine = Engine::new(Config::new().consume_fuel(enabled))?;
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
    let engine = Engine::new(Config::new().consume_fuel(true))?;
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
