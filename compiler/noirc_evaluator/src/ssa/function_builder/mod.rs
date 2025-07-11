pub mod data_bus;

use std::{borrow::Cow, collections::BTreeMap, sync::Arc};

use acvm::{FieldElement, acir::circuit::ErrorSelector};
use noirc_errors::{
    Location,
    call_stack::{CallStack, CallStackId},
};
use noirc_frontend::hir_def::types::Type as HirType;
use noirc_frontend::monomorphization::ast::InlineType;

use crate::ssa::ir::{
    basic_block::BasicBlockId,
    function::{Function, FunctionId},
    instruction::{Binary, BinaryOp, Instruction, TerminatorInstruction},
    types::Type,
    value::{Value, ValueId},
};

use super::{
    ir::{
        basic_block::BasicBlock,
        dfg::{GlobalsGraph, InsertInstructionResult},
        function::RuntimeType,
        instruction::{ArrayOffset, ConstrainError, InstructionId, Intrinsic},
        types::NumericType,
    },
    opt::pure::FunctionPurities,
    ssa_gen::Ssa,
};

/// The per-function context for each ssa function being generated.
///
/// This is split from the global SsaBuilder context to allow each function
/// to be potentially built concurrently.
///
/// Contrary to the name, this struct has the capacity to build as many
/// functions as needed, although it is limited to one function at a time.
pub struct FunctionBuilder {
    pub current_function: Function,
    current_block: BasicBlockId,
    finished_functions: Vec<Function>,
    call_stack: CallStackId,
    error_types: BTreeMap<ErrorSelector, HirType>,

    /// Whether instructions are simplified as soon as they are inserted into this builder.
    /// This is true by default unless changed to false after constructing a builder.
    pub simplify: bool,

    globals: Arc<GlobalsGraph>,
    purities: Arc<FunctionPurities>,
}

impl FunctionBuilder {
    /// Creates a new FunctionBuilder to build the function with the given FunctionId.
    ///
    /// This creates the new function internally so there is no need to call .new_function()
    /// right after constructing a new FunctionBuilder.
    pub fn new(function_name: String, function_id: FunctionId) -> Self {
        let new_function = Function::new(function_name, function_id);
        Self {
            current_block: new_function.entry_block(),
            current_function: new_function,
            finished_functions: Vec::new(),
            call_stack: CallStackId::root(),
            error_types: BTreeMap::default(),
            simplify: true,
            globals: Default::default(),
            purities: Default::default(),
        }
    }

    /// Create a function builder with a new function created with the same
    /// name, globals, and function purities taken from an existing function.
    pub fn from_existing(function: &Function, function_id: FunctionId) -> Self {
        let mut this = Self::new(function.name().to_owned(), function_id);
        this.set_globals(function.dfg.globals.clone());
        this.purities = function.dfg.function_purities.clone();
        this.current_function.set_runtime(function.runtime());
        this.current_function.dfg.set_function_purities(this.purities.clone());
        this
    }

    /// Set the runtime of the initial function that is created internally after constructing
    /// the FunctionBuilder. A function's default runtime type is `RuntimeType::Acir(InlineType::Inline)`.
    /// This should only be used immediately following construction of a FunctionBuilder
    /// and will panic if there are any already finished functions.
    pub fn set_runtime(&mut self, runtime: RuntimeType) {
        assert_eq!(
            self.finished_functions.len(),
            0,
            "Attempted to set runtime on a FunctionBuilder with finished functions. A FunctionBuilder's runtime should only be set on its initial function"
        );
        self.current_function.set_runtime(runtime);
    }

    pub fn set_globals(&mut self, globals: Arc<GlobalsGraph>) {
        self.globals = globals;
        self.apply_globals();
    }

    fn apply_globals(&mut self) {
        for (_, value) in self.globals.values_iter() {
            self.current_function.dfg.make_global(value.get_type().into_owned());
        }
        self.current_function.set_globals(self.globals.clone());
    }

    pub fn set_purities(&mut self, purities: Arc<FunctionPurities>) {
        self.purities = purities.clone();
        self.current_function.dfg.set_function_purities(purities);
    }

