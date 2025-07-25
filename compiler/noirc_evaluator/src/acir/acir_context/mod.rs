//! The `acir_context` module acts as a high-level wrapper around [`generated_acir`], maintaining a small amount
//! of type information from SSA in order to allow efficient ACIR-gen.
//!
//! The [`AcirContext`] struct defines how to translate SSA instructions into equivalent ACIR constructs. This
//! layer of ACIR-gen is aware of a small amount of SSA type information in order to ensure that the generated ACIR
//! is correct and for optimizations.
//!
//! [`AcirContext`] also tracks [`Expression`]s which have been simplified into a [`Witness`], or constant witnesses
//! allowing these to be reused later in the program.

use acvm::{
    BlackBoxFunctionSolver,
    acir::{
        AcirField, BlackBoxFunc,
        circuit::{
            AssertionPayload, ErrorSelector, ExpressionOrMemory, ExpressionWidth, Opcode,
            opcodes::{AcirFunctionId, BlockId, BlockType, MemOp},
        },
        native_types::{Expression, Witness},
    },
};
use fxhash::FxHashMap as HashMap;
use iter_extended::{try_vecmap, vecmap};
use noirc_errors::call_stack::{CallStack, CallStackHelper};
use num_bigint::BigUint;
use num_integer::Integer;
use std::{borrow::Cow, cmp::Ordering};

use crate::ssa::ir::{instruction::Endian, types::NumericType};
use crate::{
    ErrorType,
    errors::{InternalBug, InternalError, RuntimeError, SsaReport},
};

mod big_int;
mod black_box;
mod brillig_call;
mod generated_acir;

use super::{
    AcirDynamicArray, AcirValue,
    types::{AcirType, AcirVar},
};
use big_int::BigIntContext;

pub use generated_acir::GeneratedAcir;
pub(crate) use generated_acir::{BrilligStdLib, BrilligStdlibFunc};

#[derive(Debug, Default)]
/// Context object which holds the relationship between
/// `Variables`(AcirVar) and types such as `Expression` and `Witness`
/// which are placed into ACIR.
pub(crate) struct AcirContext<F: AcirField, B: BlackBoxFunctionSolver<F>> {
    pub(super) blackbox_solver: B,
    brillig_stdlib: BrilligStdLib<F>,

    vars: HashMap<AcirVar, AcirVarData<F>>,

    constant_witnesses: HashMap<F, Witness>,

    /// An in-memory representation of ACIR.
    ///
    /// This struct will progressively be populated
    /// based on the methods called.
    /// For example, If one was to add two Variables together,
    /// then the `acir_ir` will be populated to assert this
    /// addition.
    pub(super) acir_ir: GeneratedAcir<F>,

    /// The BigIntContext, used to generate identifiers for BigIntegers
    big_int_ctx: BigIntContext,

    expression_width: ExpressionWidth,

    pub(super) warnings: Vec<SsaReport>,
}

impl<F: AcirField, B: BlackBoxFunctionSolver<F>> AcirContext<F, B> {
    pub(super) fn new(brillig_stdlib: BrilligStdLib<F>, blackbox_solver: B) -> Self {
        AcirContext {
            brillig_stdlib,
            blackbox_solver,
            vars: Default::default(),
            constant_witnesses: Default::default(),
            acir_ir: Default::default(),
            big_int_ctx: Default::default(),
            expression_width: Default::default(),
            warnings: Default::default(),
        }
    }

    pub(crate) fn set_expression_width(&mut self, expression_width: ExpressionWidth) {
        self.expression_width = expression_width;
    }

    pub(crate) fn current_witness_index(&self) -> Witness {
        self.acir_ir.current_witness_index()
    }

    pub(crate) fn extract_witness(&self, inputs: &[AcirValue]) -> Vec<Witness> {
        inputs
            .iter()
            .flat_map(|value| value.clone().flatten())
            .map(|value| {
                self.vars
                    .get(&value.0)
                    .expect("ICE: undeclared AcirVar")
                    .to_expression()
                    .to_witness()
                    .expect("ICE - cannot extract a witness")
            })
            .collect()
    }

    /// Adds a constant to the context and assigns a Variable to represent it
    pub(crate) fn add_constant(&mut self, constant: impl Into<F>) -> AcirVar {
        let constant_data = AcirVarData::Const(constant.into());
        self.add_data(constant_data)
    }

    /// Returns the constant represented by the given variable.
    ///
    /// Panics: if the variable does not represent a constant.
    pub(crate) fn constant(&self, var: AcirVar) -> &F {
        self.vars[&var].as_constant().expect("ICE - expected the variable to be a constant value")
    }

    /// Adds a Variable to the context, whose exact value is resolved at
    /// runtime.
    pub(crate) fn add_variable(&mut self) -> AcirVar {
        let var_index = self.acir_ir.next_witness_index();

        let var_data = AcirVarData::Witness(var_index);

        self.add_data(var_data)
    }

    fn mark_variables_equivalent(
        &mut self,
        lhs: AcirVar,
        rhs: AcirVar,
    ) -> Result<(), InternalError> {
        if lhs == rhs {
            return Ok(());
        }

        let lhs_data = self.vars.remove(&lhs).ok_or_else(|| InternalError::UndeclaredAcirVar {
            call_stack: self.get_call_stack(),
        })?;
        let rhs_data = self.vars.remove(&rhs).ok_or_else(|| InternalError::UndeclaredAcirVar {
            call_stack: self.get_call_stack(),
        })?;

        let (new_lhs_data, new_rhs_data) = match (lhs_data, rhs_data) {
            // Always prefer a constant variable.
            (constant @ AcirVarData::Const(_), _) | (_, constant @ AcirVarData::Const(_)) => {
                (constant.clone(), constant)
            }

            // Replace any expressions with witnesses.
            (witness @ AcirVarData::Witness(_), AcirVarData::Expr(_))
            | (AcirVarData::Expr(_), witness @ AcirVarData::Witness(_)) => {
                (witness.clone(), witness)
            }

            // If both variables are witnesses then use the smaller of the two in future.
            (AcirVarData::Witness(lhs_witness), AcirVarData::Witness(rhs_witness)) => {
                let witness = AcirVarData::Witness(std::cmp::min(lhs_witness, rhs_witness));
                (witness.clone(), witness)
            }

            (AcirVarData::Expr(lhs_expr), AcirVarData::Expr(rhs_expr)) => {
                if lhs_expr.is_linear() && rhs_expr.is_linear() {
                    // If both expressions are linear, choose the one with the fewest terms.
                    let expr = if lhs_expr.linear_combinations.len()
                        <= rhs_expr.linear_combinations.len()
                    {
                        lhs_expr
                    } else {
                        rhs_expr
                    };

                    let expr = AcirVarData::Expr(expr);
                    (expr.clone(), expr)
                } else {
                    (AcirVarData::Expr(lhs_expr), AcirVarData::Expr(rhs_expr))
                }
            }
        };

        self.vars.insert(lhs, new_lhs_data);
        self.vars.insert(rhs, new_rhs_data);

        Ok(())
    }

    pub(crate) fn get_call_stack(&self) -> CallStack {
        self.acir_ir.call_stacks.get_call_stack(self.acir_ir.call_stack_id)
    }

    pub(crate) fn set_call_stack(&mut self, call_stack: CallStack) {
        self.acir_ir.call_stack_id = self.acir_ir.call_stacks.get_or_insert_locations(&call_stack);
    }

    pub(crate) fn set_call_stack_helper(&mut self, call_stack: CallStackHelper) {
        self.acir_ir.call_stacks = call_stack;
    }

    pub(crate) fn get_or_create_witness_var(
        &mut self,
        var: AcirVar,
    ) -> Result<AcirVar, InternalError> {
        if self.var_to_expression(var)?.to_witness().is_some() {
            // If called with a variable which is already a witness then return the same variable.
            return Ok(var);
        }

        let var_as_witness = self.var_to_witness(var)?;

        let witness_var = self.add_data(AcirVarData::Witness(var_as_witness));
        self.mark_variables_equivalent(var, witness_var)?;

        Ok(witness_var)
    }

    /// Converts an [`AcirVar`] to a [`Witness`]
    pub(crate) fn var_to_witness(&mut self, var: AcirVar) -> Result<Witness, InternalError> {
        let expression = self.var_to_expression(var)?;
        let witness = if let Some(constant) = expression.to_const() {
            // Check if a witness has been assigned this value already, if so reuse it.
            *self
                .constant_witnesses
                .entry(*constant)
                .or_insert_with(|| self.acir_ir.get_or_create_witness(&expression))
        } else {
            self.acir_ir.get_or_create_witness(&expression)
        };
        Ok(witness)
    }

