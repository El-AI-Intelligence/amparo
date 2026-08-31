//! Skills — named procedures as hypotheses (M6c).
//!
//! A skill is a named procedure: preconditions, an ordered list of tool-call
//! steps with fixed arguments, and an expected outcome. Skills originate
//! operator-authored (like writing a script) or as distilled candidates
//! proposed from the notebook — inert until adopted.
//!
//! The [`UseSkillTool`] executor is a registry-level placeholder: the real
//! work happens in the agent loop (`amparo-agent`), which intercepts
//! `use_skill` calls and expands them step by step, running every step
//! through the gate chain individually. A skill can never grant its steps an
//! exemption.

use super::{
    ToolCall, ToolExecutor, ToolParam, ToolRegistry, ToolResult, ToolSchema, ToolTrustTier,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// The name of the skill-invocation tool, as exposed to the LLM.
pub const USE_SKILL: &str = "use_skill";

// ─────────────────────────────────────────────── Skill definition ────────────

/// Where a skill came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillOrigin {
    /// Written by the operator accountable for the deployment.
    Operator,
    /// Distilled from the notebook as a candidate; adopted only after
    /// operator review.
    Distilled,
}

impl Default for SkillOrigin {
    fn default() -> Self {
        SkillOrigin::Operator
    }
}

/// One step of a skill — a tool call with fixed authored arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillStep {
    /// Name of the tool to call.
    pub tool: String,
    /// Fixed arguments for the call. Must be a JSON object; the step runs
    /// with exactly these arguments.
    pub arguments: serde_json::Value,
}

/// A named procedure: preconditions, ordered steps, an expected outcome.
///
/// Candidate TOML files deserialize into this type directly, and unknown
/// fields are an error (the same `deny_unknown_fields` strictness as the
/// chat config) — a typo in a candidate must surface at `add`/`adopt`,
/// not silently vanish.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillSpec {
    /// Skill name — a slug: `[a-z0-9][a-z0-9-]*`.
    pub name: String,
    /// One-line description, listed on the `use_skill` tool for the model.
    pub description: String,
    /// Declarative preconditions, shown to the operator at adoption.
    #[serde(default)]
    pub preconditions: Vec<String>,
    /// Ordered tool-call steps with fixed arguments.
    pub steps: Vec<SkillStep>,
    /// What the procedure should achieve; checked after execution.
    pub expected_outcome: String,
    /// Where the skill came from.
    #[serde(default)]
    pub origin: SkillOrigin,
    /// Notebook run ids that produced this skill (provenance, distilled
    /// candidates).
    #[serde(default)]
    pub source_run_ids: Vec<String>,
    /// RFC 3339 adoption timestamp, set when the skill is adopted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted_at: Option<String>,
}

