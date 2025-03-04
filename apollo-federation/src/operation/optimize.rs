//! # GraphQL subgraph query optimization.
//!
//! This module contains the logic to optimize (or "compress") a subgraph query by using fragments
//! (either reusing existing ones in the original query or generating new ones).
//!
//! ## Add __typename field for abstract types in named fragment definitions
//!
//! ## Selection/SelectionSet intersection/minus operations
//! These set-theoretic operation methods are used to compute the optimized selection set.
//!
//! ## Collect applicable fragments at given type.
//! This is only the first filtering step. Further validation is needed to check if they can merge
//! with other fields and fragment selections.
//!
//! ## Field validation
//! `FieldsConflictMultiBranchValidator` (and `FieldsConflictValidator`) are used to check if
//! modified subgraph GraphQL queries are still valid, since adding fragments can introduce
//! conflicts.
//!
//! ## Matching fragments with selection set
//! `try_apply_fragments` tries to match all applicable fragments one by one.
//! They are expanded into selection sets in order to match against given selection set.
//! Set-intersection/-minus/-containment operations are used to narrow down to fewer number of
//! fragments that can be used to optimize the selection set. If there is a single fragment that
//! covers the full selection set, then that fragment is used. Otherwise, we attempted to reduce
//! the number of fragments applied (but optimality is not guaranteed yet).
//!
//! ## Retain certain fragments in selection sets while expanding the rest
//! Unlike the `expand_all_fragments` method, this methods retains the listed fragments.
//!
//! ## Optimize (or reduce) the named fragments in the query
//! Optimization of named fragment definitions in query documents based on the usage of
//! fragments in (optimized) operations.
//!
//! ## `reuse_fragments` methods (putting everything together)
//! Recursive optimization of selection and selection sets.

use std::sync::Arc;

use apollo_compiler::collections::IndexMap;
use apollo_compiler::collections::IndexSet;
use apollo_compiler::executable;
use apollo_compiler::executable::VariableDefinition;
use apollo_compiler::Name;
use apollo_compiler::Node;

use super::Containment;
use super::ContainmentOptions;
use super::DirectiveList;
use super::Field;
use super::FieldSelection;
use super::Fragment;
use super::FragmentSpreadSelection;
use super::InlineFragmentSelection;
use super::NamedFragments;
use super::Operation;
use super::Selection;
use super::SelectionMapperReturn;
use super::SelectionOrSet;
use super::SelectionSet;
use crate::error::FederationError;
use crate::operation::FragmentSpread;
use crate::operation::FragmentSpreadData;
use crate::operation::SelectionValue;
use crate::schema::position::CompositeTypeDefinitionPosition;

#[derive(Debug)]
struct ReuseContext<'a> {
    fragments: &'a NamedFragments,
    operation_variables: Option<IndexSet<&'a Name>>,
}

impl<'a> ReuseContext<'a> {
    fn for_fragments(fragments: &'a NamedFragments) -> Self {
        Self {
            fragments,
            operation_variables: None,
        }
    }

    // Taking two separate parameters so the caller can still mutate the operation's selection set.
    fn for_operation(
        fragments: &'a NamedFragments,
        operation_variables: &'a [Node<VariableDefinition>],
    ) -> Self {
        Self {
            fragments,
            operation_variables: Some(operation_variables.iter().map(|var| &var.name).collect()),
        }
    }
}

//=============================================================================
// Add __typename field for abstract types in named fragment definitions

impl NamedFragments {
    // - Expands all nested fragments
    // - Applies the provided `mapper` to each selection set of the expanded fragments.
    // - Finally, re-fragments the nested fragments.
    // - `mapper` must return a fragment-spread-free selection set.
    fn map_to_expanded_selection_sets(
        &self,
        mut mapper: impl FnMut(&SelectionSet) -> Result<SelectionSet, FederationError>,
    ) -> Result<NamedFragments, FederationError> {
        let mut result = NamedFragments::default();
        // Note: `self.fragments` has insertion order topologically sorted.
        for fragment in self.fragments.values() {
            let expanded_selection_set = fragment
                .selection_set
                .expand_all_fragments()?
                .flatten_unnecessary_fragments(
                    &fragment.type_condition_position,
                    &Default::default(),
                    &fragment.schema,
                )?;
            let mut mapped_selection_set = mapper(&expanded_selection_set)?;
            // `mapped_selection_set` must be fragment-spread-free.
            mapped_selection_set.reuse_fragments(&ReuseContext::for_fragments(&result))?;
            let updated = Fragment {
                selection_set: mapped_selection_set,
                schema: fragment.schema.clone(),
                name: fragment.name.clone(),
                type_condition_position: fragment.type_condition_position.clone(),
                directives: fragment.directives.clone(),
            };
            result.insert(updated);
        }
        Ok(result)
    }

    pub(crate) fn add_typename_field_for_abstract_types_in_named_fragments(
        &self,
    ) -> Result<Self, FederationError> {
        // This method is a bit tricky due to potentially nested fragments. More precisely, suppose that
        // we have:
        //   fragment MyFragment on T {
        //     a {
        //       b {
        //         ...InnerB
        //       }
        //     }
        //   }
        //
        //   fragment InnerB on B {
        //     __typename
        //     x
        //     y
        //   }
        // then if we were to "naively" add `__typename`, the first fragment would end up being:
        //   fragment MyFragment on T {
        //     a {
        //       __typename
        //       b {
        //         __typename
        //         ...InnerX
        //       }
        //     }
        //   }
        // but that's not ideal because the inner-most `__typename` is already within `InnerX`. And that
        // gets in the way to re-adding fragments (the `SelectionSet::reuse_fragments` method) because if we start
        // with:
        //   {
        //     a {
        //       __typename
        //       b {
        //         __typename
        //         x
        //         y
        //       }
        //     }
        //   }
        // and add `InnerB` first, we get:
        //   {
        //     a {
        //       __typename
        //       b {
        //         ...InnerB
        //       }
        //     }
        //   }
        // and it becomes tricky to recognize the "updated-with-typename" version of `MyFragment` now (we "seem"
        // to miss a `__typename`).
        //
        // Anyway, to avoid this issue, what we do is that for every fragment, we:
        //  1. expand any nested fragments in its selection.
        //  2. add `__typename` where we should in that expanded selection.
        //  3. re-optimize all fragments (using the "updated-with-typename" versions).
        // which is what `mapToExpandedSelectionSets` gives us.

        if self.is_empty() {
            // PORT_NOTE: This was an assertion failure in JS version. But, it's actually ok to
            // return unchanged if empty.
            return Ok(self.clone());
        }
        let updated = self.map_to_expanded_selection_sets(|ss| {
            // Note: Since `ss` won't have any fragment spreads, `add_typename_field_for_abstract_types`'s return
            // value won't have any fragment spreads.
            ss.add_typename_field_for_abstract_types(/*parent_type_if_abstract*/ None)
        })?;
        // PORT_NOTE: The JS version asserts if `updated` is empty or not. But, we really want to
        // check the `updated` has the same set of fragments. To avoid performance hit, only the
        // size is checked here.
        if updated.len() != self.len() {
            return Err(FederationError::internal(
                "Unexpected change in the number of fragments",
            ));
        }
        Ok(updated)
    }
}

//=============================================================================
// Selection/SelectionSet intersection/minus operations

impl Selection {
    // PORT_NOTE: The definition of `minus` and `intersection` functions when either `self` or
    // `other` has no sub-selection seems unintuitive. Why `apple.minus(orange) = None` and
    // `apple.intersection(orange) = apple`?

    /// Computes the set-subtraction (self - other) and returns the result (the difference between
    /// self and other).
    /// If there are respective sub-selections, then we compute their diffs and add them (if not
    /// empty). Otherwise, we have no diff.
    fn minus(&self, other: &Selection) -> Result<Option<Selection>, FederationError> {
        if let (Some(self_sub_selection), Some(other_sub_selection)) =
            (self.selection_set(), other.selection_set())
        {
            let diff = self_sub_selection.minus(other_sub_selection)?;
            if !diff.is_empty() {
                return self
                    .with_updated_selections(
                        self_sub_selection.type_position.clone(),
                        diff.into_iter().map(|(_, v)| v),
                    )
                    .map(Some);
            }
        }
        Ok(None)
    }

    /// Computes the set-intersection of self and other
    /// - If there are respective sub-selections, then we compute their intersections and add them
    ///   (if not empty).
    /// - Otherwise, the intersection is same as `self`.
    fn intersection(&self, other: &Selection) -> Result<Option<Selection>, FederationError> {
        if let (Some(self_sub_selection), Some(other_sub_selection)) =
            (self.selection_set(), other.selection_set())
        {
            let common = self_sub_selection.intersection(other_sub_selection)?;
            if common.is_empty() {
                return Ok(None);
            } else {
                return self
                    .with_updated_selections(
                        self_sub_selection.type_position.clone(),
                        common.into_iter().map(|(_, v)| v),
                    )
                    .map(Some);
            }
        }
        Ok(Some(self.clone()))
    }
}

impl SelectionSet {
    /// Performs set-subtraction (self - other) and returns the result (the difference between self
    /// and other).
    pub(crate) fn minus(&self, other: &SelectionSet) -> Result<SelectionSet, FederationError> {
        let iter = self
            .selections
            .iter()
            .map(|(k, v)| {
                if let Some(other_v) = other.selections.get(k) {
                    v.minus(other_v)
                } else {
                    Ok(Some(v.clone()))
                }
            })
            .collect::<Result<Vec<_>, _>>()? // early break in case of Err
            .into_iter()
            .flatten();
        Ok(SelectionSet::from_raw_selections(
            self.schema.clone(),
            self.type_position.clone(),
            iter,
        ))
    }

    /// Computes the set-intersection of self and other
    fn intersection(&self, other: &SelectionSet) -> Result<SelectionSet, FederationError> {
        if self.is_empty() {
            return Ok(self.clone());
        }
        if other.is_empty() {
            return Ok(other.clone());
        }

        let iter = self
            .selections
            .iter()
            .map(|(k, v)| {
                if let Some(other_v) = other.selections.get(k) {
                    v.intersection(other_v)
                } else {
                    Ok(None)
                }
            })
            .collect::<Result<Vec<_>, _>>()? // early break in case of Err
            .into_iter()
            .flatten();
        Ok(SelectionSet::from_raw_selections(
            self.schema.clone(),
            self.type_position.clone(),
            iter,
        ))
    }
}

//=============================================================================
// Collect applicable fragments at given type.

impl Fragment {
    /// Whether this fragment may apply _directly_ at the provided type, meaning that the fragment
    /// sub-selection (_without_ the fragment condition, hence the "directly") can be normalized at
    /// `ty` without overly "widening" the runtime types.
    ///
    /// * `ty` - the type at which we're looking at applying the fragment
    //
    // The runtime types of the fragment condition must be at least as general as those of the
    // provided `ty`. Otherwise, putting it at `ty` without its condition would "generalize"
    // more than the fragment meant to (and so we'd "widen" the runtime types more than what the
    // query meant to.
    fn can_apply_directly_at_type(
        &self,
        ty: &CompositeTypeDefinitionPosition,
    ) -> Result<bool, FederationError> {
        // Short-circuit #1: the same type => trivially true.
        if self.type_condition_position == *ty {
            return Ok(true);
        }

        // Short-circuit #2: The type condition is not an abstract type (too restrictive).
        // - It will never cover all of the runtime types of `ty` unless it's the same type, which is
        //   already checked by short-circuit #1.
        if !self.type_condition_position.is_abstract_type() {
            return Ok(false);
        }

        // Short-circuit #3: The type condition is not an object (due to short-circuit #2) nor a
        // union type, but the `ty` may be too general.
        // - In other words, the type condition must be an interface but `ty` is a (different)
        //   interface or a union.
        // PORT_NOTE: In JS, this check was later on the return statement (negated). But, this
        //            should be checked before `possible_runtime_types` check, since this is
        //            cheaper to execute.
        // PORT_NOTE: This condition may be too restrictive (potentially a bug leading to
        //            suboptimal compression). If ty is a union whose members all implements the
        //            type condition (interface). Then, this function should've returned true.
        //            Thus, `!ty.is_union_type()` might be needed.
        if !self.type_condition_position.is_union_type() && !ty.is_object_type() {
            return Ok(false);
        }

        // Check if the type condition is a superset of the provided type.
        // - The fragment condition must be at least as general as the provided type.
        let condition_types = self
            .schema
            .possible_runtime_types(self.type_condition_position.clone())?;
        let ty_types = self.schema.possible_runtime_types(ty.clone())?;
        Ok(condition_types.is_superset(&ty_types))
    }
}

