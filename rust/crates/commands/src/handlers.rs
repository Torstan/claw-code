use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use plugins::{PluginError, PluginManager, PluginSummary};
use runtime::{
    compact_session_with_memory, partial_compact_session, CompactionConfig, ConfigLoader,
    ConfigSource, McpOAuthConfig, McpServerConfig, ScopedMcpServerConfig, Session,
};
use serde_json::{json, Value};

use crate::help::render_slash_command_help;
use crate::parse::SlashCommand;
use crate::shared_args::normalize_optional_args;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSlashDispatch {
    Local,
    Invoke(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommandResult {
    pub message: String,
    pub session: Session,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginsCommandResult {
    pub message: String,
    pub reload_runtime: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum DefinitionSource {
    ProjectClaw,
    ProjectCodex,
    ProjectClaude,
    UserClawConfigHome,
    UserCodexHome,
    UserClaw,
    UserCodex,
    UserClaude,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DefinitionScope {
    Project,
    UserConfigHome,
    UserHome,
}

impl DefinitionScope {
    fn label(self) -> &'static str {
        match self {
            Self::Project => "Project roots",
            Self::UserConfigHome => "User config roots",
            Self::UserHome => "User home roots",
        }
    }
}

impl DefinitionSource {
    fn report_scope(self) -> DefinitionScope {
        match self {
            Self::ProjectClaw | Self::ProjectCodex | Self::ProjectClaude => {
                DefinitionScope::Project
            }
            Self::UserClawConfigHome | Self::UserCodexHome => DefinitionScope::UserConfigHome,
            Self::UserClaw | Self::UserCodex | Self::UserClaude => DefinitionScope::UserHome,
        }
    }

    fn label(self) -> &'static str {
        self.report_scope().label()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentSummary {
    name: String,
    description: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    source: DefinitionSource,
    shadowed_by: Option<DefinitionSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillSummary {
    name: String,
    description: Option<String>,
    source: DefinitionSource,
    shadowed_by: Option<DefinitionSource>,
    origin: SkillOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillOrigin {
    SkillsDir,
    LegacyCommandsDir,
}

impl SkillOrigin {
    fn detail_label(self) -> Option<&'static str> {
        match self {
            Self::SkillsDir => None,
            Self::LegacyCommandsDir => Some("legacy /commands"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillRoot {
    pub(crate) source: DefinitionSource,
    pub(crate) path: PathBuf,
    pub(crate) origin: SkillOrigin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstalledSkill {
    pub(crate) invocation_name: String,
    pub(crate) display_name: Option<String>,
    pub(crate) source: PathBuf,
    pub(crate) registry_root: PathBuf,
    pub(crate) installed_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SkillInstallSource {
    Directory { root: PathBuf, prompt_path: PathBuf },
    MarkdownFile { path: PathBuf },
}

#[allow(clippy::too_many_lines)]
pub fn handle_plugins_slash_command(
    action: Option<&str>,
    target: Option<&str>,
    manager: &mut PluginManager,
) -> Result<PluginsCommandResult, PluginError> {
    match action {
        None | Some("list") => Ok(PluginsCommandResult {
            message: render_plugins_report(&manager.list_installed_plugins()?),
            reload_runtime: false,
        }),
        Some("install") => {
            let Some(target) = target else {
                return Ok(PluginsCommandResult {
                    message: "Usage: /plugins install <path>".to_string(),
                    reload_runtime: false,
                });
            };
            let install = manager.install(target)?;
            let plugin = manager
                .list_installed_plugins()?
                .into_iter()
                .find(|plugin| plugin.metadata.id == install.plugin_id);
            Ok(PluginsCommandResult {
                message: render_plugin_install_report(&install.plugin_id, plugin.as_ref()),
                reload_runtime: true,
            })
        }
        Some("enable") => {
            let Some(target) = target else {
                return Ok(PluginsCommandResult {
                    message: "Usage: /plugins enable <name>".to_string(),
                    reload_runtime: false,
                });
            };
            let plugin = resolve_plugin_target(manager, target)?;
            manager.enable(&plugin.metadata.id)?;
            Ok(PluginsCommandResult {
                message: format!(
                    "Plugins\n  Result           enabled {}\n  Name             {}\n  Version          {}\n  Status           enabled",
                    plugin.metadata.id, plugin.metadata.name, plugin.metadata.version
                ),
                reload_runtime: true,
            })
        }
        Some("disable") => {
            let Some(target) = target else {
                return Ok(PluginsCommandResult {
                    message: "Usage: /plugins disable <name>".to_string(),
                    reload_runtime: false,
                });
            };
            let plugin = resolve_plugin_target(manager, target)?;
            manager.disable(&plugin.metadata.id)?;
            Ok(PluginsCommandResult {
                message: format!(
                    "Plugins\n  Result           disabled {}\n  Name             {}\n  Version          {}\n  Status           disabled",
                    plugin.metadata.id, plugin.metadata.name, plugin.metadata.version
                ),
                reload_runtime: true,
            })
        }
        Some("uninstall") => {
            let Some(target) = target else {
                return Ok(PluginsCommandResult {
                    message: "Usage: /plugins uninstall <plugin-id>".to_string(),
                    reload_runtime: false,
                });
            };
            manager.uninstall(target)?;
            Ok(PluginsCommandResult {
                message: format!("Plugins\n  Result           uninstalled {target}"),
                reload_runtime: true,
            })
        }
        Some("update") => {
            let Some(target) = target else {
                return Ok(PluginsCommandResult {
                    message: "Usage: /plugins update <plugin-id>".to_string(),
                    reload_runtime: false,
                });
            };
            let update = manager.update(target)?;
            let plugin = manager
                .list_installed_plugins()?
                .into_iter()
                .find(|plugin| plugin.metadata.id == update.plugin_id);
            Ok(PluginsCommandResult {
                message: format!(
                    "Plugins\n  Result           updated {}\n  Name             {}\n  Old version      {}\n  New version      {}\n  Status           {}",
                    update.plugin_id,
                    plugin
                        .as_ref()
                        .map_or_else(|| update.plugin_id.clone(), |plugin| plugin.metadata.name.clone()),
                    update.old_version,
                    update.new_version,
                    plugin
                        .as_ref()
                        .map_or("unknown", |plugin| if plugin.enabled { "enabled" } else { "disabled" }),
                ),
                reload_runtime: true,
            })
        }
        Some(other) => Ok(PluginsCommandResult {
            message: format!(
                "Unknown /plugins action '{other}'. Use list, install, enable, disable, uninstall, or update."
            ),
            reload_runtime: false,
        }),
    }
}

pub fn handle_agents_slash_command(args: Option<&str>, cwd: &Path) -> std::io::Result<String> {
    if let Some(args) = normalize_optional_args(args) {
        if let Some(help_path) = help_path_from_args(args) {
            return Ok(match help_path.as_slice() {
                [] => render_agents_usage(None),
                _ => render_agents_usage(Some(&help_path.join(" "))),
            });
        }
    }

    match normalize_optional_args(args) {
        None | Some("list") => {
            let roots = discover_definition_roots(cwd, "agents");
            let agents = load_agents_from_roots(&roots)?;
            Ok(render_agents_report(&agents))
        }
        Some(args) if is_help_arg(args) => Ok(render_agents_usage(None)),
        Some(args) => Ok(render_agents_usage(Some(args))),
    }
}

pub fn handle_agents_slash_command_json(args: Option<&str>, cwd: &Path) -> std::io::Result<Value> {
    if let Some(args) = normalize_optional_args(args) {
        if let Some(help_path) = help_path_from_args(args) {
            return Ok(match help_path.as_slice() {
                [] => render_agents_usage_json(None),
                _ => render_agents_usage_json(Some(&help_path.join(" "))),
            });
        }
    }

    match normalize_optional_args(args) {
        None | Some("list") => {
            let roots = discover_definition_roots(cwd, "agents");
            let agents = load_agents_from_roots(&roots)?;
            Ok(render_agents_report_json(cwd, &agents))
        }
        Some(args) if is_help_arg(args) => Ok(render_agents_usage_json(None)),
        Some(args) => Ok(render_agents_usage_json(Some(args))),
    }
}

pub fn handle_mcp_slash_command(
    args: Option<&str>,
    cwd: &Path,
) -> Result<String, runtime::ConfigError> {
    let loader = ConfigLoader::default_for(cwd);
    render_mcp_report_for(&loader, cwd, args)
}

pub fn handle_mcp_slash_command_json(
    args: Option<&str>,
    cwd: &Path,
) -> Result<Value, runtime::ConfigError> {
    let loader = ConfigLoader::default_for(cwd);
    render_mcp_report_json_for(&loader, cwd, args)
}

pub fn handle_skills_slash_command(args: Option<&str>, cwd: &Path) -> std::io::Result<String> {
    if let Some(args) = normalize_optional_args(args) {
        if let Some(help_path) = help_path_from_args(args) {
            return Ok(match help_path.as_slice() {
                [] => render_skills_usage(None),
                ["install", ..] => render_skills_usage(Some("install")),
                _ => render_skills_usage(Some(&help_path.join(" "))),
            });
        }
    }

    match normalize_optional_args(args) {
        None | Some("list") => {
            let roots = discover_skill_roots(cwd);
            let skills = load_skills_from_roots(&roots)?;
            Ok(render_skills_report(&skills))
        }
        Some("install") => Ok(render_skills_usage(Some("install"))),
        Some(args) if args.starts_with("install ") => {
            let target = args["install ".len()..].trim();
            if target.is_empty() {
                return Ok(render_skills_usage(Some("install")));
            }
            let install = install_skill(target, cwd)?;
            Ok(render_skill_install_report(&install))
        }
        Some(args) if is_help_arg(args) => Ok(render_skills_usage(None)),
        Some(args) => Ok(render_skills_usage(Some(args))),
    }
}

pub fn handle_skills_slash_command_json(args: Option<&str>, cwd: &Path) -> std::io::Result<Value> {
    if let Some(args) = normalize_optional_args(args) {
        if let Some(help_path) = help_path_from_args(args) {
            return Ok(match help_path.as_slice() {
                [] => render_skills_usage_json(None),
                ["install", ..] => render_skills_usage_json(Some("install")),
                _ => render_skills_usage_json(Some(&help_path.join(" "))),
            });
        }
    }

    match normalize_optional_args(args) {
        None | Some("list") => {
            let roots = discover_skill_roots(cwd);
            let skills = load_skills_from_roots(&roots)?;
            Ok(render_skills_report_json(&skills))
        }
        Some("install") => Ok(render_skills_usage_json(Some("install"))),
        Some(args) if args.starts_with("install ") => {
            let target = args["install ".len()..].trim();
            if target.is_empty() {
                return Ok(render_skills_usage_json(Some("install")));
            }
            let install = install_skill(target, cwd)?;
            Ok(render_skill_install_report_json(&install))
        }
        Some(args) if is_help_arg(args) => Ok(render_skills_usage_json(None)),
        Some(args) => Ok(render_skills_usage_json(Some(args))),
    }
}

#[must_use]
pub fn classify_skills_slash_command(args: Option<&str>) -> SkillSlashDispatch {
    match normalize_optional_args(args) {
        None | Some("list" | "help" | "-h" | "--help") => SkillSlashDispatch::Local,
        Some(args) if args == "install" || args.starts_with("install ") => {
            SkillSlashDispatch::Local
        }
        Some(args) => SkillSlashDispatch::Invoke(format!("${}", args.trim_start_matches('/'))),
    }
}

/// Resolve a skill invocation by validating the skill exists on disk before
/// returning the dispatch.  When the skill is not found, returns `Err` with a
/// human-readable message that lists nearby skill names.
pub fn resolve_skill_invocation(
    cwd: &Path,
    args: Option<&str>,
) -> Result<SkillSlashDispatch, String> {
    let dispatch = classify_skills_slash_command(args);
    if let SkillSlashDispatch::Invoke(ref prompt) = dispatch {
        // Extract the skill name from the "$skill [args]" prompt.
        let skill_token = prompt
            .trim_start_matches('$')
            .split_whitespace()
            .next()
            .unwrap_or_default();
        if !skill_token.is_empty() {
            if let Err(error) = resolve_skill_path(cwd, skill_token) {
                let mut message = format!("Unknown skill: {skill_token} ({error})");
                let roots = discover_skill_roots(cwd);
                if let Ok(available) = load_skills_from_roots(&roots) {
                    let names: Vec<String> = available
                        .iter()
                        .filter(|s| s.shadowed_by.is_none())
                        .map(|s| s.name.clone())
                        .collect();
                    if !names.is_empty() {
                        let _ = std::fmt::Write::write_fmt(
                            &mut message,
                            format_args!("\n  Available skills: {}", names.join(", ")),
                        );
                    }
                }
                message.push_str("\n  Usage: /skills [list|install <path>|help|<skill> [args]]");
                return Err(message);
            }
        }
    }
    Ok(dispatch)
}

pub fn resolve_skill_path(cwd: &Path, skill: &str) -> std::io::Result<PathBuf> {
    let requested = skill.trim().trim_start_matches('/').trim_start_matches('$');
    if requested.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "skill must not be empty",
        ));
    }

    let roots = discover_skill_roots(cwd);
    for root in &roots {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&root.path)? {
            let entry = entry?;
            match root.origin {
                SkillOrigin::SkillsDir => {
                    if !entry.path().is_dir() {
                        continue;
                    }
                    let skill_path = entry.path().join("SKILL.md");
                    if !skill_path.is_file() {
                        continue;
                    }
                    let contents = fs::read_to_string(&skill_path)?;
                    let (name, _) = parse_skill_frontmatter(&contents);
                    entries.push((
                        name.unwrap_or_else(|| entry.file_name().to_string_lossy().to_string()),
                        skill_path,
                    ));
                }
                SkillOrigin::LegacyCommandsDir => {
                    let path = entry.path();
                    let markdown_path = if path.is_dir() {
                        let skill_path = path.join("SKILL.md");
                        if !skill_path.is_file() {
                            continue;
                        }
                        skill_path
                    } else if path
                        .extension()
                        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("md"))
                    {
                        path
                    } else {
                        continue;
                    };

                    let contents = fs::read_to_string(&markdown_path)?;
                    let fallback_name = markdown_path.file_stem().map_or_else(
                        || entry.file_name().to_string_lossy().to_string(),
                        |stem| stem.to_string_lossy().to_string(),
                    );
                    let (name, _) = parse_skill_frontmatter(&contents);
                    entries.push((name.unwrap_or(fallback_name), markdown_path));
                }
            }
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        if let Some((_, path)) = entries
            .into_iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(requested))
        {
            return Ok(path);
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("unknown skill: {requested}"),
    ))
}

pub(crate) fn render_mcp_report_for(
    loader: &ConfigLoader,
    cwd: &Path,
    args: Option<&str>,
) -> Result<String, runtime::ConfigError> {
    if let Some(args) = normalize_optional_args(args) {
        if let Some(help_path) = help_path_from_args(args) {
            return Ok(match help_path.as_slice() {
                [] => render_mcp_usage(None),
                ["show", ..] => render_mcp_usage(Some("show")),
                _ => render_mcp_usage(Some(&help_path.join(" "))),
            });
        }
    }

    match normalize_optional_args(args) {
        None | Some("list") => {
            let runtime_config = loader.load()?;
            Ok(render_mcp_summary_report(
                cwd,
                runtime_config.mcp().servers(),
            ))
        }
        Some(args) if is_help_arg(args) => Ok(render_mcp_usage(None)),
        Some("show") => Ok(render_mcp_usage(Some("show"))),
        Some(args) if args.split_whitespace().next() == Some("show") => {
            let mut parts = args.split_whitespace();
            let _ = parts.next();
            let Some(server_name) = parts.next() else {
                return Ok(render_mcp_usage(Some("show")));
            };
            if parts.next().is_some() {
                return Ok(render_mcp_usage(Some(args)));
            }
            let runtime_config = loader.load()?;
            Ok(render_mcp_server_report(
                cwd,
                server_name,
                runtime_config.mcp().get(server_name),
            ))
        }
        Some(args) => Ok(render_mcp_usage(Some(args))),
    }
}

pub(crate) fn render_mcp_report_json_for(
    loader: &ConfigLoader,
    cwd: &Path,
    args: Option<&str>,
) -> Result<Value, runtime::ConfigError> {
    if let Some(args) = normalize_optional_args(args) {
        if let Some(help_path) = help_path_from_args(args) {
            return Ok(match help_path.as_slice() {
                [] => render_mcp_usage_json(None),
                ["show", ..] => render_mcp_usage_json(Some("show")),
                _ => render_mcp_usage_json(Some(&help_path.join(" "))),
            });
        }
    }

    match normalize_optional_args(args) {
        None | Some("list") => {
            let runtime_config = loader.load()?;
            Ok(render_mcp_summary_report_json(
                cwd,
                runtime_config.mcp().servers(),
            ))
        }
        Some(args) if is_help_arg(args) => Ok(render_mcp_usage_json(None)),
        Some("show") => Ok(render_mcp_usage_json(Some("show"))),
        Some(args) if args.split_whitespace().next() == Some("show") => {
            let mut parts = args.split_whitespace();
            let _ = parts.next();
            let Some(server_name) = parts.next() else {
                return Ok(render_mcp_usage_json(Some("show")));
            };
            if parts.next().is_some() {
                return Ok(render_mcp_usage_json(Some(args)));
            }
            let runtime_config = loader.load()?;
            Ok(render_mcp_server_report_json(
                cwd,
                server_name,
                runtime_config.mcp().get(server_name),
            ))
        }
        Some(args) => Ok(render_mcp_usage_json(Some(args))),
    }
}

#[must_use]
pub fn render_plugins_report(plugins: &[PluginSummary]) -> String {
    let mut lines = vec!["Plugins".to_string()];
    if plugins.is_empty() {
        lines.push("  No plugins installed.".to_string());
        return lines.join("\n");
    }
    for plugin in plugins {
        let enabled = if plugin.enabled {
            "enabled"
        } else {
            "disabled"
        };
        lines.push(format!(
            "  {name:<20} v{version:<10} {enabled}",
            name = plugin.metadata.name,
            version = plugin.metadata.version,
        ));
    }
    lines.join("\n")
}

fn render_plugin_install_report(plugin_id: &str, plugin: Option<&PluginSummary>) -> String {
    let name = plugin.map_or(plugin_id, |plugin| plugin.metadata.name.as_str());
    let version = plugin.map_or("unknown", |plugin| plugin.metadata.version.as_str());
    let enabled = plugin.is_some_and(|plugin| plugin.enabled);
    format!(
        "Plugins\n  Result           installed {plugin_id}\n  Name             {name}\n  Version          {version}\n  Status           {}",
        if enabled { "enabled" } else { "disabled" }
    )
}

fn resolve_plugin_target(
    manager: &PluginManager,
    target: &str,
) -> Result<PluginSummary, PluginError> {
    let mut matches = manager
        .list_installed_plugins()?
        .into_iter()
        .filter(|plugin| plugin.metadata.id == target || plugin.metadata.name == target)
        .collect::<Vec<_>>();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(PluginError::NotFound(format!(
            "plugin `{target}` is not installed or discoverable"
        ))),
        _ => Err(PluginError::InvalidManifest(format!(
            "plugin name `{target}` is ambiguous; use the full plugin id"
        ))),
    }
}

fn discover_definition_roots(cwd: &Path, leaf: &str) -> Vec<(DefinitionSource, PathBuf)> {
    let mut roots = Vec::new();

    for ancestor in cwd.ancestors() {
        push_unique_root(
            &mut roots,
            DefinitionSource::ProjectClaw,
            ancestor.join(".claw").join(leaf),
        );
        push_unique_root(
            &mut roots,
            DefinitionSource::ProjectCodex,
            ancestor.join(".codex").join(leaf),
        );
        push_unique_root(
            &mut roots,
            DefinitionSource::ProjectClaude,
            ancestor.join(".claude").join(leaf),
        );
    }

    if let Ok(claw_config_home) = env::var("CLAW_CONFIG_HOME") {
        push_unique_root(
            &mut roots,
            DefinitionSource::UserClawConfigHome,
            PathBuf::from(claw_config_home).join(leaf),
        );
    }

    if let Ok(codex_home) = env::var("CODEX_HOME") {
        push_unique_root(
            &mut roots,
            DefinitionSource::UserCodexHome,
            PathBuf::from(codex_home).join(leaf),
        );
    }

    if let Ok(claude_config_dir) = env::var("CLAUDE_CONFIG_DIR") {
        push_unique_root(
            &mut roots,
            DefinitionSource::UserClaude,
            PathBuf::from(claude_config_dir).join(leaf),
        );
    }

    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        push_unique_root(
            &mut roots,
            DefinitionSource::UserClaw,
            home.join(".claw").join(leaf),
        );
        push_unique_root(
            &mut roots,
            DefinitionSource::UserCodex,
            home.join(".codex").join(leaf),
        );
        push_unique_root(
            &mut roots,
            DefinitionSource::UserClaude,
            home.join(".claude").join(leaf),
        );
    }

    roots
}

#[allow(clippy::too_many_lines)]
fn discover_skill_roots(cwd: &Path) -> Vec<SkillRoot> {
    let mut roots = Vec::new();

    for ancestor in cwd.ancestors() {
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::ProjectClaw,
            ancestor.join(".claw").join("skills"),
            SkillOrigin::SkillsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::ProjectClaw,
            ancestor.join(".omc").join("skills"),
            SkillOrigin::SkillsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::ProjectClaw,
            ancestor.join(".agents").join("skills"),
            SkillOrigin::SkillsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::ProjectCodex,
            ancestor.join(".codex").join("skills"),
            SkillOrigin::SkillsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::ProjectClaude,
            ancestor.join(".claude").join("skills"),
            SkillOrigin::SkillsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::ProjectClaw,
            ancestor.join(".claw").join("commands"),
            SkillOrigin::LegacyCommandsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::ProjectCodex,
            ancestor.join(".codex").join("commands"),
            SkillOrigin::LegacyCommandsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::ProjectClaude,
            ancestor.join(".claude").join("commands"),
            SkillOrigin::LegacyCommandsDir,
        );
    }

    if let Ok(claw_config_home) = env::var("CLAW_CONFIG_HOME") {
        let claw_config_home = PathBuf::from(claw_config_home);
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::UserClawConfigHome,
            claw_config_home.join("skills"),
            SkillOrigin::SkillsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::UserClawConfigHome,
            claw_config_home.join("commands"),
            SkillOrigin::LegacyCommandsDir,
        );
    }

    if let Ok(codex_home) = env::var("CODEX_HOME") {
        let codex_home = PathBuf::from(codex_home);
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::UserCodexHome,
            codex_home.join("skills"),
            SkillOrigin::SkillsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::UserCodexHome,
            codex_home.join("commands"),
            SkillOrigin::LegacyCommandsDir,
        );
    }

    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::UserClaw,
            home.join(".claw").join("skills"),
            SkillOrigin::SkillsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::UserClaw,
            home.join(".omc").join("skills"),
            SkillOrigin::SkillsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::UserClaw,
            home.join(".claw").join("commands"),
            SkillOrigin::LegacyCommandsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::UserCodex,
            home.join(".codex").join("skills"),
            SkillOrigin::SkillsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::UserCodex,
            home.join(".codex").join("commands"),
            SkillOrigin::LegacyCommandsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::UserClaude,
            home.join(".claude").join("skills"),
            SkillOrigin::SkillsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::UserClaude,
            home.join(".claude").join("skills").join("omc-learned"),
            SkillOrigin::SkillsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::UserClaude,
            home.join(".claude").join("commands"),
            SkillOrigin::LegacyCommandsDir,
        );
    }

    if let Ok(claude_config_dir) = env::var("CLAUDE_CONFIG_DIR") {
        let claude_config_dir = PathBuf::from(claude_config_dir);
        let skills_dir = claude_config_dir.join("skills");
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::UserClaude,
            skills_dir.clone(),
            SkillOrigin::SkillsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::UserClaude,
            skills_dir.join("omc-learned"),
            SkillOrigin::SkillsDir,
        );
        push_unique_skill_root(
            &mut roots,
            DefinitionSource::UserClaude,
            claude_config_dir.join("commands"),
            SkillOrigin::LegacyCommandsDir,
        );
    }

    roots
}