    /// Converts an [`AcirVar`] to an [`Expression`]
    pub(crate) fn var_to_expression(&self, var: AcirVar) -> Result<Expression<F>, InternalError> {
        let var_data = match self.vars.get(&var) {
            Some(var_data) => var_data,
            None => {
                return Err(InternalError::UndeclaredAcirVar { call_stack: self.get_call_stack() });
            }
        };
        Ok(var_data.to_expression().into_owned())
    }

    /// True if the given AcirVar refers to a constant one value
    pub(crate) fn is_constant_one(&self, var: &AcirVar) -> bool {
        match self.vars[var] {
            AcirVarData::Const(field) => field.is_one(),
            _ => false,
        }
    }

    /// True if the given AcirVar refers to a constant value
    pub(crate) fn is_constant(&self, var: &AcirVar) -> bool {
        matches!(self.vars[var], AcirVarData::Const(_))
    }

    /// Adds a new Variable to context whose value will
    /// be constrained to be the negation of `var`.
    ///
    /// Note: `Variables` are immutable.
    pub(crate) fn neg_var(&mut self, var: AcirVar) -> AcirVar {
        let var_data = &self.vars[&var];
        let result_data = if let AcirVarData::Const(constant) = var_data {
            AcirVarData::Const(-*constant)
        } else {
            AcirVarData::Expr(-var_data.to_expression().as_ref())
        };
        self.add_data(result_data)
    }

    /// Adds a new Variable to context whose value will
    /// be constrained to be the inverse of `var`.
    pub(crate) fn inv_var(
        &mut self,
        var: AcirVar,
        predicate: AcirVar,
    ) -> Result<AcirVar, RuntimeError> {
        let var_data = &self.vars[&var];
        if let AcirVarData::Const(constant) = var_data {
            // Note that this will return a 0 if the inverse is not available
            let inverted_var = self.add_data(AcirVarData::Const(constant.inverse()));

            // Check that the inverted var is valid.
            // This check prevents invalid divisions by zero.
            let should_be_one = self.mul_var(inverted_var, var)?;
            self.maybe_eq_predicate(should_be_one, predicate)?;

            return Ok(inverted_var);
        }

        let results = self.stdlib_brillig_call(
            predicate,
            BrilligStdlibFunc::Inverse,
            &self.brillig_stdlib.get_code(BrilligStdlibFunc::Inverse).clone(),
            vec![AcirValue::Var(var, AcirType::field())],
            vec![AcirType::field()],
            true,
        )?;
        let inverted_var = Self::expect_one_var(results);

        // Check that the inverted var is valid.
        // This check prevents invalid divisions by zero.
        let should_be_one = self.mul_var(inverted_var, var)?;
        self.maybe_eq_predicate(should_be_one, predicate)?;

        Ok(inverted_var)
    }

    // Constrains `var` to be equal to predicate if the predicate is true
    // or to be equal to 0 if the predicate is false.
    //
    // Since we multiply `var` by the predicate, this is a no-op if the predicate is false
    pub(crate) fn maybe_eq_predicate(
        &mut self,
        var: AcirVar,
        predicate: AcirVar,
    ) -> Result<(), RuntimeError> {
        let pred_mul_var = self.mul_var(var, predicate)?;
        self.assert_eq_var(pred_mul_var, predicate, None)
    }

    // Returns the variable from the results, assuming it is the only result
    fn expect_one_var(results: Vec<AcirValue>) -> AcirVar {
        assert_eq!(results.len(), 1);
        match results[0] {
            AcirValue::Var(var, _) => var,
            AcirValue::DynamicArray(_) | AcirValue::Array(_) => {
                unreachable!("ICE - expected a variable")
            }
        }
    }

    /// Returns an `AcirVar` that is `1` if `lhs` equals `rhs` and
    /// 0 otherwise.
    pub(crate) fn eq_var(&mut self, lhs: AcirVar, rhs: AcirVar) -> Result<AcirVar, RuntimeError> {
        let lhs_expr = self.var_to_expression(lhs)?;
        let rhs_expr = self.var_to_expression(rhs)?;

        // `lhs == rhs` => `lhs - rhs == 0`
        let diff_expr = &lhs_expr - &rhs_expr;

        // Check to see if equality can be determined at compile-time.
        if diff_expr.is_const() {
            return Ok(self.add_constant(diff_expr.is_zero()));
        }

        let is_equal_witness = self.acir_ir.is_equal(&lhs_expr, &rhs_expr);
        let result_var = self.add_data(AcirVarData::Witness(is_equal_witness));
        Ok(result_var)
    }

    /// Returns an `AcirVar` that is the XOR result of `lhs` & `rhs`.
    pub(crate) fn xor_var(
        &mut self,
        lhs: AcirVar,
        rhs: AcirVar,
        typ: AcirType,
    ) -> Result<AcirVar, RuntimeError> {
        let lhs_expr = self.var_to_expression(lhs)?;
        let rhs_expr = self.var_to_expression(rhs)?;

        if lhs_expr == rhs_expr {
            // x ^ x == 0
            let zero = self.add_constant(F::zero());
            return Ok(zero);
        } else if lhs_expr.is_zero() {
            // 0 ^ x == x
            return Ok(rhs);
        } else if rhs_expr.is_zero() {
            // x ^ 0 == x
            return Ok(lhs);
        }

        let bit_size = typ.bit_size::<F>();
        if bit_size == 1 {
            // Operands are booleans.
            //
            // a ^ b == a + b - 2*a*b
            let prod = self.mul_var(lhs, rhs)?;
            let sum = self.add_var(lhs, rhs)?;
            self.add_mul_var(sum, -F::from(2_u128), prod)
        } else {
            let inputs = vec![AcirValue::Var(lhs, typ.clone()), AcirValue::Var(rhs, typ)];
            let outputs = self.black_box_function(BlackBoxFunc::XOR, inputs, 1)?;
            Ok(outputs[0])
        }
    }

    /// Returns an `AcirVar` that is the AND result of `lhs` & `rhs`.
    pub(crate) fn and_var(
        &mut self,
        lhs: AcirVar,
        rhs: AcirVar,
        typ: AcirType,
    ) -> Result<AcirVar, RuntimeError> {
        let lhs_expr = self.var_to_expression(lhs)?;
        let rhs_expr = self.var_to_expression(rhs)?;

        if lhs_expr == rhs_expr {
            // x & x == x
            return Ok(lhs);
        } else if lhs_expr.is_zero() || rhs_expr.is_zero() {
            // x & 0 == 0 and 0 & x == 0
            let zero = self.add_constant(F::zero());
            return Ok(zero);
        }

        let bit_size = typ.bit_size::<F>();
        if bit_size == 1 {
            // Operands are booleans.
            self.mul_var(lhs, rhs)
        } else {
            let inputs = vec![AcirValue::Var(lhs, typ.clone()), AcirValue::Var(rhs, typ)];
            let outputs = self.black_box_function(BlackBoxFunc::AND, inputs, 1)?;
            Ok(outputs[0])
        }
    }

    /// Returns an `AcirVar` that is the OR result of `lhs` & `rhs`.
    pub(crate) fn or_var(
        &mut self,
        lhs: AcirVar,
        rhs: AcirVar,
        typ: AcirType,
    ) -> Result<AcirVar, RuntimeError> {
        let lhs_expr = self.var_to_expression(lhs)?;
        let rhs_expr = self.var_to_expression(rhs)?;
        if lhs_expr.is_zero() {
            // 0 | x == x
            return Ok(rhs);
        } else if rhs_expr.is_zero() {
            // x | 0 == x
            return Ok(lhs);
        }

        let bit_size = typ.bit_size::<F>();
        if bit_size == 1 {
            // Operands are booleans
            // a + b - ab
            let mul = self.mul_var(lhs, rhs)?;
            let sum = self.add_var(lhs, rhs)?;
            self.sub_var(sum, mul)
        } else {
            // Implement OR in terms of AND
            // (NOT a) AND (NOT b) => NOT (a OR b)
            let a = self.not_var(lhs, typ.clone())?;
            let b = self.not_var(rhs, typ.clone())?;
            let a_and_b = self.and_var(a, b, typ.clone())?;
            self.not_var(a_and_b, typ)
        }
    }

