use std::collections::HashMap;

use crate::context::AppContext;
use crate::module::{CliModule, ModuleId, ModuleStatus, TuiModule};

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

    /// Collects every visible module's Dashboard summary, skipping `exclude`
    /// (the Dashboard itself) and any module that reports `None`. Generic
    /// over `ModuleId::all_visible()` — a new module automatically appears
    /// here the moment it overrides `dashboard_summary`, with no changes
    /// needed in this function or in the Dashboard module itself.
    pub async fn collect_dashboard_summaries(
        &mut self,
        exclude: ModuleId,
        ctx: &AppContext,
    ) -> Vec<(ModuleId, ModuleStatus)> {
        let mut out = Vec::new();
        for id in ModuleId::all_visible() {
            if id == exclude {
                continue;
            }
            if let Some(m) = self.get_mut(id)
                && let Some(status) = m.dashboard_summary(ctx).await
            {
                out.push((id, status));
            }
        }
        out
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
