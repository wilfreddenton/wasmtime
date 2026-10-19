use super::{BUDGET, instantiate};
use crate::component_model::config;
use std::collections::HashMap;
use wasmtime::component::{
    Component, ComponentType, Lift, Linker, Lower, ResourceAny, Val, WasmList,
};
use wasmtime::{Engine, LiftingPrices, Result, Store, Trap};

const PRICES: LiftingPrices = LiftingPrices {
    lift_value: 3,
    string_byte: 7,
    metadata_byte: 11,
};

fn engine(prices: Option<LiftingPrices>) -> Result<Engine> {
    Engine::new(
        config()
            .consume_fuel(true)
            .wasm_component_model_fixed_length_lists(true)
            .wasm_component_model_map(true)
            .consume_lifting_fuel(prices),
    )
}

fn result_component(ty: &str, data: &str) -> String {
    format!(
        r#"(component
        (core module $m
            (memory (export "memory") 1)
            (data (i32.const 0) "{data}")
            (func (export "run") (result i32) (i32.const 0)))
        (core instance $i (instantiate $m))
        (func (export "run") (result {ty})
            (canon lift (core func $i "run") (memory (core memory $i "memory")))))"#
    )
}

#[test]
fn malformed_results_charge_structure_before_pointer_validation() -> Result<()> {
    // Result tuple + payload tuple + two scalars.
    let structure_cost = 4 * PRICES.lift_value;
    let instruction_cost = 2;
    for (ptr, message) in [(1, "aligned"), (65532, "out of bounds")] {
        let wat = format!(
            r#"(component
                (core module $m
                    (memory (export "memory") 1)
                    (func (export "run") (result i32) (i32.const {ptr})))
                (core instance $i (instantiate $m))
                (func (export "run") (result (tuple u32 u32))
                    (canon lift (core func $i "run") (memory (core memory $i "memory")))))"#
        );
        for metered in [false, true] {
            let engine = engine(metered.then_some(PRICES))?;
            for dynamic in [false, true] {
                for budget in [
                    BUDGET,
                    instruction_cost + structure_cost,
                    instruction_cost + structure_cost - 1,
                ] {
                    let (mut store, instance) = instantiate(&engine, &wat)?;
                    store.set_fuel(budget)?;
                    let error = if dynamic {
                        instance
                            .get_func(&mut store, "run")
                            .unwrap()
                            .call(&mut store, &[], &mut [Val::Bool(false)])
                            .unwrap_err()
                    } else {
                        instance
                            .get_typed_func::<(), ((u32, u32),)>(&mut store, "run")?
                            .call(&mut store, ())
                            .unwrap_err()
                    };
                    if metered && budget < instruction_cost + structure_cost {
                        assert_eq!(error.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
                        assert_eq!(store.get_fuel()?, 0);
                    } else {
                        assert!(format!("{error:#}").contains(message), "{error:#}");
                        assert_eq!(
                            store.get_fuel()?,
                            budget - instruction_cost - if metered { structure_cost } else { 0 }
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

#[test]
fn malformed_indirect_arguments_still_fail_before_structure_charges() -> Result<()> {
    for (ptr, message) in [(1, "aligned"), (65532, "out of bounds")] {
        let wat = format!(
            r#"(component
                (import "accept" (func $accept (param "v" (list u32 17))))
                (core module $memory (memory (export "memory") 1))
                (core instance $memory (instantiate $memory))
                (core func $accept (canon lower (func $accept)
                    (memory (core memory $memory "memory"))))
                (core module $m
                    (import "" "accept" (func $accept (param i32)))
                    (func (export "run") (call $accept (i32.const {ptr}))))
                (core instance $i (instantiate $m (with "" (instance
                    (export "accept" (func $accept))))))
                (func (export "run") (canon lift (core func $i "run"))))"#
        );
        for dynamic in [false, true] {
            let mut costs = Vec::new();
            for metered in [false, true] {
                let engine = engine(metered.then_some(PRICES))?;
                let component = Component::new(&engine, &wat)?;
                let mut linker = Linker::<()>::new(&engine);
                if dynamic {
                    linker.root().func_new("accept", |_, _, _, _| {
                        panic!("malformed arguments reached the host")
                    })?;
                } else {
                    linker
                        .root()
                        .func_wrap::<_, ([u32; 17],), ()>("accept", |_, _| {
                            panic!("malformed arguments reached the host")
                        })?;
                }
                let mut store = Store::new(&engine, ());
                store.set_hostcall_fuel_enabled(false);
                store.set_fuel(BUDGET)?;
                let instance = linker.instantiate(&mut store, &component)?;
                store.set_fuel(BUDGET)?;
                let error = instance
                    .get_typed_func::<(), ()>(&mut store, "run")?
                    .call(&mut store, ())
                    .unwrap_err();
                assert!(format!("{error:#}").contains(message), "{error:#}");
                costs.push(BUDGET - store.get_fuel()?);
            }
            assert_eq!(costs[0], costs[1]);
        }
    }
    Ok(())
}

#[test]
fn fixed_arrays_have_equal_typed_and_dynamic_admission() -> Result<()> {
    for dynamic in [false, true] {
        for n in [4, 1024] {
            let expected = 2 + (n + 2) * 3;
            for budget in [expected, expected - 1] {
                let engine = engine(Some(PRICES))?;
                let (mut store, instance) =
                    instantiate(&engine, &result_component(&format!("(list u32 {n})"), ""))?;
                store.set_fuel(budget)?;
                let result = if dynamic {
                    let mut result = [Val::Bool(false)];
                    instance
                        .get_func(&mut store, "run")
                        .unwrap()
                        .call(&mut store, &[], &mut result)
                        .map(|()| {
                            assert_eq!(
                                result,
                                [Val::FixedLengthList(vec![Val::U32(0); n as usize])]
                            )
                        })
                } else if n == 4 {
                    instance
                        .get_typed_func::<(), ([u32; 4],)>(&mut store, "run")?
                        .call(&mut store, ())
                        .map(|(values,)| assert_eq!(values, [0; 4]))
                } else {
                    instance
                        .get_typed_func::<(), ([u32; 1024],)>(&mut store, "run")?
                        .call(&mut store, ())
                        .map(|(values,)| assert_eq!(values, [0; 1024]))
                };
                if budget == expected {
                    result?;
                } else {
                    assert_eq!(
                        result.unwrap_err().downcast_ref::<Trap>(),
                        Some(&Trap::OutOfFuel)
                    );
                }
                assert_eq!(store.get_fuel()?, 0);
            }
        }
    }
    Ok(())
}

#[test]
fn nested_list_admission_is_not_duplicated() -> Result<()> {
    let wat = result_component(
        "(list (tuple u32 (list u32 4)))",
        r"\10\00\00\00\02\00\00\00",
    );
    for dynamic in [false, true] {
        let engine = engine(Some(PRICES))?;
        let (mut store, instance) = instantiate(&engine, &wat)?;
        if dynamic {
            instance.get_func(&mut store, "run").unwrap().call(
                &mut store,
                &[],
                &mut [Val::Bool(false)],
            )?;
        } else {
            let result = instance
                .get_typed_func::<(), (Vec<(u32, [u32; 4])>,)>(&mut store, "run")?
                .call(&mut store, ())?
                .0;
            assert_eq!(result, vec![(0, [0; 4]); 2]);
        }
        // Result tuple + list + two (tuple + scalar + array + 4 scalars).
        assert_eq!(BUDGET - store.get_fuel()?, 2 + (2 + 2 * 7) * 3);
    }
    Ok(())
}

#[test]
fn option_reserves_largest_static_payload() -> Result<()> {
    let wat = result_component("(option (list u32 4))", "");
    for dynamic in [false, true] {
        let engine = engine(Some(PRICES))?;
        let (mut store, instance) = instantiate(&engine, &wat)?;
        if dynamic {
            let mut result = [Val::Bool(false)];
            instance
                .get_func(&mut store, "run")
                .unwrap()
                .call(&mut store, &[], &mut result)?;
            assert!(matches!(result[0], Val::Option(None)));
        } else {
            assert_eq!(
                instance
                    .get_typed_func::<(), (Option<[u32; 4]>,)>(&mut store, "run")?
                    .call(&mut store, ())?
                    .0,
                None
            );
        }
        assert_eq!(BUDGET - store.get_fuel()?, 2 + 7 * 3);
    }
    Ok(())
}

#[derive(ComponentType, Lift, Lower, Debug, PartialEq)]
#[component(record)]
struct Record {
    x: u32,
    ys: [u32; 4],
}

#[test]
fn derived_record_reserves_structure_and_metadata() -> Result<()> {
    let wat = r#"(component
        (type $r (record (field "x" u32) (field "ys" (list u32 4))))
        (export $exported-r "r" (type $r))
        (core module $m
            (memory (export "memory") 1)
            (func (export "run") (result i32) (i32.const 0)))
        (core instance $i (instantiate $m))
        (func (export "run") (result $exported-r)
            (canon lift (core func $i "run") (memory (core memory $i "memory")))))"#;
    for dynamic in [false, true] {
        let engine = engine(Some(PRICES))?;
        let (mut store, instance) = instantiate(&engine, wat)?;
        if dynamic {
            instance.get_func(&mut store, "run").unwrap().call(
                &mut store,
                &[],
                &mut [Val::Bool(false)],
            )?;
        } else {
            assert_eq!(
                instance
                    .get_typed_func::<(), (Record,)>(&mut store, "run")?
                    .call(&mut store, ())?
                    .0,
                Record { x: 0, ys: [0; 4] }
            );
        }
        assert_eq!(BUDGET - store.get_fuel()?, 2 + 8 * 3 + 5 * 11);
    }
    Ok(())
}

#[test]
fn map_contents_have_equal_structural_cost() -> Result<()> {
    let wat = result_component(
        "(map u32 u32)",
        r"\10\00\00\00\02\00\00\00\00\00\00\00\00\00\00\00\01\00\00\00\02\00\00\00\03\00\00\00\04\00\00\00",
    );
    for dynamic in [false, true] {
        let engine = engine(Some(PRICES))?;
        let (mut store, instance) = instantiate(&engine, &wat)?;
        if dynamic {
            instance.get_func(&mut store, "run").unwrap().call(
                &mut store,
                &[],
                &mut [Val::Bool(false)],
            )?;
        } else {
            let result = instance
                .get_typed_func::<(), (HashMap<u32, u32>,)>(&mut store, "run")?
                .call(&mut store, ())?
                .0;
            assert_eq!(result, HashMap::from([(1, 2), (3, 4)]));
        }
        assert_eq!(BUDGET - store.get_fuel()?, 2 + 6 * 3);
    }
    Ok(())
}

#[test]
fn strings_charge_before_decoding_including_invalid_suffixes() -> Result<()> {
    for (encoding, bytes, length) in [("utf8", r"abc\ff", 4), ("utf16", r"a\00b\00\00\d8", 3)] {
        let wat = result_component(
            "string",
            &format!(r"\10\00\00\00\{length:02x}\00\00\00\00\00\00\00\00\00\00\00{bytes}"),
        )
        .replace(
            "(memory (core memory",
            &format!("string-encoding={encoding} (memory (core memory"),
        );
        let source_bytes = if encoding == "utf16" {
            length * 2
        } else {
            length
        };
        for dynamic in [false, true] {
            let engine = engine(Some(PRICES))?;
            let (mut store, instance) = instantiate(&engine, &wat)?;
            let result = if dynamic {
                instance.get_func(&mut store, "run").unwrap().call(
                    &mut store,
                    &[],
                    &mut [Val::Bool(false)],
                )
            } else {
                instance
                    .get_typed_func::<(), (String,)>(&mut store, "run")?
                    .call(&mut store, ())
                    .map(|_| ())
            };
            assert!(result.is_err());
            assert_eq!(BUDGET - store.get_fuel()?, 2 + 2 * 3 + source_bytes * 7);
        }
    }
    Ok(())
}

fn accepting_component(ty: &str) -> String {
    format!(
        r#"(component
        (core module $m
            (memory (export "memory") 1)
            (func (export "realloc") (param i32 i32 i32 i32) (result i32) (i32.const 16))
            (func (export "run") (param i32 i32)))
        (core instance $i (instantiate $m))
        (func (export "run") (param "v" {ty})
            (canon lift (core func $i "run") (memory (core memory $i "memory"))
                (realloc (core func $i "realloc")))))"#
    )
}

#[test]
fn lowering_strings_and_lists_are_not_charged() -> Result<()> {
    for ty in ["string", "(list u32)"] {
        let wat = accepting_component(ty);
        for dynamic in [false, true] {
            let mut observed = Vec::new();
            for metered in [false, true] {
                let engine = engine(metered.then_some(PRICES))?;
                let (mut store, instance) = instantiate(&engine, &wat)?;
                let result = if dynamic {
                    let arg = if ty == "string" {
                        Val::String("abcd".into())
                    } else {
                        Val::List(vec![Val::U32(0); 4])
                    };
                    instance
                        .get_func(&mut store, "run")
                        .unwrap()
                        .call(&mut store, &[arg], &mut [])
                } else if ty == "string" {
                    instance
                        .get_typed_func::<(&str,), ()>(&mut store, "run")?
                        .call(&mut store, ("abcd",))
                } else {
                    instance
                        .get_typed_func::<(&[u32],), ()>(&mut store, "run")?
                        .call(&mut store, (&[0; 4],))
                };
                result?;
                observed.push(BUDGET - store.get_fuel()?);
            }
            // Only the empty result tuple is lifted.
            assert_eq!(observed[1] - observed[0], 3);
        }
    }
    Ok(())
}

#[test]
fn resource_lifting_requires_admission_and_cleanup_remains_possible() -> Result<()> {
    let wat = r#"(component
        (type $t (resource (rep i32)))
        (export $exported-t "t" (type $t))
        (core func $new (canon resource.new $t))
        (func (export "new") (param "rep" u32) (result (own $exported-t))
            (canon lift (core func $new))))"#;
    for dynamic in [false, true] {
        for budget in [6, 5] {
            let engine = engine(Some(PRICES))?;
            let (mut store, instance) = instantiate(&engine, wat)?;
            store.set_fuel(budget)?;
            let result = if dynamic {
                let mut results = [Val::Bool(false)];
                instance
                    .get_func(&mut store, "new")
                    .unwrap()
                    .call(&mut store, &[Val::U32(42)], &mut results)
                    .map(|()| match results[0] {
                        Val::Resource(resource) => resource,
                        _ => unreachable!(),
                    })
            } else {
                instance
                    .get_typed_func::<(u32,), (ResourceAny,)>(&mut store, "new")?
                    .call(&mut store, (42,))
                    .map(|(resource,)| resource)
            };
            if budget == 6 {
                let resource = result?;
                assert_eq!(store.get_fuel()?, 0);
                resource.resource_drop(&mut store)?;
            } else {
                assert_eq!(
                    result.unwrap_err().downcast_ref::<Trap>(),
                    Some(&Trap::OutOfFuel)
                );
            }
            assert_eq!(store.get_fuel()?, 0);
        }
    }
    Ok(())
}

fn encode_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("\\{b:02x}")).collect()
}

fn adapter_component(source: &str, destination: &str, bytes: &[u8], source_len: u32) -> String {
    let data = encode_bytes(bytes);
    format!(
        r#"(component
        (component $receiver
            (core module $m
                (memory (export "memory") 1)
                (func (export "realloc") (param i32 i32 i32 i32) (result i32) (i32.const 4096))
                (func (export "accept") (param i32 i32) (result i32) (local.get 1)))
            (core instance $i (instantiate $m))
            (func (export "accept") (param "s" string) (result u32)
                (canon lift (core func $i "accept") string-encoding={destination}
                    (memory (core memory $i "memory")) (realloc (core func $i "realloc")))))
        (instance $receiver (instantiate $receiver))
        (core module $memory
            (memory (export "memory") 1) (data (i32.const 32) "{data}"))
        (core instance $memory (instantiate $memory))
        (core func $accept (canon lower (func $receiver "accept") string-encoding={source}
            (memory (core memory $memory "memory"))))
        (core module $m
            (import "" "accept" (func $accept (param i32 i32) (result i32)))
            (func (export "run") (result i32) (call $accept (i32.const 32) (i32.const {source_len}))))
        (core instance $i (instantiate $m (with "" (instance (export "accept" (func $accept))))))
        (func (export "run") (result u32) (canon lift (core func $i "run"))))"#
    )
}