    /// Constrains the `lhs` and `rhs` to be equal.
    pub(crate) fn assert_eq_var(
        &mut self,
        lhs: AcirVar,
        rhs: AcirVar,
        assert_message: Option<AssertionPayload<F>>,
    ) -> Result<(), RuntimeError> {
        let lhs_expr = self.var_to_expression(lhs)?;
        let rhs_expr = self.var_to_expression(rhs)?;

        // `lhs == rhs` => `lhs - rhs == 0`
        let diff_expr = &lhs_expr - &rhs_expr;

        // Check to see if equality can be determined at compile-time.
        if diff_expr.is_zero() {
            // Constraint is always true - assertion is unnecessary.
            self.mark_variables_equivalent(lhs, rhs)?;
            return Ok(());
        }
        if diff_expr.is_const() {
            // Constraint is always false
            let message = self.get_assertion_payload_message(assert_message.as_ref());
            let call_stack = self.get_call_stack();
            self.warnings.push(SsaReport::Bug(InternalBug::AssertFailed { call_stack, message }));
        }

        self.acir_ir.assert_is_zero(diff_expr);
        if let Some(payload) = assert_message {
            self.acir_ir
                .assertion_payloads
                .insert(self.acir_ir.last_acir_opcode_location(), payload);
        }
        self.mark_variables_equivalent(lhs, rhs)?;

        Ok(())
    }

    /// Returns Some(String) if the assertion message is present and it refers to a static string.
    fn get_assertion_payload_message(
        &self,
        assert_message: Option<&AssertionPayload<F>>,
    ) -> Option<String> {
        assert_message.as_ref().and_then(|assertion_payload| {
            if let Some(ErrorType::String(message)) =
                self.acir_ir.error_types.get(&ErrorSelector::new(assertion_payload.error_selector))
            {
                Some(message.to_string())
            } else {
                None
            }
        })
    }

    /// Constrains the `lhs` and `rhs` to be non-equal.
    ///
    /// This is done by asserting the existence of an inverse for the value `lhs - rhs`.
    /// The constraint `(lhs - rhs) * inverse == 1` will only be satisfiable if `lhs` and `rhs` are non-equal.
    pub(crate) fn assert_neq_var(
        &mut self,
        lhs: AcirVar,
        rhs: AcirVar,
        predicate: AcirVar,
        assert_message: Option<AssertionPayload<F>>,
    ) -> Result<(), RuntimeError> {
        let diff_var = self.sub_var(lhs, rhs)?;

        let old_opcodes_len = self.acir_ir.opcodes().len();
        let _ = self.inv_var(diff_var, predicate)?;
        if let Some(payload) = assert_message {
            // Non-equality can potentially be a no-op if we have all constant
            // inputs that we know satisfy the non-equality check.
            // If a no-op non-equality check were to then add an assertion payload
            // opcode location based upon `GeneratedAcir::last_acir_opcode_location`,
            // it would be pointing at the previous opcode location.
            // This at best leads to a mismatch in assertion payload opcode locations
            // and at worst an attempt to subtract with overflow if the non-equality
            // check is the first opcode.
            if self.acir_ir.opcodes().len() - old_opcodes_len == 0 {
                return Ok(());
            }
            self.acir_ir
                .assertion_payloads
                .insert(self.acir_ir.last_acir_opcode_location(), payload);
        }

        Ok(())
    }

    pub(crate) fn vars_to_expressions_or_memory(
        &self,
        values: &[AcirValue],
    ) -> Result<Vec<ExpressionOrMemory<F>>, RuntimeError> {
        let mut result = Vec::with_capacity(values.len());
        for value in values {
            match value {
                AcirValue::Var(var, _) => {
                    result.push(ExpressionOrMemory::Expression(self.var_to_expression(*var)?));
                }
                AcirValue::Array(vars) => {
                    let vars_as_vec: Vec<_> = vars.iter().cloned().collect();
                    result.extend(self.vars_to_expressions_or_memory(&vars_as_vec)?);
                }
                AcirValue::DynamicArray(AcirDynamicArray { block_id, .. }) => {
                    result.push(ExpressionOrMemory::Memory(*block_id));
                }
            }
        }
        Ok(result)
    }

    /// Adds a new Variable to context whose value will
    /// be constrained to be the division of `lhs` and `rhs`
    pub(crate) fn div_var(
        &mut self,
        lhs: AcirVar,
        rhs: AcirVar,
        typ: AcirType,
        predicate: AcirVar,
    ) -> Result<AcirVar, RuntimeError> {
        let numeric_type = match typ {
            AcirType::NumericType(numeric_type) => numeric_type,
            AcirType::Array(_, _) => {
                unreachable!("cannot divide arrays. This should have been caught by the frontend")
            }
        };
        match numeric_type {
            NumericType::NativeField => {
                let inv_rhs = self.inv_var(rhs, predicate)?;
                self.mul_var(lhs, inv_rhs)
            }
            NumericType::Unsigned { bit_size } => {
                let (quotient_var, _remainder_var) =
                    self.euclidean_division_var(lhs, rhs, bit_size, predicate)?;
                Ok(quotient_var)
            }
            NumericType::Signed { bit_size } => {
                let (quotient_var, _remainder_var) =
                    self.signed_division_var(lhs, rhs, bit_size, predicate)?;
                Ok(quotient_var)
            }
        }
    }

    /// Adds a new Variable to context whose value will
    /// be constrained to be the multiplication of `lhs` and `rhs`
    pub(crate) fn mul_var(&mut self, lhs: AcirVar, rhs: AcirVar) -> Result<AcirVar, RuntimeError> {
        let lhs_data = self.vars[&lhs].clone();
        let rhs_data = self.vars[&rhs].clone();

        let result = match (lhs_data, rhs_data) {
            // (x * 1) == (1 * x) == x
            (AcirVarData::Const(constant), _) if constant.is_one() => rhs,
            (_, AcirVarData::Const(constant)) if constant.is_one() => lhs,

            // (x * 0) == (0 * x) == 0
            (AcirVarData::Const(constant), _) | (_, AcirVarData::Const(constant))
                if constant.is_zero() =>
            {
                self.add_constant(F::zero())
            }

            (AcirVarData::Const(lhs_constant), AcirVarData::Const(rhs_constant)) => {
                self.add_constant(lhs_constant * rhs_constant)
            }
            (AcirVarData::Witness(witness), AcirVarData::Const(constant))
            | (AcirVarData::Const(constant), AcirVarData::Witness(witness)) => {
                let mut expr = Expression::default();
                expr.push_addition_term(constant, witness);
                self.add_data(AcirVarData::from(expr))
            }
            (AcirVarData::Const(constant), AcirVarData::Expr(expr))
            | (AcirVarData::Expr(expr), AcirVarData::Const(constant)) => {
                self.add_data(AcirVarData::from(&expr * constant))
            }
            (AcirVarData::Witness(lhs_witness), AcirVarData::Witness(rhs_witness)) => {
                let mut expr = Expression::default();
                expr.push_multiplication_term(F::one(), lhs_witness, rhs_witness);
                self.add_data(AcirVarData::Expr(expr))
            }
            (AcirVarData::Expr(expression), AcirVarData::Witness(witness))
            | (AcirVarData::Witness(witness), AcirVarData::Expr(expression))
                if expression.is_linear() =>
            {
                let mut expr = Expression::default();
                for term in expression.linear_combinations.iter() {
                    expr.push_multiplication_term(term.0, term.1, witness);
                }
                expr.push_addition_term(expression.q_c, witness);
                self.add_data(AcirVarData::Expr(expr))
            }
            (AcirVarData::Expr(lhs_expr), AcirVarData::Expr(rhs_expr)) => {
                let degree_one = if lhs_expr.is_linear() && rhs_expr.is_degree_one_univariate() {
                    Some((lhs_expr, rhs_expr))
                } else if rhs_expr.is_linear() && lhs_expr.is_degree_one_univariate() {
                    Some((rhs_expr, lhs_expr))
                } else {
                    None
                };
                if let Some((lin, univariate)) = degree_one {
                    let mut expr = Expression::default();
                    let rhs_term = univariate.linear_combinations[0];
                    for term in lin.linear_combinations.iter() {
                        expr.push_multiplication_term(term.0 * rhs_term.0, term.1, rhs_term.1);
                    }
                    expr.push_addition_term(lin.q_c * rhs_term.0, rhs_term.1);
                    expr.sort();
                    expr = expr.add_mul(univariate.q_c, &lin);
                    self.add_data(AcirVarData::Expr(expr))
                } else {
                    let lhs = self.get_or_create_witness_var(lhs)?;
                    let rhs = self.get_or_create_witness_var(rhs)?;
                    self.mul_var(lhs, rhs)?
                }
            }
            _ => {
                let lhs = self.get_or_create_witness_var(lhs)?;
                let rhs = self.get_or_create_witness_var(rhs)?;
                self.mul_var(lhs, rhs)?
            }
        };

        Ok(result)
    }

    /// Adds a new Variable to context whose value will
    /// be constrained to be the subtraction of `lhs` and `rhs`
    pub(crate) fn sub_var(&mut self, lhs: AcirVar, rhs: AcirVar) -> Result<AcirVar, RuntimeError> {
        let neg_rhs = self.neg_var(rhs);
        self.add_var(lhs, neg_rhs)
    }