impl NamedFragments {
    /// Returns fragments that can be applied directly at the given type.
    fn get_all_may_apply_directly_at_type<'a>(
        &'a self,
        ty: &'a CompositeTypeDefinitionPosition,
    ) -> impl Iterator<Item = Result<&'a Node<Fragment>, FederationError>> + 'a {
        self.iter().filter_map(|fragment| {
            fragment
                .can_apply_directly_at_type(ty)
                .map(|can_apply| can_apply.then_some(fragment))
                .transpose()
        })
    }
}

//=============================================================================
// Field validation

// PORT_NOTE: Not having a validator and having a FieldsConflictValidator with empty
// `by_response_name` map has no difference in behavior. So, we could drop the `Option` from
// `Option<FieldsConflictValidator>`. However, `None` validator makes it clearer that validation is
// unnecessary.
struct FieldsConflictValidator {
    by_response_name: IndexMap<Name, IndexMap<Field, Option<Arc<FieldsConflictValidator>>>>,
}

impl FieldsConflictValidator {
    /// Build a field merging validator for a selection set.
    ///
    /// # Preconditions
    /// The selection set must not contain named fragment spreads.
    fn from_selection_set(selection_set: &SelectionSet) -> Self {
        Self::for_level(&[selection_set])
    }

    fn for_level<'a>(level: &[&'a SelectionSet]) -> Self {
        // Group `level`'s fields by the response-name/field
        let mut at_level: IndexMap<Name, IndexMap<Field, Vec<&'a SelectionSet>>> =
            IndexMap::default();
        for selection_set in level {
            for field_selection in selection_set.field_selections() {
                let response_name = field_selection.field.response_name();
                let at_response_name = at_level.entry(response_name).or_default();
                let entry = at_response_name
                    .entry(field_selection.field.clone())
                    .or_default();
                if let Some(ref field_selection_set) = field_selection.selection_set {
                    entry.push(field_selection_set);
                }
            }
        }

        // Collect validators per response-name/field
        let mut by_response_name = IndexMap::default();
        for (response_name, fields) in at_level {
            let mut at_response_name: IndexMap<Field, Option<Arc<FieldsConflictValidator>>> =
                IndexMap::default();
            for (field, selection_sets) in fields {
                if selection_sets.is_empty() {
                    at_response_name.insert(field, None);
                } else {
                    let validator = Arc::new(Self::for_level(&selection_sets));
                    at_response_name.insert(field, Some(validator));
                }
            }
            by_response_name.insert(response_name, at_response_name);
        }
        Self { by_response_name }
    }

    fn for_field<'v>(&'v self, field: &Field) -> impl Iterator<Item = Arc<Self>> + 'v {
        self.by_response_name
            .get(&field.response_name())
            .into_iter()
            .flat_map(|by_response_name| by_response_name.values())
            .flatten()
            .cloned()
    }

    fn has_same_response_shape(
        &self,
        other: &FieldsConflictValidator,
    ) -> Result<bool, FederationError> {
        for (response_name, self_fields) in self.by_response_name.iter() {
            let Some(other_fields) = other.by_response_name.get(response_name) else {
                continue;
            };

            for (self_field, self_validator) in self_fields {
                for (other_field, other_validator) in other_fields {
                    if !self_field.types_can_be_merged(other_field)? {
                        return Ok(false);
                    }

                    if let Some(self_validator) = self_validator {
                        if let Some(other_validator) = other_validator {
                            if !self_validator.has_same_response_shape(other_validator)? {
                                return Ok(false);
                            }
                        }
                    }
                }
            }
        }
        Ok(true)
    }

    fn do_merge_with(&self, other: &FieldsConflictValidator) -> Result<bool, FederationError> {
        for (response_name, self_fields) in self.by_response_name.iter() {
            let Some(other_fields) = other.by_response_name.get(response_name) else {
                continue;
            };

            // We're basically checking
            // [FieldsInSetCanMerge](https://spec.graphql.org/draft/#FieldsInSetCanMerge()), but
            // from 2 set of fields (`self_fields` and `other_fields`) of the same response that we
            // know individually merge already.
            for (self_field, self_validator) in self_fields {
                for (other_field, other_validator) in other_fields {
                    if !self_field.types_can_be_merged(other_field)? {
                        return Ok(false);
                    }

                    let p1 = self_field.parent_type_position();
                    let p2 = other_field.parent_type_position();
                    if p1 == p2 || !p1.is_object_type() || !p2.is_object_type() {
                        // Additional checks of `FieldsInSetCanMerge` when same parent type or one
                        // isn't object
                        if self_field.name() != other_field.name()
                            || self_field.arguments != other_field.arguments
                        {
                            return Ok(false);
                        }
                        if let (Some(self_validator), Some(other_validator)) =
                            (self_validator, other_validator)
                        {
                            if !self_validator.do_merge_with(other_validator)? {
                                return Ok(false);
                            }
                        }
                    } else {
                        // Otherwise, the sub-selection must pass
                        // [SameResponseShape](https://spec.graphql.org/draft/#SameResponseShape()).
                        if let (Some(self_validator), Some(other_validator)) =
                            (self_validator, other_validator)
                        {
                            if !self_validator.has_same_response_shape(other_validator)? {
                                return Ok(false);
                            }
                        }
                    }
                }
            }
        }
        Ok(true)
    }

    fn do_merge_with_all<'a>(
        &self,
        mut iter: impl Iterator<Item = &'a FieldsConflictValidator>,
    ) -> Result<bool, FederationError> {
        iter.try_fold(true, |acc, v| Ok(acc && v.do_merge_with(self)?))
    }
}

struct FieldsConflictMultiBranchValidator {
    validators: Vec<Arc<FieldsConflictValidator>>,
    used_spread_trimmed_part_at_level: Vec<Arc<FieldsConflictValidator>>,
}

impl FieldsConflictMultiBranchValidator {
    fn new(validators: Vec<Arc<FieldsConflictValidator>>) -> Self {
        Self {
            validators,
            used_spread_trimmed_part_at_level: Vec::new(),
        }
    }

    fn from_initial_validator(validator: FieldsConflictValidator) -> Self {
        Self {
            validators: vec![Arc::new(validator)],
            used_spread_trimmed_part_at_level: Vec::new(),
        }
    }

    fn for_field(&self, field: &Field) -> Self {
        let for_all_branches = self.validators.iter().flat_map(|v| v.for_field(field));
        Self::new(for_all_branches.collect())
    }

    // When this method is used in the context of `try_optimize_with_fragments`, we know that the
    // fragment, restricted to the current parent type, matches a subset of the sub-selection.
    // However, there is still one case we we cannot use it that we need to check, and this is if
    // using the fragment would create a field "conflict" (in the sense of the graphQL spec
    // [`FieldsInSetCanMerge`](https://spec.graphql.org/draft/#FieldsInSetCanMerge())) and thus
    // create an invalid selection. To be clear, `at_type.selections` cannot create a conflict,
    // since it is a subset of the target selection set and it is valid by itself. *But* there may
    // be some part of the fragment that is not `at_type.selections` due to being "dead branches"
    // for type `parent_type`. And while those branches _are_ "dead" as far as execution goes, the
    // `FieldsInSetCanMerge` validation does not take this into account (it's 1st step says
    // "including visiting fragments and inline fragments" but has no logic regarding ignoring any
    // fragment that may not apply due to the intersection of runtime types between multiple
    // fragment being empty).
    fn check_can_reuse_fragment_and_track_it(
        &mut self,
        fragment_restriction: &FragmentRestrictionAtType,
    ) -> Result<bool, FederationError> {
        // No validator means that everything in the fragment selection was part of the selection
        // we're optimizing away (by using the fragment), and we know the original selection was
        // ok, so nothing to check.
        let Some(validator) = &fragment_restriction.validator else {
            return Ok(true); // Nothing to check; Trivially ok.
        };

        if !validator.do_merge_with_all(self.validators.iter().map(Arc::as_ref))? {
            return Ok(false);
        }

        // We need to make sure the trimmed parts of `fragment` merges with the rest of the
        // selection, but also that it merge with any of the trimmed parts of any fragment we have
        // added already.
        // Note: this last condition means that if 2 fragment conflict on their "trimmed" parts,
        // then the choice of which is used can be based on the fragment ordering and selection
        // order, which may not be optimal. This feels niche enough that we keep it simple for now,
        // but we can revisit this decision if we run into real cases that justify it (but making
        // it optimal would be a involved in general, as in theory you could have complex
        // dependencies of fragments that conflict, even cycles, and you need to take the size of
        // fragments into account to know what's best; and even then, this could even depend on
        // overall usage, as it can be better to reuse a fragment that is used in other places,
        // than to use one for which it's the only usage. Adding to all that the fact that conflict
        // can happen in sibling branches).
        if !validator.do_merge_with_all(
            self.used_spread_trimmed_part_at_level
                .iter()
                .map(Arc::as_ref),
        )? {
            return Ok(false);
        }

        // We're good, but track the fragment.
        self.used_spread_trimmed_part_at_level
            .push(validator.clone());
        Ok(true)
    }
}

//=============================================================================
// Matching fragments with selection set (`try_optimize_with_fragments`)

/// Return type for `expanded_selection_set_at_type` method.
struct FragmentRestrictionAtType {
    /// Selections that are expanded from a given fragment at a given type and then normalized.
    /// - This represents the part of given type's sub-selections that are covered by the fragment.
    selections: SelectionSet,

    /// A runtime validator to check the fragment selections against other fields.
    /// - `None` means that there is nothing to check.
    /// - See `check_can_reuse_fragment_and_track_it` for more details.
    validator: Option<Arc<FieldsConflictValidator>>,
}

#[derive(Default)]
struct FragmentRestrictionAtTypeCache {
    map: IndexMap<(Name, CompositeTypeDefinitionPosition), Arc<FragmentRestrictionAtType>>,
}

impl FragmentRestrictionAtTypeCache {
    fn expanded_selection_set_at_type(
        &mut self,
        fragment: &Fragment,
        ty: &CompositeTypeDefinitionPosition,
    ) -> Result<Arc<FragmentRestrictionAtType>, FederationError> {
        // I would like to avoid the Arc here, it seems unnecessary, but with `.entry()`
        // the lifetime does not really want to work out.
        // (&'cache mut self) -> Result<&'cache FragmentRestrictionAtType>
        match self.map.entry((fragment.name.clone(), ty.clone())) {
            indexmap::map::Entry::Occupied(entry) => Ok(Arc::clone(entry.get())),
            indexmap::map::Entry::Vacant(entry) => Ok(Arc::clone(
                entry.insert(Arc::new(fragment.expanded_selection_set_at_type(ty)?)),
            )),
        }
    }
}

impl FragmentRestrictionAtType {
    fn new(selections: SelectionSet, validator: Option<FieldsConflictValidator>) -> Self {
        Self {
            selections,
            validator: validator.map(Arc::new),
        }
    }

    // It's possible that while the fragment technically applies at `parent_type`, it's "rebasing" on
    // `parent_type` is empty, or contains only `__typename`. For instance, suppose we have
    // a union `U = A | B | C`, and then a fragment:
    // ```graphql
    //   fragment F on U {
    //     ... on A {
    //       x
    //     }
    //     ... on B {
    //       y
    //     }
    //   }
    // ```
    // It is then possible to apply `F` when the parent type is `C`, but this ends up selecting
    // nothing at all.
    //
    // Using `F` in those cases is, while not 100% incorrect, at least not productive, and so we
    // skip it that case. This is essentially an optimization.
    fn is_useless(&self) -> bool {
        match self.selections.selections.as_slice().split_first() {
            None => true,
            Some((first, rest)) => rest.is_empty() && first.0.is_typename_field(),
        }
    }
}

