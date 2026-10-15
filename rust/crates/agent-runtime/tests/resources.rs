use agent_runtime::resources::{
    expand_prompt_template, format_skills_for_prompt, load_resources, parse_command_args,
    parse_frontmatter, prepare_prompt, substitute_args, DiagnosticKind, ResourceOptions,
    ResourceScope,
};
use agent_runtime::session::Session;
use std::fs;
use std::path::{Path, PathBuf};

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn directory(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rust-resources-{label}-{}", Session::default().id))
}

// Removal stays confined to the temporary fixture root supplied by these tests.
fn cleanup(path: &Path) {
    let root = fs::canonicalize(path).unwrap();
    assert!(root.starts_with(fs::canonicalize(std::env::temp_dir()).unwrap()));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn yaml_frontmatter_handles_bom_crlf_blocks_booleans_and_unknown_metadata() {
    let parsed = parse_frontmatter("\u{feff}---\r\nname: review\r\ndescription: >-\r\n  Review complex\r\n  changes\r\ndisable-model-invocation: true\r\nextra:\r\n  list: [one, two]\r\n---\r\n\r\nDo work.\r\n").unwrap();
    assert_eq!(parsed.fields["description"], "Review complex changes");
    assert_eq!(parsed.fields["disable-model-invocation"], true);
    assert_eq!(parsed.fields["extra"]["list"][1], "two");
    assert_eq!(parsed.body, "Do work.");
    assert_eq!(
        parse_frontmatter("plain\r\ntext\r").unwrap().body,
        "plain\ntext\n"
    );
    assert!(parse_frontmatter("---\nname: [\n---\nbody").is_err());
}

#[test]
fn loads_real_user_project_skills_and_prompts_with_observed_precedence() {
    let root = directory("scopes");
    let project = root.join("project");
    let agent_dir = root.join("user");
    write(
        &agent_dir.join("skills/review/SKILL.md"),
        "---\nname: review\ndescription: User review\n---\nUser instructions",
    );
    write(
        &project.join(".agent-core/skills/review/SKILL.md"),
        "---\nname: review\ndescription: Project review\n---\nProject instructions",
    );
    write(
        &project.join(".agent-core/skills/project-only/SKILL.md"),
        "---\ndescription: Project only\n---\nProject body",
    );
    write(
        &agent_dir.join("prompts/review.md"),
        "---\ndescription: Review changes\nargument-hint: '<target>'\n---\nReview $1 and ${@:2}",
    );
    write(
        &project.join(".agent-core/prompts/project.md"),
        "Project ${1:-default}",
    );
    write(
        &agent_dir.join("prompts/nested/ignored.md"),
        "Not discovered recursively",
    );
    let loaded = load_resources(&ResourceOptions {
        cwd: project.clone(),
        agent_dir,
        skill_paths: vec![],
        prompt_paths: vec![],
        include_defaults: true,
    });
    assert_eq!(loaded.skills.len(), 2);
    assert_eq!(loaded.skills[0].description, "User review");
    assert_eq!(loaded.skills[0].source, ResourceScope::User);
    assert_eq!(loaded.skills[1].source, ResourceScope::Project);
    assert_eq!(loaded.prompts.len(), 2);
    assert_eq!(loaded.prompts[0].argument_hint.as_deref(), Some("<target>"));
    assert_eq!(
        expand_prompt_template("/review README.md 'test files'", &loaded.prompts),
        "Review README.md and test files"
    );
    assert!(loaded
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.kind == DiagnosticKind::Collision));
    assert!(loaded
        .skills
        .iter()
        .all(|skill| skill.file_path.is_absolute()));
    cleanup(&root);
}