    /// Adds a new Variable to context whose value will
    /// be constrained to be the addition of `lhs` and `rhs`
    pub(crate) fn add_var(&mut self, lhs: AcirVar, rhs: AcirVar) -> Result<AcirVar, RuntimeError> {
        let lhs_expr = self.var_to_expression(lhs)?;
        let rhs_expr = self.var_to_expression(rhs)?;

        let sum_expr = &lhs_expr + &rhs_expr;
        if fits_in_one_identity(&sum_expr, self.expression_width) {
            let sum_var = self.add_data(AcirVarData::from(sum_expr));

            return Ok(sum_var);
        }

        let sum_expr = match lhs_expr.width().cmp(&rhs_expr.width()) {
            Ordering::Greater => {
                let lhs_witness_var = self.get_or_create_witness_var(lhs)?;
                let lhs_witness_expr = self.var_to_expression(lhs_witness_var)?;

                let new_sum_expr = &lhs_witness_expr + &rhs_expr;
                if fits_in_one_identity(&new_sum_expr, self.expression_width) {
                    new_sum_expr
                } else {
                    let rhs_witness_var = self.get_or_create_witness_var(rhs)?;
                    let rhs_witness_expr = self.var_to_expression(rhs_witness_var)?;

                    &lhs_expr + &rhs_witness_expr
                }
            }
            Ordering::Less => {
                let rhs_witness_var = self.get_or_create_witness_var(rhs)?;
                let rhs_witness_expr = self.var_to_expression(rhs_witness_var)?;

                let new_sum_expr = &lhs_expr + &rhs_witness_expr;
                if fits_in_one_identity(&new_sum_expr, self.expression_width) {
                    new_sum_expr
                } else {
                    let lhs_witness_var = self.get_or_create_witness_var(lhs)?;
                    let lhs_witness_expr = self.var_to_expression(lhs_witness_var)?;

                    &lhs_witness_expr + &rhs_expr
                }
            }
            Ordering::Equal => {
                let lhs_witness_var = self.get_or_create_witness_var(lhs)?;
                let lhs_witness_expr = self.var_to_expression(lhs_witness_var)?;

                let new_sum_expr = &lhs_witness_expr + &rhs_expr;
                if fits_in_one_identity(&new_sum_expr, self.expression_width) {
                    new_sum_expr
                } else {
                    let rhs_witness_var = self.get_or_create_witness_var(rhs)?;
                    let rhs_witness_expr = self.var_to_expression(rhs_witness_var)?;

                    &lhs_witness_expr + &rhs_witness_expr
                }
            }
        };

        let sum_var = self.add_data(AcirVarData::from(sum_expr));

        Ok(sum_var)
    }

    /// Adds a new Variable to context whose value will
    /// be constrained to be the expression `lhs + k * rhs`
    fn add_mul_var(&mut self, lhs: AcirVar, k: F, rhs: AcirVar) -> Result<AcirVar, RuntimeError> {
        let k_var = self.add_constant(k);

        let intermediate = self.mul_var(k_var, rhs)?;
        self.add_var(lhs, intermediate)
    }

    /// Adds a new variable that is constrained to be the logical NOT of `x`.
    pub(crate) fn not_var(&mut self, x: AcirVar, typ: AcirType) -> Result<AcirVar, RuntimeError> {
        let bit_size = typ.bit_size::<F>();
        // Subtracting from max flips the bits
        let max = power_of_two::<F>(bit_size) - F::one();
        let max = self.add_constant(max);
        self.sub_var(max, x)
    }

