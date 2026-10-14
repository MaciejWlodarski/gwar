use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Opaque byte keys and values, suitable for IndexedDB binary keys.
pub type Store = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stores {
    pub openmls: Store,
    pub groups: Store,
    pub epochs: Store,
    pub outbox: Store,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Changes {
    pub openmls: Vec<Change>,
    pub groups: Vec<Change>,
    pub epochs: Vec<Change>,
    pub outbox: Vec<Change>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub key: Vec<u8>,
    /// None deletes the key.
    pub value: Option<Vec<u8>>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.openmls.is_empty() && self.groups.is_empty() && self.epochs.is_empty() && self.outbox.is_empty()
    }

    pub fn apply(&self, stores: &mut Stores) {
        for (changes, store) in [
            (&self.openmls, &mut stores.openmls),
            (&self.groups, &mut stores.groups),
            (&self.epochs, &mut stores.epochs),
            (&self.outbox, &mut stores.outbox),
        ] {
            for change in changes {
                if let Some(value) = &change.value {
                    store.insert(change.key.clone(), value.clone());
                } else {
                    store.remove(&change.key);
                }
            }
        }
    }

    pub(super) fn between(old: &Stores, new: &Stores) -> Self {
        fn diff(old: &Store, new: &Store) -> Vec<Change> {
            new.iter()
                .filter(|(k, v)| old.get(*k) != Some(*v))
                .map(|(k, v)| Change { key: k.clone(), value: Some(v.clone()) })
                .chain(old.keys().filter(|k| !new.contains_key(*k)).map(|k| Change { key: k.clone(), value: None }))
                .collect()
        }
        Self {
            openmls: diff(&old.openmls, &new.openmls),
            groups: diff(&old.groups, &new.groups),
            epochs: diff(&old.epochs, &new.epochs),
            outbox: diff(&old.outbox, &new.outbox),
        }
    }
}
