use iter_extended::vecmap;
use rustc_hash::FxHashMap as HashMap;

use crate::ResolvedGeneric;
use crate::ast::{Ident, ItemVisibility, NoirFunction};
use crate::hir::type_check::generics::TraitGenerics;
use crate::node_interner::{DefinitionId, NodeInterner};
use crate::{
    Generics, Type, TypeBindings, TypeVariable,
    graph::CrateId,
    node_interner::{FuncId, TraitId},
};
use fm::FileId;
use noirc_errors::{Location, Span};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraitFunction {
    pub name: Ident,
    pub typ: Type,
    pub location: Location,
    pub default_impl: Option<Box<NoirFunction>>,
    pub default_impl_module_id: crate::hir::def_map::LocalModuleId,
    pub trait_constraints: Vec<TraitConstraint>,
    pub direct_generics: Generics,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraitConstant {
    pub name: Ident,
    pub typ: Type,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct NamedType {
    pub name: Ident,
    pub typ: Type,
}

impl std::fmt::Display for NamedType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} = {}", self.name, self.typ)
    }
}

/// Represents a trait in the type system. Each instance of this struct
/// will be shared across all Type::Trait variants that represent
/// the same trait.
#[derive(Debug, Eq)]
pub struct Trait {
    /// A unique id representing this trait type. Used to check if two
    /// struct traits are equal.
    pub id: TraitId,

    pub crate_id: CrateId,

    pub methods: Vec<TraitFunction>,

    /// Maps method_name -> method id.
    /// This map is separate from methods since TraitFunction ids
    /// are created during collection where we don't yet have all
    /// the information needed to create the full TraitFunction.
    pub method_ids: HashMap<String, FuncId>,

    pub associated_types: Generics,
    pub associated_type_bounds: HashMap<String, Vec<ResolvedTraitBound>>,

    pub name: Ident,
    pub generics: Generics,
    pub location: Location,
    pub visibility: ItemVisibility,

    /// When resolving the types of Trait elements, all references to `Self` resolve
    /// to this TypeVariable. Then when we check if the types of trait impl elements
    /// match the definition in the trait, we bind this TypeVariable to whatever
    /// the correct Self type is for that particular impl block.
    pub self_type_typevar: TypeVariable,

    /// The resolved trait bounds (for example in `trait Foo: Bar + Baz`, this would be `Bar + Baz`)
    pub trait_bounds: Vec<ResolvedTraitBound>,

    pub where_clause: Vec<TraitConstraint>,

    pub all_generics: Generics,

    /// Map from each associated constant's name to a unique DefinitionId for that constant.
    pub associated_constant_ids: HashMap<String, DefinitionId>,
}

#[derive(Debug)]
pub struct TraitImpl {
    pub ident: Ident,
    pub location: Location,
    pub typ: Type,
    pub trait_id: TraitId,

    /// Any ordered type arguments on the trait this impl is for.
    /// E.g. `A, B` in `impl Foo<A, B, C = D> for Bar`
    ///
    /// Note that named arguments (associated types) are stored separately
    /// in the NodeInterner. This is because they're required to resolve types
    /// before the impl as a whole is finished resolving.
    pub trait_generics: Vec<Type>,

    pub file: FileId,
    pub crate_id: CrateId,
    pub methods: Vec<FuncId>, // methods[i] is the implementation of trait.methods[i] for Type typ

    /// The where clause, if present, contains each trait requirement which must
    /// be satisfied for this impl to be selected. E.g. in `impl Eq for [T] where T: Eq`,
    /// `where_clause` would contain the one `T: Eq` constraint. If there is no where clause,
    /// this Vec is empty.
    pub where_clause: Vec<TraitConstraint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitConstraint {
    pub typ: Type,
    pub trait_bound: ResolvedTraitBound,
}

impl TraitConstraint {
    pub fn apply_bindings(&mut self, type_bindings: &TypeBindings) {
        self.typ = self.typ.substitute(type_bindings);
        self.trait_bound.apply_bindings(type_bindings);
    }