impl Fragment {
    /// Computes the expanded selection set of this fragment along with its validator to check
    /// against other fragments applied under the same selection set.
    fn expanded_selection_set_at_type(
        &self,
        ty: &CompositeTypeDefinitionPosition,
    ) -> Result<FragmentRestrictionAtType, FederationError> {
        let expanded_selection_set = self.selection_set.expand_all_fragments()?;
        let selection_set = expanded_selection_set.flatten_unnecessary_fragments(
            ty,
            /*named_fragments*/ &Default::default(),
            &self.schema,
        )?;

        if !self.type_condition_position.is_object_type() {
            // When the type condition of the fragment is not an object type, the
            // `FieldsInSetCanMerge` rule is more restrictive and any fields can create conflicts.
            // Thus, we have to use the full validator in this case. (see
            // https://github.com/graphql/graphql-spec/issues/1085 for details.)
            return Ok(FragmentRestrictionAtType::new(
                selection_set.clone(),
                Some(FieldsConflictValidator::from_selection_set(
                    &expanded_selection_set,
                )),
            ));
        }

        // Use a smaller validator for efficiency.
        // Note that `trimmed` is the difference of 2 selections that may not have been normalized
        // on the same parent type, so in practice, it is possible that `trimmed` contains some of
        // the selections that `selectionSet` contains, but that they have been simplified in
        // `selectionSet` in such a way that the `minus` call does not see it. However, it is not
        // trivial to deal with this, and it is fine given that we use trimmed to create the
        // validator because we know the non-trimmed parts cannot create field conflict issues so
        // we're trying to build a smaller validator, but it's ok if trimmed is not as small as it
        // theoretically can be.
        let trimmed = expanded_selection_set.minus(&selection_set)?;
        let validator =
            (!trimmed.is_empty()).then(|| FieldsConflictValidator::from_selection_set(&trimmed));
        Ok(FragmentRestrictionAtType::new(
            selection_set.clone(),
            validator,
        ))
    }

    /// Checks whether `self` fragment includes the other fragment (`other_fragment_name`).
    //
    // Note that this is slightly different from `self` "using" `other_fragment` in that this
    // essentially checks if the full selection set of `other_fragment` is contained by `self`, so
    // this only look at "top-level" usages.
    //
    // Note that this is guaranteed to return `false` if passed self's name.
    // Note: This is a heuristic looking for the other named fragment used directly in the
    //       selection set. It may not return `true` even though the other fragment's selections
    //       are actually covered by self's selection set.
    // PORT_NOTE: The JS version memoizes the result of this function. But, the current Rust port
    // does not.
    fn includes(&self, other_fragment_name: &Name) -> bool {
        if self.name == *other_fragment_name {
            return false;
        }

        self.selection_set.selections.iter().any(|(_, selection)| {
            matches!(
                selection,
                Selection::FragmentSpread(fragment) if fragment.spread.fragment_name == *other_fragment_name
            )
        })
    }
}

enum FullMatchingFragmentCondition<'a> {
    ForFieldSelection,
    ForInlineFragmentSelection {
        // the type condition and directives on an inline fragment selection.
        type_condition_position: &'a CompositeTypeDefinitionPosition,
        directives: &'a DirectiveList,
    },
}

impl<'a> FullMatchingFragmentCondition<'a> {
    /// Determines whether the given fragment is allowed to match the whole selection set by itself
    /// (without another selection set wrapping it).
    fn check(&self, fragment: &Node<Fragment>) -> bool {
        match self {
            // We can never apply a fragments that has directives on it at the field level.
            Self::ForFieldSelection => fragment.directives.is_empty(),

            // To be able to use a matching inline fragment, it needs to have either no directives,
            // or if it has some, then:
            //  1. All it's directives should also be on the current element.
            //  2. The type condition of this element should be the fragment's condition. because
            // If those 2 conditions are true, we can replace the whole current inline fragment
            // with the match spread and directives will still match.
            Self::ForInlineFragmentSelection {
                type_condition_position,
                directives,
            } => {
                if fragment.directives.is_empty() {
                    return true;
                }

                // PORT_NOTE: The JS version handles `@defer` directive differently. However, Rust
                // version can't have `@defer` at this point (see comments on `enum SelectionKey`
                // definition)
                fragment.type_condition_position == **type_condition_position
                    && fragment
                        .directives
                        .iter()
                        .all(|d1| directives.iter().any(|d2| d1 == d2))
            }
        }
    }
}

/// The return type for `SelectionSet::try_optimize_with_fragments`.
#[derive(derive_more::From)]
enum SelectionSetOrFragment {
    SelectionSet(SelectionSet),
    Fragment(Node<Fragment>),
}

impl SelectionSet {
    /// Reduce the list of applicable fragments by eliminating fragments that directly include
    /// another fragment.
    //
    // We have found the list of fragments that applies to some subset of sub-selection. In
    // general, we want to now produce the selection set with spread for those fragments plus
    // any selection that is not covered by any of the fragments. For instance, suppose that
    // `subselection` is `{ a b c d e }` and we have found that `fragment F1 on X { a b c }`
    // and `fragment F2 on X { c d }` applies, then we will generate `{ ...F1 ...F2 e }`.
    //
    // In that example, `c` is covered by both fragments. And this is fine in this example as
    // it is worth using both fragments in general. A special case of this however is if a
    // fragment is entirely included into another. That is, consider that we now have `fragment
    // F1 on X { a ...F2 }` and `fragment F2 on X { b c }`. In that case, the code above would
    // still match both `F1 and `F2`, but as `F1` includes `F2` already, we really want to only
    // use `F1`. So in practice, we filter away any fragment spread that is known to be
    // included in another one that applies.
    //
    // TODO: note that the logic used for this is theoretically a bit sub-optimal. That is, we
    // only check if one of the fragment happens to directly include a spread for another
    // fragment at top-level as in the example above. We do this because it is cheap to check
    // and is likely the most common case of this kind of inclusion. But in theory, we would
    // have `fragment F1 on X { a b c }` and `fragment F2 on X { b c }`, in which case `F2` is
    // still included in `F1`, but we'd have to work harder to figure this out and it's unclear
    // it's a good tradeoff. And while you could argue that it's on the user to define its
    // fragments a bit more optimally, it's actually a tad more complex because we're looking
    // at fragments in a particular context/parent type. Consider an interface `I` and:
    // ```graphql
    //   fragment F3 on I {
    //     ... on X {
    //       a
    //     }
    //     ... on Y {
    //       b
    //       c
    //     }
    //   }
    //
    //   fragment F4 on I {
    //     ... on Y {
    //       c
    //     }
    //     ... on Z {
    //       d
    //     }
    //   }
    // ```
    // In that case, neither fragment include the other per-se. But what if we have
    // sub-selection `{ b c }` but where parent type is `Y`. In that case, both `F3` and `F4`
    // applies, and in that particular context, `F3` is fully included in `F4`. Long story
    // short, we'll currently return `{ ...F3 ...F4 }` in that case, but it would be
    // technically better to return only `F4`. However, this feels niche, and it might be
    // costly to verify such inclusions, so not doing it for now.
    fn reduce_applicable_fragments(
        applicable_fragments: &mut Vec<(Node<Fragment>, Arc<FragmentRestrictionAtType>)>,
    ) {
        // Note: It's not possible for two fragments to include each other. So, we don't need to
        //       worry about inclusion cycles.
        let included_fragments: IndexSet<Name> = applicable_fragments
            .iter()
            .filter(|(fragment, _)| {
                applicable_fragments
                    .iter()
                    .any(|(other_fragment, _)| other_fragment.includes(&fragment.name))
            })
            .map(|(fragment, _)| fragment.name.clone())
            .collect();

        applicable_fragments.retain(|(fragment, _)| !included_fragments.contains(&fragment.name));
    }

    /// Try to reuse existing fragments to optimize this selection set.
    /// Returns either
    /// - a new selection set partially optimized by re-using given `fragments`, or
    /// - a single fragment that covers the full selection set.
    // PORT_NOTE: Moved from `Selection` class in JS code to SelectionSet struct in Rust.
    // PORT_NOTE: `parent_type` argument seems always to be the same as `self.type_position`.
    // PORT_NOTE: In JS, this was called `tryOptimizeWithFragments`.
    fn try_apply_fragments(
        &self,
        parent_type: &CompositeTypeDefinitionPosition,
        context: &ReuseContext<'_>,
        validator: &mut FieldsConflictMultiBranchValidator,
        fragments_at_type: &mut FragmentRestrictionAtTypeCache,
        full_match_condition: FullMatchingFragmentCondition,
    ) -> Result<SelectionSetOrFragment, FederationError> {
        // We limit to fragments whose selection could be applied "directly" at `parent_type`,
        // meaning without taking the fragment condition into account. The idea being that if the
        // fragment condition would be needed inside `parent_type`, then that condition will not
        // have been "normalized away" and so we want for this very call to be called on the
        // fragment whose type _is_ the fragment condition (at which point, this
        // `can_apply_directly_at_type` method will apply. Also note that this is because we have
        // this restriction that calling `expanded_selection_set_at_type` is ok.
        let candidates = context
            .fragments
            .get_all_may_apply_directly_at_type(parent_type);

        // First, we check which of the candidates do apply inside the selection set, if any. If we
        // find a candidate that applies to the whole selection set, then we stop and only return
        // that one candidate. Otherwise, we cumulate in `applicable_fragments` the list of fragments
        // that applies to a subset.
        let mut applicable_fragments = Vec::new();
        for candidate in candidates {
            let candidate = candidate?;
            let at_type =
                fragments_at_type.expanded_selection_set_at_type(candidate, parent_type)?;
            if at_type.is_useless() {
                continue;
            }

            // I don't love this, but fragments may introduce new fields to the operation, including
            // fields that use variables that are not declared in the operation. There are two ways
            // to work around this: adjusting the fragments so they only list the fields that we
            // actually need, or excluding fragments that introduce variable references from reuse.
            // The former would be ideal, as we would not execute more fields than required. It's
            // also much trickier to do. The latter fixes this particular issue but leaves the
            // output in a less than ideal state.
            // The consideration here is: `generate_query_fragments` has significant advantages
            // over fragment reuse, and so we do not want to invest a lot of time into improving
            // fragment reuse. We do the simple, less-than-ideal thing.
            if let Some(variable_definitions) = &context.operation_variables {
                let fragment_variables = candidate.used_variables();
                if fragment_variables
                    .difference(variable_definitions)
                    .next()
                    .is_some()
                {
                    continue;
                }
            }

            // As we check inclusion, we ignore the case where the fragment queries __typename
            // but the `self` does not. The rational is that querying `__typename`
            // unnecessarily is mostly harmless (it always works and it's super cheap) so we
            // don't want to not use a fragment just to save querying a `__typename` in a few
            // cases. But the underlying context of why this matters is that the query planner
            // always requests __typename for abstract type, and will do so in fragments too,
            // but we can have a field that _does_ return an abstract type within a fragment,
            // but that _does not_ end up returning an abstract type when applied in a "more
            // specific" context (think a fragment on an interface I1 where a inside field
            // returns another interface I2, but applied in the context of a implementation
            // type of I1 where that particular field returns an implementation of I2 rather
            // than I2 directly; we would have added __typename to the fragment (because it's
            // all interfaces), but the selection itself, which only deals with object type,
            // may not have __typename requested; using the fragment might still be a good
            // idea, and querying __typename needlessly is a very small price to pay for that).
            let res = self.containment(
                &at_type.selections,
                ContainmentOptions {
                    ignore_missing_typename: true,
                },
            );
            match res {
                Containment::Equal if full_match_condition.check(candidate) => {
                    if !validator.check_can_reuse_fragment_and_track_it(&at_type)? {
                        // We cannot use it at all, so no point in adding to `applicable_fragments`.
                        continue;
                    }
                    // Special case: Found a fragment that covers the full selection set.
                    return Ok(candidate.clone().into());
                }
                // Note that if a fragment applies to only a subset of the sub-selections, then we
                // really only can use it if that fragment is defined _without_ directives.
                Containment::Equal | Containment::StrictlyContained
                    if candidate.directives.is_empty() =>
                {
                    applicable_fragments.push((candidate.clone(), at_type));
                }
                // Not eligible; Skip it.
                _ => (),
            }
        }

        if applicable_fragments.is_empty() {
            return Ok(self.clone().into()); // Not optimizable
        }

        // Narrow down the list of applicable fragments by removing those that are included in
        // another.
        Self::reduce_applicable_fragments(&mut applicable_fragments);

        // Build a new optimized selection set.
        let mut not_covered_so_far = self.clone();
        let mut optimized = SelectionSet::empty(self.schema.clone(), self.type_position.clone());
        for (fragment, at_type) in applicable_fragments {
            if !validator.check_can_reuse_fragment_and_track_it(&at_type)? {
                continue;
            }
            let not_covered = self.minus(&at_type.selections)?;
            not_covered_so_far = not_covered_so_far.intersection(&not_covered)?;

            // PORT_NOTE: The JS version uses `parent_type` as the "sourceType", which may be
            //            different from `fragment.type_condition_position`. But, Rust version does
            //            not have "sourceType" field for `FragmentSpreadSelection`.
            let fragment_selection = FragmentSpreadSelection::from_fragment(
                &fragment,
                /*directives*/ &Default::default(),
            );
            optimized.add_local_selection(&fragment_selection.into())?;
        }

        optimized.add_local_selection_set(&not_covered_so_far)?;
        Ok(optimized.into())
    }
}

