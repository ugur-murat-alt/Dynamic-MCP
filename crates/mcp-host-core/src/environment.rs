use std::env;

use thiserror::Error;

/// Provides environment values without requiring global mutation in tests.
pub trait EnvironmentProvider {
    fn get(&self, name: &str) -> Result<Option<String>, EnvironmentAccessError>;
}

/// Reads values from the current process environment.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessEnvironment;

impl EnvironmentProvider for ProcessEnvironment {
    fn get(&self, name: &str) -> Result<Option<String>, EnvironmentAccessError> {
        match env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(env::VarError::NotPresent) => Ok(None),
            Err(env::VarError::NotUnicode(_)) => Err(EnvironmentAccessError::NotUnicode),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EnvironmentAccessError {
    #[error("environment value is not valid Unicode")]
    NotUnicode,
}