    /// Finish the current function and create a new function.
    ///
    /// A FunctionBuilder can always only work on one function at a time, so care
    /// should be taken not to finish a function that is still in progress by calling
    /// new_function before the current function is finished.
    fn new_function_with_type(
        &mut self,
        name: String,
        function_id: FunctionId,
        runtime_type: RuntimeType,
    ) {
        let call_stack = self.current_function.dfg.get_call_stack(self.call_stack);
        let mut new_function = Function::new(name, function_id);
        new_function.set_runtime(runtime_type);
        self.switch_to_block(new_function.entry_block());

        let old_function = std::mem::replace(&mut self.current_function, new_function);
        // Copy the call stack to the new function
        self.call_stack =
            self.current_function.dfg.call_stack_data.get_or_insert_locations(&call_stack);
        self.finished_functions.push(old_function);

        self.current_function.dfg.set_function_purities(self.purities.clone());
        self.apply_globals();
    }

    /// Finish the current function and create a new ACIR function.
    pub fn new_function(&mut self, name: String, function_id: FunctionId, inline_type: InlineType) {
        self.new_function_with_type(name, function_id, RuntimeType::Acir(inline_type));
    }

    /// Finish the current function and create a new unconstrained function.
    pub fn new_brillig_function(
        &mut self,
        name: String,
        function_id: FunctionId,
        inline_type: InlineType,
    ) {
        self.new_function_with_type(name, function_id, RuntimeType::Brillig(inline_type));
    }

    /// Consume the FunctionBuilder returning all the functions it has generated.
    pub fn finish(mut self) -> Ssa {
        self.finished_functions.push(self.current_function);

        Ssa::new(self.finished_functions, self.error_types)
    }

    /// Add a parameter to the current function with the given parameter type.
    /// Returns the newly-added parameter.
    pub fn add_parameter(&mut self, typ: Type) -> ValueId {
        let entry = self.current_function.entry_block();
        self.current_function.dfg.add_block_parameter(entry, typ)
    }

    /// Insert a numeric constant into the current function
    pub fn numeric_constant(
        &mut self,
        value: impl Into<FieldElement>,
        typ: NumericType,
    ) -> ValueId {
        validate_numeric_type(&typ);
        self.current_function.dfg.make_constant(value.into(), typ)
    }

    /// Insert a numeric constant into the current function of type Field
    #[cfg(test)]
    pub fn field_constant(&mut self, value: impl Into<FieldElement>) -> ValueId {
        self.numeric_constant(value.into(), NumericType::NativeField)
    }

    /// Insert a numeric constant into the current function of type Type::length_type()
    pub fn length_constant(&mut self, value: impl Into<FieldElement>) -> ValueId {
        self.numeric_constant(value.into(), NumericType::length_type())
    }

    /// Returns the type of the given value.
    pub fn type_of_value(&self, value: ValueId) -> Type {
        self.current_function.dfg.type_of_value(value)
    }

    /// Insert a new block into the current function and return it.
    /// Note that this block is unreachable until another block is set to jump to it.
    pub fn insert_block(&mut self) -> BasicBlockId {
        self.current_function.dfg.make_block()
    }

    /// Adds a parameter with the given type to the given block.
    /// Returns the newly-added parameter.
    pub fn add_block_parameter(&mut self, block: BasicBlockId, typ: Type) -> ValueId {
        self.current_function.dfg.add_block_parameter(block, typ)
    }

    /// Returns the parameters of the given block in the current function.
    pub fn block_parameters(&self, block: BasicBlockId) -> &[ValueId] {
        self.current_function.dfg.block_parameters(block)
    }

    /// Inserts a new instruction at the end of the current block and returns its results
    pub fn insert_instruction(
        &mut self,
        instruction: Instruction,
        ctrl_typevars: Option<Vec<Type>>,
    ) -> InsertInstructionResult {
        let block = self.current_block();

        if self.simplify {
            self.current_function.dfg.insert_instruction_and_results(
                instruction,
                block,
                ctrl_typevars,
                self.call_stack,
            )
        } else {
            self.current_function.dfg.insert_instruction_and_results_without_simplification(
                instruction,
                block,
                ctrl_typevars,
                self.call_stack,
            )
        }
    }

    /// Switch to inserting instructions in the given block.
    /// Expects the given block to be within the same function. If you want to insert
    /// instructions into a new function, call new_function instead.
    pub fn switch_to_block(&mut self, block: BasicBlockId) {
        self.current_block = block;
    }

    /// Returns the block currently being inserted into
    pub(crate) fn current_block(&mut self) -> BasicBlockId {
        self.current_block
    }

    pub fn get_current_block_index(&self) -> BasicBlockId {
        self.current_block
    }

    /// Insert an allocate instruction at the end of the current block, allocating the
    /// given amount of field elements. Returns the result of the allocate instruction,
    /// which is always a Reference to the allocated data.
    pub fn insert_allocate(&mut self, element_type: Type) -> ValueId {
        let reference_type = Type::Reference(Arc::new(element_type));
        self.insert_instruction(Instruction::Allocate, Some(vec![reference_type])).first()
    }