fn install_skill(source: &str, cwd: &Path) -> std::io::Result<InstalledSkill> {
    let registry_root = default_skill_install_root()?;
    install_skill_into(source, cwd, &registry_root)
}

pub(crate) fn install_skill_into(
    source: &str,
    cwd: &Path,
    registry_root: &Path,
) -> std::io::Result<InstalledSkill> {
    let source = resolve_skill_install_source(source, cwd)?;
    let prompt_path = source.prompt_path();
    let contents = fs::read_to_string(prompt_path)?;
    let display_name = parse_skill_frontmatter(&contents).0;
    let invocation_name = derive_skill_install_name(&source, display_name.as_deref())?;
    let installed_path = registry_root.join(&invocation_name);

    if installed_path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "skill '{invocation_name}' is already installed at {}",
                installed_path.display()
            ),
        ));
    }

    fs::create_dir_all(&installed_path)?;
    let install_result = match &source {
        SkillInstallSource::Directory { root, .. } => {
            copy_directory_contents(root, &installed_path)
        }
        SkillInstallSource::MarkdownFile { path } => {
            fs::copy(path, installed_path.join("SKILL.md")).map(|_| ())
        }
    };
    if let Err(error) = install_result {
        let _ = fs::remove_dir_all(&installed_path);
        return Err(error);
    }

    Ok(InstalledSkill {
        invocation_name,
        display_name,
        source: source.report_path().to_path_buf(),
        registry_root: registry_root.to_path_buf(),
        installed_path,
    })
}