    /// Returns the quotient and remainder such that lhs = rhs * quotient + remainder
    fn euclidean_division_var(
        &mut self,
        lhs: AcirVar,
        rhs: AcirVar,
        bit_size: u32,
        predicate: AcirVar,
    ) -> Result<(AcirVar, AcirVar), RuntimeError> {
        let zero = self.add_constant(F::zero());
        let one = self.add_constant(F::one());

        let lhs_expr = self.var_to_expression(lhs)?;
        let rhs_expr = self.var_to_expression(rhs)?;
        let predicate_expr = self.var_to_expression(predicate)?;

        match (lhs_expr.to_const(), rhs_expr.to_const(), predicate_expr.to_const()) {
            // If predicate is zero, `quotient_var` and `remainder_var` will be 0.
            (_, _, Some(predicate_const)) if predicate_const.is_zero() => {
                return Ok((zero, zero));
            }

            // If `lhs` and `rhs` are known constants then we can calculate the result at compile time.
            // `rhs` must be non-zero.
            (Some(lhs_const), Some(rhs_const), _) if !rhs_const.is_zero() => {
                if lhs_const.num_bits() <= 128 && rhs_const.num_bits() <= 128 {
                    let lhs_const = lhs_const.to_u128();
                    let rhs_const = rhs_const.to_u128();
                    let quotient = lhs_const / rhs_const;
                    let remainder = lhs_const - quotient * rhs_const;

                    let quotient_var = self.add_constant(quotient);
                    let remainder_var = self.add_constant(remainder);
                    return Ok((quotient_var, remainder_var));
                } else {
                    // If we're truncating to 128 bits, the RHS will be 2**128, which is 1 followed by 128 zeros;
                    // when converted to u128, it becomes all zero.
                    let lhs_const = BigUint::from_bytes_be(&lhs_const.to_be_bytes());
                    let rhs_const = BigUint::from_bytes_be(&rhs_const.to_be_bytes());
                    let (quotient, remainder) = lhs_const.div_rem(&rhs_const);
                    let quotient = F::from_be_bytes_reduce(&quotient.to_bytes_be());
                    let remainder = F::from_be_bytes_reduce(&remainder.to_bytes_be());

                    let quotient_var = self.add_constant(quotient);
                    let remainder_var = self.add_constant(remainder);
                    return Ok((quotient_var, remainder_var));
                };
            }

            // If `rhs` is one then the division is a noop.
            (_, Some(rhs_const), _) if rhs_const.is_one() => {
                return Ok((lhs, zero));
            }

            // After this point, we cannot perform the division at compile-time.
            //
            // We need to check that the rhs is not zero, otherwise when executing the brillig quotient,
            // we may attempt to divide by zero and cause a VM panic.
            //
            // When the predicate is 0, the division always succeeds (as it is skipped).
            // When the predicate is 1, the rhs must not be 0.

            // If the predicate is known to be active, we simply assert that an inverse must exist.
            // This implies that `rhs != 0`.
            (_, _, Some(predicate_const)) if predicate_const.is_one() => {
                let _inverse = self.inv_var(rhs, one)?;
            }

            // Otherwise we must handle both potential cases.
            _ => {
                let rhs_is_zero = self.eq_var(rhs, zero)?;
                let rhs_is_zero_and_predicate_active = self.mul_var(rhs_is_zero, predicate)?;
                self.assert_eq_var(rhs_is_zero_and_predicate_active, zero, None)?;
            }
        }

        // maximum bit size for q and for [r and rhs]
        let (max_q_bits, max_rhs_bits) = if let Some(rhs_const) = rhs_expr.to_const() {
            // when rhs is constant, we can better estimate the maximum bit sizes
            let max_rhs_bits = rhs_const.num_bits();

            // It is possible that we have an AcirVar which is a result of a multiplication of constants
            // which resulted in an overflow, but that check will only happen at runtime, and here we
            // can't assume that the RHS will never have more bits than the operand.
            // Alternatively if the RHS is a result of an underflow, it could be a negative number which
            // is represented by a very large positive Field, which could fail to compile to ACIR in
            // `range_constrain_var` below, because it can use all 254 bits.

            // To avoid any uncertainty about how the rest of the calls would behave if we pretended that we
            // didn't know that the RHS has more bits than the operation assumes, we return zero and add an
            // assertion which will fail at runtime.
            if max_rhs_bits > bit_size {
                let msg = format!(
                    "attempted to divide by constant larger than operand type: {max_rhs_bits} > {bit_size}"
                );
                let msg = self.generate_assertion_message_payload(msg);
                self.assert_eq_var(zero, one, Some(msg))?;
                return Ok((zero, zero));
            }
            (bit_size - max_rhs_bits + 1, max_rhs_bits)
        } else {
            (bit_size, bit_size)
        };

        let [q_value, r_value]: [AcirValue; 2] = self
            .stdlib_brillig_call(
                predicate,
                BrilligStdlibFunc::Quotient,
                &self.brillig_stdlib.get_code(BrilligStdlibFunc::Quotient).clone(),
                vec![
                    AcirValue::Var(lhs, AcirType::unsigned(bit_size)),
                    AcirValue::Var(rhs, AcirType::unsigned(bit_size)),
                ],
                vec![AcirType::unsigned(max_q_bits), AcirType::unsigned(max_rhs_bits)],
                true,
            )?
            .try_into()
            .expect("quotient only returns two values");
        let quotient_var = q_value.into_var()?;
        let remainder_var = r_value.into_var()?;

        // Constrain `q < 2^{max_q_bits}`.
        //
        // We do not need to use a predicate in the range constraint because
        // `quotient_var` is the output of a brillig call
        self.range_constrain_var(
            quotient_var,
            &NumericType::Unsigned { bit_size: max_q_bits },
            None,
            one,
        )?;

        // Constrain `r < 2^{max_rhs_bits}`.
        //
        // If `rhs` is a power of 2, then is just a looser version of the following bound constraint.
        // In the case where `rhs` isn't a power of 2 then this range constraint is required
        // as the bound constraint creates a new witness.
        // This opcode will be optimized out if it is redundant so we always add it for safety.
        // Furthermore, we do not need to use a predicate in the range constraint because
        // the remainder is the output of a brillig call
        self.range_constrain_var(
            remainder_var,
            &NumericType::Unsigned { bit_size: max_rhs_bits },
            None,
            one,
        )?;

        // Constrain `r < rhs`.
        //
        // `rhs` has the correct bit-size because either it is enforced by the overflow checks
        // or `rhs` is zero when the overflow checks are disabled.
        // Indeed, in that case, rhs is replaced with 'predicate * rhs'
        //
        // Using the predicate as an offset is a small optimization:
        // * if the predicate is true, then the offset is one and this assert that 'r<rhs',
        //   without using a predicate (because 'one' is given for the predicate argument).
        // * if the predicate is false, then this will assert 'r<=rhs',
        //   which allows an extra value for `r` that doesn't make mathematical sense (r==rhs would in itself be invalid),
        //   however this constraint is still more restrictive than if we passed `one` for offset and `predicate` in the last position,
        //   because when the predicate is false, that would have asserted nothing, and accepted anything at all.
        self.bound_constraint_with_offset(remainder_var, rhs, predicate, max_rhs_bits, one)?;

        // a * predicate == (b * q + r) * predicate
        // => predicate * (a - b * q - r) == 0
        // When the predicate is 0, the equation always passes.
        // When the predicate is 1, the euclidean division needs to be
        // true.
        let rhs_constraint = self.mul_var(rhs, quotient_var)?;
        let rhs_constraint = self.add_var(rhs_constraint, remainder_var)?;
        let rhs_constraint = self.mul_var(rhs_constraint, predicate)?;

        let lhs_constraint = self.mul_var(lhs, predicate)?;
        self.assert_eq_var(lhs_constraint, rhs_constraint, None)?;

        // Avoids overflow: 'q*b+r < 2^max_q_bits*2^max_rhs_bits'
        if max_q_bits + max_rhs_bits >= F::max_num_bits() - 1 {
            // q*b+r can overflow; we avoid this when b is constant
            if let Some(rhs_const) = rhs_expr.to_const() {
                // we compute q0 = p/rhs
                let rhs_big = BigUint::from_bytes_be(&rhs_const.to_be_bytes());
                let q0_big = F::modulus() / &rhs_big;
                let q0 = F::from_be_bytes_reduce(&q0_big.to_bytes_be());
                let q0_var = self.add_constant(q0);
                // ensure that q <= q0
                // We do not need to use a predicate in the range constraint because
                // quotient_var is the output of a brillig call
                self.bound_constraint_with_offset(quotient_var, q0_var, zero, max_q_bits, one)?;

                // when q == q0, q*b+r can overflow so we need to bound r to avoid the overflow.
                let size_predicate = self.eq_var(q0_var, quotient_var)?;
                let predicate = self.mul_var(size_predicate, predicate)?;
                // Ensure that there is no overflow, under q == q0 predicate
                let max_r_big = F::modulus() - q0_big * rhs_big;
                let max_r = F::from_be_bytes_reduce(&max_r_big.to_bytes_be());
                let max_r_var = self.add_constant(max_r);

                // Bound the remainder to be <p-q0*b, if the predicate is true,
                // that is, if q0 == q then assert(r < max_r), where is max_r = p-q0*b, and q0 = p/b, so that q*b+r<p
                self.bound_constraint_with_offset(
                    remainder_var,
                    max_r_var,
                    one,
                    rhs_const.num_bits(),
                    predicate,
                )?;
            } else if bit_size == 128 {
                // q and b are u128 and q*b could overflow so we check that either q or b are less than 2^64
                let two_pow_64: F = power_of_two(64);
                let two_pow_64 = self.add_constant(two_pow_64);

                let (q_upper, _) =
                    self.euclidean_division_var(quotient_var, two_pow_64, bit_size, predicate)?;
                let (rhs_upper, _) =
                    self.euclidean_division_var(rhs, two_pow_64, bit_size, predicate)?;
                let mul_uppers = self.mul_var(q_upper, rhs_upper)?;
                self.assert_eq_var(mul_uppers, zero, None)?;
            } else {
                // we do not support unbounded division
                unreachable!("overflow in unbounded division");
            }
        }

        Ok((quotient_var, remainder_var))
    }

    /// Generate constraints that are satisfied iff
    /// lhs < rhs , when offset is 1, or
    /// lhs <= rhs, when offset is 0
    /// bits is the bit size of a and b (or an upper bound of the bit size)
    /// predicate is assumed to be 0 or 1, and when it is 0, the generated constrains will never fail.
    ///
    /// lhs<=rhs is done by constraining b-a to a bit size of 'bits':
    /// if lhs<=rhs, 0 <= rhs-lhs <= b < 2^bits
    /// if lhs>rhs, rhs-lhs = p+rhs-lhs > p-2^bits >= 2^bits  (if log(p) >= bits + 1)
    /// n.b: we do NOT check here that lhs and rhs are indeed 'bits' size
    /// lhs < rhs <=> a+1<=b
    /// TODO: Consolidate this with bounds_check function.
    pub(super) fn bound_constraint_with_offset(
        &mut self,
        lhs: AcirVar,
        rhs: AcirVar,
        offset: AcirVar,
        bits: u32,
        predicate: AcirVar,
    ) -> Result<(), RuntimeError> {
        #[allow(unused_qualifications)]
        const fn num_bits<T>() -> usize {
            std::mem::size_of::<T>() * 8
        }

        fn bit_size_u128(a: u128) -> u32 where {
            num_bits::<u128>() as u32 - a.leading_zeros()
        }

        assert!(
            bits < F::max_num_bits(),
            "range check with bit size of the prime field is not implemented yet"
        );

        let mut lhs_offset = self.add_var(lhs, offset)?;

        // Optimization when rhs is const and fits within a u128
        let rhs_expr = self.var_to_expression(rhs)?;
        // Do not attempt this optimization when the q_c is zero as otherwise
        // we will compute an rhs offset of zero and ultimately lay down a range constrain of zero bits
        // which will always fail.
        if rhs_expr.is_const() && rhs_expr.q_c.num_bits() <= 128 && !rhs_expr.q_c.is_zero() {
            // We try to move the offset to rhs
            let rhs_offset = if self.is_constant_one(&offset) && rhs_expr.q_c.to_u128() >= 1 {
                lhs_offset = lhs;
                rhs_expr.q_c.to_u128() - 1
            } else {
                rhs_expr.q_c.to_u128()
            };
            // we now have lhs+offset <= rhs <=> lhs_offset <= rhs_offset

            let bit_size = bit_size_u128(rhs_offset);
            // r = 2^bit_size - rhs_offset -1, is of bit size  'bit_size' by construction
            let two_pow_bit_size_minus_one =
                if bit_size == 128 { u128::MAX } else { (1_u128 << bit_size) - 1 };
            let r = two_pow_bit_size_minus_one - rhs_offset;
            // however, since it is a constant, we can compute it's actual bit size
            let r_bit_size = bit_size_u128(r);

            //we need to ensure lhs_offset + r does not overflow
            if bits + r_bit_size < F::max_num_bits() {
                // witness = lhs_offset + r
                let r_var = self.add_constant(r);
                let aor = self.add_var(lhs_offset, r_var)?;

                // lhs_offset<=rhs_offset <=> lhs_offset + r < rhs_offset + r = 2^bit_size <=> witness < 2^bit_size
                self.range_constrain_var(
                    aor,
                    &NumericType::Unsigned { bit_size },
                    None,
                    predicate,
                )?;
                return Ok(());
            }
        }
        // General case:  lhs_offset<=rhs <=> rhs-lhs_offset>=0 <=> rhs-lhs_offset is a 'bits' bit integer
        let sub_expression = self.sub_var(rhs, lhs_offset)?; //rhs-lhs_offset
        self.range_constrain_var(
            sub_expression,
            &NumericType::Unsigned { bit_size: bits },
            None,
            predicate,
        )?;

        Ok(())
    }

