//! Fixture loader for `fixtures/herdr/*.json`: rejects unknown
//! fields rather than silently dropping them.

use serde_json::{json, Map, Value};
use thiserror::Error;

/// The four measured list surfaces. Captures are embedded so the fake also
/// works outside a source checkout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Shape {
    Workspace,
    Pane,
    Agent,
    Tab,
}

#[derive(Debug, Error)]
#[error("invalid Herdr shape: {0}")]
pub struct ShapeError(String);

impl Shape {
    pub fn plural(self) -> &'static str {
        match self {
            Self::Workspace => "workspaces",
            Self::Pane => "panes",
            Self::Agent => "agents",
            Self::Tab => "tabs",
        }
    }

    pub fn singular(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Pane => "pane",
            Self::Agent => "agent",
            Self::Tab => "tab",
        }
    }

    pub fn capture(self) -> Value {
        let source = match self {
            Self::Workspace => include_str!("../../../fixtures/herdr/workspace-list.json"),
            Self::Pane => include_str!("../../../fixtures/herdr/pane-list.json"),
            Self::Agent => include_str!("../../../fixtures/herdr/agent-list.json"),
            Self::Tab => include_str!("../../../fixtures/herdr/tab-list.json"),
        };
        serde_json::from_str(source).expect("committed Herdr capture must be JSON")
    }

    pub fn records(self) -> Vec<Value> {
        self.capture()["result"][self.plural()]
            .as_array()
            .expect("committed capture must contain a record array")
            .clone()
    }

    /// Herdr 0.9.5 capture for this shape. Pane and agent records grew a
    /// nested `agent_session` object relative to the 0.9.3 templates
    /// [`Self::records`] returns; workspace and tab records did not.
    pub fn capture_0_9_5(self) -> Value {
        let source = match self {
            Self::Workspace => include_str!("../../../fixtures/herdr/workspace-list-0.9.5.json"),
            Self::Pane => include_str!("../../../fixtures/herdr/pane-list-0.9.5.json"),
            Self::Agent => include_str!("../../../fixtures/herdr/agent-list-0.9.5.json"),
            Self::Tab => include_str!("../../../fixtures/herdr/tab-list-0.9.5.json"),
        };
        serde_json::from_str(source).expect("committed 0.9.5 Herdr capture must be JSON")
    }

    pub fn records_0_9_5(self) -> Vec<Value> {
        self.capture_0_9_5()["result"][self.plural()]
            .as_array()
            .expect("0.9.5 capture must contain a record array")
            .clone()
    }

    /// Templates [`Self::validate`] measures a record against: the 0.9.3
    /// capture plus the 0.9.5 capture, so a field Herdr added in 0.9.5
    /// (`agent_session` on pane and agent) is accepted rather than
    /// rejected as unknown (card `20261103-b-0b69ac`).
    fn measured_templates(self) -> Vec<Value> {
        let mut templates = self.records();
        templates.extend(self.records_0_9_5());
        templates
    }

    /// Validate only present fields: deliberately missing and optional fields
    /// survive envelope construction, as they do in the Python shape helpers.
    pub fn validate(self, record: &Value) -> Result<(), ShapeError> {
        let object = record
            .as_object()
            .ok_or_else(|| ShapeError(format!("{} record must be an object", self.singular())))?;
        let templates = self.measured_templates();
        for (key, value) in object {
            let measured: Vec<_> = templates.iter().filter_map(|r| r.get(key)).collect();
            if measured.is_empty() {
                return Err(ShapeError(format!(
                    "unknown {} field {key:?}",
                    self.singular()
                )));
            }
            // Nested objects (pane scroll, pane/agent agent_session) have
            // measured field sets too.
            if let Some(nested) = value.as_object() {
                for field in nested.keys() {
                    if !measured.iter().any(|v| v.get(field).is_some()) {
                        return Err(ShapeError(format!("unknown field {key}.{field}")));
                    }
                }
            }
        }
        Ok(())
    }

    /// Clone a measured record, then apply checked overrides. Updating cwd also
    /// updates foreground_cwd unless the caller explicitly supplied both.
    pub fn record(self, overrides: Map<String, Value>) -> Result<Value, ShapeError> {
        let mut record = self.records().remove(0);
        self.validate(&Value::Object(overrides.clone()))?;
        let object = record.as_object_mut().expect("capture record is an object");
        if let Some(cwd) = overrides.get("cwd") {
            if !overrides.contains_key("foreground_cwd") {
                object.insert("foreground_cwd".into(), cwd.clone());
            }
        }
        object.extend(overrides);
        Ok(record)
    }

    pub fn envelope(self, records: Vec<Value>) -> Result<Value, ShapeError> {
        for record in &records {
            self.validate(record)?;
        }
        let mut result = Map::new();
        result.insert("type".into(), json!(format!("{}_list", self.singular())));
        result.insert(self.plural().into(), Value::Array(records));
        Ok(json!({"id": format!("cli:{}:list", self.singular()), "result": result}))
    }
}