//=============================================================================
// Retain fragments in selection sets while expanding the rest

impl Selection {
    /// Expand fragments that are not in the `fragments_to_keep`.
    // PORT_NOTE: The JS version's name was `expandFragments`, which was confusing with
    //            `expand_all_fragments`. So, it was renamed to `retain_fragments`.
    fn retain_fragments(
        &self,
        parent_type: &CompositeTypeDefinitionPosition,
        fragments_to_keep: &NamedFragments,
    ) -> Result<SelectionOrSet, FederationError> {
        match self {
            Selection::FragmentSpread(fragment) => {
                if fragments_to_keep.contains(&fragment.spread.fragment_name) {
                    // Keep this spread
                    Ok(self.clone().into())
                } else {
                    // Expand the fragment
                    let expanded_sub_selections =
                        fragment.selection_set.retain_fragments(fragments_to_keep)?;
                    if *parent_type == fragment.spread.type_condition_position
                        && fragment.spread.directives.is_empty()
                    {
                        // The fragment is of the same type as the parent, so we can just use
                        // the expanded sub-selections directly.
                        Ok(expanded_sub_selections.into())
                    } else {
                        // Create an inline fragment since type condition is necessary.
                        let inline = InlineFragmentSelection::from_selection_set(
                            parent_type.clone(),
                            expanded_sub_selections,
                            fragment.spread.directives.clone(),
                        );
                        Ok(Selection::from(inline).into())
                    }
                }
            }

            // Otherwise, expand the sub-selections.
            _ => Ok(self
                .map_selection_set(|selection_set| {
                    Ok(Some(selection_set.retain_fragments(fragments_to_keep)?))
                })?
                .into()),
        }
    }
}

// Note: `retain_fragments` methods may return a selection or a selection set.
impl From<SelectionOrSet> for SelectionMapperReturn {
    fn from(value: SelectionOrSet) -> Self {
        match value {
            SelectionOrSet::Selection(selection) => selection.into(),
            SelectionOrSet::SelectionSet(selections) => {
                // The items in a selection set needs to be cloned here, since it's sub-selections
                // are contained in an `Arc`.
                Vec::from_iter(selections.selections.values().cloned()).into()
            }
        }
    }
}

impl SelectionSet {
    /// Expand fragments that are not in the `fragments_to_keep`.
    // PORT_NOTE: The JS version's name was `expandFragments`, which was confusing with
    //            `expand_all_fragments`. So, it was renamed to `retain_fragments`.
    fn retain_fragments(
        &self,
        fragments_to_keep: &NamedFragments,
    ) -> Result<SelectionSet, FederationError> {
        self.lazy_map(fragments_to_keep, |selection| {
            Ok(selection
                .retain_fragments(&self.type_position, fragments_to_keep)?
                .into())
        })
    }
}

//=============================================================================
// Optimize (or reduce) the named fragments in the query
//
// Things to consider:
// - Unused fragment definitions can be dropped without an issue.
// - Dropping low-usage named fragments and expanding them may insert other fragments resulting in
//   increased usage of those inserted.
//
// Example:
//  ```graphql
//   query {
//      ...F1
//   }
//
//   fragment F1 {
//     a { ...F2 }
//     b { ...F2 }
//   }
//
//   fragment F2 {
//      // something
//   }
//  ```
//  then at this point where we've only counted usages in the query selection, `usages` will be
//  `{ F1: 1, F2: 0 }`. But we do not want to expand _both_ F1 and F2. Instead, we want to expand
//  F1 first, and then realize that this increases F2 usages to 2, which means we stop there and keep F2.

impl NamedFragments {
    /// Updates `self` by computing the reduced set of NamedFragments that are used in the
    /// selection set and other fragments at least `min_usage_to_optimize` times. Also, computes
    /// the new selection set that uses only the reduced set of fragments by expanding the other
    /// ones.
    /// - Returned selection set will be normalized.
    fn reduce(
        &mut self,
        selection_set: &SelectionSet,
        min_usage_to_optimize: u32,
    ) -> Result<SelectionSet, FederationError> {
        // Call `reduce_inner` repeatedly until we reach a fix-point, since newly computed
        // selection set may drop some fragment references due to normalization, which could lead
        // to further reduction.
        // - It is hard to avoid this chain reaction, since we need to account for the effects of
        //   normalization.
        let mut last_size = self.len();
        let mut last_selection_set = selection_set.clone();
        while last_size > 0 {
            let new_selection_set =
                self.reduce_inner(&last_selection_set, min_usage_to_optimize)?;

            // Reached a fix-point => stop
            if self.len() == last_size {
                // Assumes that `new_selection_set` is the same as `last_selection_set` in this
                // case.
                break;
            }

            // If we've expanded some fragments but kept others, then it's not 100% impossible that
            // some fragment was used multiple times in some expanded fragment(s), but that
            // post-expansion all of it's usages are "dead" branches that are removed by the final
            // `flatten_unnecessary_fragments`. In that case though, we need to ensure we don't include the now-unused
            // fragment in the final list of fragments.
            // TODO: remark that the same reasoning could leave a single instance of a fragment
            // usage, so if we really really want to never have less than `minUsagesToOptimize`, we
            // could do some loop of `expand then flatten` unless all fragments are provably used
            // enough. We don't bother, because leaving this is not a huge deal and it's not worth
            // the complexity, but it could be that we can refactor all this later to avoid this
            // case without additional complexity.

            // Prepare the next iteration
            last_size = self.len();
            last_selection_set = new_selection_set;
        }
        Ok(last_selection_set)
    }

    /// The inner loop body of `reduce` method.
    fn reduce_inner(
        &mut self,
        selection_set: &SelectionSet,
        min_usage_to_optimize: u32,
    ) -> Result<SelectionSet, FederationError> {
        let mut usages = selection_set.used_fragments();

        // Short-circuiting: Nothing was used => Drop everything (selection_set is unchanged).
        if usages.is_empty() {
            *self = Default::default();
            return Ok(selection_set.clone());
        }

        // Determine which one to retain.
        // - Calculate the usage count of each fragment in both query and other fragment definitions.
        //   - If a fragment is to keep, fragments used in it are counted.
        //   - If a fragment is to drop, fragments used in it are counted and multiplied by its usage.
        // - Decide in reverse dependency order, so that at each step, the fragment being visited
        //   has following properties:
        //   - It is either indirectly used by a previous fragment; Or, not used directly by any
        //     one visited & retained before.
        //   - Its usage count should be correctly calculated as if dropped fragments were expanded.
        // - We take advantage of the fact that `NamedFragments` is already sorted in dependency
        //   order.
        // PORT_NOTE: The `computeFragmentsToKeep` function is implemented here.
        let original_size = self.len();
        for fragment in self.iter_rev() {
            let usage_count = usages.get(&fragment.name).copied().unwrap_or_default();
            if usage_count >= min_usage_to_optimize {
                // Count indirect usages within the fragment definition.
                fragment.collect_used_fragment_names(&mut usages);
            } else {
                // Compute the new usage count after expanding the `fragment`.
                Self::update_usages(&mut usages, fragment, usage_count);
            }
        }

        self.retain(|name, _fragment| {
            let usage_count = usages.get(name).copied().unwrap_or_default();
            usage_count >= min_usage_to_optimize
        });

        // Short-circuiting: Nothing was dropped (fully used) => Nothing to change.
        if self.len() == original_size {
            return Ok(selection_set.clone());
        }

        // Update the fragment definitions in `self` after reduction.
        // Note: This is an unfortunate clone, since `self` can't be passed to `retain_fragments`,
        //       while being mutated.
        let fragments_to_keep = self.clone();
        for (_, fragment) in self.iter_mut() {
            Node::make_mut(fragment).selection_set = fragment
                .selection_set
                .retain_fragments(&fragments_to_keep)?
                .flatten_unnecessary_fragments(
                    &fragment.selection_set.type_position,
                    &fragments_to_keep,
                    &fragment.schema,
                )?;
        }

        // Compute the new selection set based on the new reduced set of fragments.
        // Note that optimizing all fragments to potentially re-expand some is not entirely
        // optimal, but it's unclear how to do otherwise, and it probably don't matter too much in
        // practice (we only call this optimization on the final computed query plan, so not a very
        // hot path; plus in most cases we won't even reach that point either because there is no
        // fragment, or none will have been optimized away so we'll exit above).
        let reduced_selection_set = selection_set.retain_fragments(self)?;

        // Expanding fragments could create some "inefficiencies" that we wouldn't have if we
        // hadn't re-optimized the fragments to de-optimize it later, so we do a final "flatten"
        // pass to remove those.
        reduced_selection_set.flatten_unnecessary_fragments(
            &reduced_selection_set.type_position,
            self,
            &selection_set.schema,
        )
    }

    fn update_usages(
        usages: &mut IndexMap<Name, u32>,
        fragment: &Node<Fragment>,
        usage_count: u32,
    ) {
        let mut inner_usages = IndexMap::default();
        fragment.collect_used_fragment_names(&mut inner_usages);

        for (name, inner_count) in inner_usages {
            *usages.entry(name).or_insert(0) += inner_count * usage_count;
        }
    }
}

//=============================================================================
// `reuse_fragments` methods (putting everything together)

impl Selection {
    fn reuse_fragments_inner(
        &self,
        context: &ReuseContext<'_>,
        validator: &mut FieldsConflictMultiBranchValidator,
        fragments_at_type: &mut FragmentRestrictionAtTypeCache,
    ) -> Result<Selection, FederationError> {
        match self {
            Selection::Field(field) => Ok(field
                .reuse_fragments_inner(context, validator, fragments_at_type)?
                .into()),
            Selection::FragmentSpread(_) => Ok(self.clone()), // Do nothing
            Selection::InlineFragment(inline_fragment) => Ok(inline_fragment
                .reuse_fragments_inner(context, validator, fragments_at_type)?
                .into()),
        }
    }
}

impl FieldSelection {
    fn reuse_fragments_inner(
        &self,
        context: &ReuseContext<'_>,
        validator: &mut FieldsConflictMultiBranchValidator,
        fragments_at_type: &mut FragmentRestrictionAtTypeCache,
    ) -> Result<Self, FederationError> {
        let Some(base_composite_type): Option<CompositeTypeDefinitionPosition> =
            self.field.output_base_type()?.try_into().ok()
        else {
            return Ok(self.clone());
        };
        let Some(ref selection_set) = self.selection_set else {
            return Ok(self.clone());
        };

        let mut field_validator = validator.for_field(&self.field);

        // First, see if we can reuse fragments for the selection of this field.
        let opt = selection_set.try_apply_fragments(
            &base_composite_type,
            context,
            &mut field_validator,
            fragments_at_type,
            FullMatchingFragmentCondition::ForFieldSelection,
        )?;

        let mut optimized = match opt {
            SelectionSetOrFragment::Fragment(fragment) => {
                let fragment_selection = FragmentSpreadSelection::from_fragment(
                    &fragment,
                    /*directives*/ &Default::default(),
                );
                SelectionSet::from_selection(base_composite_type, fragment_selection.into())
            }
            SelectionSetOrFragment::SelectionSet(selection_set) => selection_set,
        };
        optimized =
            optimized.reuse_fragments_inner(context, &mut field_validator, fragments_at_type)?;
        Ok(self.with_updated_selection_set(Some(optimized)))
    }
}

/// Return type for `InlineFragmentSelection::reuse_fragments`.
#[derive(derive_more::From)]
enum FragmentSelection {
    // Note: Enum variants are named to match those of `Selection`.
    InlineFragment(InlineFragmentSelection),
    FragmentSpread(FragmentSpreadSelection),
}