#[test]
fn guest_adapter_transcoding_is_outside_lifting_meter() -> Result<()> {
    for text in ["", "ASCII", "aéÿ", "aé𝄞"] {
        for source in ["utf8", "utf16", "latin1+utf16"] {
            let (bytes, source_len) = if source == "utf8" {
                (text.as_bytes().to_vec(), text.len() as u32)
            } else if source == "latin1+utf16" && text.chars().all(|c| u32::from(c) <= 255) {
                let bytes: Vec<_> = text.chars().map(|c| c as u8).collect();
                let len = bytes.len() as u32;
                (bytes, len)
            } else {
                let bytes: Vec<_> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
                let len = bytes.len() as u32 / 2;
                (
                    bytes,
                    len | if source == "latin1+utf16" { 1 << 31 } else { 0 },
                )
            };
            for destination in ["utf8", "utf16", "latin1+utf16"] {
                let wat = adapter_component(source, destination, &bytes, source_len);
                let expected_len = match destination {
                    "utf8" => text.len() as u32,
                    "utf16" => text.encode_utf16().count() as u32,
                    _ if text.chars().all(|c| u32::from(c) <= 255) => text.chars().count() as u32,
                    _ => (text.encode_utf16().count() as u32) | (1 << 31),
                };
                let mut costs = Vec::new();
                for metered in [false, true] {
                    let engine = engine(metered.then_some(PRICES))?;
                    let (mut store, instance) = instantiate(&engine, &wat)?;
                    let result = instance
                        .get_typed_func::<(), (u32,)>(&mut store, "run")?
                        .call(&mut store, ())?
                        .0;
                    assert_eq!(result, expected_len, "{source} to {destination}: {text:?}");
                    costs.push(BUDGET - store.get_fuel()?);
                }
                assert_eq!(
                    costs[1] - costs[0],
                    2 * 3,
                    "{source} to {destination}: {text:?}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn adapter_validation_is_not_billed_as_host_lifting() -> Result<()> {
    for destination in ["utf8", "utf16", "latin1+utf16"] {
        let wat = adapter_component("utf8", destination, b"a\xff", 2);
        for price in [0, u64::MAX] {
            let engine = engine(Some(LiftingPrices {
                string_byte: price,
                ..PRICES
            }))?;
            let (mut store, instance) = instantiate(&engine, &wat)?;
            let err = instance
                .get_typed_func::<(), (u32,)>(&mut store, "run")?
                .call(&mut store, ())
                .unwrap_err();
            assert!(format!("{err:?}").contains("invalid utf8"));
            assert_ne!(err.downcast_ref::<Trap>(), Some(&Trap::OutOfFuel));
        }
    }
    Ok(())
}

#[test]
fn deeply_nested_lifting_weights_survive_serialization() -> Result<()> {
    let plain = engine(None)?;
    let metered = engine(Some(PRICES))?;
    // Scalar, tuple, function, and component wrappers bring 96 options to
    // the validator's nesting limit of 100; one more option must be rejected.
    for depth in [96, 97] {
        let mut definitions = String::from("(type $t0 u32)");
        for i in 1..=depth {
            definitions.push_str(&format!("(type $t{i} (option $t{}))", i - 1));
        }
        let wat = result_component(&format!("(tuple $t{depth})"), "").replacen(
            "(component",
            &format!("(component {definitions}"),
            1,
        );
        if depth == 97 {
            for engine in [&plain, &metered] {
                let err = Component::new(engine, &wat).unwrap_err();
                assert!(format!("{err:?}").contains("type nesting is too deep"));
            }
            continue;
        }
        let component = Component::new(&plain, &wat)?;
        let bytes = component.serialize()?;
        for component in [
            Component::new(&metered, &wat)?,
            // The bytes were produced locally by this Wasmtime build.
            unsafe { Component::deserialize(&metered, &bytes)? },
        ] {
            let mut store = Store::new(&metered, ());
            store.set_fuel(BUDGET)?;
            let instance = Linker::new(&metered).instantiate(&mut store, &component)?;
            instance.get_func(&mut store, "run").unwrap().call(
                &mut store,
                &[],
                &mut [Val::Bool(false)],
            )?;
            assert_eq!(BUDGET - store.get_fuel()?, 2 + (depth + 3) * 3);
        }
    }
    Ok(())
}

#[test]
fn shared_type_weights_count_each_occurrence() -> Result<()> {
    let depth = 12;
    let mut definitions = String::from("(type $t0 u32)");
    for i in 1..=depth {
        definitions.push_str(&format!("(type $t{i} (tuple $t{0} $t{0}))", i - 1));
    }
    let wat = result_component(&format!("(option $t{depth})"), "").replacen(
        "(component",
        &format!("(component {definitions}"),
        1,
    );
    let plain = engine(None)?;
    let bytes = Component::new(&plain, &wat)?.serialize()?;
    let metered = engine(Some(PRICES))?;
    for component in [
        Component::new(&metered, &wat)?,
        // The bytes were produced locally by this Wasmtime build.
        unsafe { Component::deserialize(&metered, &bytes)? },
    ] {
        let mut store = Store::new(&metered, ());
        store.set_fuel(BUDGET)?;
        let instance = Linker::new(&metered).instantiate(&mut store, &component)?;
        let mut result = [Val::Bool(false)];
        instance
            .get_func(&mut store, "run")
            .unwrap()
            .call(&mut store, &[], &mut result)?;
        assert_eq!(result, [Val::Option(None)]);
        assert_eq!(BUDGET - store.get_fuel()?, 2 + ((1 << (depth + 1)) + 1) * 3);
    }
    Ok(())
}

#[test]
fn host_imports_charge_incoming_lifting_only() -> Result<()> {
    let wat = r#"(component
        (import "echo" (func $echo (param "s" string) (result string)))
        (core module $memory
            (memory (export "memory") 1)
            (data (i32.const 0) "abcd")
            (func (export "realloc") (param i32 i32 i32 i32) (result i32) (i32.const 64)))
        (core instance $memory (instantiate $memory))
        (core func $echo (canon lower (func $echo)
            (memory (core memory $memory "memory")) (realloc (core func $memory "realloc"))))
        (core module $m
            (import "" "echo" (func $echo (param i32 i32 i32)))
            (import "" "memory" (memory 1))
            (func (export "run")
                (call $echo (i32.const 0) (i32.const 4) (i32.const 32))
                (if (i32.ne (i32.load (i32.const 36)) (i32.const 4)) (then unreachable))
                (if (i32.ne (i32.load (i32.load (i32.const 32))) (i32.const 1684234849)) (then unreachable))))
        (core instance $i (instantiate $m (with "" (instance
            (export "echo" (func $echo)) (export "memory" (memory $memory "memory"))))))
        (func (export "run") (canon lift (core func $i "run"))))"#;
    for dynamic in [false, true] {
        let mut costs = Vec::new();
        for metered in [false, true] {
            let engine = engine(metered.then_some(PRICES))?;
            let component = Component::new(&engine, wat)?;
            let mut linker = Linker::<()>::new(&engine);
            if dynamic {
                linker.root().func_new("echo", |_, _, params, results| {
                    results[0] = params[0].clone();
                    Ok(())
                })?;
            } else {
                linker
                    .root()
                    .func_wrap("echo", |_, (s,): (String,)| Ok((s,)))?;
            }
            let mut store = Store::new(&engine, ());
            store.set_hostcall_fuel_enabled(false);
            store.set_fuel(BUDGET)?;
            let instance = linker.instantiate(&mut store, &component)?;
            instance
                .get_typed_func::<(), ()>(&mut store, "run")?
                .call(&mut store, ())?;
            costs.push(BUDGET - store.get_fuel()?);
        }
        assert_eq!(costs[1] - costs[0], 2 * 3 + 4 * 7 + 3);
    }
    Ok(())
}

#[test]
fn dynamic_lowering_names_are_not_charged() -> Result<()> {
    let wat = r#"(component
        (type $f (flags "x"))
        (export $f-export "f" (type $f))
        (core module $m (func (export "run") (param i32)))
        (core instance $i (instantiate $m))
        (func (export "run") (param "v" $f-export) (canon lift (core func $i "run"))))"#;
    for count in [0, 1, 1024] {
        let mut costs = Vec::new();
        for metered in [false, true] {
            let engine = engine(metered.then_some(PRICES))?;
            let (mut store, instance) = instantiate(&engine, wat)?;
            instance.get_func(&mut store, "run").unwrap().call(
                &mut store,
                &[Val::Flags(vec!["x".into(); count])],
                &mut [],
            )?;
            costs.push(BUDGET - store.get_fuel()?);
        }
        assert_eq!(costs[1] - costs[0], 3);
    }
    Ok(())
}

#[test]
fn lowering_fixed_arrays_and_maps_are_not_charged() -> Result<()> {
    for array in [false, true] {
        let wat = if array {
            accepting_component("(list u32 1024)").replace(
                "(export \"run\") (param i32 i32)",
                "(export \"run\") (param i32)",
            )
        } else {
            accepting_component("(map u32 u32)")
        };
        for dynamic in [false, true] {
            let mut costs = Vec::new();
            for metered in [false, true] {
                let engine = engine(metered.then_some(PRICES))?;
                let (mut store, instance) = instantiate(&engine, &wat)?;
                if dynamic {
                    let value = if array {
                        Val::FixedLengthList(vec![Val::U32(0); 1024])
                    } else {
                        Val::Map(vec![(Val::U32(1), Val::U32(2)), (Val::U32(3), Val::U32(4))])
                    };
                    instance.get_func(&mut store, "run").unwrap().call(
                        &mut store,
                        &[value],
                        &mut [],
                    )?;
                } else if array {
                    instance
                        .get_typed_func::<([u32; 1024],), ()>(&mut store, "run")?
                        .call(&mut store, ([0; 1024],))?;
                } else {
                    instance
                        .get_typed_func::<(HashMap<u32, u32>,), ()>(&mut store, "run")?
                        .call(&mut store, (HashMap::from([(1, 2), (3, 4)]),))?;
                }
                costs.push(BUDGET - store.get_fuel()?);
            }
            assert_eq!(costs[1] - costs[0], 3);
        }
    }
    Ok(())
}

#[test]
fn repeated_list_access_requires_new_structural_admission() -> Result<()> {
    let engine = engine(Some(PRICES))?;
    let wat = result_component("(list u32)", r"\10\00\00\00\04\00\00\00");
    let (mut store, instance) = instantiate(&engine, &wat)?;
    let (list,) = instance
        .get_typed_func::<(), (WasmList<u32>,)>(&mut store, "run")?
        .call(&mut store, ())?;
    store.set_fuel(3)?;
    assert_eq!(list.get(&mut store, 0).unwrap()?, 0);
    assert_eq!(store.get_fuel()?, 0);
    assert!(list.get(&mut store, 4).is_none());
    assert_eq!(
        list.get(&mut store, 0)
            .unwrap()
            .unwrap_err()
            .downcast_ref::<Trap>(),
        Some(&Trap::OutOfFuel)
    );
    store.set_fuel(12)?;
    assert_eq!(list.iter(&mut store)?.collect::<Result<Vec<_>>>()?, [0; 4]);
    assert_eq!(store.get_fuel()?, 0);
    store.set_fuel(12)?;
    let mut iter = list.iter(&mut store)?;
    assert_eq!(iter.next().unwrap()?, 0);
    drop(iter);
    assert_eq!(store.get_fuel()?, 0);
    assert!(list.iter(&mut store).is_err());
    Ok(())
}