    pub fn set_location(&mut self, location: Location) -> &mut FunctionBuilder {
        self.call_stack = self.current_function.dfg.call_stack_data.add_location_to_root(location);
        self
    }

    pub fn set_call_stack(&mut self, call_stack: CallStackId) -> &mut FunctionBuilder {
        self.call_stack = call_stack;
        self
    }

    pub fn get_call_stack(&self) -> CallStack {
        self.current_function.dfg.get_call_stack(self.call_stack)
    }

    /// Insert a Load instruction at the end of the current block, loading from the given address
    /// which should point to a previous Allocate instruction. Note that this is limited to loading
    /// a single value. Loading multiple values (such as a tuple) will require multiple loads.
    /// Returns the element that was loaded.
    pub fn insert_load(&mut self, address: ValueId, type_to_load: Type) -> ValueId {
        self.insert_instruction(Instruction::Load { address }, Some(vec![type_to_load])).first()
    }

    /// Insert a Store instruction at the end of the current block, storing the given element
    /// at the given address. Expects that the address points somewhere
    /// within a previous Allocate instruction.
    pub fn insert_store(&mut self, address: ValueId, value: ValueId) {
        self.insert_instruction(Instruction::Store { address, value }, None);
    }

    /// Insert a binary instruction at the end of the current block.
    /// Returns the result of the binary instruction.
    pub fn insert_binary(&mut self, lhs: ValueId, operator: BinaryOp, rhs: ValueId) -> ValueId {
        let instruction = Instruction::Binary(Binary { lhs, rhs, operator });
        self.insert_instruction(instruction, None).first()
    }

    /// Insert a not instruction at the end of the current block.
    /// Returns the result of the instruction.
    pub fn insert_not(&mut self, rhs: ValueId) -> ValueId {
        self.insert_instruction(Instruction::Not(rhs), None).first()
    }

    /// Insert a cast instruction at the end of the current block.
    /// Returns the result of the cast instruction.
    pub fn insert_cast(&mut self, value: ValueId, typ: NumericType) -> ValueId {
        validate_numeric_type(&typ);
        self.insert_instruction(Instruction::Cast(value, typ), None).first()
    }

    /// Insert a truncate instruction at the end of the current block.
    /// Returns the result of the truncate instruction.
    pub fn insert_truncate(&mut self, value: ValueId, bit_size: u32, max_bit_size: u32) -> ValueId {
        self.insert_instruction(Instruction::Truncate { value, bit_size, max_bit_size }, None)
            .first()
    }

    /// Insert a constrain instruction at the end of the current block.
    pub fn insert_constrain(
        &mut self,
        lhs: ValueId,
        rhs: ValueId,
        assert_message: Option<ConstrainError>,
    ) {
        self.insert_instruction(Instruction::Constrain(lhs, rhs, assert_message), None);
    }

    /// Insert a [`Instruction::RangeCheck`] instruction at the end of the current block.
    pub fn insert_range_check(
        &mut self,
        value: ValueId,
        max_bit_size: u32,
        assert_message: Option<String>,
    ) {
        self.insert_instruction(
            Instruction::RangeCheck { value, max_bit_size, assert_message },
            None,
        );
    }

    /// Insert a call instruction at the end of the current block and return
    /// the results of the call.
    pub fn insert_call(
        &mut self,
        func: ValueId,
        arguments: Vec<ValueId>,
        result_types: Vec<Type>,
    ) -> Cow<[ValueId]> {
        self.insert_instruction(Instruction::Call { func, arguments }, Some(result_types)).results()
    }

    /// Insert an instruction to extract an element from an array
    pub fn insert_array_get(
        &mut self,
        array: ValueId,
        index: ValueId,
        offset: ArrayOffset,
        element_type: Type,
    ) -> ValueId {
        let element_type = Some(vec![element_type]);
        self.insert_instruction(Instruction::ArrayGet { array, index, offset }, element_type)
            .first()
    }

    /// Insert an instruction to create a new array with the given index replaced with a new value
    pub fn insert_array_set(
        &mut self,
        array: ValueId,
        index: ValueId,
        value: ValueId,
        mutable: bool,
        offset: ArrayOffset,
    ) -> ValueId {
        let instruction = Instruction::ArraySet { array, index, value, mutable, offset };
        self.insert_instruction(instruction, None).first()
    }

