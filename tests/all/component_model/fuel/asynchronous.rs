use super::{BUDGET, prices};
use crate::component_model::ApiStyle;
use std::fmt::Debug;
use std::pin::Pin;
use std::task::{Context, Poll};
use wasmtime::component::{Component, FutureConsumer, FutureReader, Lift, Linker, Source, Val};
use wasmtime::{AsContextMut, Engine, Result, Store, StoreContextMut, Trap};

struct FutureCheck<T> {
    expected: T,
}

impl<T: Lift + Debug + PartialEq + Send + Sync + Unpin + 'static> FutureConsumer<()>
    for FutureCheck<T>
{
    type Item = T;

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: StoreContextMut<'_, ()>,
        mut source: Source<'_, T>,
        _: bool,
    ) -> Poll<Result<()>> {
        let mut value = None;
        store.set_fuel(0)?;
        source.read(&mut store, &mut value)?;
        assert_eq!(value.as_ref(), Some(&self.expected));
        assert_eq!(source.remaining(&mut store)?, 0);
        assert_eq!(store.get_fuel()?, 0);
        store.set_fuel(BUDGET)?;
        Poll::Ready(Ok(()))
    }
}

async fn future<T: Lift + Debug + PartialEq + Send + Sync + Unpin + 'static>(
    payload: &str,
    expected: T,
    native_allowance: bool,
) -> Result<()> {
    let engine = Engine::new(
        ApiStyle::Async
            .config()
            .consume_fuel(true)
            .consume_lifting_fuel(Some(prices())),
    )?;
    let component = Component::new(
        &engine,
        format!(
            r#"(component
        (core module $memory
            (memory (export "memory") 1)
            (data (i32.const 32) "\40\00\00\00\03\00\00\00")
            (data (i32.const 64) "abc"))
        (core instance $memory (instantiate $memory))
        (type $f (future {payload}))
        (core func $new (canon future.new $f))
        (core func $write (canon future.write $f async (memory (core memory $memory "memory"))))
        (core module $m
            (import "" "new" (func $new (result i64)))
            (import "" "write" (func $write (param i32 i32) (result i32)))
            (global $w (mut i32) (i32.const 0))
            (func (export "new") (result i32)
                (local $pair i64)
                (local.set $pair (call $new))
                (global.set $w (i32.wrap_i64 (i64.shr_u (local.get $pair) (i64.const 32))))
                (i32.wrap_i64 (local.get $pair)))
            (func (export "write") (param $ptr i32) (result i32)
                (call $write (global.get $w) (local.get $ptr))))
        (core instance $i (instantiate $m (with "" (instance
            (export "new" (func $new)) (export "write" (func $write))))))
        (func (export "new") (result $f) (canon lift (core func $i "new")))
        (func (export "write") (param "ptr" u32) (result u32) (canon lift (core func $i "write"))))"#
        ),
    )?;
    let mut store = Store::new(&engine, ());
    store.set_hostcall_fuel_enabled(native_allowance);
    store.set_hostcall_fuel(0);
    store.set_fuel(BUDGET)?;
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    let (reader,) = instance
        .get_typed_func::<(), (FutureReader<T>,)>(&mut store, "new")?
        .call_async(&mut store, ())
        .await?;
    reader.pipe(&mut store, FutureCheck { expected })?;
    let ptr = if payload == "string" { 32 } else { 0 };
    let write = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "write")?;
    let result = write.call_async(&mut store, (ptr,)).await;
    if native_allowance {
        let error = result.unwrap_err();
        assert!(
            format!("{error:#}").contains("fuel allocated for hostcalls"),
            "{error:#}"
        );
    } else {
        assert_eq!(result?.0, 0);
    }
    Ok(())
}

#[tokio::test]
async fn future_payloads_are_outside_function_lifting() -> Result<()> {
    future("", (), false).await?;
    future("u32", 0u32, false).await?;
    future("(tuple u32 u32)", (0u32, 0u32), false).await?;
    future("string", String::from("abc"), false).await?;
    Ok(())
}

