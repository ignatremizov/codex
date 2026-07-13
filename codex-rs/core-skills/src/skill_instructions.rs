use codex_context_fragments::ContextualUserFragment;

use crate::injection::SkillInjection;

#[derive(Debug, Clone, PartialEq)]
pub struct SkillInstructions {
    name: String,
    path: String,
    contents: String,
}

impl SkillInstructions {
    pub fn new(
        name: impl Into<String>,
        path: impl Into<String>,
        contents: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            contents: contents.into(),
        }
    }
}

impl From<&SkillInjection> for SkillInstructions {
    fn from(skill: &SkillInjection) -> Self {
        Self::new(
            skill.name.clone(),
            skill.path.clone(),
            skill.contents.clone(),
        )
    }
}

impl ContextualUserFragment for SkillInstructions {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<skill>", "</skill>")
    }

    fn body(&self) -> String {
        let name = &self.name;
        let path = &self.path;
        let contents = &self.contents;
        format!("\n<name>{name}</name>\n<path>{path}</path>\n{contents}\n")
    }
}
