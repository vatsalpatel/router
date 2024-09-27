#![warn(dead_code)]
use std::borrow::Cow;
use std::hash::BuildHasher;
use std::sync::Arc;

use apollo_compiler::Name;
use hashbrown::hash_map::DefaultHashBuilder;
use hashbrown::raw::RawTable;
use serde::ser::SerializeSeq;
use serde::Serialize;

use crate::error::FederationError;
use crate::operation::field_selection::FieldSelection;
use crate::operation::fragment_spread_selection::FragmentSpreadSelection;
use crate::operation::inline_fragment_selection::InlineFragmentSelection;
use crate::operation::DirectiveList;
use crate::operation::Selection;
use crate::operation::SelectionId;
use crate::operation::SelectionSet;
use crate::operation::SiblingTypename;

/// A selection "key" (unrelated to the federation `@key` directive) is an identifier of a selection
/// (field, inline fragment, or fragment spread) that is used to determine whether two selections
/// can be merged.
///
/// In order to merge two selections they need to
/// * reference the same field/inline fragment
/// * specify the same directives
/// * directives have to be applied in the same order
/// * directive arguments order does not matter (they get automatically sorted by their names).
/// * selection cannot specify @defer directive
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub(crate) enum SelectionKey<'a> {
    Field {
        /// The field alias (if specified) or field name in the resulting selection set.
        response_name: &'a Name,
        /// directives applied on the field
        #[serde(serialize_with = "crate::display_helpers::serialize_as_string")]
        directives: &'a DirectiveList,
    },
    FragmentSpread {
        /// The name of the fragment.
        fragment_name: &'a Name,
        /// Directives applied on the fragment spread (does not contain @defer).
        #[serde(serialize_with = "crate::display_helpers::serialize_as_string")]
        directives: &'a DirectiveList,
    },
    InlineFragment {
        /// The optional type condition of the fragment.
        type_condition: Option<&'a Name>,
        /// Directives applied on the fragment spread (does not contain @defer).
        #[serde(serialize_with = "crate::display_helpers::serialize_as_string")]
        directives: &'a DirectiveList,
    },
    Defer {
        /// Unique selection ID used to distinguish deferred fragment spreads that cannot be merged.
        #[cfg_attr(not(feature = "snapshot_tracing"), serde(skip))]
        deferred_id: SelectionId,
    },
}

impl SelectionKey<'_> {
    pub(crate) fn to_owned_key(self) -> OwnedSelectionKey {
        match self {
            Self::Field {
                response_name,
                directives,
            } => OwnedSelectionKey::Field {
                response_name: response_name.clone(),
                directives: directives.clone(),
            },
            Self::FragmentSpread {
                fragment_name,
                directives,
            } => OwnedSelectionKey::FragmentSpread {
                fragment_name: fragment_name.clone(),
                directives: directives.clone(),
            },
            Self::InlineFragment {
                type_condition,
                directives,
            } => OwnedSelectionKey::InlineFragment {
                type_condition: type_condition.cloned(),
                directives: directives.clone(),
            },
            Self::Defer { deferred_id } => OwnedSelectionKey::Defer { deferred_id },
        }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
#[deprecated = "Avoid owned keys"]
pub(crate) enum OwnedSelectionKey {
    Field {
        response_name: Name,
        directives: DirectiveList,
    },
    FragmentSpread {
        fragment_name: Name,
        directives: DirectiveList,
    },
    InlineFragment {
        type_condition: Option<Name>,
        directives: DirectiveList,
    },
    Defer {
        deferred_id: SelectionId,
    },
}

impl OwnedSelectionKey {
    pub(crate) fn as_borrowed_key(&self) -> SelectionKey<'_> {
        match self {
            OwnedSelectionKey::Field {
                response_name,
                directives,
            } => SelectionKey::Field {
                response_name,
                directives,
            },
            OwnedSelectionKey::FragmentSpread {
                fragment_name,
                directives,
            } => SelectionKey::FragmentSpread {
                fragment_name,
                directives,
            },
            OwnedSelectionKey::InlineFragment {
                type_condition,
                directives,
            } => SelectionKey::InlineFragment {
                type_condition: type_condition.as_ref(),
                directives,
            },
            OwnedSelectionKey::Defer { deferred_id } => SelectionKey::Defer {
                deferred_id: *deferred_id,
            },
        }
    }
}

