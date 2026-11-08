//! Accessibility snapshot: CDP's `Accessibility.getFullAXTree` rendered as
//! the indented text opencode's browser tools expect, with an `eN` ref per
//! element. Refs map to backend DOM node ids for click/fill/screenshot.
//!
//! ```text
//! - heading "Sign in" [level=1] [ref=e2]
//! - textbox "Email" [ref=e3]
//! - button "Continue" [disabled] [ref=e4]
//! ```

use std::collections::HashMap;

use serde_json::Value;

pub const MAX_TEXT: usize = 100_000;

/// What a ref points at.
#[derive(Clone, Debug, PartialEq)]
pub struct RefTarget {
    pub backend_node_id: i64,
    pub role: String,
    pub name: String,
    pub disabled: bool,
}

pub struct Snapshot {
    pub content: String,
    pub truncated: bool,
    pub refs: HashMap<String, RefTarget>,
}

/// Roles that only group others: drawn only when they have a name.
const STRUCTURAL: &[&str] = &[
    "generic",
    "none",
    "presentation",
    "group",
    "LineBreak",
    "RootWebArea",
    "WebArea",
    "Section",
    "div",
];
/// Text leaves: drawn as `- text: …`, never given a ref.
const TEXT: &[&str] = &["StaticText", "text"];
/// Properties worth showing, in this order.
const SHOWN: &[&str] = &[
    "disabled", "checked", "pressed", "expanded", "selected", "required", "level",
];

/// `nodes` is `getFullAXTree`'s `nodes`; `depth` limits nesting (1 = top level).
pub fn render(nodes: &[Value], depth: Option<usize>) -> Snapshot {
    let by_id: HashMap<&str, &Value> = nodes
        .iter()
        .filter_map(|n| Some((n.get("nodeId")?.as_str()?, n)))
        .collect();
    let root = nodes
        .iter()
        .find(|n| n.get("parentId").is_none())
        .or_else(|| nodes.first());
    let mut out = Snapshot {
        content: String::new(),
        truncated: false,
        refs: HashMap::new(),
    };
    if let Some(root) = root {
        walk(root, &by_id, 0, depth.unwrap_or(usize::MAX), &mut out);
    }
    if out.content.len() > MAX_TEXT {
        let mut cut = MAX_TEXT;
        while !out.content.is_char_boundary(cut) {
            cut -= 1;
        }
        out.content.truncate(cut);
        out.truncated = true;
    }
    out
}