#[tokio::test]
async fn excluded_payloads_preserve_the_native_allowance() -> Result<()> {
    future("string", String::from("abc"), true).await
}

#[tokio::test]
async fn guest_to_guest_string_payloads_are_unmetered() -> Result<()> {
    for kind in ["stream", "future"] {
        let count_param = if kind == "stream" { "i32" } else { "" };
        let count_arg = if kind == "stream" {
            "(i32.const 1)"
        } else {
            ""
        };
        let wat = format!(
            r#"(component
            (component $receiver
                (type $s ({kind} string))
                (core module $memory
                    (memory (export "memory") 1)
                    (func (export "realloc") (param i32 i32 i32 i32) (result i32) (i32.const 1024)))
                (core instance $memory (instantiate $memory))
                (core func $read (canon {kind}.read $s async
                    (memory (core memory $memory "memory")) (realloc (core func $memory "realloc"))))
                (core module $m
                    (import "" "memory" (memory 1))
                    (import "" "read" (func $read (param i32 i32 {count_param}) (result i32)))
                    (func (export "read") (param $reader i32) (result i32)
                        (drop (call $read (local.get $reader) (i32.const 512) {count_arg}))
                        (if (i32.ne (i32.load8_u (i32.load (i32.const 512))) (i32.const 97))
                            (then unreachable))
                        (i32.load (i32.const 516))))
                (core instance $i (instantiate $m (with "" (instance
                    (export "memory" (memory $memory "memory")) (export "read" (func $read))))))
                (func (export "read") (param "s" $s) (result u32) (canon lift (core func $i "read"))))
            (instance $receiver (instantiate $receiver))
            (core func $read (canon lower (func $receiver "read")))
            (core module $memory
                (memory (export "memory") 1)
                (data (i32.const 0) "\10\00\00\00\03\00\00\00")
                (data (i32.const 16) "abc"))
            (core instance $memory (instantiate $memory))
            (type $s ({kind} string))
            (core func $new (canon {kind}.new $s))
            (core func $write (canon {kind}.write $s async
                (memory (core memory $memory "memory"))))
            (core module $m
                (import "" "new" (func $new (result i64)))
                (import "" "read" (func $read (param i32) (result i32)))
                (import "" "write" (func $write (param i32 i32 {count_param}) (result i32)))
                (func (export "run") (result i32)
                    (local $pair i64)
                    (local.set $pair (call $new))
                    (if (i32.ne (call $write
                        (i32.wrap_i64 (i64.shr_u (local.get $pair) (i64.const 32)))
                        (i32.const 0) {count_arg}) (i32.const -1)) (then unreachable))
                    (call $read (i32.wrap_i64 (local.get $pair)))))
            (core instance $i (instantiate $m (with "" (instance
                (export "new" (func $new)) (export "read" (func $read)) (export "write" (func $write))))))
            (func (export "run") (result u32) (canon lift (core func $i "run"))))"#
        );
        let mut costs = Vec::new();
        for metered in [false, true] {
            let engine = Engine::new(
                ApiStyle::Async
                    .config()
                    .consume_fuel(true)
                    .consume_lifting_fuel(metered.then(prices)),
            )?;
            let component = Component::new(&engine, &wat)?;
            let mut store = Store::new(&engine, ());
            store.set_hostcall_fuel_enabled(false);
            store.set_fuel(BUDGET)?;
            let instance = Linker::new(&engine)
                .instantiate_async(&mut store, &component)
                .await?;
            let run = instance.get_typed_func::<(), (u32,)>(&mut store, "run")?;
            store.set_fuel(BUDGET)?;
            assert_eq!(run.call_async(&mut store, ()).await?.0, 3);
            costs.push(BUDGET - store.get_fuel()?);
        }
        assert_eq!(costs[1] - costs[0], 2 * 11, "{kind}");
    }
    Ok(())
}