    /// Insert an instruction to increment an array's reference count. This only has an effect
    /// in unconstrained code where arrays are reference counted and copy on write.
    pub fn insert_inc_rc(&mut self, value: ValueId) {
        self.insert_instruction(Instruction::IncrementRc { value }, None);
    }

    /// Insert an instruction to decrement an array's reference count. This only has an effect
    /// in unconstrained code where arrays are reference counted and copy on write.
    pub(crate) fn insert_dec_rc(&mut self, value: ValueId) {
        self.insert_instruction(Instruction::DecrementRc { value }, None);
    }

    /// Insert an enable_side_effects_if instruction. These are normally only automatically
    /// inserted during the flattening pass when branching is removed.
    pub fn insert_enable_side_effects_if(&mut self, condition: ValueId) {
        self.insert_instruction(Instruction::EnableSideEffectsIf { condition }, None);
    }

    /// Insert a `make_array` instruction to create a new array or slice.
    /// Returns the new array value. Expects `typ` to be an array or slice type.
    pub fn insert_make_array(&mut self, elements: im::Vector<ValueId>, typ: Type) -> ValueId {
        assert!(matches!(typ, Type::Array(..) | Type::Slice(_)));
        self.insert_instruction(Instruction::MakeArray { elements, typ }, None).first()
    }

    /// Terminates the current block with the given terminator instruction
    /// if the current block does not already have a terminator instruction.
    fn terminate_block_with(&mut self, terminator: TerminatorInstruction) {
        if self.current_function.dfg[self.current_block].terminator().is_none() {
            self.current_function.dfg.set_block_terminator(self.current_block, terminator);
        }
    }

    /// Terminate the current block with a jmp instruction to jmp to the given
    /// block with the given arguments.
    pub fn terminate_with_jmp(&mut self, destination: BasicBlockId, arguments: Vec<ValueId>) {
        let call_stack = self.call_stack;
        self.terminate_block_with(TerminatorInstruction::Jmp {
            destination,
            arguments,
            call_stack,
        });
    }

    /// Terminate the current block with a jmpif instruction to jmp with the given arguments
    /// block with the given arguments.
    pub fn terminate_with_jmpif(
        &mut self,
        condition: ValueId,
        then_destination: BasicBlockId,
        else_destination: BasicBlockId,
    ) {
        let call_stack = self.call_stack;
        self.terminate_block_with(TerminatorInstruction::JmpIf {
            condition,
            then_destination,
            else_destination,
            call_stack,
        });
    }

    /// Terminate the current block with a return instruction
    pub fn terminate_with_return(&mut self, return_values: Vec<ValueId>) {
        let call_stack = self.call_stack;
        self.terminate_block_with(TerminatorInstruction::Return { return_values, call_stack });
    }

    /// Terminate the current block with an unreachable instruction
    pub fn terminate_with_unreachable(&mut self) {
        let call_stack = self.call_stack;
        self.terminate_block_with(TerminatorInstruction::Unreachable { call_stack });
    }

    /// Returns a ValueId pointing to the given function or imports the function
    /// into the current function if it was not already, and returns that ID.
    pub fn import_function(&mut self, function: FunctionId) -> ValueId {
        self.current_function.dfg.import_function(function)
    }

    /// Returns a ValueId pointing to the given oracle/foreign function or imports the oracle
    /// into the current function if it was not already, and returns that ID.
    pub fn import_foreign_function(&mut self, function: &str) -> ValueId {
        self.current_function.dfg.import_foreign_function(function)
    }

    /// Retrieve a value reference to the given intrinsic operation.
    /// Returns None if there is no intrinsic matching the given name.
    pub fn import_intrinsic(&mut self, name: &str) -> Option<ValueId> {
        Intrinsic::lookup(name).map(|intrinsic| self.import_intrinsic_id(intrinsic))
    }

    /// Retrieve a value reference to the given intrinsic operation.
    pub fn import_intrinsic_id(&mut self, intrinsic: Intrinsic) -> ValueId {
        self.current_function.dfg.import_intrinsic(intrinsic)
    }

    pub fn get_intrinsic_from_value(&mut self, value: ValueId) -> Option<Intrinsic> {
        match self.current_function.dfg[value] {
            Value::Intrinsic(intrinsic) => Some(intrinsic),
            _ => None,
        }
    }

    /// Insert instructions to increment the reference count of any array(s) stored
    /// within the given value. If the given value is not an array and does not contain
    /// any arrays, this does nothing.
    ///
    /// Returns the ID of the array that was affected, if any.
    pub(crate) fn increment_array_reference_count(&mut self, value: ValueId) -> Option<ValueId> {
        self.update_array_reference_count(value, true)
    }