    // Returns the 2-complement of lhs, using the provided sign bit in 'leading'
    // if leading is zero, it returns lhs
    // if leading is one, it returns 2^bit_size-lhs
    fn two_complement(
        &mut self,
        lhs: AcirVar,
        leading: AcirVar,
        max_bit_size: u32,
    ) -> Result<AcirVar, RuntimeError> {
        let max_power_of_two = self.add_constant(power_of_two::<F>(max_bit_size - 1));

        let intermediate = self.sub_var(max_power_of_two, lhs)?;
        let intermediate = self.mul_var(intermediate, leading)?;

        self.add_mul_var(lhs, F::from(2_u128), intermediate)
    }

    /// Returns the quotient and remainder such that lhs = rhs * quotient + remainder
    /// and |remainder| < |rhs|
    /// and remainder has the same sign than lhs
    /// Note that this is not the euclidean division, where we have instead remainder < |rhs|
    fn signed_division_var(
        &mut self,
        lhs: AcirVar,
        rhs: AcirVar,
        bit_size: u32,
        predicate: AcirVar,
    ) -> Result<(AcirVar, AcirVar), RuntimeError> {
        // We derive the signed division from the unsigned euclidean division.
        // note that this is not euclidean division!
        // If `x` is a signed integer, then `sign(x)x >= 0`
        // so if `a` and `b` are signed integers, we can do the unsigned division:
        // `sign(a)a = q1*sign(b)b + r1`
        // => `a = sign(a)sign(b)q1*b + sign(a)r1`
        // => `a = qb+r`, with `|r|<|b|` and `a` and `r` have the same sign.

        assert_ne!(bit_size, 0, "signed integer should have at least one bit");

        // 2^{max_bit size-1}
        let max_power_of_two = self.add_constant(power_of_two::<F>(bit_size - 1));
        let zero = self.add_constant(F::zero());
        let one = self.add_constant(F::one());

        // Get the sign bit of rhs by computing rhs / max_power_of_two
        let (rhs_leading, _) = self.euclidean_division_var(rhs, max_power_of_two, bit_size, one)?;

        // Get the sign bit of lhs by computing lhs / max_power_of_two
        let (lhs_leading, _) = self.euclidean_division_var(lhs, max_power_of_two, bit_size, one)?;

        // Signed to unsigned:
        let unsigned_lhs = self.two_complement(lhs, lhs_leading, bit_size)?;
        let unsigned_rhs = self.two_complement(rhs, rhs_leading, bit_size)?;

        // Performs the division using the unsigned values of lhs and rhs
        let (q1, r1) =
            self.euclidean_division_var(unsigned_lhs, unsigned_rhs, bit_size, predicate)?;

        // Unsigned to signed: derive q and r from q1,r1 and the signs of lhs and rhs
        // Quotient sign is lhs sign * rhs sign, whose resulting sign bit is the XOR of the sign bits
        let q_sign = self.xor_var(lhs_leading, rhs_leading, AcirType::unsigned(1))?;
        let quotient = self.two_complement(q1, q_sign, bit_size)?;
        let remainder = self.two_complement(r1, lhs_leading, bit_size)?;

        // Issue #5129 - When q1 is zero and quotient sign is -1, we compute -0=2^{bit_size},
        // which is not valid because we do not wrap integer operations
        // Similar case can happen with the remainder.
        let q_is_0 = self.eq_var(q1, zero)?;
        let q_is_not_0 = self.not_var(q_is_0, AcirType::unsigned(1))?;
        let quotient = self.mul_var(quotient, q_is_not_0)?;
        let r_is_0 = self.eq_var(r1, zero)?;
        let r_is_not_0 = self.not_var(r_is_0, AcirType::unsigned(1))?;
        let remainder = self.mul_var(remainder, r_is_not_0)?;

        Ok((quotient, remainder))
    }

    /// Returns a variable which is constrained to be `lhs mod rhs`
    pub(crate) fn modulo_var(
        &mut self,
        lhs: AcirVar,
        rhs: AcirVar,
        typ: AcirType,
        bit_size: u32,
        predicate: AcirVar,
    ) -> Result<AcirVar, RuntimeError> {
        let numeric_type = match typ {
            AcirType::NumericType(numeric_type) => numeric_type,
            AcirType::Array(_, _) => {
                unreachable!("cannot modulo arrays. This should have been caught by the frontend")
            }
        };

        let (_, remainder_var) = match numeric_type {
            NumericType::Signed { bit_size } => {
                self.signed_division_var(lhs, rhs, bit_size, predicate)?
            }
            _ => self.euclidean_division_var(lhs, rhs, bit_size, predicate)?,
        };
        Ok(remainder_var)
    }

    /// Constrains the `AcirVar` variable to be of type `NumericType`.
    ///
    /// If `predicate` is false, the constrain will never fail.
    pub(crate) fn range_constrain_var(
        &mut self,
        variable: AcirVar,
        numeric_type: &NumericType,
        message: Option<String>,
        predicate: AcirVar,
    ) -> Result<AcirVar, RuntimeError> {
        match numeric_type {
            NumericType::Signed { bit_size } | NumericType::Unsigned { bit_size } => {
                // If `variable` is constant then we don't need to add a constraint.
                // We _do_ add a constraint if `variable` would fail the range check however so that we throw an error.
                if let Some(constant) = self.var_to_expression(variable)?.to_const() {
                    if constant.num_bits() <= *bit_size {
                        return Ok(variable);
                    }
                }
                // Under a predicate, a range check must not fail, so we
                // range check `predicate * variable` instead.
                let predicate_range = self.mul_var(variable, predicate)?;
                let witness_var = self.get_or_create_witness_var(predicate_range)?;
                let witness = self.var_to_witness(witness_var)?;
                self.acir_ir.range_constraint(witness, *bit_size)?;
                if let Some(message) = message {
                    let payload = self.generate_assertion_message_payload(message.clone());
                    self.acir_ir
                        .assertion_payloads
                        .insert(self.acir_ir.last_acir_opcode_location(), payload);
                }
                Ok(predicate_range)
            }
            NumericType::NativeField => {
                // Range constraining a Field is a no-op
                Ok(variable)
            }
        }
    }

    /// Returns an `AcirVar` which will be constrained to be lhs mod 2^{rhs}
    /// In order to do this, we 'simply' perform euclidean division of lhs by 2^{rhs}
    /// The remainder of the division is then lhs mod 2^{rhs}
    pub(crate) fn truncate_var(
        &mut self,
        lhs: AcirVar,
        rhs: u32,
        max_bit_size: u32,
    ) -> Result<AcirVar, RuntimeError> {
        // 2^{rhs}
        let divisor = self.add_constant(power_of_two::<F>(rhs));
        let one = self.add_constant(F::one());

        //  Computes lhs = 2^{rhs} * q + r
        let (_, remainder) = self.euclidean_division_var(lhs, divisor, max_bit_size, one)?;

        Ok(remainder)
    }

    /// Returns an 'AcirVar' containing the boolean value lhs<rhs, assuming lhs and rhs are signed integers of size bit_count.
    /// Like in the unsigned case, we compute the difference diff = lhs-rhs+2^n (and we avoid underflow)
    /// The result depends on the diff and the signs of the inputs:
    /// If same sign, lhs<rhs <=> diff<2^n, because the 2-complement representation keeps the ordering (e.g in 8 bits -1 is 255 > -2 = 254)
    /// If not, lhs positive => diff > 2^n
    /// and lhs negative => diff <= 2^n => diff < 2^n (because signs are not the same, so lhs != rhs and so diff != 2^n)
    pub(crate) fn less_than_signed(
        &mut self,
        lhs: AcirVar,
        rhs: AcirVar,
        bit_count: u32,
    ) -> Result<AcirVar, RuntimeError> {
        let pow_last = self.add_constant(F::from(1_u128 << (bit_count - 1)));
        let pow = self.add_constant(F::from(1_u128 << (bit_count)));

        // We check whether the inputs have same sign or not by computing the XOR of their bit sign

        // Predicate is always active as `pow_last` is known to be non-zero.
        let one = self.add_constant(1_u128);
        let lhs_sign = self.div_var(
            lhs,
            pow_last,
            AcirType::NumericType(NumericType::Unsigned { bit_size: bit_count }),
            one,
        )?;
        let rhs_sign = self.div_var(
            rhs,
            pow_last,
            AcirType::NumericType(NumericType::Unsigned { bit_size: bit_count }),
            one,
        )?;
        let same_sign = self.xor_var(
            lhs_sign,
            rhs_sign,
            AcirType::NumericType(NumericType::Signed { bit_size: 1 }),
        )?;

        // We compute the input difference
        let no_underflow = self.add_var(lhs, pow)?;
        let diff = self.sub_var(no_underflow, rhs)?;

        // We check the 'bit sign' of the difference
        let diff_sign = self.less_than_var(diff, pow, bit_count + 1)?;

        // Then the result is simply diff_sign XOR same_sign (can be checked with a truth table)
        self.xor_var(
            diff_sign,
            same_sign,
            AcirType::NumericType(NumericType::Signed { bit_size: 1 }),
        )
    }