#[tokio::test]
async fn error_context_messages_remain_metered() -> Result<()> {
    let wat = r#"(component
        (core module $memory (memory (export "memory") 1))
        (core instance $memory (instantiate $memory))
        (core func $new (canon error-context.new (memory (core memory $memory "memory"))))
        (core func $drop (canon error-context.drop))
        (core module $m
            (import "" "new" (func $new (param i32 i32) (result i32)))
            (import "" "drop" (func $drop (param i32)))
            (func (export "run") (param $len i32)
                (call $drop (call $new (i32.const 0) (local.get $len)))))
        (core instance $i (instantiate $m (with "" (instance
            (export "new" (func $new)) (export "drop" (func $drop))))))
        (func (export "run") (param "len" u32) (canon lift (core func $i "run"))))"#;
    let mut costs = Vec::new();
    for metered in [false, true] {
        let engine = Engine::new(
            ApiStyle::Async
                .config()
                .wasm_component_model_error_context(true)
                .consume_fuel(true)
                .consume_lifting_fuel(metered.then(prices)),
        )?;
        let component = Component::new(&engine, wat)?;
        let mut store = Store::new(&engine, ());
        store.set_hostcall_fuel_enabled(false);
        store.set_fuel(BUDGET)?;
        let instance = Linker::new(&engine)
            .instantiate_async(&mut store, &component)
            .await?;
        let run = instance.get_typed_func::<(u32,), ()>(&mut store, "run")?;
        let mut bills = Vec::new();
        for len in [0, 1024] {
            store.set_fuel(BUDGET)?;
            run.call_async(&mut store, (len,)).await?;
            bills.push(BUDGET - store.get_fuel()?);
        }
        costs.push(bills);
        if metered {
            store.set_fuel(100)?;
            let error = run.call_async(&mut store, (1024,)).await.unwrap_err();
            assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
            assert_eq!(store.get_fuel()?, 0);
        }
    }
    for (i, len) in [0, 1024].into_iter().enumerate() {
        assert_eq!(costs[1][i] - costs[0][i], 2 * 11 + len * 7);
    }
    Ok(())
}

#[tokio::test]
async fn async_import_arguments_are_metered() -> Result<()> {
    let wat = r#"(component
        (import "accept" (func $accept async (param "s" string)))
        (core module $memory
            (memory (export "memory") 1)
            (data (i32.const 0) "abc"))
        (core instance $memory (instantiate $memory))
        (core func $accept (canon lower (func $accept) async
            (memory (core memory $memory "memory"))))
        (core module $m
            (import "" "accept" (func $accept (param i32 i32) (result i32)))
            (func (export "run")
                (drop (call $accept (i32.const 0) (i32.const 3)))))
        (core instance $i (instantiate $m (with "" (instance (export "accept" (func $accept))))))
        (func (export "run") (canon lift (core func $i "run"))))"#;
    for dynamic in [false, true] {
        let mut costs = Vec::new();
        let mut balances = Vec::new();
        for metered in [false, true] {
            let engine = Engine::new(
                ApiStyle::Async
                    .config()
                    .consume_fuel(true)
                    .consume_lifting_fuel(metered.then(prices)),
            )?;
            let component = Component::new(&engine, wat)?;
            let mut linker = Linker::<Option<u64>>::new(&engine);
            if dynamic {
                linker
                    .root()
                    .func_new_concurrent("accept", |accessor, _, params, _| {
                        Box::pin(async move {
                            assert!(matches!(&params[0], Val::String(s) if s == "abc"));
                            accessor.with(|mut access| {
                                let fuel = access.as_context_mut().get_fuel()?;
                                *access.as_context_mut().data_mut() = Some(fuel);
                                Result::<()>::Ok(())
                            })
                        })
                    })?;
            } else {
                linker
                    .root()
                    .func_wrap_concurrent("accept", |accessor, (s,): (String,)| {
                        Box::pin(async move {
                            assert_eq!(s, "abc");
                            accessor.with(|mut access| {
                                let fuel = access.as_context_mut().get_fuel()?;
                                *access.as_context_mut().data_mut() = Some(fuel);
                                Result::<()>::Ok(())
                            })
                        })
                    })?;
            }
            let mut store = Store::new(&engine, None);
            store.set_hostcall_fuel_enabled(false);
            store.set_fuel(BUDGET)?;
            let instance = linker.instantiate_async(&mut store, &component).await?;
            let run = instance.get_typed_func::<(), ()>(&mut store, "run")?;
            store.set_fuel(BUDGET)?;
            run.call_async(&mut store, ()).await?;
            costs.push(BUDGET - store.get_fuel()?);
            balances.push(store.data().unwrap());
            if metered {
                *store.data_mut() = None;
                store.set_fuel(BUDGET - balances[0] + 2 * 11 + 3 * 7 - 1)?;
                let error = run.call_async(&mut store, ()).await.unwrap_err();
                assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
                assert!(store.data().is_none());
            }
        }
        assert_eq!(balances[0] - balances[1], 2 * 11 + 3 * 7);
        assert_eq!(costs[1] - costs[0], 3 * 11 + 3 * 7);
    }
    Ok(())
}