fn default_skill_install_root() -> std::io::Result<PathBuf> {
    if let Ok(claw_config_home) = env::var("CLAW_CONFIG_HOME") {
        return Ok(PathBuf::from(claw_config_home).join("skills"));
    }
    if let Ok(codex_home) = env::var("CODEX_HOME") {
        return Ok(PathBuf::from(codex_home).join("skills"));
    }
    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".claw").join("skills"));
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "unable to resolve a skills install root; set CLAW_CONFIG_HOME or HOME",
    ))
}

fn resolve_skill_install_source(source: &str, cwd: &Path) -> std::io::Result<SkillInstallSource> {
    let candidate = PathBuf::from(source);
    let source = if candidate.is_absolute() {
        candidate
    } else {
        cwd.join(candidate)
    };
    let source = fs::canonicalize(&source)?;

    if source.is_dir() {
        let prompt_path = source.join("SKILL.md");
        if !prompt_path.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "skill directory '{}' must contain SKILL.md",
                    source.display()
                ),
            ));
        }
        return Ok(SkillInstallSource::Directory {
            root: source,
            prompt_path,
        });
    }

    if source
        .extension()
        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("md"))
    {
        return Ok(SkillInstallSource::MarkdownFile { path: source });
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "skill source '{}' must be a directory with SKILL.md or a markdown file",
            source.display()
        ),
    ))
}

