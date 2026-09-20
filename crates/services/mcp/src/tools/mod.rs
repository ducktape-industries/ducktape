//! the tool table: one flat registry of every tool the plane exposes, split
//! read / write because the two halves have genuinely different rules.
//!
//! - READ tools ([`read`]) serve `whoami`, the operation catalog, one generic
//!   `query` over a host-side table of read operations (whose floor is the
//!   `query` operation: any module's own query, verbatim), and receipt lookup.
//!   reads are not gated.
//! - the ONE WRITE tool ([`write`]) carries a catalog envelope the runs module
//!   decodes in consensus, whose floor is the `submit` operation: any module's
//!   own message, verbatim, under the run's program account. there is exactly
//!   one catalog of operations, owned by the module that executes them.
//!
//! a tool's `description` is not decoration: it is the entire interface the
//! model has. it says what the tool reads or writes and where the catalog is,
//! so a refused agent can read what the module could not accept.

use serde_json::{Value, json};

use crate::identity::Run;
use crate::node::Result;

mod control;
mod read;
mod write;

/// one tool: its MCP declaration and the handler behind it.
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    /// the JSON Schema for `arguments`, built by [`schema`].
    pub schema: fn() -> Value,
    pub handler: fn(&Run, &Value) -> Result<Value>,
}

/// every tool, in the order `tools/list` reports them: `whoami` first (an agent
/// that reads nothing else should still read this), then the rest of the read
/// plane, then the write plane.
pub fn all() -> Vec<Tool> {
    let mut tools = read::tools();
    tools.extend(control::tools());
    tools.extend(write::tools());
    tools
}

pub fn find(name: &str) -> Option<Tool> {
    all().into_iter().find(|t| t.name == name)
}

/// the `tools/list` payload.
pub fn list() -> Value {
    let tools: Vec<Value> = all()
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "title": t.name.replacen("ducktape_", "ducktape::", 1),
                "description": t.description,
                "inputSchema": (t.schema)(),
            })
        })
        .collect();
    json!({"tools": tools})
}

/// A JSON Schema object from `(name, type, required, description)` rows.
/// Tools with structured arguments extend these scalar properties in their
/// own schema builder.
pub fn schema(props: &[(&str, &str, bool, &str)]) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, ty, req, desc) in props {
        properties.insert(
            (*name).to_string(),
            json!({"type": ty, "description": desc}),
        );
        if *req {
            required.push(Value::String((*name).to_string()));
        }
    }
    json!({
        "type": "object",
        "properties": Value::Object(properties),
        "required": required,
    })
}

/// a required string argument, or a refusal naming it. the model gets the
/// argument's NAME back, not "invalid input" — it can only fix what it can see.
pub fn arg_str(args: &Value, name: &str) -> Result<String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            crate::node::NodeError::Rejected(format!(
                "this tool needs a string {name:?} argument"
            ))
        })
}

/// an optional integer argument.
pub fn opt_u64(args: &Value, name: &str) -> Option<u64> {
    args.get(name).and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module_contracts::runs;

    #[test]
    fn every_tool_is_uniquely_named_and_declares_a_schema() {
        let tools = all();
        let mut names: Vec<&str> = tools.iter().map(|t| t.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "tool names must be unique");
        for t in &tools {
            assert!(
                t.name.starts_with("ducktape_"),
                "{} is not namespaced",
                t.name
            );
            assert!(!t.description.is_empty(), "{} has no description", t.name);
            let schema = (t.schema)();
            assert_eq!(
                schema["type"], "object",
                "{} schema is not an object",
                t.name
            );
        }
    }

    #[test]
    fn the_one_write_tool_points_at_the_catalog() {
        // the description is the model's only view of the door. the write tool
        // must send the model to the catalog that names each operation's
        // schemas, and every live catalog operation must be reachable through it.
        let [write] = write::tools().try_into().ok().expect("one write tool");
        assert_eq!(write.name, "ducktape_action");
        assert!(write.description.contains("ducktape_actions"));
        for operation in runs::catalog(None) {
            if !operation.lanes.contains(&runs::LaneKind::Live) {
                continue;
            }
            assert!(
                write.description.contains(&operation.name),
                "the write tool does not name the live operation {}",
                operation.name
            );
        }
    }

    #[test]
    fn list_reports_every_tool() {
        let listed = list();
        assert_eq!(listed["tools"].as_array().unwrap().len(), all().len());
    }

    #[test]
    fn find_resolves_by_name_and_rejects_an_unknown_one() {
        assert!(find("ducktape_whoami").is_some());
        assert!(find("ducktape_not_a_tool").is_none());
    }

    #[test]
    fn schema_marks_only_the_required_rows() {
        let s = schema(&[
            ("a", "string", true, "required"),
            ("b", "integer", false, "optional"),
        ]);
        assert_eq!(s["required"], json!(["a"]));
        assert_eq!(s["properties"]["b"]["type"], "integer");
    }
}