impl<'a> SelectionKey<'a> {
    /// Returns true if the selection key is `__typename` *without directives*.
    #[deprecated = "Use the Selection type instead"]
    pub(crate) fn is_typename_field(self) -> bool {
        matches!(self, SelectionKey::Field { response_name, directives } if *response_name == super::TYPENAME_FIELD && directives.is_empty())
    }

    /// Create a selection key for a specific field name.
    ///
    /// This is available for tests only as selection keys should not normally be created outside of
    /// `HasSelectionKey::key`.
    #[cfg(test)]
    pub(crate) fn field_name(name: &'a Name) -> Self {
        static EMPTY_LIST: DirectiveList = DirectiveList::new();
        SelectionKey::Field {
            response_name: name,
            directives: &EMPTY_LIST,
        }
    }
}

pub(crate) trait HasSelectionKey {
    fn key(&self) -> SelectionKey<'_>;
}

/// A selection map is the underlying representation of a selection set. It contains an ordered
/// list of selections with unique selection keys. Selections with the same key should be merged
/// together by the user of this structure: the selection map API itself will overwrite selections
/// with the same key.
///
/// Once a selection is in the selection map, it must not be modified in a way that changes the
/// selection key. Therefore, the selection map only hands out mutable access through the
/// SelectionValue types, which expose the parts of selections that are safe to modify.
#[derive(Clone)]
pub(crate) struct SelectionMap {
    hash_builder: DefaultHashBuilder,
    table: RawTable<usize>,
    selections: Vec<Selection>,
}

impl std::fmt::Debug for SelectionMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.values()).finish()
    }
}

impl PartialEq for SelectionMap {
    /// Compare two selection maps. This is order independent.
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self
                .values()
                .all(|left| other.get(left.key()).is_some_and(|right| left == right))
    }
}

impl Eq for SelectionMap {}

impl Serialize for SelectionMap {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut seq = serializer.serialize_seq(Some(self.len()))?;
        for value in self.values() {
            seq.serialize_element(value)?;
        }
        seq.end()
    }
}

impl Default for SelectionMap {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) type Values<'a> = std::slice::Iter<'a, Selection>;
pub(crate) type ValuesMut<'a> =
    std::iter::Map<std::slice::IterMut<'a, Selection>, fn(&'a mut Selection) -> SelectionValue<'a>>;
pub(crate) type IntoValues = std::vec::IntoIter<Selection>;

impl SelectionMap {
    pub(crate) fn new() -> Self {
        SelectionMap {
            hash_builder: Default::default(),
            table: RawTable::new(),
            selections: Vec::new(),
        }
    }

    /// Returns the number of selections in the map.
    pub(crate) fn len(&self) -> usize {
        self.selections.len()
    }

    /// Returns true if there are no selections in the map.
    pub(crate) fn is_empty(&self) -> bool {
        self.selections.is_empty()
    }

    /// Returns the first selection in the map, or None if the map is empty.
    pub(crate) fn first(&self) -> Option<&Selection> {
        self.selections.first()
    }

    fn hash(&self, key: SelectionKey) -> u64 {
        self.hash_builder.hash_one(key)
    }

    /// Returns true if the given key exists in the map.
    pub(crate) fn contains_key(&self, key: SelectionKey) -> bool {
        let hash = self.hash(key);
        self.table
            .find(hash, |selection| self.selections[*selection].key() == key)
            .is_some()
    }

    /// Returns true if the given key exists in the map.
    pub(crate) fn get(&self, key: SelectionKey<'_>) -> Option<&Selection> {
        let hash = self.hash(key);
        let index = self
            .table
            .get(hash, |selection| self.selections[*selection].key() == key)?;
        Some(&self.selections[*index])
    }