fn derive_skill_install_name(
    source: &SkillInstallSource,
    declared_name: Option<&str>,
) -> std::io::Result<String> {
    for candidate in [declared_name, source.fallback_name().as_deref()] {
        if let Some(candidate) = candidate.and_then(sanitize_skill_invocation_name) {
            return Ok(candidate);
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "unable to derive an installable invocation name from '{}'",
            source.report_path().display()
        ),
    ))
}

fn sanitize_skill_invocation_name(candidate: &str) -> Option<String> {
    let trimmed = candidate
        .trim()
        .trim_start_matches('/')
        .trim_start_matches('$');
    if trimmed.is_empty() {
        return None;
    }

    let mut sanitized = String::new();
    let mut last_was_separator = false;
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            sanitized.push(ch.to_ascii_lowercase());
            last_was_separator = false;
        } else if (ch.is_whitespace() || matches!(ch, '/' | '\\'))
            && !last_was_separator
            && !sanitized.is_empty()
        {
            sanitized.push('-');
            last_was_separator = true;
        }
    }

    let sanitized = sanitized
        .trim_matches(|ch| matches!(ch, '-' | '_' | '.'))
        .to_string();
    (!sanitized.is_empty()).then_some(sanitized)
}

fn copy_directory_contents(source: &Path, destination: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let entry_type = entry.file_type()?;
        let destination_path = destination.join(entry.file_name());
        if entry_type.is_dir() {
            fs::create_dir_all(&destination_path)?;
            copy_directory_contents(&entry.path(), &destination_path)?;
        } else {
            fs::copy(entry.path(), destination_path)?;
        }
    }
    Ok(())
}

impl SkillInstallSource {
    fn prompt_path(&self) -> &Path {
        match self {
            Self::Directory { prompt_path, .. } => prompt_path,
            Self::MarkdownFile { path } => path,
        }
    }

