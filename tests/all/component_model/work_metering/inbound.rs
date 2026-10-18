use super::{BUDGET, StreamState, prices, stream_probe, stream_probe_data};
use crate::component_model::ApiStyle;
use futures::channel::oneshot;
use std::fmt::Debug;
use std::io::Read;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use wasmtime::component::{
    Component, FutureConsumer, FutureReader, Lift, Linker, Source, StreamConsumer, StreamReader,
    StreamResult, Val,
};
use wasmtime::{AsContextMut, Engine, Result, Store, StoreContextMut, Trap};

struct PartialBytes {
    exhaust: bool,
}

impl StreamConsumer<StreamState> for PartialBytes {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: StoreContextMut<'_, StreamState>,
        mut source: Source<'_, u8>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        let count = source.remaining(&mut store)?;
        let mut copied = 0;
        while copied < count {
            let n = 3.min(count - copied);
            let mut bytes = vec![0xff; n];
            if self.exhaust && copied > 0 {
                store.set_fuel(n as u64 * 11 - 1)?;
                let error = source
                    .reborrow()
                    .as_direct(store.as_context_mut())
                    .read(&mut bytes)
                    .unwrap_err();
                assert!(error.to_string().contains("fuel"), "{error:?}");
                assert_eq!(store.get_fuel()?, 0);
                assert_eq!(bytes, vec![0xff; n]);
                assert_eq!(source.remaining(&mut store)?, count - copied);
                return Poll::Ready(Err(error.into()));
            }
            store.set_fuel(n as u64 * 11)?;
            assert_eq!(
                source
                    .reborrow()
                    .as_direct(store.as_context_mut())
                    .read(&mut bytes)?,
                n
            );
            assert_eq!(bytes, vec![0; n]);
            assert_eq!(store.get_fuel()?, 0);
            copied += n;
            assert_eq!(source.remaining(&mut store)?, count - copied);
        }
        let mut empty = [];
        assert_eq!(
            source.as_direct(store.as_context_mut()).read(&mut empty)?,
            0
        );
        store.data_mut().count += count;
        store.data_mut().balances.push((count as u64 * 11, 0));
        store.set_fuel(BUDGET)?;
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

#[tokio::test]
async fn direct_reads_admit_only_the_requested_chunk_and_preserve_unpaid_progress() -> Result<()> {
    assert_eq!(
        stream_probe("u8", PartialBytes { exhaust: false }, 8).await?,
        8 * 11
    );
    let error = stream_probe("u8", PartialBytes { exhaust: true }, 8)
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("fuel"), "{error:?}");
    Ok(())
}

struct FutureCheck<T> {
    expected: T,
    cost: u64,
    exhaust: bool,
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
        if self.exhaust {
            store.set_fuel(self.cost - 1)?;
            let error = source.read(&mut store, &mut value).unwrap_err();
            assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
            assert!(value.is_none());
            assert_eq!(source.remaining(&mut store)?, 1);
            assert_eq!(store.get_fuel()?, 0);
            return Poll::Ready(Err(error));
        }
        store.set_fuel(self.cost)?;
        source.read(&mut store, &mut value)?;
        assert_eq!(value.as_ref(), Some(&self.expected));
        assert_eq!(source.remaining(&mut store)?, 0);
        assert_eq!(store.get_fuel()?, 0);
        store.set_fuel(BUDGET)?;
        Poll::Ready(Ok(()))
    }
}

async fn future<T: Lift + Clone + Debug + PartialEq + Send + Sync + Unpin + 'static>(
    payload: &str,
    expected: T,
    cost: u64,
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
    for exhaust in [false, true] {
        let mut store = Store::new(&engine, ());
        store.set_hostcall_fuel_enabled(false);
        store.set_fuel(BUDGET)?;
        let instance = Linker::new(&engine)
            .instantiate_async(&mut store, &component)
            .await?;
        let (reader,) = instance
            .get_typed_func::<(), (FutureReader<T>,)>(&mut store, "new")?
            .call_async(&mut store, ())
            .await?;
        reader.pipe(
            &mut store,
            FutureCheck {
                expected: expected.clone(),
                cost,
                exhaust,
            },
        )?;
        let ptr = if payload == "string" { 32 } else { 0 };
        let write = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "write")?;
        let result = write.call_async(&mut store, (ptr,)).await;
        if exhaust {
            assert_eq!(
                result.unwrap_err().downcast_ref::<Trap>(),
                Some(&Trap::OutOfFuel)
            );
            store.set_fuel(BUDGET)?;
            assert_eq!(
                write
                    .call_async(&mut store, (ptr,))
                    .await
                    .unwrap_err()
                    .downcast_ref::<Trap>(),
                Some(&Trap::CannotEnterComponent)
            );
        } else {
            assert_eq!(result?.0, 0);
        }
    }
    Ok(())
}