    pub fn to_string(&self, interner: &NodeInterner) -> String {
        interner.trait_constraint_string(
            &self.typ,
            self.trait_bound.trait_id,
            &self.trait_bound.trait_generics.ordered,
            &self.trait_bound.trait_generics.named,
        )
    }
}

#[derive(Debug, Clone, Eq)]
pub struct ResolvedTraitBound {
    pub trait_id: TraitId,
    pub trait_generics: TraitGenerics,
    pub location: Location,
}

impl ResolvedTraitBound {
    pub fn apply_bindings(&mut self, type_bindings: &TypeBindings) {
        for typ in &mut self.trait_generics.ordered {
            *typ = typ.substitute(type_bindings);
        }

        for named in &mut self.trait_generics.named {
            named.typ = named.typ.substitute(type_bindings);
        }
    }
}

impl PartialEq for ResolvedTraitBound {
    fn eq(&self, other: &Self) -> bool {
        // Location doesn't matter for equality
        self.trait_id == other.trait_id && self.trait_generics == other.trait_generics
    }
}

impl std::hash::Hash for Trait {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl PartialEq for Trait {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Trait {
    pub fn set_methods(&mut self, methods: Vec<TraitFunction>) {
        self.methods = methods;
    }

    pub fn set_trait_bounds(&mut self, trait_bounds: Vec<ResolvedTraitBound>) {
        self.trait_bounds = trait_bounds;
    }

    pub fn set_where_clause(&mut self, where_clause: Vec<TraitConstraint>) {
        self.where_clause = where_clause;
    }

    pub fn set_visibility(&mut self, visibility: ItemVisibility) {
        self.visibility = visibility;
    }

    pub fn set_all_generics(&mut self, generics: Generics) {
        self.all_generics = generics;
    }

    pub fn set_associated_type_bounds(
        &mut self,
        associated_type_bounds: HashMap<String, Vec<ResolvedTraitBound>>,
    ) {
        self.associated_type_bounds = associated_type_bounds;
    }

    pub fn find_method(&self, name: &str, interner: &NodeInterner) -> Option<DefinitionId> {
        for method in self.methods.iter() {
            if &method.name == name {
                let id = *self.method_ids.get(name).unwrap();
                return Some(interner.function_definition_id(id));
            }
        }
        None
    }

    pub fn find_method_or_constant(
        &self,
        name: &str,
        interner: &NodeInterner,
    ) -> Option<DefinitionId> {
        if let Some(method) = self.find_method(name, interner) {
            return Some(method);
        }
        self.associated_constant_ids.get(name).copied()
    }

    pub fn get_associated_type(&self, last_name: &str) -> Option<&ResolvedGeneric> {
        self.associated_types.iter().find(|typ| typ.name.as_ref() == last_name)
    }

    /// Returns both the ordered generics of this type, and its named, associated types.
    /// These types are all as-is and are not instantiated.
    pub fn get_generics(&self) -> (Vec<Type>, Vec<Type>) {
        let ordered = vecmap(&self.generics, |generic| generic.clone().as_named_generic());
        let named = vecmap(&self.associated_types, |generic| generic.clone().as_named_generic());
        (ordered, named)
    }

    pub fn get_trait_generics(&self, location: Location) -> TraitGenerics {
        let ordered = vecmap(&self.generics, |generic| generic.clone().as_named_generic());
        let named = vecmap(&self.associated_types, |generic| {
            let name = Ident::new(generic.name.to_string(), location);
            NamedType { name, typ: generic.clone().as_named_generic() }
        });
        TraitGenerics { ordered, named }
    }

    /// Returns a TraitConstraint for this trait using Self as the object
    /// type and the uninstantiated generics for any trait generics.
    pub fn as_constraint(&self, location: Location) -> TraitConstraint {
        let trait_generics = self.get_trait_generics(location);
        TraitConstraint {
            typ: Type::TypeVariable(self.self_type_typevar.clone()),
            trait_bound: ResolvedTraitBound { trait_generics, trait_id: self.id, location },
        }
    }
}

impl std::fmt::Display for Trait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl TraitFunction {
    pub fn arguments(&self) -> &[Type] {
        match &self.typ {
            Type::Function(args, _, _, _) => args,
            Type::Forall(_, typ) => match typ.as_ref() {
                Type::Function(args, _, _, _) => args,
                _ => unreachable!("Trait function does not have a function type"),
            },
            _ => unreachable!("Trait function does not have a function type"),
        }
    }

    pub fn generics(&self) -> &[TypeVariable] {
        match &self.typ {
            Type::Function(..) => &[],
            Type::Forall(generics, _) => generics,
            _ => unreachable!("Trait function does not have a function type"),
        }
    }

    pub fn return_type(&self) -> &Type {
        match &self.typ {
            Type::Function(_, return_type, _, _) => return_type,
            Type::Forall(_, typ) => match typ.as_ref() {
                Type::Function(_, return_type, _, _) => return_type,
                _ => unreachable!("Trait function does not have a function type"),
            },
            _ => unreachable!("Trait function does not have a function type"),
        }
    }
}
