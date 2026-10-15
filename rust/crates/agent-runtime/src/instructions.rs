use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct ContextFiles {
    pub text: String,
    pub paths: Vec<PathBuf>,
    pub diagnostics: Vec<String>,
}

pub fn load_context_files(workspace: &Path) -> ContextFiles {
    let mut loaded = ContextFiles::default();
    let mut directories: Vec<_> = workspace.ancestors().collect();
    directories.reverse();
    for directory in directories {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let path = directory.join(name);
            if !path.is_file() {
                continue;
            }
            let result = fs::metadata(&path)
                .map_err(|e| e.to_string())
                .and_then(|metadata| {
                    if metadata.len() > 1_048_576 {
                        return Err("Context file exceeds 1MB".into());
                    }
                    fs::read_to_string(&path).map_err(|e| e.to_string())
                });
            match result {
                Ok(text) if loaded.text.len() + text.len() < 4_194_304 => {
                    loaded.text.push_str(&format!(
                        "\n\n# Project instructions: {}\n{text}",
                        path.display()
                    ));
                    loaded.paths.push(path);
                }
                Ok(_) => loaded
                    .diagnostics
                    .push(format!("Context files exceed 4MB: {}", path.display())),
                Err(error) => loaded
                    .diagnostics
                    .push(format!("{}: {error}", path.display())),
            }
        }
    }
    loaded
}
