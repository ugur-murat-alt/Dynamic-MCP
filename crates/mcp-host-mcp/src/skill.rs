use std::collections::BTreeMap;

use mcp_host_core::{
    CallPolicy, MAX_SKILL_STEPS, RuntimeError, RuntimeErrorCode, RuntimeSkill, SkillRunFailure,
    SkillRunResult, SkillRunStatus, SkillStep, SkillStepResult, SkillTemplatePart,
    SkillTemplateReference, ToolCallResult, parse_skill_template,
};
use serde_json::{Map, Value};

use crate::runtime::RuntimeManager;

const MAX_SKILL_RESULT_BYTES: usize = 7 * 1024 * 1024;

pub(crate) struct SkillEngine;

impl SkillEngine {
    pub(crate) async fn run(
        runtime: &RuntimeManager,
        skill: &RuntimeSkill,
        inputs: Value,
    ) -> Result<SkillRunResult, RuntimeError> {
        if skill.steps().is_empty() || skill.steps().len() > MAX_SKILL_STEPS {
            return Err(RuntimeError::new(
                RuntimeErrorCode::SkillInvalid,
                "skill_run",
                "the skill has an invalid number of steps",
            ));
        }
        let inputs = resolve_inputs(skill, inputs)?;
        let mut outputs = BTreeMap::new();
        let mut results = Vec::with_capacity(skill.steps().len());

        for (step_index, step) in skill.steps().iter().enumerate() {
            let arguments = match render_value(step.arguments(), &inputs, &outputs) {
                Ok(arguments) => arguments,
                Err(()) => {
                    return Ok(failed_result(
                        skill,
                        results,
                        step_index,
                        step,
                        RuntimeError::new(
                            RuntimeErrorCode::SkillTemplateError,
                            "skill_run",
                            "a skill template reference could not be resolved",
                        ),
                    ));
                }
            };
            let result = match runtime
                .call_tool(
                    step.server_id(),
                    step.tool_name(),
                    arguments,
                    step.timeout_ms(),
                    CallPolicy::default(),
                )
                .await
            {
                Ok(result) => result,
                Err(mut error) => {
                    error.operation = "skill_run".to_owned();
                    return Ok(failed_result(skill, results, step_index, step, error));
                }
            };
            let is_error = result.value().get("isError").and_then(Value::as_bool) == Some(true);
            results.push(step_result(step, result));
            if serde_json::to_vec(&results)
                .map_or(true, |encoded| encoded.len() > MAX_SKILL_RESULT_BYTES)
            {
                results.pop();
                return Ok(failed_result(
                    skill,
                    results,
                    step_index,
                    step,
                    RuntimeError::new(
                        RuntimeErrorCode::SkillOutputTooLarge,
                        "skill_run",
                        "the accumulated skill output is too large",
                    ),
                ));
            }
            if is_error {
                return Ok(failed_result(
                    skill,
                    results,
                    step_index,
                    step,
                    RuntimeError::new(
                        RuntimeErrorCode::SkillUpstreamError,
                        "skill_run",
                        "the downstream tool returned an error result",
                    ),
                ));
            }
            outputs.insert(
                step.id().to_owned(),
                results
                    .last()
                    .expect("the current step result was appended")
                    .result
                    .value()
                    .clone(),
            );
        }

        Ok(SkillRunResult {
            skill_id: skill.id().to_owned(),
            status: SkillRunStatus::Ok,
            steps_completed: results.len() as u64,
            steps_total: skill.steps().len() as u64,
            results,
            failure: None,
        })
    }
}

fn failed_result(
    skill: &RuntimeSkill,
    results: Vec<SkillStepResult>,
    step_index: usize,
    step: &SkillStep,
    error: RuntimeError,
) -> SkillRunResult {
    let steps_completed = results
        .iter()
        .filter(|result| {
            result
                .result
                .value()
                .get("isError")
                .and_then(Value::as_bool)
                != Some(true)
        })
        .count() as u64;
    SkillRunResult {
        skill_id: skill.id().to_owned(),
        status: SkillRunStatus::Error,
        steps_completed,
        steps_total: skill.steps().len() as u64,
        results,
        failure: Some(SkillRunFailure {
            step_index: step_index as u64,
            step_id: step.id().to_owned(),
            server_id: step.server_id().to_owned(),
            tool_name: step.tool_name().to_owned(),
            error,
        }),
    }
}

fn step_result(step: &SkillStep, result: ToolCallResult) -> SkillStepResult {
    SkillStepResult {
        step_id: step.id().to_owned(),
        server_id: step.server_id().to_owned(),
        tool_name: step.tool_name().to_owned(),
        result,
    }
}

fn resolve_inputs(skill: &RuntimeSkill, inputs: Value) -> Result<Map<String, Value>, RuntimeError> {
    let Value::Object(mut provided) = inputs else {
        return Err(input_error("skill inputs must be a JSON object"));
    };
    let mut resolved = Map::new();
    for input in skill.inputs() {
        let (value, absent_optional) = match provided.remove(input.name()) {
            Some(value) => (value, false),
            None => match input.default_value() {
                Some(value) => (value.clone(), false),
                None if input.required() => {
                    return Err(input_error("a required skill input is missing"));
                }
                None => (Value::Null, true),
            },
        };
        if !absent_optional && !input.input_type().accepts(&value) {
            return Err(input_error("a skill input has the wrong JSON type"));
        }
        resolved.insert(input.name().to_owned(), value);
    }
    if !provided.is_empty() {
        return Err(input_error("the skill input object has unknown fields"));
    }
    Ok(resolved)
}