impl From<FragmentSelection> for Selection {
    fn from(value: FragmentSelection) -> Self {
        match value {
            FragmentSelection::InlineFragment(inline_fragment) => inline_fragment.into(),
            FragmentSelection::FragmentSpread(fragment_spread) => fragment_spread.into(),
        }
    }
}

impl InlineFragmentSelection {
    fn reuse_fragments_inner(
        &self,
        context: &ReuseContext<'_>,
        validator: &mut FieldsConflictMultiBranchValidator,
        fragments_at_type: &mut FragmentRestrictionAtTypeCache,
    ) -> Result<FragmentSelection, FederationError> {
        let optimized;

        let type_condition_position = &self.inline_fragment.type_condition_position;
        if let Some(type_condition_position) = type_condition_position {
            let opt = self.selection_set.try_apply_fragments(
                type_condition_position,
                context,
                validator,
                fragments_at_type,
                FullMatchingFragmentCondition::ForInlineFragmentSelection {
                    type_condition_position,
                    directives: &self.inline_fragment.directives,
                },
            )?;

            match opt {
                SelectionSetOrFragment::Fragment(fragment) => {
                    // We're fully matching the sub-selection. If the fragment condition is also
                    // this element condition, then we can replace the whole element by the spread
                    // (not just the sub-selection).
                    if *type_condition_position == fragment.type_condition_position {
                        // Optimized as `...<fragment>`, dropping the original inline spread (`self`).

                        // Note that `FullMatchingFragmentCondition::ForInlineFragmentSelection`
                        // above guarantees that this element directives are a superset of the
                        // fragment directives. But there can be additional directives, and in that
                        // case they should be kept on the spread.
                        // PORT_NOTE: We are assuming directives on fragment definitions are
                        //            carried over to their spread sites as JS version does, which
                        //            is handled differently in Rust version (see `FragmentSpreadData`).
                        let directives: executable::DirectiveList = self
                            .inline_fragment
                            .directives
                            .iter()
                            .filter(|d1| !fragment.directives.iter().any(|d2| *d1 == d2))
                            .cloned()
                            .collect();
                        return Ok(
                            FragmentSpreadSelection::from_fragment(&fragment, &directives).into(),
                        );
                    } else {
                        // Otherwise, we keep this element and use a sub-selection with just the spread.
                        // Optimized as `...on <type_condition_position> { ...<fragment> }`
                        optimized = SelectionSet::from_selection(
                            type_condition_position.clone(),
                            FragmentSpreadSelection::from_fragment(
                                &fragment,
                                /*directives*/ &Default::default(),
                            )
                            .into(),
                        );
                    }
                }
                SelectionSetOrFragment::SelectionSet(selection_set) => {
                    optimized = selection_set;
                }
            }
        } else {
            optimized = self.selection_set.clone();
        }

        Ok(InlineFragmentSelection::new(
            self.inline_fragment.clone(),
            // Then, recurse inside the field sub-selection (note that if we matched some fragments
            // above, this recursion will "ignore" those as `FragmentSpreadSelection`'s
            // `reuse_fragments()` is a no-op).
            optimized.reuse_fragments_inner(context, validator, fragments_at_type)?,
        )
        .into())
    }
}

impl SelectionSet {
    fn reuse_fragments_inner(
        &self,
        context: &ReuseContext<'_>,
        validator: &mut FieldsConflictMultiBranchValidator,
        fragments_at_type: &mut FragmentRestrictionAtTypeCache,
    ) -> Result<SelectionSet, FederationError> {
        self.lazy_map(context.fragments, |selection| {
            Ok(selection
                .reuse_fragments_inner(context, validator, fragments_at_type)?
                .into())
        })
    }

    fn contains_fragment_spread(&self) -> bool {
        self.iter().any(|selection| {
            matches!(selection, Selection::FragmentSpread(_))
                || selection
                    .selection_set()
                    .map(|subselection| subselection.contains_fragment_spread())
                    .unwrap_or(false)
        })
    }

    /// ## Errors
    /// Returns an error if the selection set contains a named fragment spread.
    fn reuse_fragments(&mut self, context: &ReuseContext<'_>) -> Result<(), FederationError> {
        if context.fragments.is_empty() {
            return Ok(());
        }

        if self.contains_fragment_spread() {
            return Err(FederationError::internal("reuse_fragments() must only be used on selection sets that do not contain named fragment spreads"));
        }

        // Calling reuse_fragments() will not match a fragment that would have expanded at
        // top-level. That is, say we have the selection set `{ x y }` for a top-level `Query`, and
        // we have a fragment
        // ```
        // fragment F on Query {
        //   x
        //   y
        // }
        // ```
        // then calling `self.reuse_fragments(fragments)` would only apply check if F apply to
        // `x` and then `y`.
        //
        // To ensure the fragment match in this case, we "wrap" the selection into a trivial
        // fragment of the selection parent, so in the example above, we create selection `... on
        // Query { x y }`. With that, `reuse_fragments` will correctly match on the `on Query`
        // fragment; after which we can unpack the final result.
        let wrapped = InlineFragmentSelection::from_selection_set(
            self.type_position.clone(), // parent type
            self.clone(),               // selection set
            Default::default(),         // directives
        );
        let mut validator = FieldsConflictMultiBranchValidator::from_initial_validator(
            FieldsConflictValidator::from_selection_set(self),
        );
        let optimized = wrapped.reuse_fragments_inner(
            context,
            &mut validator,
            &mut FragmentRestrictionAtTypeCache::default(),
        )?;

        // Now, it's possible we matched a full fragment, in which case `optimized` will be just
        // the named fragment, and in that case we return a singleton selection with just that.
        // Otherwise, it's our wrapping inline fragment with the sub-selections optimized, and we
        // just return that subselection.
        *self = match optimized {
            FragmentSelection::FragmentSpread(spread) => {
                SelectionSet::from_selection(self.type_position.clone(), spread.into())
            }
            FragmentSelection::InlineFragment(inline_fragment) => inline_fragment.selection_set,
        };
        Ok(())
    }
}

impl Operation {
    // PORT_NOTE: The JS version of `reuse_fragments` takes an optional `minUsagesToOptimize` argument.
    //            However, it's only used in tests. So, it's removed in the Rust version.
    const DEFAULT_MIN_USAGES_TO_OPTIMIZE: u32 = 2;

    // `fragments` - rebased fragment definitions for the operation's subgraph
    // - `self.selection_set` must be fragment-spread-free.
    fn reuse_fragments_inner(
        &mut self,
        fragments: &NamedFragments,
        min_usages_to_optimize: u32,
    ) -> Result<(), FederationError> {
        if fragments.is_empty() {
            return Ok(());
        }

        // Optimize the operation's selection set by re-using existing fragments.
        let before_optimization = self.selection_set.clone();
        self.selection_set
            .reuse_fragments(&ReuseContext::for_operation(fragments, &self.variables))?;
        if before_optimization == self.selection_set {
            return Ok(());
        }

        // Optimize the named fragment definitions by dropping low-usage ones.
        let mut final_fragments = fragments.clone();
        let final_selection_set =
            final_fragments.reduce(&self.selection_set, min_usages_to_optimize)?;

        self.selection_set = final_selection_set;
        self.named_fragments = final_fragments;
        Ok(())
    }

    /// Optimize the parsed size of the operation by applying fragment spreads. Fragment spreads
    /// are reused from the original user-provided fragments.
    ///
    /// `fragments` - rebased fragment definitions for the operation's subgraph
    ///
    // PORT_NOTE: In JS, this function was called "optimize".
    pub(crate) fn reuse_fragments(
        &mut self,
        fragments: &NamedFragments,
    ) -> Result<(), FederationError> {
        self.reuse_fragments_inner(fragments, Self::DEFAULT_MIN_USAGES_TO_OPTIMIZE)
    }

    /// Optimize the parsed size of the operation by generating fragments based on the selections
    /// in the operation.
    pub(crate) fn generate_fragments(&mut self) -> Result<(), FederationError> {
        // Currently, this method simply pulls out every inline fragment into a named fragment. If
        // multiple inline fragments are the same, they use the same named fragment.
        //
        // This method can generate named fragments that are only used once. It's not ideal, but it
        // also doesn't seem that bad. Avoiding this is possible but more work, and keeping this
        // as simple as possible is a big benefit for now.
        //
        // When we have more advanced correctness testing, we can add more features to fragment
        // generation, like factoring out partial repeated slices of selection sets or only
        // introducing named fragments for patterns that occur more than once.
        let mut generator = FragmentGenerator::default();
        generator.visit_selection_set(&mut self.selection_set)?;
        self.named_fragments = generator.into_inner();
        Ok(())
    }

    /// Used by legacy roundtrip tests.
    /// - This lowers `min_usages_to_optimize` to `1` in order to make it easier to write unit tests.
    #[cfg(test)]
    fn reuse_fragments_for_roundtrip_test(
        &mut self,
        fragments: &NamedFragments,
    ) -> Result<(), FederationError> {
        self.reuse_fragments_inner(fragments, /*min_usages_to_optimize*/ 1)
    }

    // PORT_NOTE: This mirrors the JS version's `Operation.expandAllFragments`. But this method is
    // mainly for unit tests. The actual port of `expandAllFragments` is in `normalize_operation`.
    #[cfg(test)]
    fn expand_all_fragments_and_normalize(&self) -> Result<Self, FederationError> {
        let selection_set = self
            .selection_set
            .expand_all_fragments()?
            .flatten_unnecessary_fragments(
                &self.selection_set.type_position,
                &self.named_fragments,
                &self.schema,
            )?;
        Ok(Self {
            named_fragments: Default::default(),
            selection_set,
            ..self.clone()
        })
    }
}

#[derive(Debug, Default)]
struct FragmentGenerator {
    fragments: NamedFragments,
    // XXX(@goto-bus-stop): This is temporary to support mismatch testing with JS!
    names: IndexMap<(String, usize), usize>,
}

impl FragmentGenerator {
    // XXX(@goto-bus-stop): This is temporary to support mismatch testing with JS!
    // In the future, we will just use `.next_name()`.
    fn generate_name(&mut self, frag: &InlineFragmentSelection) -> Name {
        use std::fmt::Write as _;

        let type_condition = frag
            .inline_fragment
            .type_condition_position
            .as_ref()
            .map_or_else(
                || "undefined".to_string(),
                |condition| condition.to_string(),
            );
        let selections = frag.selection_set.selections.len();
        let mut name = format!("_generated_on{type_condition}{selections}");

        let key = (type_condition, selections);
        let index = self
            .names
            .entry(key)
            .and_modify(|index| *index += 1)
            .or_default();
        _ = write!(&mut name, "_{index}");

        Name::new_unchecked(&name)
    }

    /// Is a selection set worth using for a newly generated named fragment?
    fn is_worth_using(selection_set: &SelectionSet) -> bool {
        let mut iter = selection_set.iter();
        let Some(first) = iter.next() else {
            // An empty selection is not worth using (and invalid!)
            return false;
        };
        let Selection::Field(field) = first else {
            return true;
        };
        // If there's more than one selection, or one selection with a subselection,
        // it's probably worth using
        iter.next().is_some() || field.selection_set.is_some()
    }

