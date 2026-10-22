use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSeg {
    Key(String),
    Index(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Addition {
    pub path: Vec<PathSeg>,
    pub value: Value,
}

impl Addition {
    pub fn pretty_path(&self) -> String {
        let mut out = String::new();
        for (i, seg) in self.path.iter().enumerate() {
            match seg {
                PathSeg::Key(k) => {
                    if i > 0 {
                        out.push('.');
                    }
                    out.push_str(k);
                }
                PathSeg::Index(idx) => {
                    out.push('[');
                    out.push_str(&idx.to_string());
                    out.push(']');
                }
            }
        }
        out
    }

    pub fn pretty_value(&self) -> String {
        serde_json::to_string(&self.value).unwrap_or_else(|_| "<unserializable>".into())
    }
}

pub fn diff_additions(template: &Value, workspace: &Value) -> Vec<Addition> {
    let mut out = Vec::new();
    let mut path = Vec::new();
    diff_at(template, workspace, &mut path, &mut out);
    out
}

fn diff_at(template: &Value, workspace: &Value, path: &mut Vec<PathSeg>, out: &mut Vec<Addition>) {
    match (template, workspace) {
        (Value::Object(t), Value::Object(w)) => {
            for (k, wv) in w {
                path.push(PathSeg::Key(k.clone()));
                match t.get(k) {
                    Some(tv) => diff_at(tv, wv, path, out),
                    None => out.push(Addition {
                        path: path.clone(),
                        value: wv.clone(),
                    }),
                }
                path.pop();
            }
        }
        (Value::Array(t), Value::Array(w)) => {
            for (i, wv) in w.iter().enumerate() {
                if !t.iter().any(|tv| tv == wv) {
                    path.push(PathSeg::Index(i));
                    out.push(Addition {
                        path: path.clone(),
                        value: wv.clone(),
                    });
                    path.pop();
                }
            }
        }
        (t, w) if t != w => {
            out.push(Addition {
                path: path.clone(),
                value: w.clone(),
            });
        }
        _ => {}
    }
}

pub fn apply_additions(template: &mut Value, additions: &[Addition]) {
    for add in additions {
        apply_one(template, &add.path, &add.value);
    }
}

fn apply_one(root: &mut Value, path: &[PathSeg], value: &Value) {
    if path.is_empty() {
        *root = value.clone();
        return;
    }
    let mut cursor: &mut Value = root;
    for (i, seg) in path.iter().enumerate() {
        let is_last = i + 1 == path.len();
        match seg {
            PathSeg::Key(k) => {
                if !matches!(cursor, Value::Object(_)) {
                    *cursor = Value::Object(Map::new());
                }
                let Value::Object(obj) = cursor else {
                    unreachable!()
                };
                if is_last {
                    obj.insert(k.clone(), value.clone());
                    return;
                }
                let next_is_index = matches!(path.get(i + 1), Some(PathSeg::Index(_)));
                cursor = obj.entry(k.clone()).or_insert_with(|| {
                    if next_is_index {
                        Value::Array(Vec::new())
                    } else {
                        Value::Object(Map::new())
                    }
                });
            }
            PathSeg::Index(_) => {
                if !matches!(cursor, Value::Array(_)) {
                    *cursor = Value::Array(Vec::new());
                }
                let Value::Array(arr) = cursor else {
                    unreachable!()
                };
                if is_last {
                    if !arr.iter().any(|v| v == value) {
                        arr.push(value.clone());
                    }
                    return;
                }
                // Nested arrays: not encountered in agent settings; bail to
                // avoid silently doing the wrong thing.
                return;
            }
        }
    }
}

/// Deep-merge `overlay` into `target` with append+dedupe semantics for arrays.
pub fn merge_values(target: &mut Value, overlay: &Value) {
    match (target, overlay) {
        (Value::Object(t), Value::Object(o)) => {
            for (k, ov) in o {
                match t.get_mut(k) {
                    Some(tv) => merge_values(tv, ov),
                    None => {
                        t.insert(k.clone(), ov.clone());
                    }
                }
            }
        }
        (Value::Array(t), Value::Array(o)) => {
            for ov in o {
                if !t.iter().any(|tv| tv == ov) {
                    t.push(ov.clone());
                }
            }
        }
        (target, overlay) => {
            *target = overlay.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diff_new_top_level_key() {
        let t = json!({});
        let w = json!({"a": 1});
        let adds = diff_additions(&t, &w);
        assert_eq!(adds.len(), 1);
        assert_eq!(adds[0].path, vec![PathSeg::Key("a".into())]);
        assert_eq!(adds[0].value, json!(1));
    }

    #[test]
    fn diff_value_mismatch_records_workspace_value() {
        let t = json!({"a": 1});
        let w = json!({"a": 2});
        let adds = diff_additions(&t, &w);
        assert_eq!(adds.len(), 1);
        assert_eq!(adds[0].value, json!(2));
    }

    #[test]
    fn diff_new_array_element() {
        let t = json!({"a": [1, 2]});
        let w = json!({"a": [1, 2, 3]});
        let adds = diff_additions(&t, &w);
        assert_eq!(adds.len(), 1);
        assert_eq!(
            adds[0].path,
            vec![PathSeg::Key("a".into()), PathSeg::Index(2)]
        );
        assert_eq!(adds[0].value, json!(3));
    }

    #[test]
    fn diff_array_elements_present_in_template_are_not_additions() {
        let t = json!({"a": ["A", "B"]});
        let w = json!({"a": ["B", "A"]}); // reordered, no new content
        let adds = diff_additions(&t, &w);
        assert!(adds.is_empty(), "got: {adds:?}");
    }

    #[test]
    fn diff_nested_new_path() {
        let t = json!({});
        let w = json!({"perms": {"allow": ["A"]}});
        let adds = diff_additions(&t, &w);
        // Single addition for the whole subtree at `perms`.
        assert_eq!(adds.len(), 1);
        assert_eq!(adds[0].path, vec![PathSeg::Key("perms".into())]);
    }

    #[test]
    fn apply_new_top_level_key() {
        let mut t = json!({});
        let adds = vec![Addition {
            path: vec![PathSeg::Key("a".into())],
            value: json!(1),
        }];
        apply_additions(&mut t, &adds);
        assert_eq!(t, json!({"a": 1}));
    }

    #[test]
    fn apply_array_element_append_and_dedupe() {
        let mut t = json!({"a": ["A"]});
        let adds = vec![
            Addition {
                path: vec![PathSeg::Key("a".into()), PathSeg::Index(1)],
                value: json!("B"),
            },
            // Duplicate of an existing template entry — should be skipped.
            Addition {
                path: vec![PathSeg::Key("a".into()), PathSeg::Index(2)],
                value: json!("A"),
            },
        ];
        apply_additions(&mut t, &adds);
        assert_eq!(t, json!({"a": ["A", "B"]}));
    }

    #[test]
    fn apply_creates_missing_parents() {
        let mut t = json!({});
        let adds = vec![Addition {
            path: vec![
                PathSeg::Key("perms".into()),
                PathSeg::Key("allow".into()),
                PathSeg::Index(0),
            ],
            value: json!("Bash(ls:*)"),
        }];
        apply_additions(&mut t, &adds);
        assert_eq!(t, json!({"perms": {"allow": ["Bash(ls:*)"]}}));
    }

    #[test]
    fn apply_round_trips_with_diff() {
        let t = json!({"perms": {"allow": ["A"]}});
        let w = json!({"perms": {"allow": ["A", "B"], "deny": ["X"]}, "extra": true});
        let adds = diff_additions(&t, &w);
        let mut t_mut = t.clone();
        apply_additions(&mut t_mut, &adds);
        assert_eq!(t_mut, w);
    }

    #[test]
    fn merge_arrays_append_and_dedupe() {
        let mut t = json!({"a": [1, 2]});
        merge_values(&mut t, &json!({"a": [2, 3]}));
        assert_eq!(t, json!({"a": [1, 2, 3]}));
    }

    #[test]
    fn merge_objects_recursively() {
        let mut t = json!({"a": {"x": 1}});
        merge_values(&mut t, &json!({"a": {"y": 2}, "b": 3}));
        assert_eq!(t, json!({"a": {"x": 1, "y": 2}, "b": 3}));
    }

    #[test]
    fn pretty_path_renders_dotted_with_brackets() {
        let add = Addition {
            path: vec![
                PathSeg::Key("perms".into()),
                PathSeg::Key("allow".into()),
                PathSeg::Index(2),
            ],
            value: json!("X"),
        };
        assert_eq!(add.pretty_path(), "perms.allow[2]");
    }
}