#[test]
fn skill_root_stops_recursion_and_ignore_rules_apply_with_negation() {
    let root = directory("discovery");
    let skills = root.join("skills");
    write(
        &skills.join("preferred/SKILL.md"),
        "---\ndescription: Root wins\n---\nRoot",
    );
    write(
        &skills.join("preferred/child/SKILL.md"),
        "---\nname: child\ndescription: Not discovered\n---\nChild",
    );
    write(&skills.join(".gitignore"), "ignored/\n*.md\n!SKILL.md\n");
    write(
        &skills.join("ignored/SKILL.md"),
        "---\ndescription: Ignored\n---\nIgnore",
    );
    write(
        &skills.join("nested/child/SKILL.md"),
        "---\nname: nested\ndescription: Nested\n---\nNested",
    );
    write(&skills.join("hidden/.ignore"), "excluded/\n");
    write(
        &skills.join("hidden/excluded/SKILL.md"),
        "---\nname: excluded\ndescription: Excluded\n---\nExcluded",
    );
    write(
        &skills.join(".hidden/SKILL.md"),
        "---\nname: hidden\ndescription: Hidden\n---\nHidden",
    );
    let loaded = load_resources(&ResourceOptions {
        cwd: root.clone(),
        agent_dir: root.join("none"),
        skill_paths: vec![skills],
        prompt_paths: vec![],
        include_defaults: false,
    });
    assert_eq!(
        loaded
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>(),
        ["nested", "preferred"]
    );
    cleanup(&root);
}

#[test]
fn disabled_skills_load_but_are_not_advertised_and_invalid_names_warn() {
    let root = directory("diagnostics");
    write(&root.join("hidden/SKILL.md"), "---\nname: manual\ndescription: Explicit only\ndisable-model-invocation: true\n---\nInstructions");
    write(
        &root.join("warn/SKILL.md"),
        "---\nname: Bad--Name\ndescription: 'Use <tag> & values'\n---\nWarn",
    );
    write(
        &root.join("missing/SKILL.md"),
        "---\nname: missing\n---\nNo description",
    );
    write(
        &root.join("broken/SKILL.md"),
        "---\nname: [\n---\nMalformed",
    );
    let loaded = load_resources(&ResourceOptions {
        cwd: root.clone(),
        agent_dir: root.join("none"),
        skill_paths: vec![root.clone()],
        prompt_paths: vec![root.join("absent.md")],
        include_defaults: false,
    });
    assert_eq!(loaded.skills.len(), 2);
    assert!(loaded
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message == "description is required"));
    assert!(loaded
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message.contains("Invalid frontmatter")));
    assert!(loaded
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message.contains("invalid characters")));
    let prompt = format_skills_for_prompt(&loaded.skills);
    assert!(!prompt.contains("<name>manual</name>"));
    assert!(prompt.contains("Use &lt;tag&gt; &amp; values"));
    assert!(format_skills_for_prompt(&[]).is_empty());
    cleanup(&root);
}

#[test]
fn explicit_files_deduplicate_and_templates_do_not_execute_commands() {
    let root = directory("explicit");
    let skill = root.join("custom/SKILL.md");
    let prompt = root.join("commands/test.md");
    write(
        &skill,
        "---\nname: custom\ndescription: Explicit skill\n---\nSkill body",
    );
    write(
        &prompt,
        "---\nargument-hint: '[value]'\n---\nLiteral $(touch marker) $1",
    );
    let loaded = load_resources(&ResourceOptions {
        cwd: root.clone(),
        agent_dir: root.join("none"),
        skill_paths: vec![skill.clone(), skill],
        prompt_paths: vec![prompt.clone(), prompt],
        include_defaults: false,
    });
    assert_eq!(loaded.skills.len(), 1);
    assert_eq!(loaded.prompts.len(), 1);
    assert_eq!(loaded.skills[0].source, ResourceScope::Path);
    assert_eq!(
        expand_prompt_template("/test value", &loaded.prompts),
        "Literal $(touch marker) value"
    );
    assert!(!root.join("marker").exists());
    cleanup(&root);
}