#[tokio::test]
async fn async_string_results_cannot_amplify_a_small_instruction_budget() -> Result<()> {
    let wat = r#"(component
        (core module $memory (memory (export "memory") 1))
        (core instance $memory (instantiate $memory))
        (core func $return (canon task.return (result string) (memory (core memory $memory "memory"))))
        (core module $m
            (import "" "return" (func $return (param i32 i32)))
            (func (export "run") (param $len i32) (result i32)
                (call $return (i32.const 0) (local.get $len)) (i32.const 0))
            (func (export "callback") (param i32 i32 i32) (result i32) (i32.const 0)))
        (core instance $i (instantiate $m (with "" (instance (export "return" (func $return))))))
        (func (export "run") async (param "len" u32) (result string)
            (canon lift (core func $i "run") async (memory (core memory $memory "memory"))
                (callback (core func $i "callback")))))"#;
    for dynamic in [false, true] {
        let mut costs = Vec::new();
        for metered in [false, true] {
            let engine = Engine::new(
                ApiStyle::Async
                    .config()
                    .consume_fuel(true)
                    .consume_lifting_fuel(metered.then(prices)),
            )?;
            let component = Component::new(&engine, wat)?;
            let mut store = Store::new(&engine, ());
            store.set_hostcall_fuel_enabled(false);
            store.set_fuel(BUDGET)?;
            let instance = Linker::new(&engine)
                .instantiate_async(&mut store, &component)
                .await?;
            let run = instance.get_typed_func::<(u32,), (String,)>(&mut store, "run")?;
            let mut bills = Vec::new();
            for len in [0u32, 1024, 16_384] {
                store.set_fuel(BUDGET)?;
                let value = if dynamic {
                    let mut results = [Val::Bool(false)];
                    run.func()
                        .call_async(&mut store, &[Val::U32(len)], &mut results)
                        .await?;
                    let [Val::String(value)] = results else {
                        unreachable!()
                    };
                    value
                } else {
                    run.call_async(&mut store, (len,)).await?.0
                };
                assert_eq!(value.len(), len as usize);
                bills.push(BUDGET - store.get_fuel()?);
            }
            if metered {
                store.set_fuel(1000)?;
                let error = if dynamic {
                    run.func()
                        .call_async(&mut store, &[Val::U32(16_384)], &mut [Val::Bool(false)])
                        .await
                        .unwrap_err()
                } else {
                    run.call_async(&mut store, (16_384,)).await.unwrap_err()
                };
                assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
                assert_eq!(store.get_fuel()?, 0);
            }
            costs.push(bills);
        }
        assert!(costs[0].windows(2).all(|p| p[0] == p[1]));
        for (i, len) in [0, 1024, 16_384].into_iter().enumerate() {
            assert_eq!(costs[1][i] - costs[0][i], 2 * 11 + len * 7);
        }
    }
    Ok(())
}
