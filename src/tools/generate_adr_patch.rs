use std::path::{Component, Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::Error;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateAdrPatchParams {
    /// Absolute path to the repository root.
    pub repo_path: String,
    /// ADR markdown path relative to the repository root.
    pub adr_path: String,
    /// Complete ADR markdown content to write at `adr_path`.
    pub draft: String,
}

#[derive(Debug, Serialize)]
pub struct GenerateAdrPatchResult {
    pub adr_path: String,
    pub operation: String,
    pub patch: String,
    pub warnings: Vec<String>,
}

pub async fn run(params: GenerateAdrPatchParams) -> Result<GenerateAdrPatchResult, Error> {
    let repo_path = dunce::canonicalize(&params.repo_path)
        .map_err(|_| Error::InvalidInput {
            field: "repo_path",
            reason: format!(
                "path does not exist or is not accessible: {}",
                params.repo_path
            ),
        })?;

    let target = normalize_lexical(&repo_path.join(&params.adr_path));
    if !target.starts_with(&repo_path) {
        return Err(Error::InvalidInput {
            field: "adr_path",
            reason: "ADR path escapes the repository root".to_string(),
        });
    }

    let rel = target
        .strip_prefix(&repo_path)
        .expect("just checked starts_with")
        .to_string_lossy()
        .replace('\\', "/");

    if rel.trim().is_empty() || rel.ends_with('/') {
        return Err(Error::InvalidInput {
            field: "adr_path",
            reason: "ADR path must name a file".to_string(),
        });
    }

    let old = match std::fs::read_to_string(&target) {
        Ok(content) => Some(content),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(Error::Other(err.into())),
    };

    let operation = if old.is_some() { "update" } else { "create" }.to_string();
    let patch = render_unified_patch(&rel, old.as_deref(), &params.draft);

    Ok(GenerateAdrPatchResult {
        adr_path: rel,
        operation,
        patch,
        warnings: vec![],
    })
}

fn render_unified_patch(path: &str, old: Option<&str>, new: &str) -> String {
    let old_lines = old.map(split_patch_lines).unwrap_or_default();
    let new_lines = split_patch_lines(new);

    let old_count = old_lines.len();
    let new_count = new_lines.len();
    let old_marker = if old.is_some() {
        format!("a/{}", path)
    } else {
        "/dev/null".to_string()
    };
    let new_marker = format!("b/{}", path);

    let mut patch = String::new();
    patch.push_str(&format!("diff --git a/{0} b/{0}\n", path));
    if old.is_none() {
        patch.push_str("new file mode 100644\n");
    }
    patch.push_str(&format!("--- {}\n", old_marker));
    patch.push_str(&format!("+++ {}\n", new_marker));
    patch.push_str(&format!(
        "@@ -1,{} +1,{} @@\n",
        old_count.max(1),
        new_count.max(1)
    ));

    for line in old_lines {
        patch.push('-');
        patch.push_str(&line);
        patch.push('\n');
    }

    for line in new_lines {
        patch.push('+');
        patch.push_str(&line);
        patch.push('\n');
    }

    patch
}

fn split_patch_lines(text: &str) -> Vec<String> {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(str::to_string)
        .collect()
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = vec![];
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(c);
                }
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[tokio::test]
    async fn generates_create_patch_without_writing_file() -> TestResult {
        let dir = tempdir()?;

        let result = run(GenerateAdrPatchParams {
            repo_path: dir.path().to_string_lossy().to_string(),
            adr_path: "docs/adr/ADR-0001.md".to_string(),
            draft: "# ADR-0001 Test\n".to_string(),
        })
        .await?;

        assert_eq!(result.operation, "create");
        assert!(result.patch.contains("--- /dev/null"));
        assert!(result
            .patch
            .contains("diff --git a/docs/adr/ADR-0001.md b/docs/adr/ADR-0001.md"));
        assert!(result.patch.contains("+++ b/docs/adr/ADR-0001.md"));
        assert!(result.patch.contains("+# ADR-0001 Test"));
        assert!(!dir.path().join("docs/adr/ADR-0001.md").exists());

        Ok(())
    }

    #[tokio::test]
    async fn rejects_paths_outside_repo() {
        let dir = tempdir().expect("tempdir");

        let err = run(GenerateAdrPatchParams {
            repo_path: dir.path().to_string_lossy().to_string(),
            adr_path: "../ADR-0001.md".to_string(),
            draft: "# ADR-0001 Test\n".to_string(),
        })
        .await
        .expect_err("expected path rejection");

        assert!(err.to_string().contains("escapes the repository root"));
    }
}