/// A skill name is a slug: `[a-z0-9][a-z0-9-]*` (lowercase letters, digits,
/// internal hyphens).
fn is_slug(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

impl SkillSpec {
    /// Validate the spec against a tool registry: the name must be a slug,
    /// description and expected outcome non-empty, at least one step, and
    /// every step must call a registered tool (never `use_skill` — nested
    /// skills are not supported) with an object of arguments.
    pub fn validate(&self, registry: &ToolRegistry) -> Result<(), String> {
        if !is_slug(&self.name) {
            return Err(format!(
                "skill name {:?} is not a slug (lowercase letters, digits, internal hyphens)",
                self.name
            ));
        }
        if self.description.trim().is_empty() {
            return Err(format!(
                "skill {:?} description must be non-empty",
                self.name
            ));
        }
        if self.expected_outcome.trim().is_empty() {
            return Err(format!(
                "skill {:?} expected_outcome must be non-empty",
                self.name
            ));
        }
        if self.steps.is_empty() {
            return Err(format!("skill {:?} has no steps", self.name));
        }
        for (i, step) in self.steps.iter().enumerate() {
            if step.tool == USE_SKILL {
                return Err(format!(
                    "skill {:?} step {} calls use_skill; nested skills are not supported",
                    self.name,
                    i + 1
                ));
            }
            if !step.arguments.is_object() {
                return Err(format!(
                    "skill {:?} step {} arguments must be a JSON object",
                    self.name,
                    i + 1
                ));
            }
            if registry.get_executor(&step.tool).is_none() {
                return Err(format!(
                    "skill {:?} step {} calls unknown tool {:?}",
                    self.name,
                    i + 1,
                    step.tool
                ));
            }
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────── Skill library seam ──────────

/// The library of adopted skills an agent may invoke.
///
/// Synchronous by design: implementations load a tenant's skills up front
/// (tenant scoping is the implementation's job) and serve lookups from
/// memory. Engram is a candidate backend, never a dependency.
pub trait SkillLibrary: Send + Sync {
    /// Names of every skill in the library.
    fn names(&self) -> Vec<String>;
    /// Fetch a skill by name.
    fn get(&self, name: &str) -> Option<SkillSpec>;
}

// ─────────────────────────────────────────────── UseSkillTool ────────────────

/// The `use_skill` tool — the model-facing entry point for skills.
///
/// Its schema is rendered fresh on every request, so the skill listing the
/// model sees always matches the library. Execution is deliberately
/// unsupported here: the agent loop (`amparo-agent`) intercepts `use_skill`
/// calls and expands them step by step through the gate chain. The executor
/// is a defensive placeholder so a direct dispatch (e.g. over MCP, which
/// cannot expand in the loop) fails loudly instead of silently skipping
/// gates.
pub struct UseSkillTool {
    library: Arc<dyn SkillLibrary>,
}

impl UseSkillTool {
    /// Creates a `use_skill` tool over a skill library.
    pub fn new(library: Arc<dyn SkillLibrary>) -> Self {
        Self { library }
    }

    /// Render the adopted-skill listing (name + one-line description) for a
    /// tool description. At most `cap` skills are listed; the remainder are
    /// summarized as "… (N more)". An empty library renders the
    /// no-skills message.
    pub fn render_description(names: &[(String, String)], cap: usize) -> String {
        if names.is_empty() {
            return "No adopted skills for this tenant.".to_string();
        }
        let listed: Vec<String> = names
            .iter()
            .take(cap)
            .map(|(name, desc)| format!("{name} — {desc}"))
            .collect();
        let mut text = format!("Adopted skills: {}", listed.join("; "));
        if names.len() > cap {
            text.push_str(&format!(" … ({} more)", names.len() - cap));
        }
        text
    }
}

#[async_trait]
impl ToolExecutor for UseSkillTool {
    fn schema(&self) -> ToolSchema {
        let mut pairs: Vec<(String, String)> = self
            .library
            .names()
            .into_iter()
            .filter_map(|name| self.library.get(&name).map(|spec| (name, spec.description)))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        ToolSchema {
            name: USE_SKILL.to_string(),
            description: format!(
                "Invoke an adopted skill by name; the skill expands into its \
                 ordered tool steps, and every step still passes the gate \
                 chain individually.\n{}",
                Self::render_description(&pairs, 12)
            ),
            parameters: vec![ToolParam {
                name: "skill_name".to_string(),
                description: "Name of the adopted skill to invoke.".to_string(),
                param_type: "string".to_string(),
                enum_values: if pairs.is_empty() {
                    None
                } else {
                    Some(pairs.iter().map(|(name, _)| name.clone()).collect())
                },
                required: true,
            }],
            trust_tier: ToolTrustTier::Observational,
        }
    }

    async fn execute(&self, call: &ToolCall) -> ToolResult {
        let name = call.arg_str("skill_name").unwrap_or("");
        ToolResult {
            tool_call_id: call.id.clone(),
            tool_name: call.name.clone(),
            success: false,
            output: serde_json::json!({
                "error": "skills expand inside the agent loop; direct execution is not supported",
                "skill_name": name,
            }),
            display_summary:
                "use_skill expands inside the agent loop; direct execution is not supported"
                    .to_string(),
            duration_ms: 0,
        }
    }
}

// ───────────────────────────────────────────────────────────── Tests ─────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::default_registry;

    fn spec(name: &str) -> SkillSpec {
        SkillSpec {
            name: name.to_string(),
            description: "reads a file".to_string(),
            preconditions: vec![],
            steps: vec![SkillStep {
                tool: "read_file".to_string(),
                arguments: serde_json::json!({"path": "notes.txt"}),
            }],
            expected_outcome: "file contents returned".to_string(),
            origin: SkillOrigin::Operator,
            source_run_ids: vec![],
            adopted_at: None,
        }
    }

    /// A test library over an owned spec list.
    struct VecLibrary(Vec<SkillSpec>);

    impl SkillLibrary for VecLibrary {
        fn names(&self) -> Vec<String> {
            self.0.iter().map(|s| s.name.clone()).collect()
        }
        fn get(&self, name: &str) -> Option<SkillSpec> {
            self.0.iter().find(|s| s.name == name).cloned()
        }
    }

    fn tool_with(specs: Vec<SkillSpec>) -> UseSkillTool {
        UseSkillTool::new(Arc::new(VecLibrary(specs)))
    }

    #[test]
    fn validate_accepts_a_well_formed_spec() {
        let registry = default_registry();
        let mut s = spec("read-notes");
        s.steps = vec![
            SkillStep {
                tool: "read_file".to_string(),
                arguments: serde_json::json!({"path": "a.txt"}),
            },
            SkillStep {
                tool: "run_tests".to_string(),
                arguments: serde_json::json!({}),
            },
        ];
        assert_eq!(s.validate(&registry), Ok(()));
    }

    #[test]
    fn validate_rejects_bad_slugs() {
        let registry = default_registry();
        // Note: `[a-z0-9][a-z0-9-]*` permits a trailing hyphen by definition.
        for bad in ["Bad_Name", "-leading", "UPPER", ""] {
            let mut s = spec("valid");
            s.name = bad.to_string();
            assert!(
                s.validate(&registry).is_err(),
                "slug {:?} must be rejected",
                bad
            );
        }
    }

    #[test]
    fn validate_rejects_empty_fields_and_steps() {
        let registry = default_registry();
        let mut s = spec("ok-name");
        s.description = "  ".to_string();
        assert!(
            s.validate(&registry).is_err(),
            "blank description must be rejected"
        );

        let mut s = spec("ok-name");
        s.expected_outcome = String::new();
        assert!(
            s.validate(&registry).is_err(),
            "blank outcome must be rejected"
        );

        let mut s = spec("ok-name");
        s.steps.clear();
        assert!(
            s.validate(&registry).is_err(),
            "empty steps must be rejected"
        );
    }

    #[test]
    fn validate_rejects_nested_use_skill() {
        let registry = default_registry();
        let mut s = spec("nested");
        s.steps[0].tool = USE_SKILL.to_string();
        let err = s.validate(&registry).unwrap_err();
        assert!(err.contains("nested skills"), "got: {}", err);
    }

    #[test]
    fn validate_rejects_non_object_arguments() {
        let registry = default_registry();
        let mut s = spec("bad-args");
        s.steps[0].arguments = serde_json::json!("ls -la");
        let err = s.validate(&registry).unwrap_err();
        assert!(err.contains("JSON object"), "got: {}", err);
    }

    #[test]
    fn validate_rejects_unknown_step_tools() {
        let registry = default_registry();
        let mut s = spec("ghost-step");
        s.steps[0].tool = "no_such_tool".to_string();
        let err = s.validate(&registry).unwrap_err();
        assert!(err.contains("no_such_tool"), "got: {}", err);
    }

    #[test]
    fn render_description_lists_names_and_caps() {
        let names: Vec<(String, String)> = (0..3)
            .map(|i| (format!("skill-{i}"), format!("does thing {i}")))
            .collect();
        let text = UseSkillTool::render_description(&names, 2);
        assert!(text.contains("skill-0 — does thing 0"));
        assert!(text.contains("skill-1 — does thing 1"));
        assert!(
            !text.contains("skill-2 — does thing 2"),
            "cap must trim: {}",
            text
        );
        assert!(text.contains("(1 more)"), "got: {}", text);
    }

    #[test]
    fn render_description_empty_library() {
        assert_eq!(
            UseSkillTool::render_description(&[], 12),
            "No adopted skills for this tenant."
        );
    }

    #[test]
    fn schema_lists_adopted_names_at_observational() {
        let tool = tool_with(vec![spec("alpha"), spec("beta")]);
        let schema = tool.schema();
        assert_eq!(schema.name, USE_SKILL);
        assert_eq!(schema.trust_tier, ToolTrustTier::Observational);
        assert_eq!(schema.parameters.len(), 1);
        let param = &schema.parameters[0];
        assert_eq!(param.name, "skill_name");
        assert_eq!(param.param_type, "string");
        assert!(param.required);
        let enums = param
            .enum_values
            .as_ref()
            .expect("names must be enumerated");
        assert_eq!(enums, &vec!["alpha".to_string(), "beta".to_string()]);
        assert!(schema.description.contains("alpha — reads a file"));
        assert!(schema.description.contains("beta — reads a file"));
    }

    #[test]
    fn schema_omits_enum_when_library_empty() {
        let schema = tool_with(vec![]).schema();
        assert!(schema.parameters[0].enum_values.is_none());
        assert!(schema.description.contains("No adopted skills"));
    }

    #[tokio::test]
    async fn execute_is_an_explanatory_failure() {
        let tool = tool_with(vec![spec("alpha")]);
        let call = ToolCall {
            id: "1".to_string(),
            name: USE_SKILL.to_string(),
            arguments: serde_json::json!({"skill_name": "alpha"}),
        };
        let result = tool.execute(&call).await;
        assert!(!result.success);
        assert_eq!(result.tool_call_id, "1");
        assert!(result.output["error"]
            .as_str()
            .unwrap()
            .contains("expand inside the agent loop"));
        assert_eq!(result.output["skill_name"], "alpha");
    }
}
