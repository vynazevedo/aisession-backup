use std::path::PathBuf;

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "action", content = "reason", rename_all = "snake_case")]
pub enum Action {
    Create,
    Overwrite,
    Merge,
    Skip(String),
    Refuse(String),
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Normal,
    Sensitive,
    ExecutesCode,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedOp {
    pub provider: String,
    pub dest: PathBuf,
    pub action: Action,
    pub risk: Risk,
    pub size: u64,
    pub rewrites: usize,
    pub note: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PlanSummary {
    pub create: usize,
    pub overwrite: usize,
    pub merge: usize,
    pub skip: usize,
    pub refuse: usize,
    pub executes_code: usize,
    pub sensitive: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Plan {
    pub ops: Vec<PlannedOp>,
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn failed(reason: impl Into<String>) -> Self {
        Self {
            ops: vec![PlannedOp {
                provider: "plan".into(),
                dest: PathBuf::new(),
                action: Action::Refuse(reason.into()),
                risk: Risk::Normal,
                size: 0,
                rewrites: 0,
                note: String::new(),
            }],
            warnings: Vec::new(),
        }
    }

    pub fn summary(&self) -> PlanSummary {
        let mut summary = PlanSummary::default();
        for op in &self.ops {
            match op.action {
                Action::Create => summary.create += 1,
                Action::Overwrite => summary.overwrite += 1,
                Action::Merge => summary.merge += 1,
                Action::Skip(_) => summary.skip += 1,
                Action::Refuse(_) => summary.refuse += 1,
            }
            if matches!(op.action, Action::Skip(_) | Action::Refuse(_)) {
                continue;
            }
            summary.bytes += op.size;
            match op.risk {
                Risk::ExecutesCode => summary.executes_code += 1,
                Risk::Sensitive => summary.sensitive += 1,
                Risk::Normal => {}
            }
        }
        summary
    }

    pub fn has_refusals(&self) -> bool {
        self.ops
            .iter()
            .any(|op| matches!(op.action, Action::Refuse(_)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(action: Action, risk: Risk, size: u64) -> PlannedOp {
        PlannedOp {
            provider: "claude".into(),
            dest: PathBuf::from("/x"),
            action,
            risk,
            size,
            rewrites: 0,
            note: String::new(),
        }
    }

    #[test]
    fn summary_ignores_skipped_bytes_and_risk() {
        let plan = Plan {
            ops: vec![
                op(Action::Create, Risk::ExecutesCode, 10),
                op(Action::Skip("exists".into()), Risk::Sensitive, 99),
                op(Action::Refuse("unsafe".into()), Risk::Normal, 7),
            ],
            warnings: vec![],
        };
        let summary = plan.summary();
        assert_eq!(summary.create, 1);
        assert_eq!(summary.skip, 1);
        assert_eq!(summary.refuse, 1);
        assert_eq!(summary.bytes, 10);
        assert_eq!(summary.executes_code, 1);
        assert_eq!(summary.sensitive, 0);
        assert!(plan.has_refusals());
    }
}