    /// Insert instructions to decrement the reference count of any array(s) stored
    /// within the given value. If the given value is not an array and does not contain
    /// any arrays, this does nothing.
    ///
    /// Returns the ID of the array that was affected, if any.
    pub(crate) fn decrement_array_reference_count(&mut self, value: ValueId) -> Option<ValueId> {
        self.update_array_reference_count(value, false)
    }

    /// Increment or decrement the given value's reference count if it is an array.
    /// If it is not an array, this does nothing. Note that inc_rc and dec_rc instructions
    /// are ignored outside of unconstrained code.
    ///
    /// If there is an `original` value it indicates that we're now decrementing it,
    /// but that should only happen if it hasn't been changed by an `array_set` in
    /// the meantime, which in itself would make sure its RC is decremented.
    ///
    /// Returns the ID of the array that was affected, if any.
    fn update_array_reference_count(&mut self, value: ValueId, increment: bool) -> Option<ValueId> {
        if self.current_function.runtime().is_acir() {
            return None;
        }
        match self.type_of_value(value) {
            Type::Numeric(_) | Type::Function | Type::Reference(_) => None,
            Type::Array(..) | Type::Slice(..) => {
                // If there are nested arrays or slices, we wait until ArrayGet
                // is issued to increment the count of that array.
                if increment {
                    self.insert_inc_rc(value);
                } else {
                    self.insert_dec_rc(value);
                }
                Some(value)
            }
        }
    }

    pub fn record_error_type(&mut self, selector: ErrorSelector, typ: HirType) {
        self.error_types.insert(selector, typ);
    }
}

impl std::ops::Index<ValueId> for FunctionBuilder {
    type Output = Value;

    fn index(&self, id: ValueId) -> &Self::Output {
        &self.current_function.dfg[id]
    }
}

impl std::ops::Index<InstructionId> for FunctionBuilder {
    type Output = Instruction;

    fn index(&self, id: InstructionId) -> &Self::Output {
        &self.current_function.dfg[id]
    }
}

impl std::ops::Index<BasicBlockId> for FunctionBuilder {
    type Output = BasicBlock;

    fn index(&self, id: BasicBlockId) -> &Self::Output {
        &self.current_function.dfg[id]
    }
}

fn validate_numeric_type(typ: &NumericType) {
    match &typ {
        NumericType::Signed { bit_size } => match bit_size {
            8 | 16 | 32 | 64 | 128 => (),
            _ => {
                panic!(
                    "Invalid bit size for signed numeric type: {bit_size}. Expected one of 8, 16, 32, 64 or 128."
                );
            }
        },
        NumericType::Unsigned { bit_size } => match bit_size {
            1 | 8 | 16 | 32 | 64 | 128 => (),
            _ => {
                panic!(
                    "Invalid bit size for unsigned numeric type: {bit_size}. Expected one of 1, 8, 16, 32, 64, or 128."
                );
            }
        },
        _ => (),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use acvm::{FieldElement, acir::AcirField};

    use crate::ssa::ir::{
        instruction::{Endian, Intrinsic},
        map::Id,
        types::{NumericType, Type},
    };

    use super::FunctionBuilder;

    #[test]
    fn insert_constant_call() {
        // `bits` should be an array of constants [1, 1, 1, 0...] of length 8:
        // let x = 7;
        // let bits: [u1; 8] = x.to_le_bits();
        let func_id = Id::test_new(0);
        let mut builder = FunctionBuilder::new("func".into(), func_id);
        let one = builder.numeric_constant(FieldElement::one(), NumericType::bool());
        let zero = builder.numeric_constant(FieldElement::zero(), NumericType::bool());

        let to_bits_id = builder.import_intrinsic_id(Intrinsic::ToBits(Endian::Little));
        let input = builder.field_constant(FieldElement::from(7_u128));
        let length = builder.field_constant(FieldElement::from(8_u128));
        let result_types = vec![Type::Array(Arc::new(vec![Type::bool()]), 8)];
        let call_results =
            builder.insert_call(to_bits_id, vec![input, length], result_types).into_owned();

        let slice = builder.current_function.dfg.get_array_constant(call_results[0]).unwrap().0;
        assert_eq!(slice[0], one);
        assert_eq!(slice[1], one);
        assert_eq!(slice[2], one);
        assert_eq!(slice[3], zero);
    }
}
