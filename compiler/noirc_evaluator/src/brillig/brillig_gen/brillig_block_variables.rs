//! This module handles allocation, tracking, and lifetime management of variables
//! within a Brillig compiled SSA basic block.
//!
//! [BlockVariables] maintains a set of SSA [ValueId]s that are live and available
//! during the compilation of a single SSA block into Brillig instructions. It cooperates
//! with the [FunctionContext] to manage the mapping from SSA values to [BrilligVariable]s
//! and with the [BrilligContext] for allocating registers.
//!
//! Variables are:
//! - Allocated when first defined in a block (if not already global or hoisted to the global space).
//! - Cached for reuse to avoid redundant register allocation.
//! - Deallocated explicitly when no longer needed (as determined by SSA liveness).
use acvm::FieldElement;
use fxhash::FxHashSet as HashSet;

use crate::{
    brillig::brillig_ir::{
        BrilligContext,
        brillig_variable::{
            BrilligArray, BrilligVariable, BrilligVector, SingleAddrVariable,
            get_bit_size_from_ssa_type,
        },
        registers::RegisterAllocator,
    },
    ssa::ir::{
        dfg::DataFlowGraph,
        types::{CompositeType, Type},
        value::ValueId,
    },
};

use super::brillig_fn::FunctionContext;

/// Tracks SSA variables that are live and usable during Brillig compilation of a block.
///
/// This structure is meant to be instantiated per SSA basic block and initialized using the
/// the set of live variables that must be available at the block's entry.
///
/// It implements:
/// - A set of active [ValueId]s that are allocated and usable.
/// - The interface to define new variables as needed for instructions within the block.
/// - Utilities to remove, check, and retrieve variables during Brillig codegen.
#[derive(Debug, Default)]
pub(crate) struct BlockVariables {
    available_variables: HashSet<ValueId>,
}

impl BlockVariables {
    /// Creates a BlockVariables instance. It uses the variables that are live in to the block and the global available variables (block parameters)
    pub(crate) fn new(live_in: HashSet<ValueId>) -> Self {
        BlockVariables { available_variables: live_in }
    }

    /// Returns all variables that have not been removed at this point.
    pub(crate) fn get_available_variables(
        &self,
        function_context: &FunctionContext,
    ) -> Vec<BrilligVariable> {
        self.available_variables
            .iter()
            .map(|value_id| {
                function_context
                    .ssa_value_allocations
                    .get(value_id)
                    .unwrap_or_else(|| panic!("ICE: Value not found in cache {value_id}"))
            })
            .cloned()
            .collect()
    }

    /// For a given SSA value id, define the variable and return the corresponding cached allocation.
    pub(crate) fn define_variable<Registers: RegisterAllocator>(
        &mut self,
        function_context: &mut FunctionContext,
        brillig_context: &mut BrilligContext<FieldElement, Registers>,
        value_id: ValueId,
        dfg: &DataFlowGraph,
    ) -> BrilligVariable {
        let variable = allocate_value(value_id, brillig_context, dfg);

        if function_context.ssa_value_allocations.insert(value_id, variable).is_some() {
            unreachable!("ICE: ValueId {value_id:?} was already in cache");
        }

        self.available_variables.insert(value_id);

        variable
    }

    /// Defines a variable that fits in a single register and returns the allocated register.
    pub(crate) fn define_single_addr_variable<Registers: RegisterAllocator>(
        &mut self,
        function_context: &mut FunctionContext,
        brillig_context: &mut BrilligContext<FieldElement, Registers>,
        value: ValueId,
        dfg: &DataFlowGraph,
    ) -> SingleAddrVariable {
        let variable = self.define_variable(function_context, brillig_context, value, dfg);
        variable.extract_single_addr()
    }

    /// Removes a variable so it's not used anymore within this block.
    pub(crate) fn remove_variable<Registers: RegisterAllocator>(
        &mut self,
        value_id: &ValueId,
        function_context: &mut FunctionContext,
        brillig_context: &mut BrilligContext<FieldElement, Registers>,
    ) {
        assert!(self.available_variables.remove(value_id), "ICE: Variable is not available");
        let variable = function_context
            .ssa_value_allocations
            .get(value_id)
            .expect("ICE: Variable allocation not found");
        brillig_context.deallocate_register(variable.extract_register());
    }

    /// Checks if a variable is allocated.
    pub(crate) fn is_allocated(&self, value_id: &ValueId) -> bool {
        self.available_variables.contains(value_id)
    }

    /// For a given SSA value id, return the corresponding cached allocation.
    pub(crate) fn get_allocation(
        &mut self,
        function_context: &FunctionContext,
        value_id: ValueId,
    ) -> BrilligVariable {
        assert!(
            self.available_variables.contains(&value_id),
            "ICE: ValueId {value_id:?} is not available"
        );

        *function_context
            .ssa_value_allocations
            .get(&value_id)
            .unwrap_or_else(|| panic!("ICE: Value not found in cache {value_id}"))
    }
}

/// Computes the length of an array. This will match with the indexes that SSA will issue
pub(crate) fn compute_array_length(item_typ: &CompositeType, elem_count: usize) -> usize {
    item_typ.len() * elem_count
}

/// For a given value_id, allocates the necessary registers to hold it.
pub(crate) fn allocate_value<F, Registers: RegisterAllocator>(
    value_id: ValueId,
    brillig_context: &mut BrilligContext<F, Registers>,
    dfg: &DataFlowGraph,
) -> BrilligVariable {
    let typ = dfg.type_of_value(value_id);

    allocate_value_with_type(brillig_context, typ)
}

/// For a given value_id, allocates the necessary registers to hold it.
pub(crate) fn allocate_value_with_type<F, Registers: RegisterAllocator>(
    brillig_context: &mut BrilligContext<F, Registers>,
    typ: Type,
) -> BrilligVariable {
    match typ {
        Type::Numeric(_) | Type::Reference(_) | Type::Function => {
            BrilligVariable::SingleAddr(SingleAddrVariable {
                address: brillig_context.allocate_register(),
                bit_size: get_bit_size_from_ssa_type(&typ),
            })
        }
        Type::Array(item_typ, elem_count) => BrilligVariable::BrilligArray(BrilligArray {
            pointer: brillig_context.allocate_register(),
            size: compute_array_length(&item_typ, elem_count as usize),
        }),
        Type::Slice(_) => BrilligVariable::BrilligVector(BrilligVector {
            pointer: brillig_context.allocate_register(),
        }),
    }
}