#[test]
fn command_argument_parser_preserves_the_ts_quote_and_empty_argument_semantics() {
    for (text, expected) in [
        ("a b c", vec!["a", "b", "c"]),
        (
            "\"first arg\" 'second arg'",
            vec!["first arg", "second arg"],
        ),
        ("\"\" \" \"", vec![" "]),
        ("a\n\n\tb c", vec!["a", "b", "c"]),
        ("\"line1\nline2\" rest", vec!["line1\nline2", "rest"]),
        ("\"unfinished", vec!["unfinished"]),
    ] {
        assert_eq!(parse_command_args(text), expected);
    }
}

#[test]
fn substitutes_positional_defaults_slices_and_all_args_non_recursively() {
    let args: Vec<String> = ["a", "b", "c", "d"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    for (template, expected) in [
        ("$1:$2:$0:$99", "a:b::"),
        ("$@/$ARGUMENTS", "a b c d/a b c d"),
        ("${@:2}", "b c d"),
        ("${@:2:2}", "b c"),
        ("${@:0}", "a b c d"),
        ("${@:2:0}", ""),
        ("${@:99:2}", ""),
        ("${5:-fallback}", "fallback"),
        ("${5:-$1}", "$1"),
        ("$1.5/$A/$$", "a.5/$A/$$"),
    ] {
        assert_eq!(substitute_args(template, &args), expected, "{template}");
    }
    assert_eq!(
        substitute_args("$1 $@", &["$2".into(), "${@:1}".into()]),
        "$2 $2 ${@:1}"
    );
    assert_eq!(
        substitute_args("${1:-default}", &[String::new()]),
        "default"
    );
    assert_eq!(substitute_args("${ARGUMENTS:-empty}", &[]), "empty");
}

#[test]
fn explicit_skill_invocation_refreshes_the_file_and_resolves_relative_resources() {
    let root = directory("invoke");
    let skill = root.join("manual/SKILL.md");
    write(&skill, "---\nname: manual\ndescription: Explicit only\ndisable-model-invocation: true\n---\nOld body");
    let loaded = load_resources(&ResourceOptions {
        cwd: root.clone(),
        agent_dir: root.join("none"),
        skill_paths: vec![skill.clone()],
        prompt_paths: vec![],
        include_defaults: false,
    });
    write(
        &skill,
        "---\nname: manual\ndescription: Explicit only\n---\nRead references/example.md first.",
    );
    let expanded = prepare_prompt("/skill:manual   additional task  ", &loaded).unwrap();
    assert!(expanded.starts_with("<skill name=\"manual\" location=\""));
    assert!(expanded.contains(&format!(
        "References are relative to {}.",
        loaded.skills[0].base_dir.display()
    )));
    assert!(expanded.contains("Read references/example.md first."));
    assert!(!expanded.contains("Old body"));
    assert!(expanded.ends_with("</skill>\n\nadditional task"));
    assert!(prepare_prompt("/skill:missing", &loaded)
        .unwrap_err()
        .contains("Skill not found"));
    assert_eq!(prepare_prompt("plain text", &loaded).unwrap(), "plain text");
    cleanup(&root);
}

#[test]
fn loads_preserved_skill_fixtures_without_format_translation() {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/skills");
    let loaded = load_resources(&ResourceOptions {
        cwd: fixtures.clone(),
        agent_dir: fixtures.join("absent"),
        skill_paths: vec![fixtures],
        prompt_paths: vec![],
        include_defaults: false,
    });
    assert!(loaded.skills.iter().any(|skill| skill.name == "valid-skill"
        && skill.description == "A valid skill for testing purposes."));
    assert!(loaded
        .skills
        .iter()
        .any(|skill| skill.name == "root-skill-preferred"));
    assert!(!loaded
        .skills
        .iter()
        .any(|skill| skill.name == "nested-child"));
    assert!(loaded
        .skills
        .iter()
        .any(|skill| skill.name == "multiline-description" && skill.description.contains('\n')));
    assert!(loaded
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.message == "description is required"));
}