fn input_error(message: &str) -> RuntimeError {
    RuntimeError::new(RuntimeErrorCode::SkillInputInvalid, "skill_run", message)
}

fn render_value(
    value: &Value,
    inputs: &Map<String, Value>,
    outputs: &BTreeMap<String, Value>,
) -> Result<Value, ()> {
    match value {
        Value::String(template) => render_string(template, inputs, outputs),
        Value::Array(values) => values
            .iter()
            .map(|value| render_value(value, inputs, outputs))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| {
                render_value(value, inputs, outputs).map(|value| (key.clone(), value))
            })
            .collect::<Result<Map<_, _>, _>>()
            .map(Value::Object),
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(value.clone()),
    }
}

fn render_string(
    template: &str,
    inputs: &Map<String, Value>,
    outputs: &BTreeMap<String, Value>,
) -> Result<Value, ()> {
    let parts = parse_skill_template(template).map_err(|_| ())?;
    if let [SkillTemplatePart::Reference(reference)] = parts.as_slice() {
        return resolve_reference(reference, inputs, outputs).cloned();
    }
    let mut rendered = String::new();
    for part in parts {
        match part {
            SkillTemplatePart::Literal(literal) => rendered.push_str(&literal),
            SkillTemplatePart::Reference(reference) => {
                rendered.push_str(&stringify(resolve_reference(&reference, inputs, outputs)?));
            }
        }
    }
    Ok(Value::String(rendered))
}

fn resolve_reference<'a>(
    reference: &SkillTemplateReference,
    inputs: &'a Map<String, Value>,
    outputs: &'a BTreeMap<String, Value>,
) -> Result<&'a Value, ()> {
    match reference {
        SkillTemplateReference::Input { name } => inputs.get(name).ok_or(()),
        SkillTemplateReference::StepOutput { step_id, path } => {
            let mut value = outputs.get(step_id).ok_or(())?;
            for segment in path {
                value = match value {
                    Value::Object(object) => object.get(segment).ok_or(())?,
                    Value::Array(array) => array
                        .get(segment.parse::<usize>().map_err(|_| ())?)
                        .ok_or(())?,
                    _ => return Err(()),
                };
            }
            Ok(value)
        }
    }
}

fn stringify(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Null => "null".to_owned(),
        Value::Bool(_) | Value::Number(_) | Value::Array(_) | Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use mcp_host_core::{SkillCatalog, SkillTemplateReference};
    use serde_json::{Map, Value, json};
    use tempfile::tempdir;

    use super::{render_value, resolve_inputs, resolve_reference};

    #[test]
    fn templates_preserve_typed_values_and_interpolate_strings() {
        let inputs = Map::from_iter([
            ("count".to_owned(), json!(3)),
            ("title".to_owned(), json!("Bug")),
        ]);
        let outputs = BTreeMap::from([(
            "create".to_owned(),
            json!({"structuredContent": {"url": "https://example.test/1"}}),
        )]);

        assert_eq!(
            render_value(&json!("${input.count}"), &inputs, &outputs),
            Ok(json!(3))
        );
        assert_eq!(
            render_value(
                &json!("${input.title}: ${steps.create.output.structuredContent.url}"),
                &inputs,
                &outputs,
            ),
            Ok(json!("Bug: https://example.test/1"))
        );
    }

    #[test]
    fn array_paths_are_bounded_and_missing_paths_fail() {
        let inputs = Map::new();
        let outputs = BTreeMap::from([("step".to_owned(), json!({"items": ["one"]}))]);
        assert_eq!(
            resolve_reference(
                &SkillTemplateReference::StepOutput {
                    step_id: "step".to_owned(),
                    path: vec!["items".to_owned(), "0".to_owned()]
                },
                &inputs,
                &outputs,
            )
            .cloned(),
            Ok(json!("one"))
        );
        assert!(render_value(&json!("${steps.step.output.items.1}"), &inputs, &outputs).is_err());
    }

    #[test]
    fn inputs_reject_missing_unknown_null_and_wrong_typed_values() {
        let directory = tempdir().expect("temporary directory");
        fs::write(
            directory.path().join("inputs.skill.toml"),
            "id='inputs'\nname='Inputs'\ndescription='Inputs'\n[[inputs]]\nname='count'\ntype='number'\n[[inputs]]\nname='label'\ntype='string'\nrequired=false\n[[steps]]\nid='run'\nserver='fixture'\ntool='echo'\n",
        )
        .expect("skill fixture");
        let skill = SkillCatalog::load_directory(directory.path())
            .expect("skill catalog")
            .get("inputs")
            .expect("skill");

        assert_eq!(
            resolve_inputs(&skill, json!({}))
                .expect_err("required input should fail")
                .code,
            mcp_host_core::RuntimeErrorCode::SkillInputInvalid
        );
        assert!(resolve_inputs(&skill, json!({"count": "wrong"})).is_err());
        assert!(resolve_inputs(&skill, json!({"count": null})).is_err());
        assert!(resolve_inputs(&skill, json!({"count": 1, "extra": true})).is_err());
        assert_eq!(
            resolve_inputs(&skill, json!({"count": 1}))
                .expect("valid inputs")
                .get("label"),
            Some(&Value::Null)
        );
    }
}