    /// Modify the selection set so that eligible inline fragments are moved to named fragment spreads.
    fn visit_selection_set(
        &mut self,
        selection_set: &mut SelectionSet,
    ) -> Result<(), FederationError> {
        let mut new_selection_set = SelectionSet::empty(
            selection_set.schema.clone(),
            selection_set.type_position.clone(),
        );

        for (_key, selection) in Arc::make_mut(&mut selection_set.selections).iter_mut() {
            match selection {
                SelectionValue::Field(mut field) => {
                    if let Some(selection_set) = field.get_selection_set_mut() {
                        self.visit_selection_set(selection_set)?;
                    }
                    new_selection_set
                        .add_local_selection(&Selection::Field(Arc::clone(field.get())))?;
                }
                SelectionValue::FragmentSpread(frag) => {
                    new_selection_set
                        .add_local_selection(&Selection::FragmentSpread(Arc::clone(frag.get())))?;
                }
                SelectionValue::InlineFragment(frag)
                    if !Self::is_worth_using(&frag.get().selection_set) =>
                {
                    new_selection_set
                        .add_local_selection(&Selection::InlineFragment(Arc::clone(frag.get())))?;
                }
                SelectionValue::InlineFragment(mut candidate) => {
                    self.visit_selection_set(candidate.get_selection_set_mut())?;

                    let directives = &candidate.get().inline_fragment.directives;
                    let skip_include = directives
                        .iter()
                        .map(|directive| match directive.name.as_str() {
                            "skip" | "include" => Ok(directive.clone()),
                            _ => Err(()),
                        })
                        .collect::<Result<executable::DirectiveList, _>>();

                    // If there are any directives *other* than @skip and @include,
                    // we can't just transfer them to the generated fragment spread,
                    // so we have to keep this inline fragment.
                    let Ok(skip_include) = skip_include else {
                        new_selection_set.add_local_selection(&Selection::InlineFragment(
                            Arc::clone(candidate.get()),
                        ))?;
                        continue;
                    };

                    // XXX(@goto-bus-stop): This is temporary to support mismatch testing with JS!
                    // JS does not special-case @skip and @include. It never extracts a fragment if
                    // there's any directives on it. This code duplicates the body from the
                    // previous condition so it's very easy to remove when we're ready :)
                    if !skip_include.is_empty() {
                        new_selection_set.add_local_selection(&Selection::InlineFragment(
                            Arc::clone(candidate.get()),
                        ))?;
                        continue;
                    }

                    let existing = self.fragments.iter().find(|existing| {
                        existing.type_condition_position
                            == candidate.get().inline_fragment.casted_type()
                            && existing.selection_set == candidate.get().selection_set
                    });

                    let existing = if let Some(existing) = existing {
                        existing
                    } else {
                        // XXX(@goto-bus-stop): This is temporary to support mismatch testing with JS!
                        // This should be reverted to `self.next_name();` when we're ready.
                        let name = self.generate_name(candidate.get());
                        self.fragments.insert(Fragment {
                            schema: selection_set.schema.clone(),
                            name: name.clone(),
                            type_condition_position: candidate.get().inline_fragment.casted_type(),
                            directives: Default::default(),
                            selection_set: candidate.get().selection_set.clone(),
                        });
                        self.fragments.get(&name).unwrap()
                    };
                    new_selection_set.add_local_selection(&Selection::from(
                        FragmentSpreadSelection {
                            spread: FragmentSpread::new(FragmentSpreadData {
                                schema: selection_set.schema.clone(),
                                fragment_name: existing.name.clone(),
                                type_condition_position: existing.type_condition_position.clone(),
                                directives: skip_include.into(),
                                fragment_directives: existing.directives.clone(),
                                selection_id: crate::operation::SelectionId::new(),
                            }),
                            selection_set: existing.selection_set.clone(),
                        },
                    ))?;
                }
            }
        }

        *selection_set = new_selection_set;

        Ok(())
    }

    /// Consumes the generator and returns the fragments it generated.
    fn into_inner(self) -> NamedFragments {
        self.fragments
    }
}

//=============================================================================
// Tests

#[cfg(test)]
mod tests {
    use apollo_compiler::ExecutableDocument;

    use super::*;
    use crate::operation::tests::*;

    macro_rules! assert_without_fragments {
        ($operation: expr, @$expected: literal) => {{
            let without_fragments = $operation.expand_all_fragments_and_normalize().unwrap();
            insta::assert_snapshot!(without_fragments, @$expected);
            without_fragments
        }};
    }

    macro_rules! assert_optimized {
        ($operation: expr, $named_fragments: expr, @$expected: literal) => {{
            let mut optimized = $operation.clone();
            optimized.reuse_fragments(&$named_fragments).unwrap();
            validate_operation(&$operation.schema, &optimized.to_string());
            insta::assert_snapshot!(optimized, @$expected)
        }};
    }

    /// Returns a consistent GraphQL name for the given index.
    fn fragment_name(mut index: usize) -> Name {
        /// https://spec.graphql.org/draft/#NameContinue
        const NAME_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_";
        /// https://spec.graphql.org/draft/#NameStart
        const NAME_START_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_";

        if index < NAME_START_CHARS.len() {
            Name::new_static_unchecked(&NAME_START_CHARS[index..index + 1])
        } else {
            let mut s = String::new();

            let i = index % NAME_START_CHARS.len();
            s.push(NAME_START_CHARS.as_bytes()[i].into());
            index /= NAME_START_CHARS.len();

            while index > 0 {
                let i = index % NAME_CHARS.len();
                s.push(NAME_CHARS.as_bytes()[i].into());
                index /= NAME_CHARS.len();
            }

            Name::new_unchecked(&s)
        }
    }

    #[test]
    fn generated_fragment_names() {
        assert_eq!(fragment_name(0), "a");
        assert_eq!(fragment_name(100), "Vb");
        assert_eq!(fragment_name(usize::MAX), "oS5Uz8g3Iqw");
    }