    pub(crate) fn get_mut(&mut self, key: SelectionKey<'_>) -> Option<SelectionValue> {
        let hash = self.hash(key);
        let index = self
            .table
            .get(hash, |selection| self.selections[*selection].key() == key)?;
        Some(SelectionValue::new(&mut self.selections[*index]))
    }

    /// Insert a selection into the map.
    fn raw_insert(&mut self, hash: u64, value: Selection) -> &mut Selection {
        let index = self.selections.len();

        self.table.insert(hash, index, |existing| {
            self.hash_builder.hash_one(self.selections[*existing].key())
        });

        self.selections.push(value);
        &mut self.selections[index]
    }

    /// Resets and rebuilds the hash table.
    ///
    /// Preconditions:
    /// - The table must have enough capacity for `self.selections.len()` elements.
    fn rebuild_table_no_grow(&mut self) {
        assert!(self.table.capacity() >= self.selections.len());
        self.table.clear();
        for (index, selection) in self.selections.iter().enumerate() {
            let hash = self.hash(selection.key());
            // SAFETY: Capacity is guaranteed by the assert at the top of the function
            unsafe {
                self.table.insert_no_grow(hash, index);
            }
        }
    }

    pub(crate) fn insert(&mut self, value: Selection) {
        let hash = self.hash(value.key());
        self.raw_insert(hash, value);
    }

    /// Remove a selection from the map. Returns the selection and its numeric index.
    pub(crate) fn remove(&mut self, key: SelectionKey) -> Option<(usize, Selection)> {
        let hash = self.hash(key);
        let index = self
            .table
            .remove_entry(hash, |selection| self.selections[*selection].key() == key)?;
        let selection = self.selections.remove(index);
        // TODO: adjust indices
        self.rebuild_table_no_grow();
        Some((index, selection))
    }

    pub(crate) fn retain(&mut self, mut predicate: impl FnMut(SelectionKey, &Selection) -> bool) {
        self.selections.retain(|selection| {
            let key = selection.key();
            predicate(key, selection)
        });
        if self.selections.len() < self.table.len() {
            self.rebuild_table_no_grow();
        }
        assert!(self.selections.len() == self.table.len());
    }