fn str_value<'a>(n: &'a Value, key: &str) -> &'a str {
    n.pointer(&format!("/{key}/value"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn walk(n: &Value, by_id: &HashMap<&str, &Value>, level: usize, max: usize, out: &mut Snapshot) {
    if out.content.len() > MAX_TEXT {
        return;
    }
    let ignored = n.get("ignored").and_then(Value::as_bool).unwrap_or(false);
    let role = str_value(n, "role");
    let name = str_value(n, "name").trim();
    // The document itself is never a line; groups only when named.
    let structural = matches!(role, "RootWebArea" | "WebArea")
        || (STRUCTURAL.contains(&role) && name.is_empty());
    let drawn = !ignored && !structural && role != "InlineTextBox" && !role.is_empty();
    let child_level = if drawn { level + 1 } else { level };
    if drawn && level < max {
        let indent = "  ".repeat(level);
        if TEXT.contains(&role) {
            if !name.is_empty() {
                out.content
                    .push_str(&format!("{indent}- text: {}\n", quote(name)));
            }
        } else {
            let mut line = format!("{indent}- {role}");
            if !name.is_empty() {
                line.push(' ');
                line.push_str(&quote(name));
            }
            let mut disabled = false;
            for (prop, shown) in properties(n) {
                if prop == "disabled" {
                    disabled = true;
                }
                line.push_str(&format!(" [{shown}]"));
            }
            if let Some(id) = n.get("backendDOMNodeId").and_then(Value::as_i64) {
                let r = format!("e{}", out.refs.len() + 1);
                line.push_str(&format!(" [ref={r}]"));
                out.refs.insert(
                    r,
                    RefTarget {
                        backend_node_id: id,
                        role: role.to_string(),
                        name: name.to_string(),
                        disabled,
                    },
                );
            }
            out.content.push_str(&line);
            out.content.push('\n');
        }
    }
    if child_level > max {
        return;
    }
    let Some(kids) = n.get("childIds").and_then(Value::as_array) else {
        return;
    };
    for k in kids {
        if let Some(child) = k.as_str().and_then(|id| by_id.get(id)) {
            walk(child, by_id, child_level, max, out);
        }
    }
}

/// `(name, display)` for the interesting true/valued properties.
fn properties(n: &Value) -> Vec<(&'static str, String)> {
    let Some(props) = n.get("properties").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for want in SHOWN {
        let Some(v) = props
            .iter()
            .find(|p| p.get("name").and_then(Value::as_str) == Some(want))
            .and_then(|p| p.pointer("/value/value"))
        else {
            continue;
        };
        match (want, v) {
            (&"level", Value::Number(l)) => out.push((*want, format!("level={l}"))),
            (_, Value::Bool(true)) => out.push((*want, want.to_string())),
            (&"checked" | &"pressed", Value::String(s)) if s == "true" || s == "mixed" => {
                out.push((
                    *want,
                    if s == "mixed" {
                        format!("{want}=mixed")
                    } else {
                        want.to_string()
                    },
                ))
            }
            _ => {}
        }
    }
    out
}

fn quote(s: &str) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let clipped: String = one_line.chars().take(200).collect();
    serde_json::to_string(&clipped).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(
        id: &str,
        parent: Option<&str>,
        role: &str,
        name: &str,
        kids: &[&str],
        backend: Option<i64>,
        props: Value,
    ) -> Value {
        let mut n = json!({
            "nodeId": id, "ignored": false,
            "role": {"type": "role", "value": role},
            "name": {"type": "computedString", "value": name},
            "childIds": kids, "properties": props,
        });
        if let Some(p) = parent {
            n["parentId"] = json!(p);
        }
        if let Some(b) = backend {
            n["backendDOMNodeId"] = json!(b);
        }
        n
    }

    #[test]
    fn renders_roles_names_properties_and_refs() {
        let nodes = vec![
            node(
                "1",
                None,
                "RootWebArea",
                "Login",
                &["2"],
                Some(1),
                json!([]),
            ),
            node(
                "2",
                Some("1"),
                "generic",
                "",
                &["3", "4", "5", "6"],
                Some(2),
                json!([]),
            ),
            node(
                "3",
                Some("2"),
                "heading",
                "Sign  in",
                &["7"],
                Some(3),
                json!([{"name": "level", "value": {"type": "integer", "value": 1}}]),
            ),
            node(
                "4",
                Some("2"),
                "textbox",
                "Email",
                &[],
                Some(4),
                json!([{"name": "required", "value": {"type": "boolean", "value": true}}]),
            ),
            node(
                "5",
                Some("2"),
                "button",
                "Continue",
                &[],
                Some(5),
                json!([{"name": "disabled", "value": {"type": "boolean", "value": true}}]),
            ),
            node(
                "6",
                Some("2"),
                "checkbox",
                "Remember",
                &[],
                Some(6),
                json!([{"name": "checked", "value": {"type": "tristate", "value": "true"}}]),
            ),
            node(
                "7",
                Some("3"),
                "StaticText",
                "Sign in",
                &[],
                Some(7),
                json!([]),
            ),
        ];
        let s = render(&nodes, None);
        assert_eq!(
            s.content,
            "- heading \"Sign in\" [level=1] [ref=e1]\n  - text: \"Sign in\"\n- textbox \"Email\" [required] [ref=e2]\n- button \"Continue\" [disabled] [ref=e3]\n- checkbox \"Remember\" [checked] [ref=e4]\n"
        );
        assert_eq!(s.refs["e3"].backend_node_id, 5);
        assert!(s.refs["e3"].disabled);
        assert!(!s.refs["e2"].disabled);
        assert!(!s.truncated);
    }

    #[test]
    fn depth_and_ignored_nodes() {
        let mut ignored = node(
            "3",
            Some("2"),
            "button",
            "Hidden",
            &["4"],
            Some(3),
            json!([]),
        );
        ignored["ignored"] = json!(true);
        let nodes = vec![
            node("1", None, "RootWebArea", "", &["2"], Some(1), json!([])),
            node(
                "2",
                Some("1"),
                "navigation",
                "Main",
                &["3"],
                Some(2),
                json!([]),
            ),
            ignored,
            node("4", Some("3"), "link", "Home", &[], Some(4), json!([])),
        ];
        // Ignored nodes vanish but their children stay, one level up.
        assert_eq!(
            render(&nodes, None).content,
            "- navigation \"Main\" [ref=e1]\n  - link \"Home\" [ref=e2]\n"
        );
        assert_eq!(
            render(&nodes, Some(1)).content,
            "- navigation \"Main\" [ref=e1]\n"
        );
    }

    #[test]
    fn truncates_huge_pages() {
        let mut nodes = vec![node("r", None, "RootWebArea", "", &[], Some(1), json!([]))];
        let kids: Vec<String> = (0..20_000).map(|i| format!("n{i}")).collect();
        nodes[0]["childIds"] = json!(kids);
        for k in &kids {
            nodes.push(node(
                k,
                Some("r"),
                "link",
                "a fairly long link label here",
                &[],
                Some(9),
                json!([]),
            ));
        }
        let s = render(&nodes, None);
        assert!(s.truncated && s.content.len() <= MAX_TEXT);
    }
}