    #[test]
    fn duplicate_fragment_spreads_after_fragment_expansion() {
        // This is a regression test for FED-290, making sure `make_select` method can handle
        // duplicate fragment spreads.
        // During optimization, `make_selection` may merge multiple fragment spreads with the same
        // key. This can happen in the case below where `F1` and `F2` are expanded and generating
        // two duplicate `F_shared` spreads in the definition of `fragment F_target`.
        let schema_doc = r#"
            type Query {
                t: T
                t2: T
            }

            type T {
                id: ID!
                a: Int!
                b: Int!
                c: Int!
            }
        "#;

        let query = r#"
            fragment F_shared on T {
                id
                a
            }
            fragment F1 on T {
                ...F_shared
                b
            }

            fragment F2 on T {
                ...F_shared
                c
            }

            fragment F_target on T {
                ...F1
                ...F2
            }

            query {
                t {
                    ...F_target
                }
                t2 {
                    ...F_target
                }
            }
        "#;

        let operation = parse_operation(&parse_schema(schema_doc), query);
        let expanded = operation.expand_all_fragments_and_normalize().unwrap();
        assert_optimized!(expanded, operation.named_fragments, @r###"
        fragment F_target on T {
          id
          a
          b
          c
        }

        {
          t {
            ...F_target
          }
          t2 {
            ...F_target
          }
        }
        "###);
    }

    #[test]
    fn optimize_fragments_using_other_fragments_when_possible() {
        let schema = r#"
              type Query {
                t: I
              }

              interface I {
                b: Int
                u: U
              }

              type T1 implements I {
                a: Int
                b: Int
                u: U
              }

              type T2 implements I {
                x: String
                y: String
                b: Int
                u: U
              }

              union U = T1 | T2
        "#;

        let query = r#"
              fragment OnT1 on T1 {
                a
                b
              }

              fragment OnT2 on T2 {
                x
                y
              }

              fragment OnI on I {
                b
              }

              fragment OnU on U {
                ...OnI
                ...OnT1
                ...OnT2
              }

              query {
                t {
                  ...OnT1
                  ...OnT2
                  ...OnI
                  u {
                    ...OnU
                  }
                }
              }
        "#;

        let operation = parse_operation(&parse_schema(schema), query);

        let expanded = assert_without_fragments!(
            operation,
            @r###"
        {
          t {
            ... on T1 {
              a
              b
            }
            ... on T2 {
              x
              y
            }
            b
            u {
              ... on I {
                b
              }
              ... on T1 {
                a
                b
              }
              ... on T2 {
                x
                y
              }
            }
          }
        }
        "###
        );

        assert_optimized!(expanded, operation.named_fragments, @r###"
              fragment OnU on U {
                ... on I {
                  b
                }
                ... on T1 {
                  a
                  b
                }
                ... on T2 {
                  x
                  y
                }
              }

              {
                t {
                  ...OnU
                  u {
                    ...OnU
                  }
                }
              }
        "###);
    }

    #[test]
    fn handles_fragments_using_other_fragments() {
        let schema = r#"
              type Query {
                t: I
              }

              interface I {
                b: Int
                c: Int
                u1: U
                u2: U
              }

              type T1 implements I {
                a: Int
                b: Int
                c: Int
                me: T1
                u1: U
                u2: U
              }

              type T2 implements I {
                x: String
                y: String
                b: Int
                c: Int
                u1: U
                u2: U
              }

              union U = T1 | T2
        "#;

        let query = r#"
              fragment OnT1 on T1 {
                a
                b
              }

              fragment OnT2 on T2 {
                x
                y
              }

              fragment OnI on I {
                b
                c
              }

              fragment OnU on U {
                ...OnI
                ...OnT1
                ...OnT2
              }

              query {
                t {
                  ...OnT1
                  ...OnT2
                  u1 {
                    ...OnU
                  }
                  u2 {
                    ...OnU
                  }
                  ... on T1 {
                    me {
                      ...OnI
                    }
                  }
                }
              }
        "#;

        let operation = parse_operation(&parse_schema(schema), query);

        let expanded = assert_without_fragments!(
            &operation,
            @r###"
              {
                t {
                  ... on T1 {
                    a
                    b
                    me {
                      b
                      c
                    }
                  }
                  ... on T2 {
                    x
                    y
                  }
                  u1 {
                    ... on I {
                      b
                      c
                    }
                    ... on T1 {
                      a
                      b
                    }
                    ... on T2 {
                      x
                      y
                    }
                  }
                  u2 {
                    ... on I {
                      b
                      c
                    }
                    ... on T1 {
                      a
                      b
                    }
                    ... on T2 {
                      x
                      y
                    }
                  }
                }
              }
        "###);

        // We should reuse and keep all fragments, because 1) onU is used twice and 2)
        // all the other ones are used once in the query, and once in onU definition.
        assert_optimized!(expanded, operation.named_fragments, @r###"
              fragment OnT1 on T1 {
                a
                b
              }

              fragment OnT2 on T2 {
                x
                y
              }

              fragment OnI on I {
                b
                c
              }

              fragment OnU on U {
                ...OnI
                ...OnT1
                ...OnT2
              }

              {
                t {
                  ... on T1 {
                    ...OnT1
                    me {
                      ...OnI
                    }
                  }
                  ...OnT2
                  u1 {
                    ...OnU
                  }
                  u2 {
                    ...OnU
                  }
                }
              }
        "###);
    }

    macro_rules! test_fragments_roundtrip {
        ($schema_doc: expr, $query: expr, @$expanded: literal) => {{
            let schema = parse_schema($schema_doc);
            let operation = parse_operation(&schema, $query);
            let without_fragments = operation.expand_all_fragments_and_normalize().unwrap();
            insta::assert_snapshot!(without_fragments, @$expanded);

            let mut optimized = without_fragments;
            optimized.reuse_fragments(&operation.named_fragments).unwrap();
            validate_operation(&operation.schema, &optimized.to_string());
            assert_eq!(optimized.to_string(), operation.to_string());
        }};
    }

    /// Tests ported from JS codebase rely on special behavior of
    /// `Operation::reuse_fragments_for_roundtrip_test` that is specific for testing, since it makes it
    /// easier to write tests.
    macro_rules! test_fragments_roundtrip_legacy {
        ($schema_doc: expr, $query: expr, @$expanded: literal) => {{
            let schema = parse_schema($schema_doc);
            let operation = parse_operation(&schema, $query);
            let without_fragments = operation.expand_all_fragments_and_normalize().unwrap();
            insta::assert_snapshot!(without_fragments, @$expanded);

            let mut optimized = without_fragments;
            optimized.reuse_fragments_for_roundtrip_test(&operation.named_fragments).unwrap();
            validate_operation(&operation.schema, &optimized.to_string());
            assert_eq!(optimized.to_string(), operation.to_string());
        }};
    }

    #[test]
    fn handles_fragments_with_nested_selections() {
        let schema_doc = r#"
              type Query {
                t1a: T1
                t2a: T1
              }

              type T1 {
                t2: T2
              }

              type T2 {
                x: String
                y: String
              }
        "#;

        let query = r#"
                fragment OnT1 on T1 {
                  t2 {
                    x
                  }
                }

                query {
                  t1a {
                    ...OnT1
                    t2 {
                      y
                    }
                  }
                  t2a {
                    ...OnT1
                  }
                }
        "#;

        test_fragments_roundtrip!(schema_doc, query, @r###"
                {
                  t1a {
                    t2 {
                      x
                      y
                    }
                  }
                  t2a {
                    t2 {
                      x
                    }
                  }
                }
        "###);
    }

    #[test]
    fn handles_nested_fragments_with_field_intersection() {
        let schema_doc = r#"
            type Query {
                t: T
            }

            type T {
                a: A
                b: Int
            }

            type A {
                x: String
                y: String
                z: String
            }
        "#;

        // The subtlety here is that `FA` contains `__typename` and so after we're reused it, the
        // selection will look like:
        // {
        //   t {
        //     a {
        //       ...FA
        //     }
        //   }
        // }
        // But to recognize that `FT` can be reused from there, we need to be able to see that
        // the `__typename` that `FT` wants is inside `FA` (and since FA applies on the parent type `A`
        // directly, it is fine to reuse).
        let query = r#"
            fragment FA on A {
                __typename
                x
                y
            }

            fragment FT on T {
                a {
                __typename
                ...FA
                }
            }

            query {
                t {
                ...FT
                }
            }
        "#;

        test_fragments_roundtrip_legacy!(schema_doc, query, @r###"
        {
          t {
            a {
              __typename
              x
              y
            }
          }
        }
        "###);
    }

    #[test]
    fn handles_fragment_matching_subset_of_field_selection() {
        let schema_doc = r#"
              type Query {
                t: T
              }

              type T {
                a: String
                b: B
                c: Int
                d: D
              }

              type B {
                x: String
                y: String
              }

              type D {
                m: String
                n: String
              }
        "#;

        let query = r#"
                fragment FragT on T {
                  b {
                    __typename
                    x
                  }
                  c
                  d {
                    m
                  }
                }

                {
                  t {
                    ...FragT
                    d {
                      n
                    }
                    a
                  }
                }
        "#;

        test_fragments_roundtrip_legacy!(schema_doc, query, @r###"
                {
                  t {
                    b {
                      __typename
                      x
                    }
                    c
                    d {
                      m
                      n
                    }
                    a
                  }
                }
        "###);
    }

    #[test]
    fn handles_fragment_matching_subset_of_inline_fragment_selection() {
        // Pretty much the same test than the previous one, but matching inside a fragment selection inside
        // of inside a field selection.
        // PORT_NOTE: ` implements I` was added in the definition of `type T`, so that validation can pass.
        let schema_doc = r#"
          type Query {
            i: I
          }

          interface I {
            a: String
          }

          type T implements I {
            a: String
            b: B
            c: Int
            d: D
          }

          type B {
            x: String
            y: String
          }

          type D {
            m: String
            n: String
          }
        "#;

        let query = r#"
            fragment FragT on T {
              b {
                __typename
                x
              }
              c
              d {
                m
              }
            }

            {
              i {
                ... on T {
                  ...FragT
                  d {
                    n
                  }
                  a
                }
              }
            }
        "#;

        test_fragments_roundtrip_legacy!(schema_doc, query, @r###"
            {
              i {
                ... on T {
                  b {
                    __typename
                    x
                  }
                  c
                  d {
                    m
                    n
                  }
                  a
                }
              }
            }
        "###);
    }

    #[test]
    fn intersecting_fragments() {
        let schema_doc = r#"
              type Query {
                t: T
              }

              type T {
                a: String
                b: B
                c: Int
                d: D
              }

              type B {
                x: String
                y: String
              }

              type D {
                m: String
                n: String
              }
        "#;

        // Note: the code that reuse fragments iterates on fragments in the order they are defined
        // in the document, but when it reuse a fragment, it puts it at the beginning of the
        // selection (somewhat random, it just feel often easier to read), so the net effect on
        // this example is that `Frag2`, which will be reused after `Frag1` will appear first in
        // the re-optimized selection. So we put it first in the input too so that input and output
        // actually match (the `testFragmentsRoundtrip` compares strings, so it is sensible to
        // ordering; we could theoretically use `Operation.equals` instead of string equality,
        // which wouldn't really on ordering, but `Operation.equals` is not entirely trivial and
        // comparing strings make problem a bit more obvious).
        let query = r#"
                fragment Frag1 on T {
                  b {
                    x
                  }
                  c
                  d {
                    m
                  }
                }

                fragment Frag2 on T {
                  a
                  b {
                    __typename
                    x
                  }
                  d {
                    m
                    n
                  }
                }

                {
                  t {
                    ...Frag1
                    ...Frag2
                  }
                }
        "#;

        // PORT_NOTE: `__typename` and `x`'s placements are switched in Rust.
        test_fragments_roundtrip_legacy!(schema_doc, query, @r###"
                {
                  t {
                    b {
                      __typename
                      x
                    }
                    c
                    d {
                      m
                      n
                    }
                    a
                  }
                }
        "###);
    }

    #[test]
    fn fragments_application_makes_type_condition_trivial() {
        let schema_doc = r#"
              type Query {
                t: T
              }

              interface I {
                x: String
              }

              type T implements I {
                x: String
                a: String
              }
        "#;

        let query = r#"
                fragment FragI on I {
                  x
                  ... on T {
                    a
                  }
                }

                {
                  t {
                    ...FragI
                  }
                }
        "#;

        test_fragments_roundtrip_legacy!(schema_doc, query, @r###"
                {
                  t {
                    x
                    a
                  }
                }
        "###);
    }

    #[test]
    fn handles_fragment_matching_at_the_top_level_of_another_fragment() {
        let schema_doc = r#"
              type Query {
                t: T
              }

              type T {
                a: String
                u: U
              }

              type U {
                x: String
                y: String
              }
        "#;

        let query = r#"
                fragment Frag1 on T {
                  a
                }

                fragment Frag2 on T {
                  u {
                    x
                    y
                  }
                  ...Frag1
                }

                fragment Frag3 on Query {
                  t {
                    ...Frag2
                  }
                }

                {
                  ...Frag3
                }
        "#;

        test_fragments_roundtrip_legacy!(schema_doc, query, @r###"
                {
                  t {
                    u {
                      x
                      y
                    }
                    a
                  }
                }
        "###);
    }

    #[test]
    fn handles_fragments_used_in_context_where_they_get_trimmed() {
        let schema_doc = r#"
              type Query {
                t1: T1
              }

              interface I {
                x: Int
              }

              type T1 implements I {
                x: Int
                y: Int
              }

              type T2 implements I {
                x: Int
                z: Int
              }
        "#;

        let query = r#"
                fragment FragOnI on I {
                  ... on T1 {
                    y
                  }
                  ... on T2 {
                    z
                  }
                }

                {
                  t1 {
                    ...FragOnI
                  }
                }
        "#;

        test_fragments_roundtrip_legacy!(schema_doc, query, @r###"
                {
                  t1 {
                    y
                  }
                }
        "###);
    }

    #[test]
    fn handles_fragments_used_in_the_context_of_non_intersecting_abstract_types() {
        let schema_doc = r#"
              type Query {
                i2: I2
              }

              interface I1 {
                x: Int
              }

              interface I2 {
                y: Int
              }

              interface I3 {
                z: Int
              }

              type T1 implements I1 & I2 {
                x: Int
                y: Int
              }

              type T2 implements I1 & I3 {
                x: Int
                z: Int
              }
        "#;

        let query = r#"
                fragment FragOnI1 on I1 {
                  ... on I2 {
                    y
                  }
                  ... on I3 {
                    z
                  }
                }

                {
                  i2 {
                    ...FragOnI1
                  }
                }
        "#;

        test_fragments_roundtrip_legacy!(schema_doc, query, @r###"
                {
                  i2 {
                    ... on I1 {
                      ... on I2 {
                        y
                      }
                      ... on I3 {
                        z
                      }
                    }
                  }
                }
        "###);
    }

    #[test]
    fn handles_fragments_on_union_in_context_with_limited_intersection() {
        let schema_doc = r#"
              type Query {
                t1: T1
              }

              union U = T1 | T2

              type T1 {
                x: Int
              }

              type T2 {
                y: Int
              }
        "#;

        let query = r#"
                fragment OnU on U {
                  ... on T1 {
                    x
                  }
                  ... on T2 {
                    y
                  }
                }

                {
                  t1 {
                    ...OnU
                  }
                }
        "#;

        test_fragments_roundtrip_legacy!(schema_doc, query, @r###"
                {
                  t1 {
                    x
                  }
                }
        "###);
    }

    #[test]
    fn off_by_1_error() {
        let schema = r#"
              type Query {
                t: T
              }
              type T {
                id: String!
                a: A
                v: V
              }
              type A {
                id: String!
              }
              type V {
                t: T!
              }
        "#;

        let query = r#"
              {
                t {
                  ...TFrag
                  v {
                    t {
                      id
                      a {
                        __typename
                        id
                      }
                    }
                  }
                }
              }

              fragment TFrag on T {
                __typename
                id
              }
        "#;

        let operation = parse_operation(&parse_schema(schema), query);

        let expanded = assert_without_fragments!(
            operation,
            @r###"
              {
                t {
                  __typename
                  id
                  v {
                    t {
                      id
                      a {
                        __typename
                        id
                      }
                    }
                  }
                }
              }
            "###
        );

        assert_optimized!(expanded, operation.named_fragments, @r###"
        fragment TFrag on T {
          __typename
          id
        }

        {
          t {
            ...TFrag
            v {
              t {
                ...TFrag
                a {
                  __typename
                  id
                }
              }
            }
          }
        }
        "###);
    }

    #[test]
    fn removes_all_unused_fragments() {
        let schema = r#"
              type Query {
                t1: T1
              }

              union U1 = T1 | T2 | T3
              union U2 =      T2 | T3

              type T1 {
                x: Int
              }

              type T2 {
                y: Int
              }

              type T3 {
                z: Int
              }
        "#;

        let query = r#"
              query {
                t1 {
                  ...Outer
                }
              }

              fragment Outer on U1 {
                ... on T1 {
                  x
                }
                ... on T2 {
                  ... Inner
                }
                ... on T3 {
                  ... Inner
                }
              }

              fragment Inner on U2 {
                ... on T2 {
                  y
                }
              }
        "#;

        let operation = parse_operation(&parse_schema(schema), query);

        let expanded = assert_without_fragments!(
            operation,
            @r###"
              {
                t1 {
                  x
                }
              }
            "###
        );

        // This is a bit of contrived example, but the reusing code will be able
        // to figure out that the `Outer` fragment can be reused and will initially
        // do so, but it's only use once, so it will expand it, which yields:
        // {
        //   t1 {
        //     ... on T1 {
        //       x
        //     }
        //     ... on T2 {
        //       ... Inner
        //     }
        //     ... on T3 {
        //       ... Inner
        //     }
        //   }
        // }
        // and so `Inner` will not be expanded (it's used twice). Except that
        // the `flatten_unnecessary_fragments` code is apply then and will _remove_ both instances
        // of `.... Inner`. Which is ok, but we must make sure the fragment
        // itself is removed since it is not used now, which this test ensures.
        assert_optimized!(expanded, operation.named_fragments, @r###"
              {
                t1 {
                  x
                }
              }
        "###);
    }

    #[test]
    fn removes_fragments_only_used_by_unused_fragments() {
        // Similar to the previous test, but we artificially add a
        // fragment that is only used by the fragment that is finally
        // unused.
        let schema = r#"
              type Query {
                t1: T1
              }

              union U1 = T1 | T2 | T3
              union U2 =      T2 | T3

              type T1 {
                x: Int
              }

              type T2 {
                y1: Y
                y2: Y
              }

              type T3 {
                z: Int
              }

              type Y {
                v: Int
              }
        "#;

        let query = r#"
              query {
                t1 {
                  ...Outer
                }
              }

              fragment Outer on U1 {
                ... on T1 {
                  x
                }
                ... on T2 {
                  ... Inner
                }
                ... on T3 {
                  ... Inner
                }
              }

              fragment Inner on U2 {
                ... on T2 {
                  y1 {
                    ...WillBeUnused
                  }
                  y2 {
                    ...WillBeUnused
                  }
                }
              }

              fragment WillBeUnused on Y {
                v
              }
        "#;

        let operation = parse_operation(&parse_schema(schema), query);

        let expanded = assert_without_fragments!(
            operation,
            @r###"
              {
                t1 {
                  x
                }
              }
            "###
        );

        assert_optimized!(expanded, operation.named_fragments, @r###"
              {
                t1 {
                  x
                }
              }
        "###);
    }

    #[test]
    fn keeps_fragments_used_by_other_fragments() {
        let schema = r#"
              type Query {
                t1: T
                t2: T
              }

              type T {
                a1: Int
                a2: Int
                b1: B
                b2: B
              }

              type B {
                x: Int
                y: Int
              }
        "#;

        let query = r#"
              query {
                t1 {
                  ...TFields
                }
                t2 {
                  ...TFields
                }
              }

              fragment TFields on T {
                ...DirectFieldsOfT
                b1 {
                  ...BFields
                }
                b2 {
                  ...BFields
                }
              }

              fragment DirectFieldsOfT on T {
                a1
                a2
              }

              fragment BFields on B {
                x
                y
              }
        "#;

        let operation = parse_operation(&parse_schema(schema), query);

        let expanded = assert_without_fragments!(
            operation,
            @r###"
              {
                t1 {
                  a1
                  a2
                  b1 {
                    x
                    y
                  }
                  b2 {
                    x
                    y
                  }
                }
                t2 {
                  a1
                  a2
                  b1 {
                    x
                    y
                  }
                  b2 {
                    x
                    y
                  }
                }
              }
            "###
        );

        // The `DirectFieldsOfT` fragments should not be kept as it is used only once within `TFields`,
        // but the `BFields` one should be kept.
        assert_optimized!(expanded, operation.named_fragments, @r###"
        fragment BFields on B {
          x
          y
        }

        fragment TFields on T {
          a1
          a2
          b1 {
            ...BFields
          }
          b2 {
            ...BFields
          }
        }

        {
          t1 {
            ...TFields
          }
          t2 {
            ...TFields
          }
        }
        "###);
    }

    ///
    /// applied directives
    ///

    #[test]
    fn reuse_fragments_with_same_directive_in_the_fragment_selection() {
        let schema_doc = r#"
                type Query {
                  t1: T
                  t2: T
                  t3: T
                }

                type T {
                  a: Int
                  b: Int
                  c: Int
                  d: Int
                }
        "#;

        let query = r#"
                  fragment DirectiveInDef on T {
                    a @include(if: $cond1)
                  }

                  query myQuery($cond1: Boolean!, $cond2: Boolean!) {
                    t1 {
                      a
                    }
                    t2 {
                      ...DirectiveInDef
                    }
                    t3 {
                      a @include(if: $cond2)
                    }
                  }
        "#;

        test_fragments_roundtrip_legacy!(schema_doc, query, @r###"
                  query myQuery($cond1: Boolean!, $cond2: Boolean!) {
                    t1 {
                      a
                    }
                    t2 {
                      a @include(if: $cond1)
                    }
                    t3 {
                      a @include(if: $cond2)
                    }
                  }
        "###);
    }

    #[test]
    fn reuse_fragments_with_directives_on_inline_fragments() {
        let schema_doc = r#"
                type Query {
                  t1: T
                  t2: T
                  t3: T
                }

                type T {
                  a: Int
                  b: Int
                  c: Int
                  d: Int
                }
        "#;

        let query = r#"
                  fragment NoDirectiveDef on T {
                    a
                  }

                  query myQuery($cond1: Boolean!) {
                    t1 {
                      ...NoDirectiveDef
                    }
                    t2 {
                      ...NoDirectiveDef @include(if: $cond1)
                    }
                  }
        "#;

        test_fragments_roundtrip!(schema_doc, query, @r###"
                  query myQuery($cond1: Boolean!) {
                    t1 {
                      a
                    }
                    t2 {
                      ... on T @include(if: $cond1) {
                        a
                      }
                    }
                  }
        "###);
    }

    #[test]
    fn reuse_fragments_with_directive_on_typename() {
        let schema = r#"
            type Query {
              t1: T
              t2: T
              t3: T
            }

            type T {
              a: Int
              b: Int
              c: Int
              d: Int
            }
        "#;
        let query = r#"
            query A ($if: Boolean!) {
              t1 { b a ...x }
              t2 { ...x }
            }
            query B {
              # Because this inline fragment is exactly the same shape as `x`,
              # except for a `__typename` field, it may be tempting to reuse it.
              # But `x.__typename` has a directive with a variable, and this query
              # does not have that variable declared, so it can't be used.
              t3 { ... on T { a c } }
            }
            fragment x on T {
                __typename @include(if: $if)
                a
                c
            }
        "#;
        let schema = parse_schema(schema);
        let query = ExecutableDocument::parse_and_validate(schema.schema(), query, "query.graphql")
            .unwrap();

        let operation_a =
            Operation::from_operation_document(schema.clone(), &query, Some("A")).unwrap();
        let operation_b =
            Operation::from_operation_document(schema.clone(), &query, Some("B")).unwrap();
        let expanded_b = operation_b.expand_all_fragments_and_normalize().unwrap();

        assert_optimized!(expanded_b, operation_a.named_fragments, @r###"
        query B {
          t3 {
            a
            c
          }
        }
        "###);
    }

    #[test]
    fn reuse_fragments_with_non_intersecting_types() {
        let schema = r#"
            type Query {
              t: T
              s: S
              s2: S
              i: I
            }

            interface I {
                a: Int
                b: Int
            }

            type T implements I {
              a: Int
              b: Int

              c: Int
              d: Int
            }
            type S implements I {
              a: Int
              b: Int

              f: Int
              g: Int
            }
        "#;
        let query = r#"
            query A ($if: Boolean!) {
              t { ...x }
              s { ...x }
              i { ...x }
            }
            query B {
              s {
                # this matches fragment x once it is flattened,
                # because the `...on T` condition does not intersect with our
                # current type `S`
                __typename
                a b
              }
              s2 {
                # same snippet to get it to use the fragment
                __typename
                a b
              }
            }
            fragment x on I {
                __typename
                a
                b
                ... on T { c d @include(if: $if) }
            }
        "#;
        let schema = parse_schema(schema);
        let query = ExecutableDocument::parse_and_validate(schema.schema(), query, "query.graphql")
            .unwrap();

        let operation_a =
            Operation::from_operation_document(schema.clone(), &query, Some("A")).unwrap();
        let operation_b =
            Operation::from_operation_document(schema.clone(), &query, Some("B")).unwrap();
        let expanded_b = operation_b.expand_all_fragments_and_normalize().unwrap();

        assert_optimized!(expanded_b, operation_a.named_fragments, @r###"
        query B {
          s {
            __typename
            a
            b
          }
          s2 {
            __typename
            a
            b
          }
        }
        "###);
    }

    ///
    /// empty branches removal
    ///

    mod test_empty_branch_removal {
        use apollo_compiler::name;

        use super::*;
        use crate::operation::SelectionKey;

        const TEST_SCHEMA_FOR_EMPTY_BRANCH_REMOVAL: &str = r#"
            type Query {
                t: T
                u: Int
            }

            type T {
                a: Int
                b: Int
                c: C
            }

            type C {
                x: String
                y: String
            }
        "#;

        fn operation_without_empty_branches(operation: &Operation) -> Option<String> {
            operation
                .selection_set
                .without_empty_branches()
                .unwrap()
                .map(|s| s.to_string())
        }

        fn without_empty_branches(query: &str) -> Option<String> {
            let operation =
                parse_operation(&parse_schema(TEST_SCHEMA_FOR_EMPTY_BRANCH_REMOVAL), query);
            operation_without_empty_branches(&operation)
        }

        // To test `without_empty_branches` method, we need to test operations with empty selection
        // sets. However, such operations can't be constructed from strings, since the parser will
        // reject them. Thus, we first create a valid query with non-empty selection sets and then
        // clear some of them.
        // PORT_NOTE: The JS tests use `astSSet` function to construct queries with
        // empty selection sets using graphql-js's SelectionSetNode API. In Rust version,
        // instead of re-creating such API, we will selectively clear selection sets.

        fn clear_selection_set_at_path(
            ss: &mut SelectionSet,
            path: &[Name],
        ) -> Result<(), FederationError> {
            match path.split_first() {
                None => {
                    // Base case
                    Arc::make_mut(&mut ss.selections).clear();
                    Ok(())
                }

                Some((first, rest)) => {
                    let result = Arc::make_mut(&mut ss.selections).get_mut(&SelectionKey::Field {
                        response_name: (*first).clone(),
                        directives: Default::default(),
                    });
                    let Some(mut value) = result else {
                        return Err(FederationError::internal("No matching field found"));
                    };
                    match value.get_selection_set_mut() {
                        None => Err(FederationError::internal(
                            "Sub-selection expected, but not found.",
                        )),
                        Some(sub_selection_set) => {
                            // Recursive case
                            clear_selection_set_at_path(sub_selection_set, rest)?;
                            Ok(())
                        }
                    }
                }
            }
        }

        #[test]
        fn operation_not_modified_if_no_empty_branches() {
            let test_vec = vec!["{ t { a } }", "{ t { a b } }", "{ t { a c { x y } } }"];
            for query in test_vec {
                assert_eq!(without_empty_branches(query).unwrap(), query);
            }
        }

        #[test]
        fn removes_simple_empty_branches() {
            {
                // query to test: "{ t { a c { } } }"
                let expected = "{ t { a } }";

                // Since the parser won't accept empty selection set, we first create
                // a valid query and then clear the selection set.
                let valid_query = r#"{ t { a c { x } } }"#;
                let mut operation = parse_operation(
                    &parse_schema(TEST_SCHEMA_FOR_EMPTY_BRANCH_REMOVAL),
                    valid_query,
                );
                clear_selection_set_at_path(
                    &mut operation.selection_set,
                    &[name!("t"), name!("c")],
                )
                .unwrap();
                // Note: Unfortunately, this assertion won't work since SelectionSet.to_string() can't
                // display empty selection set.
                // assert_eq!(operation.selection_set.to_string(), "{ t { a c { } } }");
                assert_eq!(
                    operation_without_empty_branches(&operation).unwrap(),
                    expected
                );
            }

            {
                // query to test: "{ t { c { } a } }"
                let expected = "{ t { a } }";

                let valid_query = r#"{ t { c { x } a } }"#;
                let mut operation = parse_operation(
                    &parse_schema(TEST_SCHEMA_FOR_EMPTY_BRANCH_REMOVAL),
                    valid_query,
                );
                clear_selection_set_at_path(
                    &mut operation.selection_set,
                    &[name!("t"), name!("c")],
                )
                .unwrap();
                assert_eq!(
                    operation_without_empty_branches(&operation).unwrap(),
                    expected
                );
            }

            {
                // query to test: "{ t { } }"
                let expected = None;

                let valid_query = r#"{ t { a } }"#;
                let mut operation = parse_operation(
                    &parse_schema(TEST_SCHEMA_FOR_EMPTY_BRANCH_REMOVAL),
                    valid_query,
                );
                clear_selection_set_at_path(&mut operation.selection_set, &[name!("t")]).unwrap();
                assert_eq!(operation_without_empty_branches(&operation), expected);
            }
        }

        #[test]
        fn removes_cascading_empty_branches() {
            {
                // query to test: "{ t { c { } } }"
                let expected = None;

                let valid_query = r#"{ t { c { x } } }"#;
                let mut operation = parse_operation(
                    &parse_schema(TEST_SCHEMA_FOR_EMPTY_BRANCH_REMOVAL),
                    valid_query,
                );
                clear_selection_set_at_path(
                    &mut operation.selection_set,
                    &[name!("t"), name!("c")],
                )
                .unwrap();
                assert_eq!(operation_without_empty_branches(&operation), expected);
            }

            {
                // query to test: "{ u t { c { } } }"
                let expected = "{ u }";

                let valid_query = r#"{ u t { c { x } } }"#;
                let mut operation = parse_operation(
                    &parse_schema(TEST_SCHEMA_FOR_EMPTY_BRANCH_REMOVAL),
                    valid_query,
                );
                clear_selection_set_at_path(
                    &mut operation.selection_set,
                    &[name!("t"), name!("c")],
                )
                .unwrap();
                assert_eq!(
                    operation_without_empty_branches(&operation).unwrap(),
                    expected
                );
            }

            {
                // query to test: "{ t { c { } } u }"
                let expected = "{ u }";

                let valid_query = r#"{ t { c { x } } u }"#;
                let mut operation = parse_operation(
                    &parse_schema(TEST_SCHEMA_FOR_EMPTY_BRANCH_REMOVAL),
                    valid_query,
                );
                clear_selection_set_at_path(
                    &mut operation.selection_set,
                    &[name!("t"), name!("c")],
                )
                .unwrap();
                assert_eq!(
                    operation_without_empty_branches(&operation).unwrap(),
                    expected
                );
            }
        }
    }
}