/// An agent detected before the caller has assigned its stable name.
pub fn pre_rename_agent(overrides: Map<String, Value>) -> Result<Value, ShapeError> {
    let mut agent = Shape::Agent.record(overrides)?;
    agent
        .as_object_mut()
        .expect("agent is an object")
        .remove("name");
    Ok(agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overrides(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn builders_start_from_live_capture() {
        for shape in [Shape::Workspace, Shape::Pane, Shape::Agent, Shape::Tab] {
            assert_eq!(shape.envelope(shape.records()).unwrap(), shape.capture());
            assert_eq!(shape.record(Map::new()).unwrap(), shape.records()[0]);
            assert_eq!(
                shape.envelope(vec![]).unwrap()["result"][shape.plural()],
                json!([])
            );
        }
    }

    #[test]
    fn workspace_and_tab_cannot_invent_cwd() {
        for shape in [Shape::Workspace, Shape::Tab] {
            assert!(shape.record(overrides(json!({"cwd": "/project"}))).is_err());
            assert!(shape.envelope(vec![json!({"cwd": "/project"})]).is_err());
        }
        assert!(Shape::Pane
            .record(overrides(json!({"invented": true})))
            .is_err());
        assert!(Shape::Pane
            .record(overrides(json!({"scroll": {"invented": 1}})))
            .is_err());
    }

    #[test]
    fn overrides_and_mutable_records_are_isolated() {
        let mut first = Shape::Pane
            .record(overrides(json!({"cwd": "/project"})))
            .unwrap();
        assert_eq!(first["foreground_cwd"], "/project");
        first["scroll"]["viewport_rows"] = json!(1);
        assert_ne!(
            Shape::Pane.record(Map::new()).unwrap()["scroll"]["viewport_rows"],
            1
        );
        assert_ne!(Shape::Pane.records()[0]["cwd"], "/project");
    }

    #[test]
    fn explicit_foreground_cwd_preserves_precedence_cases() {
        let record = Shape::Agent
            .record(overrides(
                json!({"cwd": "/base", "foreground_cwd": "/actual"}),
            ))
            .unwrap();
        assert_eq!(record["cwd"], "/base");
        assert_eq!(record["foreground_cwd"], "/actual");
    }

    #[test]
    fn pre_rename_is_explicit_and_keeps_named_entries() {
        let unnamed = pre_rename_agent(Map::new()).unwrap();
        let named = Shape::Agent
            .record(overrides(json!({"name": "test-agent"})))
            .unwrap();
        let envelope = Shape::Agent.envelope(vec![unnamed, named]).unwrap();
        assert!(envelope["result"]["agents"][0].get("name").is_none());
        assert_eq!(envelope["result"]["agents"][1]["name"], "test-agent");
    }

    #[test]
    fn deliberate_missing_field_survives_envelope_builder() {
        let mut agent = Shape::Agent.records().remove(1);
        assert!(agent.get("completion_seq").is_some());
        agent.as_object_mut().unwrap().remove("pane_id");
        let envelope = Shape::Agent.envelope(vec![agent]).unwrap();
        assert!(envelope["result"]["agents"][0].get("pane_id").is_none());
        assert!(envelope["result"]["agents"][0]
            .get("completion_seq")
            .is_some());
    }

    // --- Second capture (herdr 0.9.5), alongside the committed 0.9.3
    // captures. `Shape::validate` now unions both field sets so a 0.9.5
    // `agent_session` is a legal override, not an unknown field.

    fn second_capture(shape: Shape) -> Value {
        shape.capture_0_9_5()
    }

    fn second_capture_records(shape: Shape) -> Vec<Value> {
        shape.records_0_9_5()
    }

    fn collect_fields(value: &Value, prefix: &str, out: &mut std::collections::BTreeSet<String>) {
        if let Some(object) = value.as_object() {
            for (key, v) in object {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                out.insert(path.clone());
                collect_fields(v, &path, out);
            }
        }
    }

    fn field_set(records: &[Value]) -> std::collections::BTreeSet<String> {
        let mut fields = std::collections::BTreeSet::new();
        for record in records {
            collect_fields(record, "", &mut fields);
        }
        fields
    }

    #[test]
    fn second_capture_parses_into_the_same_envelope_shape() {
        for shape in [Shape::Workspace, Shape::Pane, Shape::Agent, Shape::Tab] {
            let capture = second_capture(shape);
            assert_eq!(
                capture["result"]["type"],
                json!(format!("{}_list", shape.singular()))
            );
            let records = second_capture_records(shape);
            assert!(!records.is_empty());
            for record in &records {
                assert!(record.is_object());
            }
        }
    }

    /// Every field measured in the committed 0.9.3 captures is still
    /// present somewhere in the fresh 0.9.5 capture, except
    /// `agent.completion_seq`: no agent in the 0.9.5 sample is `done`, so
    /// that field never appears in this sample. Its absence here is a
    /// property of the sampled data, not evidence Herdr 0.9.5 dropped it.
    #[test]
    fn second_capture_fields_are_a_superset_of_first_capture() {
        for shape in [Shape::Workspace, Shape::Pane, Shape::Agent, Shape::Tab] {
            let first = field_set(&shape.records());
            let second = field_set(&second_capture_records(shape));
            let missing: Vec<_> = first
                .difference(&second)
                .filter(|field| field.as_str() != "completion_seq")
                .collect();
            assert!(
                missing.is_empty(),
                "{shape:?} fields in the 0.9.3 capture missing from 0.9.5: {missing:?}"
            );
        }
    }

    /// The only field-set change observed between 0.9.3 and 0.9.5: pane and
    /// agent records gained a nested `agent_session` object. Workspace and
    /// tab records are unchanged.
    #[test]
    fn second_capture_adds_agent_session_to_pane_and_agent_only() {
        for shape in [Shape::Pane, Shape::Agent] {
            let second = field_set(&second_capture_records(shape));
            for field in [
                "agent_session",
                "agent_session.agent",
                "agent_session.kind",
                "agent_session.source",
                "agent_session.value",
            ] {
                assert!(second.contains(field), "{shape:?} 0.9.5 missing {field}");
            }
        }
        for shape in [Shape::Workspace, Shape::Tab] {
            let second = field_set(&second_capture_records(shape));
            assert!(!second.contains("agent_session"));
        }
    }

    #[test]
    fn validate_accepts_0_9_5_agent_session_on_pane_and_agent() {
        let session = json!({
            "agent_session": {
                "agent": "claude",
                "kind": "id",
                "source": "herdr:claude",
                "value": "00000000-0000-4000-8000-000000000006"
            }
        });
        assert!(Shape::Pane.record(overrides(session.clone())).is_ok());
        assert!(Shape::Agent.record(overrides(session.clone())).is_ok());
        assert!(Shape::Workspace.record(overrides(session.clone())).is_err());
        assert!(Shape::Tab.record(overrides(session.clone())).is_err());
        assert!(Shape::Pane
            .record(overrides(json!({"agent_session": {"invented": true}})))
            .is_err());
    }
}