    /// Returns an `AcirVar` which will be `1` if lhs >= rhs
    /// and `0` otherwise.
    pub(crate) fn more_than_eq_var(
        &mut self,
        lhs: AcirVar,
        rhs: AcirVar,
        max_bits: u32,
    ) -> Result<AcirVar, RuntimeError> {
        // Returns a `Witness` that is constrained to be:
        // - `1` if lhs >= rhs
        // - `0` otherwise
        //
        // We essentially computes the sign bit of `b-a`
        // For this we sign-extend `b-a` with `c = 2^{max_bits} - (b - a)`, since both `a` and `b` are less than `2^{max_bits}`
        // Then we get the bit sign of `c`, the 2-complement representation of `(b-a)`, which is a `max_bits+1` integer,
        // by doing the euclidean division `c / 2^{max_bits}`
        //
        // To see why it really works;
        // We first note that `c` is an integer of `(max_bits+1)` bits. Therefore,
        // if `b-a>0`, then `c < 2^{max_bits}`, so the division by `2^{max_bits}` will give `0`
        // If `b-a<=0`, then `c >= 2^{max_bits}`, so the division by `2^{max_bits}` will give `1`.
        //
        // In other words, `1` means `a >= b` and `0` means `b > a`.
        // The important thing here is that `c` does not overflow nor underflow the field;
        // - By construction we have `c >= 0`, so there is no underflow
        // - We assert at the beginning that `2^{max_bits+1}` does not overflow the field, so neither c.

        // Ensure that 2^{max_bits + 1} is less than the field size
        //
        // TODO: perhaps this should be a user error, instead of an assert
        assert!(max_bits + 1 < F::max_num_bits());

        let two_max_bits = self.add_constant(power_of_two::<F>(max_bits));
        let diff = self.sub_var(lhs, rhs)?;
        let comparison_evaluation = self.add_var(diff, two_max_bits)?;

        // Euclidean division by 2^{max_bits}  : 2^{max_bits} + a - b = q * 2^{max_bits} + r
        //
        // 2^{max_bits} is of max_bits+1 bit size
        // If a>b, then a-b is less than 2^{max_bits} - 1, so 2^{max_bits} + a - b is less than 2^{max_bits} + 2^{max_bits} - 1 = 2^{max_bits+1} - 1
        // If a <= b, then 2^{max_bits} + a - b is less than 2^{max_bits} <= 2^{max_bits+1} - 1
        // This means that both operands of the division have at most max_bits+1 bit size.
        //
        // case: a == b
        //
        //   let k = 0;
        // - 2^{max_bits} == q *  2^{max_bits} + r
        // - This is only the case when q == 1 and r == 0 (assuming r is bounded to be less than 2^{max_bits})
        //
        // case: a > b
        //
        //   let k = a - b;
        // - k + 2^{max_bits} == q * 2^{max_bits} + r
        // - This is the case when q == 1 and r = k
        //
        // case: a < b
        //
        //   let k = b - a
        // - 2^{max_bits} - k == q * 2^{max_bits} + r
        // - This is only the case when q == 0 and r == 2^{max_bits} - k

        // Predicate is always active as we know `two_max_bits` is always non-zero.
        let one = self.add_constant(1_u128);
        let (q, _) =
            self.euclidean_division_var(comparison_evaluation, two_max_bits, max_bits + 1, one)?;
        Ok(q)
    }

    /// Returns an `AcirVar` which will be `1` if lhs < rhs
    /// and `0` otherwise.
    pub(crate) fn less_than_var(
        &mut self,
        lhs: AcirVar,
        rhs: AcirVar,
        bit_size: u32,
    ) -> Result<AcirVar, RuntimeError> {
        // Flip the result of calling more than equal method to
        // compute less than.
        let comparison = self.more_than_eq_var(lhs, rhs, bit_size)?;

        let one = self.add_constant(F::one());
        self.sub_var(one, comparison) // comparison_negated
    }

    /// Returns a vector of `AcirVar`s constrained to be the decomposition of the given input
    /// over given radix.
    ///
    /// The `AcirVar`s for the `radix_var` and `limb_count_var` must be a constant
    ///
    /// TODO: support radix larger than field modulus
    pub(crate) fn radix_decompose(
        &mut self,
        endian: Endian,
        input_var: AcirVar,
        radix_var: AcirVar,
        limb_count: u32,
        result_element_type: AcirType,
    ) -> Result<AcirValue, RuntimeError> {
        let radix = match self.vars[&radix_var].as_constant() {
            Some(radix) => radix.to_u128() as u32,
            None => {
                return Err(RuntimeError::InternalError(InternalError::NotAConstant {
                    name: "radix".to_string(),
                    call_stack: self.get_call_stack(),
                }));
            }
        };

        let input_expr = self.var_to_expression(input_var)?;

        let bit_size = u32::BITS - (radix - 1).leading_zeros();
        let limbs = self.acir_ir.radix_le_decompose(&input_expr, radix, limb_count, bit_size)?;

        let mut limb_vars = vecmap(limbs, |witness| {
            let witness = self.add_data(AcirVarData::Witness(witness));
            AcirValue::Var(witness, result_element_type.clone())
        });

        if endian == Endian::Big {
            limb_vars.reverse();
        }

        // `Intrinsic::ToRadix` returns slices which are represented
        // by tuples with the structure (length, slice contents)
        Ok(AcirValue::Array(limb_vars.into()))
    }

    /// Returns `AcirVar`s constrained to be the bit decomposition of the provided input
    pub(crate) fn bit_decompose(
        &mut self,
        endian: Endian,
        input_var: AcirVar,
        limb_count: u32,
        result_element_type: AcirType,
    ) -> Result<AcirValue, RuntimeError> {
        let two_var = self.add_constant(2_u128);
        self.radix_decompose(endian, input_var, two_var, limb_count, result_element_type)
    }

    /// Recursive helper to flatten a single AcirValue into the result vector.
    /// This helper differs from `flatten()` on the `AcirValue` type, as this method has access to the AcirContext
    /// which lets us flatten an `AcirValue::DynamicArray` by reading its variables from memory.
    pub(crate) fn flatten(
        &mut self,
        value: AcirValue,
    ) -> Result<Vec<(AcirVar, AcirType)>, InternalError> {
        match value {
            AcirValue::Var(acir_var, typ) => Ok(vec![(acir_var, typ)]),
            AcirValue::Array(array) => {
                let mut values = Vec::new();
                for value in array {
                    values.append(&mut self.flatten(value)?);
                }
                Ok(values)
            }
            AcirValue::DynamicArray(AcirDynamicArray { block_id, len, value_types, .. }) => {
                try_vecmap(0..len, |i| {
                    let index_var = self.add_constant(i);

                    Ok::<(AcirVar, AcirType), InternalError>((
                        self.read_from_memory(block_id, &index_var)?,
                        value_types[i].into(),
                    ))
                })
            }
        }
    }

    /// Terminates the context and takes the resulting `GeneratedAcir`
    pub(crate) fn finish(
        mut self,
        inputs: Vec<Witness>,
        return_values: Vec<Witness>,
        warnings: Vec<SsaReport>,
    ) -> GeneratedAcir<F> {
        self.acir_ir.input_witnesses = inputs;
        self.acir_ir.return_witnesses = return_values;
        self.acir_ir.warnings = warnings;
        self.acir_ir
    }

    /// Adds `Data` into the context and assigns it a Variable.
    ///
    /// Variable can be seen as an index into the context.
    /// We use a two-way map so that it is efficient to lookup
    /// either the key or the value.
    fn add_data(&mut self, data: AcirVarData<F>) -> AcirVar {
        let id = AcirVar::new(self.vars.len());
        self.vars.insert(id, data);
        id
    }

