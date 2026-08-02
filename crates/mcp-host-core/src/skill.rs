use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsStr,
    fmt, fs, io,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{ServerId, loader::compare_source_paths, registry::McpServerRegistry};

pub const MAX_SKILL_STEPS: usize = 16;

#[derive(Clone, Default, PartialEq)]
pub struct SkillCatalog {
    skills: BTreeMap<String, Arc<RuntimeSkill>>,
}

impl SkillCatalog {
    pub fn load_directory(directory: &Path) -> Result<Self, SkillLoadError> {
        let entries = fs::read_dir(directory).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                SkillLoadError::DirectoryNotFound {
                    path: directory.to_path_buf(),
                }
            } else {
                SkillLoadError::DirectoryUnreadable {
                    path: directory.to_path_buf(),
                    source,
                }
            }
        })?;
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| SkillLoadError::DirectoryUnreadable {
                path: directory.to_path_buf(),
                source,
            })?;
            let file_type =
                entry
                    .file_type()
                    .map_err(|source| SkillLoadError::DirectoryUnreadable {
                        path: directory.to_path_buf(),
                        source,
                    })?;
            let path = entry.path();
            if file_type.is_file() && is_skill_file(&path) {
                paths.push(path);
            }
        }
        paths.sort_by(|left, right| compare_source_paths(left, right));

        let mut skills = BTreeMap::new();
        let mut sources = BTreeMap::new();
        for path in paths {
            let contents = fs::read_to_string(&path).map_err(|source| {
                SkillLoadError::SkillFileUnreadable {
                    path: path.clone(),
                    source,
                }
            })?;
            let raw: SkillFile =
                toml::from_str(&contents).map_err(|error| SkillLoadError::SkillParse {
                    path: path.clone(),
                    span: error.span(),
                })?;
            let skill = validate_skill(raw).map_err(|source| SkillLoadError::SkillValidation {
                path: path.clone(),
                source,
            })?;
            let id = skill.id.clone();
            if let Some(first_path) = sources.insert(id.clone(), path.clone()) {
                return Err(SkillLoadError::DuplicateSkillId {
                    id,
                    first_path,
                    second_path: path,
                });
            }
            skills.insert(id, Arc::new(skill));
        }
        Ok(Self { skills })
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<RuntimeSkill>> {
        self.skills.get(id).cloned()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<RuntimeSkill>> {
        self.skills.values()
    }

    pub fn validate_server_references(
        &self,
        registry: &McpServerRegistry,
    ) -> Result<(), SkillReferenceError> {
        for skill in self.iter() {
            for step in skill.steps() {
                let registered = ServerId::parse(step.server_id())
                    .ok()
                    .is_some_and(|server_id| registry.get(&server_id).is_some());
                if !registered {
                    return Err(SkillReferenceError {
                        skill_id: skill.id().to_owned(),
                        step_id: step.id().to_owned(),
                        server_id: step.server_id().to_owned(),
                    });
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.skills.len()
    }
}

impl fmt::Debug for SkillCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SkillCatalog")
            .field("ids", &self.skills.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub struct RuntimeSkill {
    id: String,
    name: String,
    description: String,
    inputs: Vec<SkillInput>,
    steps: Vec<SkillStep>,
}

impl RuntimeSkill {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    #[must_use]
    pub fn inputs(&self) -> &[SkillInput] {
        &self.inputs
    }

    #[must_use]
    pub fn steps(&self) -> &[SkillStep] {
        &self.steps
    }

    #[must_use]
    pub fn summary(&self) -> SkillSummary {
        SkillSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            inputs: self.inputs.iter().map(SkillInput::summary).collect(),
            step_count: self.steps.len() as u64,
        }
    }
}

impl fmt::Debug for RuntimeSkill {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeSkill")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("input_count", &self.inputs.len())
            .field("step_count", &self.steps.len())
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub struct SkillInput {
    name: String,
    input_type: SkillInputType,
    required: bool,
    default: Option<Value>,
}

impl SkillInput {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn input_type(&self) -> SkillInputType {
        self.input_type
    }

    #[must_use]
    pub const fn required(&self) -> bool {
        self.required
    }

    #[must_use]
    pub fn default_value(&self) -> Option<&Value> {
        self.default.as_ref()
    }

    fn summary(&self) -> SkillInputSummary {
        SkillInputSummary {
            name: self.name.clone(),
            input_type: self.input_type,
            required: self.required,
            has_default: self.default.is_some(),
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct SkillStep {
    id: String,
    server_id: String,
    tool_name: String,
    arguments: Value,
    timeout_ms: Option<u64>,
}

impl SkillStep {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    #[must_use]
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    #[must_use]
    pub fn arguments(&self) -> &Value {
        &self.arguments
    }

    #[must_use]
    pub const fn timeout_ms(&self) -> Option<u64> {
        self.timeout_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillInputType {
    String,
    Number,
    Boolean,
    Json,
}

impl SkillInputType {
    #[must_use]
    pub fn accepts(self, value: &Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Number => value.is_number(),
            Self::Boolean => value.is_boolean(),
            Self::Json => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInputSummary {
    pub name: String,
    #[serde(rename = "type")]
    pub input_type: SkillInputType,
    pub required: bool,
    pub has_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub inputs: Vec<SkillInputSummary>,
    pub step_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillTemplatePart {
    Literal(String),
    Reference(SkillTemplateReference),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillTemplateReference {
    Input { name: String },
    StepOutput { step_id: String, path: Vec<String> },
}

pub fn parse_skill_template(value: &str) -> Result<Vec<SkillTemplatePart>, SkillTemplateError> {
    let mut parts = Vec::new();
    let mut cursor = 0;
    while let Some(relative_start) = value[cursor..].find("${") {
        let start = cursor + relative_start;
        if start > cursor {
            parts.push(SkillTemplatePart::Literal(value[cursor..start].to_owned()));
        }
        let expression_start = start + 2;
        let relative_end = value[expression_start..]
            .find('}')
            .ok_or(SkillTemplateError::UnterminatedReference)?;
        let end = expression_start + relative_end;
        parts.push(SkillTemplatePart::Reference(parse_reference(
            &value[expression_start..end],
        )?));
        cursor = end + 1;
    }
    if cursor < value.len() || parts.is_empty() {
        parts.push(SkillTemplatePart::Literal(value[cursor..].to_owned()));
    }
    Ok(parts)
}

fn parse_reference(expression: &str) -> Result<SkillTemplateReference, SkillTemplateError> {
    let segments = expression.split('.').collect::<Vec<_>>();
    if segments.iter().any(|segment| segment.is_empty()) {
        return Err(SkillTemplateError::InvalidReference);
    }
    match segments.as_slice() {
        ["input", name] if valid_local_id(name) => Ok(SkillTemplateReference::Input {
            name: (*name).to_owned(),
        }),
        ["steps", step_id, "output", path @ ..] if valid_local_id(step_id) => {
            Ok(SkillTemplateReference::StepOutput {
                step_id: (*step_id).to_owned(),
                path: path.iter().map(|segment| (*segment).to_owned()).collect(),
            })
        }
        _ => Err(SkillTemplateError::InvalidReference),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SkillTemplateError {
    #[error("unterminated skill template reference")]
    UnterminatedReference,
    #[error("invalid skill template reference")]
    InvalidReference,
}

#[derive(Debug, Error)]
pub enum SkillLoadError {
    #[error("skill directory `{path}` was not found", path = path.display())]
    DirectoryNotFound { path: PathBuf },
    #[error("skill directory `{path}` could not be read: {source}", path = path.display())]
    DirectoryUnreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("skill file `{path}` could not be read: {source}", path = path.display())]
    SkillFileUnreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "skill file `{path}` contains invalid TOML or skill structure at byte range {span:?}",
        path = path.display()
    )]
    SkillParse {
        path: PathBuf,
        span: Option<Range<usize>>,
    },
    #[error("skill file `{path}` failed validation: {source}", path = path.display())]
    SkillValidation {
        path: PathBuf,
        #[source]
        source: SkillValidationError,
    },
    #[error(
        "duplicate normalized skill ID `{id}` in `{first_path}` and `{second_path}`",
        first_path = first_path.display(),
        second_path = second_path.display()
    )]
    DuplicateSkillId {
        id: String,
        first_path: PathBuf,
        second_path: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("skill `{skill_id}` step `{step_id}` references unknown server `{server_id}`")]
pub struct SkillReferenceError {
    pub skill_id: String,
    pub step_id: String,
    pub server_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SkillValidationError {
    #[error("invalid skill ID")]
    InvalidSkillId,
    #[error("skill name and description must not be empty")]
    EmptyMetadata,
    #[error("skill must contain between 1 and {MAX_SKILL_STEPS} steps")]
    InvalidStepCount,
    #[error("invalid skill input name")]
    InvalidInput,
    #[error("duplicate skill input `{0}`")]
    DuplicateInput(String),
    #[error("input `{0}` has a default value of the wrong type")]
    InvalidInputDefault(String),
    #[error("invalid skill step ID")]
    InvalidStep,
    #[error("duplicate skill step `{0}`")]
    DuplicateStep(String),
    #[error("step `{0}` has an invalid server, tool, arguments, or timeout")]
    InvalidStepConfiguration(String),
    #[error("step `{0}` contains an invalid or forward template reference")]
    InvalidTemplateReference(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillFile {
    id: String,
    name: String,
    description: String,
    #[serde(default)]
    inputs: Vec<SkillInputFile>,
    #[serde(default)]
    steps: Vec<SkillStepFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillInputFile {
    name: String,
    #[serde(rename = "type")]
    input_type: SkillInputType,
    #[serde(default = "required_by_default")]
    required: bool,
    #[serde(default)]
    default: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillStepFile {
    id: String,
    server: String,
    tool: String,
    #[serde(default = "empty_arguments")]
    arguments: Value,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

const fn required_by_default() -> bool {
    true
}

fn empty_arguments() -> Value {
    Value::Object(Map::new())
}

fn validate_skill(file: SkillFile) -> Result<RuntimeSkill, SkillValidationError> {
    let id = ServerId::parse(&file.id)
        .map_err(|_| SkillValidationError::InvalidSkillId)?
        .as_str()
        .to_owned();
    if file.name.trim().is_empty() || file.description.trim().is_empty() {
        return Err(SkillValidationError::EmptyMetadata);
    }
    if file.steps.is_empty() || file.steps.len() > MAX_SKILL_STEPS {
        return Err(SkillValidationError::InvalidStepCount);
    }

    let mut input_names = HashSet::new();
    let mut inputs = Vec::with_capacity(file.inputs.len());
    for input in file.inputs {
        if !valid_local_id(&input.name) {
            return Err(SkillValidationError::InvalidInput);
        }
        if !input_names.insert(input.name.clone()) {
            return Err(SkillValidationError::DuplicateInput(input.name));
        }
        if input
            .default
            .as_ref()
            .is_some_and(|value| !input.input_type.accepts(value))
        {
            return Err(SkillValidationError::InvalidInputDefault(input.name));
        }
        inputs.push(SkillInput {
            name: input.name,
            input_type: input.input_type,
            required: input.required,
            default: input.default,
        });
    }

    let mut step_ids = HashSet::new();
    let mut previous_steps = HashSet::new();
    let mut steps = Vec::with_capacity(file.steps.len());
    for step in file.steps {
        if !valid_local_id(&step.id) {
            return Err(SkillValidationError::InvalidStep);
        }
        if !step_ids.insert(step.id.clone()) {
            return Err(SkillValidationError::DuplicateStep(step.id));
        }
        let server_id = ServerId::parse(&step.server)
            .map_err(|_| SkillValidationError::InvalidStepConfiguration(step.id.clone()))?
            .as_str()
            .to_owned();
        if step.tool.trim().is_empty()
            || !step.arguments.is_object()
            || step
                .timeout_ms
                .is_some_and(|timeout| timeout == 0 || timeout > 300_000)
        {
            return Err(SkillValidationError::InvalidStepConfiguration(step.id));
        }
        validate_template_value(&step.arguments, &input_names, &previous_steps, &step.id)?;
        previous_steps.insert(step.id.clone());
        steps.push(SkillStep {
            id: step.id,
            server_id,
            tool_name: step.tool,
            arguments: step.arguments,
            timeout_ms: step.timeout_ms,
        });
    }

    Ok(RuntimeSkill {
        id,
        name: file.name,
        description: file.description,
        inputs,
        steps,
    })
}

fn validate_template_value(
    value: &Value,
    inputs: &HashSet<String>,
    previous_steps: &HashSet<String>,
    current_step: &str,
) -> Result<(), SkillValidationError> {
    match value {
        Value::String(value) => {
            let parts = parse_skill_template(value).map_err(|_| {
                SkillValidationError::InvalidTemplateReference(current_step.to_owned())
            })?;
            for part in parts {
                let SkillTemplatePart::Reference(reference) = part else {
                    continue;
                };
                let valid = match reference {
                    SkillTemplateReference::Input { name } => inputs.contains(&name),
                    SkillTemplateReference::StepOutput { step_id, .. } => {
                        previous_steps.contains(&step_id)
                    }
                };
                if !valid {
                    return Err(SkillValidationError::InvalidTemplateReference(
                        current_step.to_owned(),
                    ));
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                validate_template_value(value, inputs, previous_steps, current_step)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_template_value(value, inputs, previous_steps, current_step)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn valid_local_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn is_skill_file(path: &Path) -> bool {
    let Some(file_name) = path.file_name() else {
        return false;
    };
    let file_name = file_name.to_string_lossy();
    !file_name.starts_with(['.', '~', '#'])
        && file_name.ends_with(".skill.toml")
        && path.extension() == Some(OsStr::new("toml"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        MAX_SKILL_STEPS, SkillCatalog, SkillLoadError, SkillTemplatePart, SkillTemplateReference,
        parse_skill_template,
    };

    #[test]
    fn loads_skills_deterministically_and_redacts_arguments_from_debug() {
        let directory = tempdir().expect("temporary directory");
        fs::write(
            directory.path().join("z.skill.toml"),
            skill("zeta", "secret-sentinel", "${input.title}"),
        )
        .expect("skill fixture");
        fs::write(
            directory.path().join("a.skill.toml"),
            skill("alpha", "safe", "${input.title}"),
        )
        .expect("skill fixture");
        fs::write(directory.path().join("server.toml"), "not a skill").expect("server fixture");

        let catalog = SkillCatalog::load_directory(directory.path()).expect("skills should load");
        let ids = catalog.iter().map(|skill| skill.id()).collect::<Vec<_>>();
        assert_eq!(ids, ["alpha", "zeta"]);
        assert!(!format!("{catalog:?}").contains("secret-sentinel"));
        assert!(!format!("{:?}", catalog.get("zeta")).contains("secret-sentinel"));
    }

    #[test]
    fn rejects_empty_large_and_forward_steps() {
        let directory = tempdir().expect("temporary directory");
        fs::write(
            directory.path().join("empty.skill.toml"),
            "id='empty'\nname='Empty'\ndescription='Empty'\n",
        )
        .expect("empty fixture");
        assert!(matches!(
            SkillCatalog::load_directory(directory.path()),
            Err(SkillLoadError::SkillValidation { .. })
        ));

        fs::remove_file(directory.path().join("empty.skill.toml")).expect("remove fixture");
        let mut large = "id='large'\nname='Large'\ndescription='Large'\n".to_owned();
        for index in 0..=MAX_SKILL_STEPS {
            large.push_str(&format!(
                "[[steps]]\nid='s{index}'\nserver='fixture'\ntool='echo'\n"
            ));
        }
        fs::write(directory.path().join("large.skill.toml"), large).expect("large fixture");
        assert!(SkillCatalog::load_directory(directory.path()).is_err());

        fs::remove_file(directory.path().join("large.skill.toml")).expect("remove fixture");
        fs::write(
            directory.path().join("forward.skill.toml"),
            "id='forward'\nname='Forward'\ndescription='Forward'\n[[steps]]\nid='first'\nserver='fixture'\ntool='echo'\narguments={value='${steps.second.output}'}\n[[steps]]\nid='second'\nserver='fixture'\ntool='echo'\n",
        )
        .expect("forward fixture");
        assert!(SkillCatalog::load_directory(directory.path()).is_err());
    }

    #[test]
    fn validates_typed_defaults_and_timeout_bounds() {
        let directory = tempdir().expect("temporary directory");
        fs::write(
            directory.path().join("invalid.skill.toml"),
            "id='invalid'\nname='Invalid'\ndescription='Invalid'\n[[inputs]]\nname='count'\ntype='number'\ndefault='wrong'\n[[steps]]\nid='run'\nserver='fixture'\ntool='echo'\ntimeout_ms=300001\n",
        )
        .expect("invalid fixture");
        assert!(SkillCatalog::load_directory(directory.path()).is_err());
    }

    #[test]
    fn rejects_duplicate_normalized_skill_ids_and_local_ids() {
        let directory = tempdir().expect("temporary directory");
        fs::write(
            directory.path().join("one.skill.toml"),
            skill("duplicate", "safe", "${input.title}"),
        )
        .expect("first skill fixture");
        fs::write(
            directory.path().join("two.skill.toml"),
            skill(" DUPLICATE ", "safe", "${input.title}"),
        )
        .expect("second skill fixture");
        assert!(matches!(
            SkillCatalog::load_directory(directory.path()),
            Err(SkillLoadError::DuplicateSkillId { .. })
        ));

        fs::remove_file(directory.path().join("two.skill.toml")).expect("remove fixture");
        fs::write(
            directory.path().join("one.skill.toml"),
            "id='duplicate'\nname='Duplicate'\ndescription='Duplicate'\n[[inputs]]\nname='value'\ntype='string'\n[[inputs]]\nname='value'\ntype='string'\n[[steps]]\nid='run'\nserver='fixture'\ntool='echo'\n",
        )
        .expect("duplicate input fixture");
        assert!(SkillCatalog::load_directory(directory.path()).is_err());
    }

    #[test]
    fn rejects_skill_steps_that_reference_an_unknown_server() {
        let directory = tempdir().expect("temporary directory");
        fs::write(
            directory.path().join("one.skill.toml"),
            skill("one", "safe", "${input.title}"),
        )
        .expect("skill fixture");
        let catalog = SkillCatalog::load_directory(directory.path()).expect("skill catalog");
        assert!(
            catalog
                .validate_server_references(&crate::McpServerRegistry::default())
                .is_err()
        );
    }

    #[test]
    fn parses_typed_and_embedded_template_parts() {
        assert_eq!(
            parse_skill_template("${input.title}").expect("template"),
            [SkillTemplatePart::Reference(
                SkillTemplateReference::Input {
                    name: "title".to_owned()
                }
            )]
        );
        assert_eq!(
            parse_skill_template("Issue ${steps.create.output.structuredContent.url}")
                .expect("template")
                .len(),
            2
        );
        assert!(parse_skill_template("${steps.missing}").is_err());
        assert!(parse_skill_template("${input.title").is_err());
    }

    #[test]
    fn summaries_do_not_expose_defaults_or_arguments() {
        let directory = tempdir().expect("temporary directory");
        fs::write(
            directory.path().join("one.skill.toml"),
            skill("one", "secret-sentinel", "${input.title}"),
        )
        .expect("skill fixture");
        let catalog = SkillCatalog::load_directory(directory.path()).expect("skills should load");
        let summary = catalog.get("one").expect("skill").summary();
        let encoded = serde_json::to_string(&summary).expect("summary serializes");
        assert!(!encoded.contains("secret-sentinel"));
        assert_eq!(summary.inputs[0].input_type, super::SkillInputType::String);
        assert_eq!(summary.step_count, 1);
    }

    fn skill(id: &str, argument: &str, template: &str) -> String {
        format!(
            "id='{id}'\nname='Skill'\ndescription='Skill'\n[[inputs]]\nname='title'\ntype='string'\ndefault='default-sentinel'\n[[steps]]\nid='run'\nserver='fixture'\ntool='echo'\narguments={{message='{template}',constant='{argument}'}}\n"
        )
    }
}
