//! A document store with atomic batches: if any write in a batch fails, none is applied.
//!
//! The test below checks that a failing batch returns an error, but never looks at the
//! store afterwards. An implementation that applies writes one by one until the first
//! failure passes it.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Write {
    /// Creates or replaces a document.
    Set { key: String, value: String },
    /// Replaces a document that must already exist.
    Update { key: String, value: String },
}

#[derive(Debug, PartialEq)]
pub enum CommitError {
    NotFound(String),
}

#[derive(Default)]
pub struct Store {
    docs: BTreeMap<String, String>,
}

impl Store {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.docs.get(key).map(String::as_str)
    }

    pub fn commit(&mut self, batch: &[Write]) -> Result<(), CommitError> {
        self.check_preconditions(batch)?;
        for write in batch {
            self.apply(write)?;
        }
        Ok(())
    }

    /// Fails if any update in the batch targets a document that will not exist by then.
    fn check_preconditions(&self, batch: &[Write]) -> Result<(), CommitError> {
        let mut known: Vec<&str> = self.docs.keys().map(String::as_str).collect();
        for write in batch {
            match write {
                Write::Set { key, .. } => known.push(key),
                Write::Update { key, .. } => {
                    if !known.contains(&key.as_str()) {
                        return Err(CommitError::NotFound(key.clone()));
                    }
                }
            }
        }
        Ok(())
    }

    fn apply(&mut self, write: &Write) -> Result<(), CommitError> {
        match write {
            Write::Set { key, value } => {
                self.docs.insert(key.clone(), value.clone());
            }
            Write::Update { key, value } => {
                if !self.docs.contains_key(key) {
                    return Err(CommitError::NotFound(key.clone()));
                }
                self.docs.insert(key.clone(), value.clone());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(key: &str, value: &str) -> Write {
        Write::Set { key: key.into(), value: value.into() }
    }

    fn update(key: &str, value: &str) -> Write {
        Write::Update { key: key.into(), value: value.into() }
    }

    #[test]
    fn batch_writes_are_visible_after_commit() {
        let mut store = Store::default();
        store.commit(&[set("a", "1"), update("a", "2")]).unwrap();
        assert_eq!(store.get("a"), Some("2"));
    }

    #[test]
    fn commit_fails_when_an_update_target_is_missing() {
        let mut store = Store::default();
        let result = store.commit(&[set("a", "1"), update("b", "2")]);
        assert_eq!(result, Err(CommitError::NotFound("b".into())));
    }
}