#[tokio::test]
async fn future_payloads_admit_structure_and_bytes_before_delivery() -> Result<()> {
    future("", (), 11).await?;
    future("u32", 0u32, 11).await?;
    future("(tuple u32 u32)", (0u32, 0u32), 3 * 11).await?;
    future("string", String::from("abc"), 11 + 3 * 7).await?;
    Ok(())
}

struct PartialStrings;

impl StreamConsumer<StreamState> for PartialStrings {
    type Item = String;

    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        mut store: StoreContextMut<'_, StreamState>,
        mut source: Source<'_, String>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        let mut values = Vec::with_capacity(2);
        // Admit both string nodes and only the first string's bytes.
        store.set_fuel(2 * 11 + 3 * 7)?;
        let error = source.read(&mut store, &mut values).unwrap_err();
        assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
        assert_eq!(values, ["abc"]);
        assert_eq!(store.get_fuel()?, 0);
        Poll::Ready(Err(error))
    }
}

#[tokio::test]
async fn failed_stream_batch_retains_completed_values_and_traps() -> Result<()> {
    let error = stream_probe_data(
        "string",
        PartialStrings,
        2,
        r#"(data (i32.const 0) "\20\00\00\00\03\00\00\00\23\00\00\00\03\00\00\00")
        (data (i32.const 32) "abcdef")"#,
    )
    .await
    .unwrap_err();
    assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
    Ok(())
}

struct NestedHandleConsumer {
    defer: bool,
    budget: u64,
    dropped: Arc<AtomicBool>,
    delivered: Option<oneshot::Sender<()>>,
}

impl Drop for NestedHandleConsumer {
    fn drop(&mut self) {
        assert!(!self.dropped.swap(true, Ordering::SeqCst));
    }
}

impl StreamConsumer<usize> for NestedHandleConsumer {
    type Item = (FutureReader<()>, String);

