use std::collections::HashMap;

use crate::module::{CliModule, ModuleId, TuiModule};

/// Registry for TUI modules, mapping ModuleId to active TuiModule traits.
pub struct TuiRegistry(HashMap<ModuleId, Box<dyn TuiModule>>);

impl TuiRegistry {
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    pub fn insert(&mut self, module: Box<dyn TuiModule>) {
        self.0.insert(module.id(), module);
    }

    pub fn get_mut(&mut self, id: ModuleId) -> Option<&mut Box<dyn TuiModule>> {
        self.0.get_mut(&id)
    }

    pub fn contains(&self, id: ModuleId) -> bool {
        self.0.contains_key(&id)
    }
}

impl Default for TuiRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Registry for CLI modules, mapping ModuleId to active CliModule traits.
pub struct CliRegistry(HashMap<ModuleId, Box<dyn CliModule>>);

impl CliRegistry {
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    pub fn insert(&mut self, module: Box<dyn CliModule>) {
        self.0.insert(module.id(), module);
    }

    pub fn get(&self, id: ModuleId) -> Option<&Box<dyn CliModule>> {
        self.0.get(&id)
    }
}

impl Default for CliRegistry {
    fn default() -> Self {
        Self::new()
    }
}