    fn fallback_name(&self) -> Option<String> {
        match self {
            Self::Directory { root, .. } => root
                .file_name()
                .map(|name| name.to_string_lossy().to_string()),
            Self::MarkdownFile { path } => path
                .file_stem()
                .map(|name| name.to_string_lossy().to_string()),
        }
    }

    fn report_path(&self) -> &Path {
        match self {
            Self::Directory { root, .. } => root,
            Self::MarkdownFile { path } => path,
        }
    }
}

fn push_unique_root(
    roots: &mut Vec<(DefinitionSource, PathBuf)>,
    source: DefinitionSource,
    path: PathBuf,
) {
    if path.is_dir() && !roots.iter().any(|(_, existing)| existing == &path) {
        roots.push((source, path));
    }
}

fn push_unique_skill_root(
    roots: &mut Vec<SkillRoot>,
    source: DefinitionSource,
    path: PathBuf,
    origin: SkillOrigin,
) {
    if path.is_dir() && !roots.iter().any(|existing| existing.path == path) {
        roots.push(SkillRoot {
            source,
            path,
            origin,
        });
    }
}

pub(crate) fn load_agents_from_roots(
    roots: &[(DefinitionSource, PathBuf)],
) -> std::io::Result<Vec<AgentSummary>> {
    let mut agents = Vec::new();
    let mut active_sources = BTreeMap::<String, DefinitionSource>::new();

    for (source, root) in roots {
        let mut root_agents = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if entry.path().extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let contents = fs::read_to_string(entry.path())?;
            let fallback_name = entry.path().file_stem().map_or_else(
                || entry.file_name().to_string_lossy().to_string(),
                |stem| stem.to_string_lossy().to_string(),
            );
            root_agents.push(AgentSummary {
                name: parse_toml_string(&contents, "name").unwrap_or(fallback_name),
                description: parse_toml_string(&contents, "description"),
                model: parse_toml_string(&contents, "model"),
                reasoning_effort: parse_toml_string(&contents, "model_reasoning_effort"),
                source: *source,
                shadowed_by: None,
            });
        }
        root_agents.sort_by(|left, right| left.name.cmp(&right.name));

        for mut agent in root_agents {
            let key = agent.name.to_ascii_lowercase();
            if let Some(existing) = active_sources.get(&key) {
                agent.shadowed_by = Some(*existing);
            } else {
                active_sources.insert(key, agent.source);
            }
            agents.push(agent);
        }
    }

    Ok(agents)
}

pub(crate) fn load_skills_from_roots(roots: &[SkillRoot]) -> std::io::Result<Vec<SkillSummary>> {
    let mut skills = Vec::new();
    let mut active_sources = BTreeMap::<String, DefinitionSource>::new();

    for root in roots {
        let mut root_skills = Vec::new();
        for entry in fs::read_dir(&root.path)? {
            let entry = entry?;
            match root.origin {
                SkillOrigin::SkillsDir => {
                    if !entry.path().is_dir() {
                        continue;
                    }
                    let skill_path = entry.path().join("SKILL.md");
                    if !skill_path.is_file() {
                        continue;
                    }
                    let contents = fs::read_to_string(skill_path)?;
                    let (name, description) = parse_skill_frontmatter(&contents);
                    root_skills.push(SkillSummary {
                        name: name
                            .unwrap_or_else(|| entry.file_name().to_string_lossy().to_string()),
                        description,
                        source: root.source,
                        shadowed_by: None,
                        origin: root.origin,
                    });
                }
                SkillOrigin::LegacyCommandsDir => {
                    let path = entry.path();
                    let markdown_path = if path.is_dir() {
                        let skill_path = path.join("SKILL.md");
                        if !skill_path.is_file() {
                            continue;
                        }
                        skill_path
                    } else if path
                        .extension()
                        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("md"))
                    {
                        path
                    } else {
                        continue;
                    };

                    let contents = fs::read_to_string(&markdown_path)?;
                    let fallback_name = markdown_path.file_stem().map_or_else(
                        || entry.file_name().to_string_lossy().to_string(),
                        |stem| stem.to_string_lossy().to_string(),
                    );
                    let (name, description) = parse_skill_frontmatter(&contents);
                    root_skills.push(SkillSummary {
                        name: name.unwrap_or(fallback_name),
                        description,
                        source: root.source,
                        shadowed_by: None,
                        origin: root.origin,
                    });
                }
            }
        }
        root_skills.sort_by(|left, right| left.name.cmp(&right.name));

        for mut skill in root_skills {
            let key = skill.name.to_ascii_lowercase();
            if let Some(existing) = active_sources.get(&key) {
                skill.shadowed_by = Some(*existing);
            } else {
                active_sources.insert(key, skill.source);
            }
            skills.push(skill);
        }
    }

    Ok(skills)
}

fn parse_toml_string(contents: &str, key: &str) -> Option<String> {
    let prefix = format!("{key} =");
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        let Some(value) = trimmed.strip_prefix(&prefix) else {
            continue;
        };
        let value = value.trim();
        let Some(value) = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
        else {
            continue;
        };
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

pub(crate) fn parse_skill_frontmatter(contents: &str) -> (Option<String>, Option<String>) {
    let mut lines = contents.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }

    let mut name = None;
    let mut description = None;
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("name:") {
            let value = unquote_frontmatter_value(value.trim());
            if !value.is_empty() {
                name = Some(value);
            }
            continue;
        }
        if let Some(value) = trimmed.strip_prefix("description:") {
            let value = unquote_frontmatter_value(value.trim());
            if !value.is_empty() {
                description = Some(value);
            }
        }
    }

    (name, description)
}

fn unquote_frontmatter_value(value: &str) -> String {
    value
        .strip_prefix('"')
        .and_then(|trimmed| trimmed.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|trimmed| trimmed.strip_suffix('\''))
        })
        .unwrap_or(value)
        .trim()
        .to_string()
}

pub(crate) fn render_agents_report(agents: &[AgentSummary]) -> String {
    if agents.is_empty() {
        return "No agents found.".to_string();
    }

    let total_active = agents
        .iter()
        .filter(|agent| agent.shadowed_by.is_none())
        .count();
    let mut lines = vec![
        "Agents".to_string(),
        format!("  {total_active} active agents"),
        String::new(),
    ];

    for scope in [
        DefinitionScope::Project,
        DefinitionScope::UserConfigHome,
        DefinitionScope::UserHome,
    ] {
        let group = agents
            .iter()
            .filter(|agent| agent.source.report_scope() == scope)
            .collect::<Vec<_>>();
        if group.is_empty() {
            continue;
        }

        lines.push(format!("{}:", scope.label()));
        for agent in group {
            let detail = agent_detail(agent);
            match agent.shadowed_by {
                Some(winner) => lines.push(format!("  (shadowed by {}) {detail}", winner.label())),
                None => lines.push(format!("  {detail}")),
            }
        }
        lines.push(String::new());
    }

    lines.join("\n").trim_end().to_string()
}

pub(crate) fn render_agents_report_json(cwd: &Path, agents: &[AgentSummary]) -> Value {
    let active = agents
        .iter()
        .filter(|agent| agent.shadowed_by.is_none())
        .count();
    json!({
        "kind": "agents",
        "action": "list",
        "working_directory": cwd.display().to_string(),
        "count": agents.len(),
        "summary": {
            "total": agents.len(),
            "active": active,
            "shadowed": agents.len().saturating_sub(active),
        },
        "agents": agents.iter().map(agent_summary_json).collect::<Vec<_>>(),
    })
}