    /// Iterate over all selections.
    pub(crate) fn values(&self) -> Values<'_> {
        self.selections.iter()
    }

    /// Iterate over all selections.
    pub(crate) fn values_mut(&mut self) -> ValuesMut<'_> {
        self.selections.iter_mut().map(SelectionValue::new)
    }

    /// Iterate over all selections.
    pub(crate) fn into_values(self) -> IntoValues {
        self.selections.into_iter()
    }

    pub(super) fn entry<'a>(&'a mut self, key: SelectionKey<'a>) -> Entry<'a> {
        let hash = self.hash(key);
        let slot = self
            .table
            .find(hash, |selection| self.selections[*selection].key() == key);
        match slot {
            Some(occupied) => {
                let index = unsafe { *occupied.as_ptr() };
                let selection = &mut self.selections[index];
                Entry::Occupied(OccupiedEntry(selection))
            }
            None => Entry::Vacant(VacantEntry {
                map: self,
                hash,
                key,
            }),
        }
    }

    pub(crate) fn extend(&mut self, other: SelectionMap) {
        for selection in other.into_values() {
            self.insert(selection);
        }
    }

    pub(crate) fn extend_ref(&mut self, other: &SelectionMap) {
        for selection in other.values() {
            self.insert(selection.clone());
        }
    }

    pub(crate) fn as_slice(&self) -> &[Selection] {
        &self.selections
    }

    /// Returns the selection set resulting from "recursively" filtering any selection
    /// that does not match the provided predicate.
    /// This method calls `predicate` on every selection of the selection set,
    /// not just top-level ones, and apply a "depth-first" strategy:
    /// when the predicate is called on a given selection it is guaranteed that
    /// filtering has happened on all the selections of its sub-selection.
    pub(crate) fn filter_recursive_depth_first(
        &self,
        predicate: &mut dyn FnMut(&Selection) -> Result<bool, FederationError>,
    ) -> Result<Cow<'_, Self>, FederationError> {
        fn recur_sub_selections<'sel>(
            selection: &'sel Selection,
            predicate: &mut dyn FnMut(&Selection) -> Result<bool, FederationError>,
        ) -> Result<Cow<'sel, Selection>, FederationError> {
            Ok(match selection {
                Selection::Field(field) => {
                    if let Some(sub_selections) = &field.selection_set {
                        match sub_selections.filter_recursive_depth_first(predicate)? {
                            Cow::Borrowed(_) => Cow::Borrowed(selection),
                            Cow::Owned(new) => {
                                Cow::Owned(Selection::from_field(field.field.clone(), Some(new)))
                            }
                        }
                    } else {
                        Cow::Borrowed(selection)
                    }
                }
                Selection::InlineFragment(fragment) => match fragment
                    .selection_set
                    .filter_recursive_depth_first(predicate)?
                {
                    Cow::Borrowed(_) => Cow::Borrowed(selection),
                    Cow::Owned(selection_set) => Cow::Owned(Selection::InlineFragment(Arc::new(
                        InlineFragmentSelection::new(
                            fragment.inline_fragment.clone(),
                            selection_set,
                        ),
                    ))),
                },
                Selection::FragmentSpread(_) => {
                    return Err(FederationError::internal("unexpected fragment spread"))
                }
            })
        }
        let mut iter = self.values();
        let mut enumerated = (&mut iter).enumerate();
        let mut new_map: Self;
        loop {
            let Some((index, selection)) = enumerated.next() else {
                return Ok(Cow::Borrowed(self));
            };
            let filtered = recur_sub_selections(selection, predicate)?;
            let keep = predicate(&filtered)?;
            if keep && matches!(filtered, Cow::Borrowed(_)) {
                // Nothing changed so far, continue without cloning
                continue;
            }

            // Clone the map so far
            new_map = self.as_slice()[..index].iter().cloned().collect();

            if keep {
                new_map.insert(filtered.into_owned());
            }
            break;
        }
        for selection in iter {
            let filtered = recur_sub_selections(selection, predicate)?;
            if predicate(&filtered)? {
                new_map.insert(filtered.into_owned());
            }
        }
        Ok(Cow::Owned(new_map))
    }
}

impl<A> FromIterator<A> for SelectionMap
where
    A: Into<Selection>,
{
    fn from_iter<T: IntoIterator<Item = A>>(iter: T) -> Self {
        let mut map = Self::new();
        for selection in iter {
            map.insert(selection.into());
        }
        map
    }
}

/// A mutable reference to a `Selection` value in a `SelectionMap`, which
/// also disallows changing key-related data (to maintain the invariant that a value's key is
/// the same as it's map entry's key).
#[derive(Debug)]
pub(crate) enum SelectionValue<'a> {
    Field(FieldSelectionValue<'a>),
    FragmentSpread(FragmentSpreadSelectionValue<'a>),
    InlineFragment(InlineFragmentSelectionValue<'a>),
}

impl<'a> SelectionValue<'a> {
    pub(crate) fn new(selection: &'a mut Selection) -> Self {
        match selection {
            Selection::Field(field_selection) => {
                SelectionValue::Field(FieldSelectionValue::new(field_selection))
            }
            Selection::FragmentSpread(fragment_spread_selection) => SelectionValue::FragmentSpread(
                FragmentSpreadSelectionValue::new(fragment_spread_selection),
            ),
            Selection::InlineFragment(inline_fragment_selection) => SelectionValue::InlineFragment(
                InlineFragmentSelectionValue::new(inline_fragment_selection),
            ),
        }
    }

    pub(super) fn key(&self) -> SelectionKey {
        match self {
            Self::Field(field) => field.get().key(),
            Self::FragmentSpread(frag) => frag.get().key(),
            Self::InlineFragment(frag) => frag.get().key(),
        }
    }

    pub(super) fn directives(&self) -> &'_ DirectiveList {
        match self {
            Self::Field(field) => &field.get().field.directives,
            Self::FragmentSpread(frag) => &frag.get().spread.directives,
            Self::InlineFragment(frag) => &frag.get().inline_fragment.directives,
        }
    }

    pub(super) fn get_selection_set_mut(&mut self) -> Option<&mut SelectionSet> {
        match self {
            Self::Field(field) => field.get_selection_set_mut().as_mut(),
            Self::FragmentSpread(spread) => Some(spread.get_selection_set_mut()),
            Self::InlineFragment(inline) => Some(inline.get_selection_set_mut()),
        }
    }
}

#[derive(Debug)]
pub(crate) struct FieldSelectionValue<'a>(&'a mut Arc<FieldSelection>);