    fn poll_consume(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut store: StoreContextMut<'_, usize>,
        mut source: Source<'_, Self::Item>,
        _: bool,
    ) -> Poll<Result<StreamResult>> {
        *store.data_mut() += 1;
        if self.defer {
            self.defer = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        store.set_fuel(self.budget)?;
        let mut values = Vec::with_capacity(1);
        let result = source.read(&mut store, &mut values);
        if self.budget == 54 {
            result?;
            let (mut reader, string) = values.pop().unwrap();
            assert_eq!(string, "abc");
            reader.close(&mut store)?;
            assert_eq!(source.remaining(&mut store)?, 0);
            store.set_fuel(BUDGET)?;
            self.delivered.take().unwrap().send(()).unwrap();
            return Poll::Ready(Ok(StreamResult::Completed));
        }
        let error = result.unwrap_err();
        assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
        assert!(values.is_empty());
        assert_eq!(source.remaining(&mut store)?, 1);
        assert_eq!(store.get_fuel()?, 0);
        Poll::Ready(Err(error))
    }
}

#[tokio::test]
async fn nested_handle_exhaustion_propagates_through_existing_trap_handling() -> Result<()> {
    let engine = Engine::new(
        ApiStyle::Async
            .config()
            .consume_fuel(true)
            .consume_lifting_fuel(Some(prices())),
    )?;
    let component = Component::new(
        &engine,
        r#"(component
        (core module $memory (memory (export "memory") 1)
            (data (i32.const 4) "\20\00\00\00\03\00\00\00")
            (data (i32.const 32) "abc"))
        (core instance $memory (instantiate $memory))
        (type $f (future))
        (type $item (tuple $f string))
        (type $s (stream $item))
        (core func $newf (canon future.new $f))
        (core func $new (canon stream.new $s))
        (core func $write (canon stream.write $s async (memory (core memory $memory "memory"))))
        (core module $m
            (import "" "memory" (memory 1))
            (import "" "newf" (func $newf (result i64)))
            (import "" "new" (func $new (result i64)))
            (import "" "write" (func $write (param i32 i32 i32) (result i32)))
            (global $writer (mut i32) (i32.const 0))
            (func (export "start") (result i32)
                (local $pair i64)
                (i32.store (i32.const 0) (i32.wrap_i64 (call $newf)))
                (local.set $pair (call $new))
                (global.set $writer (i32.wrap_i64 (i64.shr_u (local.get $pair) (i64.const 32))))
                (i32.wrap_i64 (local.get $pair)))
            (func (export "write") (result i32)
                (call $write (global.get $writer) (i32.const 0) (i32.const 1))))
        (core instance $i (instantiate $m (with "" (instance
            (export "memory" (memory $memory "memory"))
            (export "newf" (func $newf)) (export "new" (func $new))
            (export "write" (func $write))))))
        (func (export "start") (result $s) (canon lift (core func $i "start")))
        (func (export "write") (result u32) (canon lift (core func $i "write"))))"#,
    )?;
    for defer in [false, true] {
        for budget in [33, 54] {
            let mut store = Store::new(&engine, 0usize);
            store.set_hostcall_fuel_enabled(false);
            store.set_fuel(BUDGET)?;
            let instance = Linker::new(&engine)
                .instantiate_async(&mut store, &component)
                .await?;
            let start = instance
                .get_typed_func::<(), (StreamReader<(FutureReader<()>, String)>,)>(
                    &mut store, "start",
                )?;
            let write = instance.get_typed_func::<(), (u32,)>(&mut store, "write")?;
            let (reader,) = start.call_async(&mut store, ()).await?;
            let dropped = Arc::new(AtomicBool::new(false));
            let (delivered, received) = oneshot::channel();
            reader.pipe(
                &mut store,
                NestedHandleConsumer {
                    defer,
                    budget,
                    dropped: dropped.clone(),
                    delivered: Some(delivered),
                },
            )?;
            let result = store
                .run_concurrent(async |accessor| {
                    write.call_concurrent(accessor, ()).await?;
                    received.await?;
                    Result::<()>::Ok(())
                })
                .await
                .and_then(|result| result);
            if budget == 33 {
                assert_eq!(
                    result.unwrap_err().downcast_ref::<Trap>(),
                    Some(&Trap::OutOfFuel)
                );
                store.set_fuel(BUDGET)?;
                assert_eq!(
                    write
                        .call_async(&mut store, ())
                        .await
                        .unwrap_err()
                        .downcast_ref::<Trap>(),
                    Some(&Trap::CannotEnterComponent)
                );
            } else {
                result?;
            }
            assert_eq!(*store.data(), if defer { 2 } else { 1 });
            drop(store);
            assert!(dropped.load(Ordering::SeqCst));
        }
    }
    Ok(())
}

#[tokio::test]
async fn async_string_results_cannot_amplify_a_small_instruction_budget() -> Result<()> {
    let wat = r#"(component
        (core module $memory (memory (export "memory") 257))
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
            for len in [0u32, 1024, 16_777_216] {
                store.set_fuel(200_000_000)?;
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
                bills.push(200_000_000 - store.get_fuel()?);
            }
            if metered {
                store.set_fuel(1000)?;
                let error = if dynamic {
                    run.func()
                        .call_async(&mut store, &[Val::U32(16_777_216)], &mut [Val::Bool(false)])
                        .await
                        .unwrap_err()
                } else {
                    run.call_async(&mut store, (16_777_216,)).await.unwrap_err()
                };
                assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
                assert_eq!(store.get_fuel()?, 0);
            }
            costs.push(bills);
        }
        assert!(costs[0].windows(2).all(|p| p[0] == p[1]));
        for (i, len) in [0, 1024, 16_777_216].into_iter().enumerate() {
            assert_eq!(costs[1][i] - costs[0][i], 2 * 11 + len * 7);
        }
    }
    Ok(())
}
