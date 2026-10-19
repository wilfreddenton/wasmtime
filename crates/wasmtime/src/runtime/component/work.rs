//! Precomputed conservative admission weights for component lifting.

use crate::prelude::*;
use wasmtime_environ::component::{ComponentTypes, InterfaceType};

#[derive(Clone, Copy, Debug)]
pub(crate) struct WorkWeight {
    // Preserve overflow as `None` so a zero price can still make it free.
    pub nodes: Option<u64>,
    pub metadata: Option<u64>,
}

impl WorkWeight {
    const ZERO: Self = Self {
        nodes: Some(0),
        metadata: Some(0),
    };

    const LEAF: Self = Self {
        nodes: Some(1),
        metadata: Some(0),
    };

    fn add(self, other: Self) -> Self {
        Self {
            nodes: self
                .nodes
                .zip(other.nodes)
                .and_then(|(a, b)| a.checked_add(b)),
            metadata: self
                .metadata
                .zip(other.metadata)
                .and_then(|(a, b)| a.checked_add(b)),
        }
    }

    fn repeat(self, count: u64) -> Self {
        if count == 0 {
            return Self::ZERO;
        }
        Self {
            nodes: self.nodes.and_then(|n| n.checked_mul(count)),
            metadata: self.metadata.and_then(|n| n.checked_mul(count)),
        }
    }

    fn max(self, other: Self) -> Self {
        Self {
            nodes: self.nodes.zip(other.nodes).map(|(a, b)| a.max(b)),
            metadata: self.metadata.zip(other.metadata).map(|(a, b)| a.max(b)),
        }
    }
}

pub(crate) struct WorkWeights(TryHashMap<InterfaceType, WorkWeight>);

impl WorkWeights {
    pub fn new(types: &ComponentTypes) -> Result<Self> {
        let mut result = Self(TryHashMap::new());
        for ty in types.work_accounting_types() {
            result.compute(types, ty)?;
        }
        Ok(result)
    }

    fn compute(&mut self, types: &ComponentTypes, ty: InterfaceType) -> Result<WorkWeight> {
        if let Some(weight) = self.0.get(&ty) {
            return Ok(*weight);
        }
        // Validated component types have bounded nesting. Memoization keeps
        // shared subtrees from being traversed again for each parent.
        let mut weight = WorkWeight::LEAF;
        let mut name = |name: &str| {
            // Account for per-entry work as well as copying the name.
            weight.metadata = weight
                .metadata
                .and_then(|n| n.checked_add(name.len() as u64))
                .and_then(|n| n.checked_add(1));
        };
        match ty {
            InterfaceType::Record(i) => {
                for f in &types[i].fields {
                    name(&f.name);
                }
                for f in &types[i].fields {
                    weight = weight.add(self.compute(types, f.ty)?);
                }
            }
            InterfaceType::Tuple(i) => {
                for t in &types[i].types {
                    weight = weight.add(self.compute(types, *t)?);
                }
            }
            InterfaceType::FixedLengthList(i) => {
                weight = weight.add(
                    self.compute(types, types[i].element)?
                        .repeat(u64::from(types[i].size)),
                );
            }
            InterfaceType::Variant(i) => {
                for n in types[i].cases.keys() {
                    name(n);
                }
                let mut payload = WorkWeight::ZERO;
                for t in types[i].cases.values().flatten() {
                    payload = payload.max(self.compute(types, *t)?);
                }
                weight = weight.add(payload);
            }
            InterfaceType::Option(i) => {
                weight = weight.add(self.compute(types, types[i].ty)?);
            }
            InterfaceType::Result(i) => {
                let mut payload = WorkWeight::ZERO;
                for t in [types[i].ok, types[i].err].into_iter().flatten() {
                    payload = payload.max(self.compute(types, t)?);
                }
                weight = weight.add(payload);
            }
            InterfaceType::Enum(i) => {
                for n in &types[i].names {
                    name(n);
                }
            }
            InterfaceType::Flags(i) => {
                for n in &types[i].names {
                    name(n);
                }
            }
            _ => (),
        }
        self.0.insert(ty, weight)?;
        Ok(weight)
    }

    pub fn get(&self, ty: InterfaceType) -> WorkWeight {
        match ty {
            InterfaceType::Record(_)
            | InterfaceType::Tuple(_)
            | InterfaceType::Variant(_)
            | InterfaceType::Enum(_)
            | InterfaceType::Flags(_)
            | InterfaceType::Option(_)
            | InterfaceType::Result(_)
            | InterfaceType::FixedLengthList(_) => *self
                .0
                .get(&ty)
                .expect("composite accounting weight missing"),
            InterfaceType::Bool
            | InterfaceType::S8
            | InterfaceType::U8
            | InterfaceType::S16
            | InterfaceType::U16
            | InterfaceType::S32
            | InterfaceType::U32
            | InterfaceType::S64
            | InterfaceType::U64
            | InterfaceType::Float32
            | InterfaceType::Float64
            | InterfaceType::Char
            | InterfaceType::String
            | InterfaceType::List(_)
            | InterfaceType::Map(_)
            | InterfaceType::Own(_)
            | InterfaceType::Borrow(_)
            | InterfaceType::Future(_)
            | InterfaceType::Stream(_)
            | InterfaceType::ErrorContext(_) => WorkWeight::LEAF,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::WorkWeight;

    #[test]
    fn overflow_is_retained_and_empty_repetition_is_free() {
        let large = WorkWeight::LEAF.repeat(u64::MAX);
        assert_eq!(large.nodes, Some(u64::MAX));
        assert_eq!(large.add(WorkWeight::LEAF).nodes, None);
        assert_eq!(large.repeat(2).nodes, None);
        assert_eq!(large.repeat(2).repeat(0).nodes, Some(0));
        assert_eq!(large.repeat(2).max(WorkWeight::LEAF).nodes, None);
        assert_eq!(large.repeat(2).metadata, Some(0));
    }
}
