use std::path::{Path, PathBuf};

use commands::{
    classify_skills_slash_command, handle_agents_slash_command,
    handle_agents_slash_command_json, handle_mcp_slash_command, handle_mcp_slash_command_json,
    handle_plugins_slash_command, handle_skills_slash_command, handle_skills_slash_command_json,
    handle_slash_command, render_plugins_report, render_slash_command_help,
    render_slash_command_help_detail, render_slash_command_help_filtered,
    resolve_skill_invocation, resolve_skill_path, resume_supported_slash_commands,
    slash_command_specs, suggest_slash_commands, validate_slash_command_input,
    CommandManifestEntry, CommandRegistry, CommandSource, PluginsCommandResult,
    SkillSlashDispatch, SlashCommand, SlashCommandParseError, SlashCommandResult,
    SlashCommandSpec,
};
use plugins::{PluginError, PluginManager};

fn assert_public_type<T>() {}

#[test]
fn crate_root_exports_slash_command_api() {
    assert_public_type::<CommandManifestEntry>();
    assert_public_type::<CommandRegistry>();
    assert_public_type::<CommandSource>();
    assert_public_type::<PluginsCommandResult>();
    assert_public_type::<SlashCommandParseError>();
    assert_public_type::<SlashCommandResult>();
    assert_public_type::<SlashCommandSpec>();

    let _agents_text: fn(Option<&str>, &Path) -> std::io::Result<String> =
        handle_agents_slash_command;
    let _agents_json: fn(Option<&str>, &Path) -> std::io::Result<serde_json::Value> =
        handle_agents_slash_command_json;
    let _mcp_text: fn(Option<&str>, &Path) -> Result<String, runtime::ConfigError> =
        handle_mcp_slash_command;
    let _mcp_json: fn(Option<&str>, &Path) -> Result<serde_json::Value, runtime::ConfigError> =
        handle_mcp_slash_command_json;
    let _plugins: fn(
        Option<&str>,
        Option<&str>,
        &mut PluginManager,
    ) -> Result<PluginsCommandResult, PluginError> = handle_plugins_slash_command;
    let _plugins_report: fn(&[plugins::PluginSummary]) -> String = render_plugins_report;
    let _skills_text: fn(Option<&str>, &Path) -> std::io::Result<String> =
        handle_skills_slash_command;
    let _skills_json: fn(Option<&str>, &Path) -> std::io::Result<serde_json::Value> =
        handle_skills_slash_command_json;
    let _resolve_path: fn(&Path, &str) -> std::io::Result<PathBuf> = resolve_skill_path;
    let _handle: fn(
        &str,
        &runtime::Session,
        runtime::CompactionConfig,
        bool,
    ) -> Option<SlashCommandResult> = handle_slash_command;

    let specs = slash_command_specs();
    assert!(specs.iter().any(|spec| spec.name == "skills"));
    assert!(resume_supported_slash_commands()
        .iter()
        .any(|spec| spec.name == "help"));

    assert!(matches!(
        validate_slash_command_input("/skills list").expect("parse should succeed"),
        Some(SlashCommand::Skills { args: Some(args) }) if args == "list"
    ));

    assert_eq!(
        classify_skills_slash_command(Some("list")),
        SkillSlashDispatch::Local
    );
    assert_eq!(
        classify_skills_slash_command(Some("help overview")),
        SkillSlashDispatch::Invoke("$help overview".to_string())
    );

    let help = render_slash_command_help();
    assert!(help.contains("/skills"));
    assert!(render_slash_command_help_filtered(&["skills"]).contains("Slash commands"));
    assert!(render_slash_command_help_detail("skills").is_some());
    assert_eq!(suggest_slash_commands("skils", 1), vec!["/skills"]);
}

#[test]
fn slash_command_specs_keep_public_order_and_aliases() {
    let names = slash_command_specs()
        .iter()
        .map(|spec| spec.name)
        .collect::<Vec<_>>();

    assert_eq!(
        names,
        vec![
            "help",
            "status",
            "sandbox",
            "compact",
            "model",
            "permissions",
            "clear",
            "cost",
            "resume",
            "config",
            "mcp",
            "memory",
            "init",
            "diff",
            "version",
            "bughunter",
            "commit",
            "pr",
            "issue",
            "ultraplan",
            "teleport",
            "debug-tool-call",
            "export",
            "session",
            "plugin",
            "agents",
            "skills",
            "simplify",
            "doctor",
            "login",
            "logout",
            "plan",
            "review",
            "tasks",
            "theme",
            "vim",
            "voice",
            "upgrade",
            "usage",
            "stats",
            "rename",
            "copy",
            "share",
            "feedback",
            "hooks",
            "files",
            "context",
            "color",
            "effort",
            "fast",
            "exit",
            "branch",
            "rewind",
            "summary",
            "desktop",
            "ide",
            "tag",
            "brief",
            "advisor",
            "stickers",
            "insights",
            "thinkback",
            "release-notes",
            "security-review",
            "keybindings",
            "privacy-settings",
            "output-style",
            "add-dir",
            "allowed-tools",
            "api-key",
            "approve",
            "deny",
            "undo",
            "stop",
            "retry",
            "paste",
            "screenshot",
            "image",
            "terminal-setup",
            "search",
            "listen",
            "speak",
            "language",
            "profile",
            "max-tokens",
            "temperature",
            "system-prompt",
            "tool-details",
            "format",
            "pin",
            "unpin",
            "bookmarks",
            "workspace",
            "history",
            "tokens",
            "cache",
            "providers",
            "notifications",
            "changelog",
            "test",
            "lint",
            "build",
            "run",
            "git",
            "stash",
            "blame",
            "log",
            "cron",
            "team",
            "benchmark",
            "migrate",
            "reset",
            "telemetry",
            "env",
            "project",
            "templates",
            "explain",
            "refactor",
            "docs",
            "fix",
            "perf",
            "chat",
            "focus",
            "unfocus",
            "web",
            "map",
            "symbols",
            "references",
            "definition",
            "hover",
            "diagnostics",
            "autofix",
            "multi",
            "macro",
            "alias",
            "parallel",
            "agent",
            "subagent",
            "reasoning",
            "budget",
            "rate-limit",
            "metrics",
        ]
    );

    let plugin = slash_command_specs()
        .iter()
        .find(|spec| spec.name == "plugin")
        .expect("plugin command should exist");
    assert_eq!(plugin.aliases, &["plugins", "marketplace"]);

    let skills = slash_command_specs()
        .iter()
        .find(|spec| spec.name == "skills")
        .expect("skills command should exist");
    assert_eq!(skills.aliases, &["skill"]);
}

#[test]
fn missing_skill_resolution_reports_unknown_skill() {
    let cwd = std::env::current_dir().expect("current dir should be available");
    let error = resolve_skill_invocation(&cwd, Some("definitely-missing-skill-name"))
        .expect_err("missing skill should be rejected");

    assert!(error.contains("Unknown skill: definitely-missing-skill-name"));
    assert!(error.contains("Usage: /skills"));
}