fn agent_detail(agent: &AgentSummary) -> String {
    let mut parts = vec![agent.name.clone()];
    if let Some(description) = &agent.description {
        parts.push(description.clone());
    }
    if let Some(model) = &agent.model {
        parts.push(model.clone());
    }
    if let Some(reasoning) = &agent.reasoning_effort {
        parts.push(reasoning.clone());
    }
    parts.join(" · ")
}

pub(crate) fn render_skills_report(skills: &[SkillSummary]) -> String {
    if skills.is_empty() {
        return "No skills found.".to_string();
    }

    let total_active = skills
        .iter()
        .filter(|skill| skill.shadowed_by.is_none())
        .count();
    let mut lines = vec![
        "Skills".to_string(),
        format!("  {total_active} available skills"),
        String::new(),
    ];

    for scope in [
        DefinitionScope::Project,
        DefinitionScope::UserConfigHome,
        DefinitionScope::UserHome,
    ] {
        let group = skills
            .iter()
            .filter(|skill| skill.source.report_scope() == scope)
            .collect::<Vec<_>>();
        if group.is_empty() {
            continue;
        }

        lines.push(format!("{}:", scope.label()));
        for skill in group {
            let mut parts = vec![skill.name.clone()];
            if let Some(description) = &skill.description {
                parts.push(description.clone());
            }
            if let Some(detail) = skill.origin.detail_label() {
                parts.push(detail.to_string());
            }
            let detail = parts.join(" · ");
            match skill.shadowed_by {
                Some(winner) => lines.push(format!("  (shadowed by {}) {detail}", winner.label())),
                None => lines.push(format!("  {detail}")),
            }
        }
        lines.push(String::new());
    }

    lines.join("\n").trim_end().to_string()
}

pub(crate) fn render_skills_report_json(skills: &[SkillSummary]) -> Value {
    let active = skills
        .iter()
        .filter(|skill| skill.shadowed_by.is_none())
        .count();
    json!({
        "kind": "skills",
        "action": "list",
        "summary": {
            "total": skills.len(),
            "active": active,
            "shadowed": skills.len().saturating_sub(active),
        },
        "skills": skills.iter().map(skill_summary_json).collect::<Vec<_>>(),
    })
}

pub(crate) fn render_skill_install_report(skill: &InstalledSkill) -> String {
    let mut lines = vec![
        "Skills".to_string(),
        format!("  Result           installed {}", skill.invocation_name),
        format!("  Invoke as        ${}", skill.invocation_name),
    ];
    if let Some(display_name) = &skill.display_name {
        lines.push(format!("  Display name     {display_name}"));
    }
    lines.push(format!("  Source           {}", skill.source.display()));
    lines.push(format!(
        "  Registry         {}",
        skill.registry_root.display()
    ));
    lines.push(format!(
        "  Installed path   {}",
        skill.installed_path.display()
    ));
    lines.join("\n")
}

fn render_skill_install_report_json(skill: &InstalledSkill) -> Value {
    json!({
        "kind": "skills",
        "action": "install",
        "result": "installed",
        "invocation_name": &skill.invocation_name,
        "invoke_as": format!("${}", skill.invocation_name),
        "display_name": &skill.display_name,
        "source": skill.source.display().to_string(),
        "registry_root": skill.registry_root.display().to_string(),
        "installed_path": skill.installed_path.display().to_string(),
    })
}

fn render_mcp_summary_report(
    cwd: &Path,
    servers: &BTreeMap<String, ScopedMcpServerConfig>,
) -> String {
    let mut lines = vec![
        "MCP".to_string(),
        format!("  Working directory {}", cwd.display()),
        format!("  Configured servers {}", servers.len()),
    ];
    if servers.is_empty() {
        lines.push("  No MCP servers configured.".to_string());
        return lines.join("\n");
    }

    lines.push(String::new());
    for (name, server) in servers {
        lines.push(format!(
            "  {name:<16} {transport:<13} {scope:<7} {summary}",
            transport = mcp_transport_label(&server.config),
            scope = config_source_label(server.scope),
            summary = mcp_server_summary(&server.config)
        ));
    }

    lines.join("\n")
}

fn render_mcp_summary_report_json(
    cwd: &Path,
    servers: &BTreeMap<String, ScopedMcpServerConfig>,
) -> Value {
    json!({
        "kind": "mcp",
        "action": "list",
        "working_directory": cwd.display().to_string(),
        "configured_servers": servers.len(),
        "servers": servers
            .iter()
            .map(|(name, server)| mcp_server_json(name, server))
            .collect::<Vec<_>>(),
    })
}

fn render_mcp_server_report(
    cwd: &Path,
    server_name: &str,
    server: Option<&ScopedMcpServerConfig>,
) -> String {
    let Some(server) = server else {
        return format!(
            "MCP\n  Working directory {}\n  Result            server `{server_name}` is not configured",
            cwd.display()
        );
    };

    let mut lines = vec![
        "MCP".to_string(),
        format!("  Working directory {}", cwd.display()),
        format!("  Name              {server_name}"),
        format!("  Scope             {}", config_source_label(server.scope)),
        format!(
            "  Transport         {}",
            mcp_transport_label(&server.config)
        ),
    ];

    match &server.config {
        McpServerConfig::Stdio(config) => {
            lines.push(format!("  Command           {}", config.command));
            lines.push(format!(
                "  Args              {}",
                format_optional_list(&config.args)
            ));
            lines.push(format!(
                "  Env keys          {}",
                format_optional_keys(config.env.keys().cloned().collect())
            ));
            lines.push(format!(
                "  Tool timeout      {}",
                config
                    .tool_call_timeout_ms
                    .map_or_else(|| "<default>".to_string(), |value| format!("{value} ms"))
            ));
        }
        McpServerConfig::Sse(config) | McpServerConfig::Http(config) => {
            lines.push(format!("  URL               {}", config.url));
            lines.push(format!(
                "  Header keys       {}",
                format_optional_keys(config.headers.keys().cloned().collect())
            ));
            lines.push(format!(
                "  Header helper     {}",
                config.headers_helper.as_deref().unwrap_or("<none>")
            ));
            lines.push(format!(
                "  OAuth             {}",
                format_mcp_oauth(config.oauth.as_ref())
            ));
        }
        McpServerConfig::Ws(config) => {
            lines.push(format!("  URL               {}", config.url));
            lines.push(format!(
                "  Header keys       {}",
                format_optional_keys(config.headers.keys().cloned().collect())
            ));
            lines.push(format!(
                "  Header helper     {}",
                config.headers_helper.as_deref().unwrap_or("<none>")
            ));
        }
        McpServerConfig::Sdk(config) => {
            lines.push(format!("  SDK name          {}", config.name));
        }
        McpServerConfig::ManagedProxy(config) => {
            lines.push(format!("  URL               {}", config.url));
            lines.push(format!("  Proxy id          {}", config.id));
        }
    }

    lines.join("\n")
}