    /// Returns a Variable that is constrained to be the result of reading
    /// from the memory `block_id` at the given `index`.
    pub(crate) fn read_from_memory(
        &mut self,
        block_id: BlockId,
        index: &AcirVar,
    ) -> Result<AcirVar, InternalError> {
        // Fetch the witness corresponding to the index
        let index_var = self.get_or_create_witness_var(*index)?;
        let index_witness = self.var_to_witness(index_var)?;

        // Create a Variable to hold the result of the read and extract the corresponding Witness
        let value_read_var = self.add_variable();
        let value_read_witness = self.var_to_witness(value_read_var)?;

        // Add the memory read operation to the list of opcodes
        let op = MemOp::read_at_mem_index(index_witness.into(), value_read_witness);
        self.acir_ir.push_opcode(Opcode::MemoryOp { block_id, op, predicate: None });

        Ok(value_read_var)
    }

    /// Constrains the Variable `value` to be the new value located at `index` in the memory `block_id`.
    pub(crate) fn write_to_memory(
        &mut self,
        block_id: BlockId,
        index: &AcirVar,
        value: &AcirVar,
    ) -> Result<(), InternalError> {
        // Fetch the witness corresponding to the index
        let index_var = self.get_or_create_witness_var(*index)?;
        let index_witness = self.var_to_witness(index_var)?;

        // Fetch the witness corresponding to the value to be written
        let value_write_var = self.get_or_create_witness_var(*value)?;
        let value_write_witness = self.var_to_witness(value_write_var)?;

        // Add the memory write operation to the list of opcodes
        let op = MemOp::write_to_mem_index(index_witness.into(), value_write_witness.into());
        self.acir_ir.push_opcode(Opcode::MemoryOp { block_id, op, predicate: None });

        Ok(())
    }

    /// Insert the MemoryInit for the Return Data array, using the provided witnesses
    pub(crate) fn initialize_return_data(&mut self, block_id: BlockId, init: Vec<Witness>) {
        self.acir_ir.push_opcode(Opcode::MemoryInit {
            block_id,
            init,
            block_type: BlockType::ReturnData,
        });
    }

    /// Initializes an array in memory with the given values `optional_values`.
    /// If `optional_values` is empty, then the array is initialized with zeros.
    pub(crate) fn initialize_array(
        &mut self,
        block_id: BlockId,
        len: usize,
        optional_value: Option<AcirValue>,
        databus: BlockType,
    ) -> Result<(), InternalError> {
        let initialized_values = match optional_value {
            None => {
                let zero = self.add_constant(F::zero());
                let zero_witness = self.var_to_witness(zero)?;
                vec![zero_witness; len]
            }
            Some(optional_value) => {
                let mut values = Vec::new();
                if let AcirValue::DynamicArray(_) = optional_value {
                    unreachable!("Dynamic array should already be initialized");
                }
                self.initialize_array_inner(&mut values, optional_value)?;
                values
            }
        };

        self.acir_ir.push_opcode(Opcode::MemoryInit {
            block_id,
            init: initialized_values,
            block_type: databus,
        });

        Ok(())
    }

    fn initialize_array_inner(
        &mut self,
        witnesses: &mut Vec<Witness>,
        input: AcirValue,
    ) -> Result<(), InternalError> {
        match input {
            AcirValue::Var(var, _) => {
                let var = self.get_or_create_witness_var(var)?;
                witnesses.push(self.var_to_witness(var)?);
            }
            AcirValue::Array(values) => {
                for value in values {
                    self.initialize_array_inner(witnesses, value)?;
                }
            }
            AcirValue::DynamicArray(AcirDynamicArray { block_id, len, .. }) => {
                let dynamic_array_values = try_vecmap(0..len, |i| {
                    let index_var = self.add_constant(i);

                    let read = self.read_from_memory(block_id, &index_var)?;
                    Ok::<AcirValue, InternalError>(AcirValue::Var(read, AcirType::field()))
                })?;
                for value in dynamic_array_values {
                    self.initialize_array_inner(witnesses, value)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn call_acir_function(
        &mut self,
        id: AcirFunctionId,
        inputs: Vec<AcirValue>,
        output_count: usize,
        predicate: AcirVar,
    ) -> Result<Vec<AcirVar>, RuntimeError> {
        let inputs = self.prepare_inputs_for_black_box_func_call(inputs, false)?;
        let inputs = inputs
            .iter()
            .flat_map(|input| vecmap(input, |input| input.to_witness()))
            .collect::<Vec<_>>();
        let outputs = vecmap(0..output_count, |_| self.acir_ir.next_witness_index());

        // Convert `Witness` values which are now constrained to be the output of the
        // ACIR function call into `AcirVar`s.
        // Similar to black box functions, we do not apply range information on the output of the  function.
        // See issue https://github.com/noir-lang/noir/issues/1439
        let results =
            vecmap(&outputs, |witness_index| self.add_data(AcirVarData::Witness(*witness_index)));

        let predicate = Some(self.var_to_expression(predicate)?);
        self.acir_ir.push_opcode(Opcode::Call { id, inputs, outputs, predicate });
        Ok(results)
    }

    pub(crate) fn generate_assertion_message_payload(
        &mut self,
        message: String,
    ) -> AssertionPayload<F> {
        self.acir_ir.generate_assertion_message_payload(message)
    }
}

/// Returns an `F` representing the value `2**power`
///
/// # Panics
///
/// Panics if `2**power` exceeds `F::modulus()`.
pub(super) fn power_of_two<F: AcirField>(power: u32) -> F {
    if power >= F::max_num_bits() {
        panic!("Field cannot represent this power of two");
    }
    let full_bytes = power / 8;
    let extra_bits = power % 8;
    let most_significant_byte: u8 = match extra_bits % 8 {
        0 => 0x01,
        1 => 0x02,
        2 => 0x04,
        3 => 0x08,
        4 => 0x10,
        5 => 0x20,
        6 => 0x40,
        7 => 0x80,
        _ => unreachable!("We cover the full range of x % 8"),
    };

    let bytes_be: Vec<u8> = std::iter::once(most_significant_byte)
        .chain(std::iter::repeat_n(0, full_bytes as usize))
        .collect();

    F::from_be_bytes_reduce(&bytes_be)
}

/// Enum representing the possible values that a
/// Variable can be given.
#[derive(Debug, Eq, Clone, PartialEq)]
enum AcirVarData<F> {
    Witness(Witness),
    Expr(Expression<F>),
    Const(F),
}

impl<F> AcirVarData<F> {
    /// Returns a FieldElement, if the underlying `AcirVarData`
    /// represents a constant.
    pub(crate) fn as_constant(&self) -> Option<&F> {
        if let AcirVarData::Const(field) = self {
            return Some(field);
        }
        None
    }
}

impl<F: AcirField> AcirVarData<F> {
    /// Converts all enum variants to an Expression.
    pub(crate) fn to_expression(&self) -> Cow<Expression<F>> {
        match self {
            AcirVarData::Witness(witness) => Cow::Owned(Expression::from(*witness)),
            AcirVarData::Expr(expr) => Cow::Borrowed(expr),
            AcirVarData::Const(constant) => Cow::Owned(Expression::from(*constant)),
        }
    }
}

impl<F> From<Witness> for AcirVarData<F> {
    fn from(witness: Witness) -> Self {
        AcirVarData::Witness(witness)
    }
}

impl<F: AcirField> From<Expression<F>> for AcirVarData<F> {
    fn from(expr: Expression<F>) -> Self {
        // Prefer simpler variants if possible.
        if let Some(constant) = expr.to_const() {
            AcirVarData::Const(*constant)
        } else if let Some(witness) = expr.to_witness() {
            AcirVarData::from(witness)
        } else {
            AcirVarData::Expr(expr)
        }
    }
}

/// Checks if this expression can fit into one arithmetic identity
fn fits_in_one_identity<F: AcirField>(expr: &Expression<F>, width: ExpressionWidth) -> bool {
    let width = match &width {
        ExpressionWidth::Unbounded => {
            return true;
        }
        ExpressionWidth::Bounded { width } => *width,
    };

    // A Polynomial with more than one mul term cannot fit into one opcode
    if expr.mul_terms.len() > 1 {
        return false;
    };

    expr.width() <= width
}

#[cfg(test)]
mod test {
    use acvm::{AcirField, FieldElement};
    use proptest::prelude::*;

    use super::power_of_two;

    #[test]
    #[should_panic = "Field cannot represent this power of two"]
    fn power_of_two_panics_on_overflow() {
        power_of_two::<FieldElement>(FieldElement::max_num_bits());
    }

    proptest! {
        #[test]
        fn power_of_two_agrees_with_generic_impl(bit_size in (0..=128u32)) {
            let power_of_two_general =
                FieldElement::from(2_u128).pow(&FieldElement::from(bit_size));
            let power_of_two_opt: FieldElement = power_of_two(bit_size);

            prop_assert_eq!(power_of_two_opt, power_of_two_general);
        }

    }
}
