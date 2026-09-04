/// Semantic tone for a `ModuleStatus`, used by the Dashboard to pick a
/// theme color without any module needing to know about colors itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusTone {
    Normal,
    Locked,
    Warning,
    Error,
}

/// A short, human-readable status line a module reports for display on the
/// Dashboard (see `TuiModule::dashboard_summary`).
#[derive(Debug, Clone)]
pub struct ModuleStatus {
    pub text: String,
    pub tone: StatusTone,
}

impl ModuleStatus {
    pub fn normal(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tone: StatusTone::Normal,
        }
    }

    pub fn locked() -> Self {
        Self {
            text: "Locked".to_string(),
            tone: StatusTone::Locked,
        }
    }

    pub fn warning(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tone: StatusTone::Warning,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            tone: StatusTone::Error,
        }
    }
}
