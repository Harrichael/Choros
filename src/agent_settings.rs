use std::path::{Path, PathBuf};

use color_eyre::eyre::{Context, Result};
use serde_json::Value;

use crate::choros::templates_dir;
use crate::json_diff::merge_values;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSource {
    Settings,
    Local,
    Both,
}

impl DiffSource {
    pub fn label(&self) -> &'static str {
        match self {
            DiffSource::Settings => "settings.json",
            DiffSource::Local => "settings.local.json",
            DiffSource::Both => "both",
        }
    }

    pub fn next(&self) -> Self {
        match self {
            DiffSource::Both => DiffSource::Settings,
            DiffSource::Settings => DiffSource::Local,
            DiffSource::Local => DiffSource::Both,
        }
    }
}

pub const CLAUDE_SETTINGS_REL: &str = ".claude/settings.json";
pub const CLAUDE_LOCAL_REL: &str = ".claude/settings.local.json";

pub fn claude_template_path(root: &Path) -> PathBuf {
    templates_dir(root).join(CLAUDE_SETTINGS_REL)
}

pub fn claude_workspace_settings_path(workspace: &Path) -> PathBuf {
    workspace.join(CLAUDE_SETTINGS_REL)
}

pub fn claude_workspace_local_path(workspace: &Path) -> PathBuf {
    workspace.join(CLAUDE_LOCAL_REL)
}

fn read_json_or_empty(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    let body = std::fs::read_to_string(path)
        .wrap_err_with(|| format!("reading {path:?}"))?;
    if body.trim().is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    serde_json::from_str(&body).wrap_err_with(|| format!("parsing {path:?} as JSON"))
}

pub fn load_template(root: &Path) -> Result<Value> {
    read_json_or_empty(&claude_template_path(root))
}

pub fn load_workspace(workspace: &Path, source: DiffSource) -> Result<Value> {
    match source {
        DiffSource::Settings => read_json_or_empty(&claude_workspace_settings_path(workspace)),
        DiffSource::Local => read_json_or_empty(&claude_workspace_local_path(workspace)),
        DiffSource::Both => {
            let mut merged = read_json_or_empty(&claude_workspace_settings_path(workspace))?;
            let local = read_json_or_empty(&claude_workspace_local_path(workspace))?;
            merge_values(&mut merged, &local);
            Ok(merged)
        }
    }
}

pub fn save_template(root: &Path, value: &Value) -> Result<()> {
    let path = claude_template_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut body = serde_json::to_string_pretty(value)
        .wrap_err("serializing template settings")?;
    body.push('\n');
    std::fs::write(&path, body).wrap_err_with(|| format!("writing {path:?}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn load_workspace_missing_files_returns_empty_object() {
        let tmp = tempdir();
        let ws = tmp.path();
        let v = load_workspace(ws, DiffSource::Both).unwrap();
        assert_eq!(v, Value::Object(serde_json::Map::new()));
    }

    #[test]
    fn load_workspace_both_merges_settings_and_local() {
        let tmp = tempdir();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join(".claude")).unwrap();
        std::fs::write(
            ws.join(".claude/settings.json"),
            br#"{"permissions":{"allow":["A"]}}"#,
        )
        .unwrap();
        std::fs::write(
            ws.join(".claude/settings.local.json"),
            br#"{"permissions":{"allow":["B"]}}"#,
        )
        .unwrap();

        let v = load_workspace(ws, DiffSource::Both).unwrap();
        let allow = &v["permissions"]["allow"];
        assert_eq!(allow, &serde_json::json!(["A", "B"]));
    }

    #[test]
    fn diff_source_cycle_order() {
        assert_eq!(DiffSource::Both.next(), DiffSource::Settings);
        assert_eq!(DiffSource::Settings.next(), DiffSource::Local);
        assert_eq!(DiffSource::Local.next(), DiffSource::Both);
    }

    #[test]
    fn save_template_round_trip() {
        let tmp = tempdir();
        let root = tmp.path();
        let v = serde_json::json!({"permissions":{"allow":["X"]}});
        save_template(root, &v).unwrap();
        let back = load_template(root).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn diff_and_promote_round_trip() {
        use crate::json_diff::{apply_additions, diff_additions};

        let tmp = tempdir();
        let root = tmp.path();
        let workspace = root.join("PROJ-1");

        // Template baseline.
        save_template(
            root,
            &serde_json::json!({"permissions":{"allow":["Bash(ls:*)"]}}),
        )
        .unwrap();

        // Workspace files: committed settings has nothing new, local has two new grants.
        std::fs::create_dir_all(workspace.join(".claude")).unwrap();
        std::fs::write(
            workspace.join(".claude/settings.json"),
            br#"{"permissions":{"allow":["Bash(ls:*)"]}}"#,
        )
        .unwrap();
        std::fs::write(
            workspace.join(".claude/settings.local.json"),
            br#"{"permissions":{"allow":["Bash(grep:*)","Bash(rg:*)"],"deny":["Bash(rm:*)"]}}"#,
        )
        .unwrap();

        // Diff against Both.
        let template = load_template(root).unwrap();
        let merged = load_workspace(&workspace, DiffSource::Both).unwrap();
        let adds = diff_additions(&template, &merged);
        assert!(adds.len() >= 3, "expected new entries, got: {adds:?}");

        // Promote all.
        let mut t_mut = template.clone();
        apply_additions(&mut t_mut, &adds);
        save_template(root, &t_mut).unwrap();

        // After promote, the diff is empty.
        let template2 = load_template(root).unwrap();
        let merged2 = load_workspace(&workspace, DiffSource::Both).unwrap();
        let adds2 = diff_additions(&template2, &merged2);
        assert!(adds2.is_empty(), "expected empty diff after promote, got: {adds2:?}");

        // Template now contains the promoted entries.
        let allow = &template2["permissions"]["allow"];
        let allow_arr = allow.as_array().unwrap();
        let allow_strs: Vec<&str> = allow_arr.iter().filter_map(|v| v.as_str()).collect();
        assert!(allow_strs.contains(&"Bash(ls:*)"));
        assert!(allow_strs.contains(&"Bash(grep:*)"));
        assert!(allow_strs.contains(&"Bash(rg:*)"));
        let deny = &template2["permissions"]["deny"];
        assert_eq!(deny, &serde_json::json!(["Bash(rm:*)"]));
    }
}