impl<'a> FieldSelectionValue<'a> {
    pub(crate) fn new(field_selection: &'a mut Arc<FieldSelection>) -> Self {
        Self(field_selection)
    }

    pub(crate) fn get(&self) -> &Arc<FieldSelection> {
        self.0
    }

    pub(crate) fn get_sibling_typename_mut(&mut self) -> &mut Option<SiblingTypename> {
        Arc::make_mut(self.0).field.sibling_typename_mut()
    }

    pub(crate) fn get_selection_set_mut(&mut self) -> &mut Option<SelectionSet> {
        &mut Arc::make_mut(self.0).selection_set
    }
}

#[derive(Debug)]
pub(crate) struct FragmentSpreadSelectionValue<'a>(&'a mut Arc<FragmentSpreadSelection>);

impl<'a> FragmentSpreadSelectionValue<'a> {
    pub(crate) fn new(fragment_spread_selection: &'a mut Arc<FragmentSpreadSelection>) -> Self {
        Self(fragment_spread_selection)
    }

    pub(crate) fn get_selection_set_mut(&mut self) -> &mut SelectionSet {
        &mut Arc::make_mut(self.0).selection_set
    }

    pub(crate) fn get(&self) -> &Arc<FragmentSpreadSelection> {
        self.0
    }
}

#[derive(Debug)]
pub(crate) struct InlineFragmentSelectionValue<'a>(&'a mut Arc<InlineFragmentSelection>);

impl<'a> InlineFragmentSelectionValue<'a> {
    pub(crate) fn new(inline_fragment_selection: &'a mut Arc<InlineFragmentSelection>) -> Self {
        Self(inline_fragment_selection)
    }

    pub(crate) fn get(&self) -> &Arc<InlineFragmentSelection> {
        self.0
    }

    pub(crate) fn get_selection_set_mut(&mut self) -> &mut SelectionSet {
        &mut Arc::make_mut(self.0).selection_set
    }
}

pub(crate) enum Entry<'a> {
    Occupied(OccupiedEntry<'a>),
    Vacant(VacantEntry<'a>),
}

impl<'a> Entry<'a> {
    pub fn or_insert(
        self,
        produce: impl FnOnce() -> Result<Selection, FederationError>,
    ) -> Result<SelectionValue<'a>, FederationError> {
        match self {
            Self::Occupied(entry) => Ok(entry.into_mut()),
            Self::Vacant(entry) => entry.insert(produce()?),
        }
    }
}

pub(crate) struct OccupiedEntry<'a>(&'a mut Selection);

impl<'a> OccupiedEntry<'a> {
    pub(crate) fn get(&self) -> &Selection {
        self.0
    }

    pub(crate) fn into_mut(self) -> SelectionValue<'a> {
        SelectionValue::new(self.0)
    }
}

pub(crate) struct VacantEntry<'a> {
    map: &'a mut SelectionMap,
    hash: u64,
    key: SelectionKey<'a>,
}

impl<'a> VacantEntry<'a> {
    pub(crate) fn key(&self) -> SelectionKey<'a> {
        self.key
    }

    pub(crate) fn insert(self, value: Selection) -> Result<SelectionValue<'a>, FederationError> {
        if self.key() != value.key() {
            return Err(FederationError::internal(format!(
                "Key mismatch when inserting selection {value} into vacant entry "
            )));
        };
        Ok(SelectionValue::new(self.map.raw_insert(self.hash, value)))
    }
}