fn render_mcp_server_report_json(
    cwd: &Path,
    server_name: &str,
    server: Option<&ScopedMcpServerConfig>,
) -> Value {
    match server {
        Some(server) => json!({
            "kind": "mcp",
            "action": "show",
            "working_directory": cwd.display().to_string(),
            "found": true,
            "server": mcp_server_json(server_name, server),
        }),
        None => json!({
            "kind": "mcp",
            "action": "show",
            "working_directory": cwd.display().to_string(),
            "found": false,
            "server_name": server_name,
            "message": format!("server `{server_name}` is not configured"),
        }),
    }
}

fn is_help_arg(arg: &str) -> bool {
    matches!(arg, "help" | "-h" | "--help")
}

fn help_path_from_args(args: &str) -> Option<Vec<&str>> {
    let parts = args.split_whitespace().collect::<Vec<_>>();
    let help_index = parts.iter().position(|part| is_help_arg(part))?;
    Some(parts[..help_index].to_vec())
}

fn render_agents_usage(unexpected: Option<&str>) -> String {
    let mut lines = vec![
        "Agents".to_string(),
        "  Usage            /agents [list|help]".to_string(),
        "  Direct CLI       claw agents".to_string(),
        "  Sources          .claw/agents, ~/.claw/agents, $CLAW_CONFIG_HOME/agents".to_string(),
    ];
    if let Some(args) = unexpected {
        lines.push(format!("  Unexpected       {args}"));
    }
    lines.join("\n")
}

fn render_agents_usage_json(unexpected: Option<&str>) -> Value {
    json!({
        "kind": "agents",
        "action": "help",
        "usage": {
            "slash_command": "/agents [list|help]",
            "direct_cli": "claw agents [list|help]",
            "sources": [".claw/agents", "~/.claw/agents", "$CLAW_CONFIG_HOME/agents"],
        },
        "unexpected": unexpected,
    })
}

fn render_skills_usage(unexpected: Option<&str>) -> String {
    let mut lines = vec![
        "Skills".to_string(),
        "  Usage            /skills [list|install <path>|help|<skill> [args]]".to_string(),
        "  Alias            /skill".to_string(),
        "  Direct CLI       claw skills [list|install <path>|help|<skill> [args]]".to_string(),
        "  Invoke           /skills help overview -> $help overview".to_string(),
        "  Install root     $CLAW_CONFIG_HOME/skills or ~/.claw/skills".to_string(),
        "  Sources          .claw/skills, .omc/skills, .agents/skills, .codex/skills, .claude/skills, ~/.claw/skills, ~/.omc/skills, ~/.claude/skills/omc-learned, ~/.codex/skills, ~/.claude/skills, legacy /commands".to_string(),
    ];
    if let Some(args) = unexpected {
        lines.push(format!("  Unexpected       {args}"));
    }
    lines.join("\n")
}

fn render_skills_usage_json(unexpected: Option<&str>) -> Value {
    json!({
        "kind": "skills",
        "action": "help",
        "usage": {
            "slash_command": "/skills [list|install <path>|help|<skill> [args]]",
            "aliases": ["/skill"],
            "direct_cli": "claw skills [list|install <path>|help|<skill> [args]]",
            "invoke": "/skills help overview -> $help overview",
            "install_root": "$CLAW_CONFIG_HOME/skills or ~/.claw/skills",
            "sources": [
                ".claw/skills",
                ".omc/skills",
                ".agents/skills",
                ".codex/skills",
                ".claude/skills",
                "~/.claw/skills",
                "~/.omc/skills",
                "~/.claude/skills/omc-learned",
                "~/.codex/skills",
                "~/.claude/skills",
                "legacy /commands",
                "legacy fallback dirs still load automatically"
            ],
        },
        "unexpected": unexpected,
    })
}

fn render_mcp_usage(unexpected: Option<&str>) -> String {
    let mut lines = vec![
        "MCP".to_string(),
        "  Usage            /mcp [list|show <server>|help]".to_string(),
        "  Direct CLI       claw mcp [list|show <server>|help]".to_string(),
        "  Sources          .claw/settings.json, .claw/settings.local.json".to_string(),
    ];
    if let Some(args) = unexpected {
        lines.push(format!("  Unexpected       {args}"));
    }
    lines.join("\n")
}

fn render_mcp_usage_json(unexpected: Option<&str>) -> Value {
    json!({
        "kind": "mcp",
        "action": "help",
        "usage": {
            "slash_command": "/mcp [list|show <server>|help]",
            "direct_cli": "claw mcp [list|show <server>|help]",
            "sources": [".claw/settings.json", ".claw/settings.local.json"],
        },
        "unexpected": unexpected,
    })
}

fn config_source_label(source: ConfigSource) -> &'static str {
    match source {
        ConfigSource::User => "user",
        ConfigSource::Project => "project",
        ConfigSource::Local => "local",
    }
}

fn mcp_transport_label(config: &McpServerConfig) -> &'static str {
    match config {
        McpServerConfig::Stdio(_) => "stdio",
        McpServerConfig::Sse(_) => "sse",
        McpServerConfig::Http(_) => "http",
        McpServerConfig::Ws(_) => "ws",
        McpServerConfig::Sdk(_) => "sdk",
        McpServerConfig::ManagedProxy(_) => "managed-proxy",
    }
}

fn mcp_server_summary(config: &McpServerConfig) -> String {
    match config {
        McpServerConfig::Stdio(config) => {
            if config.args.is_empty() {
                config.command.clone()
            } else {
                format!("{} {}", config.command, config.args.join(" "))
            }
        }
        McpServerConfig::Sse(config) | McpServerConfig::Http(config) => config.url.clone(),
        McpServerConfig::Ws(config) => config.url.clone(),
        McpServerConfig::Sdk(config) => config.name.clone(),
        McpServerConfig::ManagedProxy(config) => format!("{} ({})", config.id, config.url),
    }
}

fn format_optional_list(values: &[String]) -> String {
    if values.is_empty() {
        "<none>".to_string()
    } else {
        values.join(" ")
    }
}

fn format_optional_keys(mut keys: Vec<String>) -> String {
    if keys.is_empty() {
        return "<none>".to_string();
    }
    keys.sort();
    keys.join(", ")
}

fn format_mcp_oauth(oauth: Option<&McpOAuthConfig>) -> String {
    let Some(oauth) = oauth else {
        return "<none>".to_string();
    };

    let mut parts = Vec::new();
    if let Some(client_id) = &oauth.client_id {
        parts.push(format!("client_id={client_id}"));
    }
    if let Some(port) = oauth.callback_port {
        parts.push(format!("callback_port={port}"));
    }
    if let Some(url) = &oauth.auth_server_metadata_url {
        parts.push(format!("metadata_url={url}"));
    }
    if let Some(xaa) = oauth.xaa {
        parts.push(format!("xaa={xaa}"));
    }
    if parts.is_empty() {
        "enabled".to_string()
    } else {
        parts.join(", ")
    }
}

fn definition_source_id(source: DefinitionSource) -> &'static str {
    match source {
        DefinitionSource::ProjectClaw
        | DefinitionSource::ProjectCodex
        | DefinitionSource::ProjectClaude => "project_claw",
        DefinitionSource::UserClawConfigHome | DefinitionSource::UserCodexHome => {
            "user_claw_config_home"
        }
        DefinitionSource::UserClaw | DefinitionSource::UserCodex | DefinitionSource::UserClaude => {
            "user_claw"
        }
    }
}

fn definition_source_json(source: DefinitionSource) -> Value {
    json!({
        "id": definition_source_id(source),
        "label": source.label(),
    })
}

fn agent_summary_json(agent: &AgentSummary) -> Value {
    json!({
        "name": &agent.name,
        "description": &agent.description,
        "model": &agent.model,
        "reasoning_effort": &agent.reasoning_effort,
        "source": definition_source_json(agent.source),
        "active": agent.shadowed_by.is_none(),
        "shadowed_by": agent.shadowed_by.map(definition_source_json),
    })
}

fn skill_origin_id(origin: SkillOrigin) -> &'static str {
    match origin {
        SkillOrigin::SkillsDir => "skills_dir",
        SkillOrigin::LegacyCommandsDir => "legacy_commands_dir",
    }
}

fn skill_origin_json(origin: SkillOrigin) -> Value {
    json!({
        "id": skill_origin_id(origin),
        "detail_label": origin.detail_label(),
    })
}

fn skill_summary_json(skill: &SkillSummary) -> Value {
    json!({
        "name": &skill.name,
        "description": &skill.description,
        "source": definition_source_json(skill.source),
        "origin": skill_origin_json(skill.origin),
        "active": skill.shadowed_by.is_none(),
        "shadowed_by": skill.shadowed_by.map(definition_source_json),
    })
}

fn config_source_id(source: ConfigSource) -> &'static str {
    match source {
        ConfigSource::User => "user",
        ConfigSource::Project => "project",
        ConfigSource::Local => "local",
    }
}

fn config_source_json(source: ConfigSource) -> Value {
    json!({
        "id": config_source_id(source),
        "label": config_source_label(source),
    })
}

fn mcp_transport_json(config: &McpServerConfig) -> Value {
    let label = mcp_transport_label(config);
    json!({
        "id": label,
        "label": label,
    })
}

fn mcp_oauth_json(oauth: Option<&McpOAuthConfig>) -> Value {
    let Some(oauth) = oauth else {
        return Value::Null;
    };
    json!({
        "client_id": &oauth.client_id,
        "callback_port": oauth.callback_port,
        "auth_server_metadata_url": &oauth.auth_server_metadata_url,
        "xaa": oauth.xaa,
    })
}

fn mcp_server_details_json(config: &McpServerConfig) -> Value {
    match config {
        McpServerConfig::Stdio(config) => json!({
            "command": &config.command,
            "args": &config.args,
            "env_keys": config.env.keys().cloned().collect::<Vec<_>>(),
            "tool_call_timeout_ms": config.tool_call_timeout_ms,
        }),
        McpServerConfig::Sse(config) | McpServerConfig::Http(config) => json!({
            "url": &config.url,
            "header_keys": config.headers.keys().cloned().collect::<Vec<_>>(),
            "headers_helper": &config.headers_helper,
            "oauth": mcp_oauth_json(config.oauth.as_ref()),
        }),
        McpServerConfig::Ws(config) => json!({
            "url": &config.url,
            "header_keys": config.headers.keys().cloned().collect::<Vec<_>>(),
            "headers_helper": &config.headers_helper,
        }),
        McpServerConfig::Sdk(config) => json!({
            "name": &config.name,
        }),
        McpServerConfig::ManagedProxy(config) => json!({
            "url": &config.url,
            "id": &config.id,
        }),
    }
}

fn mcp_server_json(name: &str, server: &ScopedMcpServerConfig) -> Value {
    json!({
        "name": name,
        "scope": config_source_json(server.scope),
        "transport": mcp_transport_json(&server.config),
        "summary": mcp_server_summary(&server.config),
        "details": mcp_server_details_json(&server.config),
    })
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn handle_slash_command(
    input: &str,
    session: &Session,
    compaction: CompactionConfig,
    compaction_enabled: bool,
) -> Option<SlashCommandResult> {
    let command = match SlashCommand::parse(input) {
        Ok(Some(command)) => command,
        Ok(None) => return None,
        Err(error) => {
            return Some(SlashCommandResult {
                message: error.to_string(),
                session: session.clone(),
            });
        }
    };

    match command {
        SlashCommand::Compact { mode } => {
            if !compaction_enabled {
                return Some(SlashCommandResult {
                    message: "Compaction is disabled.".to_string(),
                    session: session.clone(),
                });
            }
            let result = if let Some(mode) = mode {
                partial_compact_session(session, mode)
            } else {
                compact_session_with_memory(session, compaction)
            };
            let message = if result.removed_message_count == 0 {
                "Compaction skipped: session is below the compaction threshold.".to_string()
            } else {
                format!(
                    "Compacted {} messages into a resumable system summary.",
                    result.removed_message_count
                )
            };
            Some(SlashCommandResult {
                message,
                session: result.compacted_session,
            })
        }
        SlashCommand::Help => Some(SlashCommandResult {
            message: render_slash_command_help(),
            session: session.clone(),
        }),
        SlashCommand::Status
        | SlashCommand::Bughunter { .. }
        | SlashCommand::Commit
        | SlashCommand::Pr { .. }
        | SlashCommand::Issue { .. }
        | SlashCommand::Ultraplan { .. }
        | SlashCommand::Teleport { .. }
        | SlashCommand::DebugToolCall
        | SlashCommand::Sandbox
        | SlashCommand::Model { .. }
        | SlashCommand::Permissions { .. }
        | SlashCommand::Clear { .. }
        | SlashCommand::Cost
        | SlashCommand::Resume { .. }
        | SlashCommand::Config { .. }
        | SlashCommand::Mcp { .. }
        | SlashCommand::Memory
        | SlashCommand::Init
        | SlashCommand::Diff
        | SlashCommand::Version
        | SlashCommand::Export { .. }
        | SlashCommand::Session { .. }
        | SlashCommand::Plugins { .. }
        | SlashCommand::Agents { .. }
        | SlashCommand::Skills { .. }
        | SlashCommand::Simplify { .. }
        | SlashCommand::Doctor
        | SlashCommand::Login
        | SlashCommand::Logout
        | SlashCommand::Vim
        | SlashCommand::Upgrade
        | SlashCommand::Stats
        | SlashCommand::Share
        | SlashCommand::Feedback
        | SlashCommand::Files
        | SlashCommand::Fast
        | SlashCommand::Exit
        | SlashCommand::Summary
        | SlashCommand::Desktop
        | SlashCommand::Brief
        | SlashCommand::Advisor
        | SlashCommand::Stickers
        | SlashCommand::Insights
        | SlashCommand::Thinkback
        | SlashCommand::ReleaseNotes
        | SlashCommand::SecurityReview
        | SlashCommand::Keybindings
        | SlashCommand::PrivacySettings
        | SlashCommand::Plan { .. }
        | SlashCommand::Review { .. }
        | SlashCommand::Tasks { .. }
        | SlashCommand::Theme { .. }
        | SlashCommand::Voice { .. }
        | SlashCommand::Usage { .. }
        | SlashCommand::Rename { .. }
        | SlashCommand::Copy { .. }
        | SlashCommand::Hooks { .. }
        | SlashCommand::Context { .. }
        | SlashCommand::Color { .. }
        | SlashCommand::Effort { .. }
        | SlashCommand::Branch { .. }
        | SlashCommand::Rewind { .. }
        | SlashCommand::Ide { .. }
        | SlashCommand::Tag { .. }
        | SlashCommand::OutputStyle { .. }
        | SlashCommand::AddDir { .. }
        | SlashCommand::History { .. }
        | SlashCommand::Unknown(_) => None,
    }
}
