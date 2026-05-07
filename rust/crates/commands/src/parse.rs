use std::fmt;

use runtime::PartialCompactMode;

use crate::help::{command_root_name, render_slash_command_help_detail};
use crate::shared_args::normalize_optional_args;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Status,
    Sandbox,
    Compact {
        mode: Option<PartialCompactMode>,
    },
    Bughunter {
        scope: Option<String>,
    },
    Commit,
    Pr {
        context: Option<String>,
    },
    Issue {
        context: Option<String>,
    },
    Ultraplan {
        task: Option<String>,
    },
    Teleport {
        target: Option<String>,
    },
    DebugToolCall,
    Model {
        model: Option<String>,
    },
    Permissions {
        mode: Option<String>,
    },
    Clear {
        confirm: bool,
    },
    Cost,
    Resume {
        session_path: Option<String>,
    },
    Config {
        section: Option<String>,
    },
    Mcp {
        action: Option<String>,
        target: Option<String>,
    },
    Memory,
    Init,
    Diff,
    Version,
    Export {
        path: Option<String>,
    },
    Session {
        action: Option<String>,
        target: Option<String>,
    },
    Plugins {
        action: Option<String>,
        target: Option<String>,
    },
    Agents {
        args: Option<String>,
    },
    Skills {
        args: Option<String>,
    },
    Simplify {
        args: Option<String>,
    },
    Doctor,
    Login,
    Logout,
    Vim,
    Upgrade,
    Stats,
    Share,
    Feedback,
    Files,
    Fast,
    Exit,
    Summary,
    Desktop,
    Brief,
    Advisor,
    Stickers,
    Insights,
    Thinkback,
    ReleaseNotes,
    SecurityReview,
    Keybindings,
    PrivacySettings,
    Plan {
        mode: Option<String>,
    },
    Review {
        scope: Option<String>,
    },
    Tasks {
        args: Option<String>,
    },
    Theme {
        name: Option<String>,
    },
    Voice {
        mode: Option<String>,
    },
    Usage {
        scope: Option<String>,
    },
    Rename {
        name: Option<String>,
    },
    Copy {
        target: Option<String>,
    },
    Hooks {
        args: Option<String>,
    },
    Context {
        action: Option<String>,
    },
    Color {
        scheme: Option<String>,
    },
    Effort {
        level: Option<String>,
    },
    Branch {
        name: Option<String>,
    },
    Rewind {
        steps: Option<String>,
    },
    Ide {
        target: Option<String>,
    },
    Tag {
        label: Option<String>,
    },
    OutputStyle {
        style: Option<String>,
    },
    AddDir {
        path: Option<String>,
    },
    History {
        count: Option<String>,
    },
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommandParseError {
    message: String,
}

impl SlashCommandParseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SlashCommandParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SlashCommandParseError {}

impl SlashCommand {
    pub fn parse(input: &str) -> Result<Option<Self>, SlashCommandParseError> {
        validate_slash_command_input(input)
    }

    /// Returns the canonical slash-command name (e.g. `"/branch"`) for use in
    /// error messages and logging. Derived from the spec table so it always
    /// matches what the user would have typed.
    #[must_use]
    pub fn slash_name(&self) -> &'static str {
        match self {
            Self::Help => "/help",
            Self::Clear { .. } => "/clear",
            Self::Compact { .. } => "/compact",
            Self::Cost => "/cost",
            Self::Doctor => "/doctor",
            Self::Config { .. } => "/config",
            Self::Memory { .. } => "/memory",
            Self::History { .. } => "/history",
            Self::Diff => "/diff",
            Self::Status => "/status",
            Self::Stats => "/stats",
            Self::Version => "/version",
            Self::Commit { .. } => "/commit",
            Self::Pr { .. } => "/pr",
            Self::Issue { .. } => "/issue",
            Self::Init => "/init",
            Self::Bughunter { .. } => "/bughunter",
            Self::Ultraplan { .. } => "/ultraplan",
            Self::Teleport { .. } => "/teleport",
            Self::DebugToolCall { .. } => "/debug-tool-call",
            Self::Resume { .. } => "/resume",
            Self::Model { .. } => "/model",
            Self::Permissions { .. } => "/permissions",
            Self::Session { .. } => "/session",
            Self::Plugins { .. } => "/plugins",
            Self::Simplify { .. } => "/simplify",
            Self::Login => "/login",
            Self::Logout => "/logout",
            Self::Vim => "/vim",
            Self::Upgrade => "/upgrade",
            Self::Share => "/share",
            Self::Feedback => "/feedback",
            Self::Files => "/files",
            Self::Fast => "/fast",
            Self::Exit => "/exit",
            Self::Summary => "/summary",
            Self::Desktop => "/desktop",
            Self::Brief => "/brief",
            Self::Advisor => "/advisor",
            Self::Stickers => "/stickers",
            Self::Insights => "/insights",
            Self::Thinkback => "/thinkback",
            Self::ReleaseNotes => "/release-notes",
            Self::SecurityReview => "/security-review",
            Self::Keybindings => "/keybindings",
            Self::PrivacySettings => "/privacy-settings",
            Self::Plan { .. } => "/plan",
            Self::Review { .. } => "/review",
            Self::Tasks { .. } => "/tasks",
            Self::Theme { .. } => "/theme",
            Self::Voice { .. } => "/voice",
            Self::Usage { .. } => "/usage",
            Self::Rename { .. } => "/rename",
            Self::Copy { .. } => "/copy",
            Self::Hooks { .. } => "/hooks",
            Self::Context { .. } => "/context",
            Self::Color { .. } => "/color",
            Self::Effort { .. } => "/effort",
            Self::Branch { .. } => "/branch",
            Self::Rewind { .. } => "/rewind",
            Self::Ide { .. } => "/ide",
            Self::Tag { .. } => "/tag",
            Self::OutputStyle { .. } => "/output-style",
            Self::AddDir { .. } => "/add-dir",
            Self::Sandbox => "/sandbox",
            Self::Mcp { .. } => "/mcp",
            Self::Export { .. } => "/export",
            #[allow(unreachable_patterns)]
            _ => "/unknown",
        }
    }
}

#[allow(clippy::too_many_lines)]
pub fn validate_slash_command_input(
    input: &str,
) -> Result<Option<SlashCommand>, SlashCommandParseError> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return Ok(None);
    }

    let mut parts = trimmed.trim_start_matches('/').split_whitespace();
    let command = parts.next().unwrap_or_default();
    if command.is_empty() {
        return Err(SlashCommandParseError::new(
            "Slash command name is missing. Use /help to list available slash commands.",
        ));
    }

    let args = parts.collect::<Vec<_>>();
    let remainder = remainder_after_command(trimmed, command);

    Ok(Some(match command {
        "help" => {
            validate_no_args(command, &args)?;
            SlashCommand::Help
        }
        "status" => {
            validate_no_args(command, &args)?;
            SlashCommand::Status
        }
        "sandbox" => {
            validate_no_args(command, &args)?;
            SlashCommand::Sandbox
        }
        "compact" => parse_compact_args(command, &args, remainder.as_ref())?,
        "bughunter" => SlashCommand::Bughunter { scope: remainder },
        "commit" => {
            validate_no_args(command, &args)?;
            SlashCommand::Commit
        }
        "pr" => SlashCommand::Pr { context: remainder },
        "issue" => SlashCommand::Issue { context: remainder },
        "ultraplan" => SlashCommand::Ultraplan { task: remainder },
        "teleport" => SlashCommand::Teleport {
            target: Some(require_remainder(command, remainder, "<symbol-or-path>")?),
        },
        "debug-tool-call" => {
            validate_no_args(command, &args)?;
            SlashCommand::DebugToolCall
        }
        "model" => SlashCommand::Model {
            model: optional_single_arg(command, &args, "[model]")?,
        },
        "permissions" => SlashCommand::Permissions {
            mode: parse_permissions_mode(&args)?,
        },
        "clear" => SlashCommand::Clear {
            confirm: parse_clear_args(&args)?,
        },
        "cost" => {
            validate_no_args(command, &args)?;
            SlashCommand::Cost
        }
        "resume" => SlashCommand::Resume {
            session_path: Some(require_remainder(command, remainder, "<session-path>")?),
        },
        "config" => SlashCommand::Config {
            section: parse_config_section(&args)?,
        },
        "mcp" => parse_mcp_command(&args)?,
        "memory" => {
            validate_no_args(command, &args)?;
            SlashCommand::Memory
        }
        "init" => {
            validate_no_args(command, &args)?;
            SlashCommand::Init
        }
        "diff" => {
            validate_no_args(command, &args)?;
            SlashCommand::Diff
        }
        "version" => {
            validate_no_args(command, &args)?;
            SlashCommand::Version
        }
        "export" => SlashCommand::Export { path: remainder },
        "session" => parse_session_command(&args)?,
        "plugin" | "plugins" | "marketplace" => parse_plugin_command(&args)?,
        "agents" => SlashCommand::Agents {
            args: parse_list_or_help_args(command, remainder)?,
        },
        "skills" | "skill" => SlashCommand::Skills {
            args: parse_skills_args(remainder.as_deref())?,
        },
        "simplify" => SlashCommand::Simplify { args: remainder },
        "doctor" | "providers" => {
            validate_no_args(command, &args)?;
            SlashCommand::Doctor
        }
        "login" => {
            validate_no_args(command, &args)?;
            SlashCommand::Login
        }
        "logout" => {
            validate_no_args(command, &args)?;
            SlashCommand::Logout
        }
        "vim" => {
            validate_no_args(command, &args)?;
            SlashCommand::Vim
        }
        "upgrade" => {
            validate_no_args(command, &args)?;
            SlashCommand::Upgrade
        }
        "stats" | "tokens" | "cache" => {
            validate_no_args(command, &args)?;
            SlashCommand::Stats
        }
        "share" => {
            validate_no_args(command, &args)?;
            SlashCommand::Share
        }
        "feedback" => {
            validate_no_args(command, &args)?;
            SlashCommand::Feedback
        }
        "files" => {
            validate_no_args(command, &args)?;
            SlashCommand::Files
        }
        "fast" => {
            validate_no_args(command, &args)?;
            SlashCommand::Fast
        }
        "exit" => {
            validate_no_args(command, &args)?;
            SlashCommand::Exit
        }
        "summary" => {
            validate_no_args(command, &args)?;
            SlashCommand::Summary
        }
        "desktop" => {
            validate_no_args(command, &args)?;
            SlashCommand::Desktop
        }
        "brief" => {
            validate_no_args(command, &args)?;
            SlashCommand::Brief
        }
        "advisor" => {
            validate_no_args(command, &args)?;
            SlashCommand::Advisor
        }
        "stickers" => {
            validate_no_args(command, &args)?;
            SlashCommand::Stickers
        }
        "insights" => {
            validate_no_args(command, &args)?;
            SlashCommand::Insights
        }
        "thinkback" => {
            validate_no_args(command, &args)?;
            SlashCommand::Thinkback
        }
        "release-notes" => {
            validate_no_args(command, &args)?;
            SlashCommand::ReleaseNotes
        }
        "security-review" => {
            validate_no_args(command, &args)?;
            SlashCommand::SecurityReview
        }
        "keybindings" => {
            validate_no_args(command, &args)?;
            SlashCommand::Keybindings
        }
        "privacy-settings" => {
            validate_no_args(command, &args)?;
            SlashCommand::PrivacySettings
        }
        "plan" => SlashCommand::Plan { mode: remainder },
        "review" => SlashCommand::Review { scope: remainder },
        "tasks" => SlashCommand::Tasks { args: remainder },
        "theme" => SlashCommand::Theme { name: remainder },
        "voice" => SlashCommand::Voice { mode: remainder },
        "usage" => SlashCommand::Usage { scope: remainder },
        "rename" => SlashCommand::Rename { name: remainder },
        "copy" => SlashCommand::Copy { target: remainder },
        "hooks" => SlashCommand::Hooks { args: remainder },
        "context" => SlashCommand::Context { action: remainder },
        "color" => SlashCommand::Color { scheme: remainder },
        "effort" => SlashCommand::Effort { level: remainder },
        "branch" => SlashCommand::Branch { name: remainder },
        "rewind" => SlashCommand::Rewind { steps: remainder },
        "ide" => SlashCommand::Ide { target: remainder },
        "tag" => SlashCommand::Tag { label: remainder },
        "output-style" => SlashCommand::OutputStyle { style: remainder },
        "add-dir" => SlashCommand::AddDir { path: remainder },
        "history" => SlashCommand::History {
            count: optional_single_arg(command, &args, "[count]")?,
        },
        other => SlashCommand::Unknown(other.to_string()),
    }))
}
fn validate_no_args(command: &str, args: &[&str]) -> Result<(), SlashCommandParseError> {
    if args.is_empty() {
        return Ok(());
    }

    Err(command_error(
        &format!("Unexpected arguments for /{command}."),
        command,
        &format!("/{command}"),
    ))
}

fn parse_compact_args(
    command: &str,
    args: &[&str],
    _remainder: Option<&String>,
) -> Result<SlashCommand, SlashCommandParseError> {
    if args.is_empty() {
        return Ok(SlashCommand::Compact { mode: None });
    }

    let usage = "/compact [--up-to-prompt N | --from-prompt N]";
    let mut up_to_prompt: Option<usize> = None;
    let mut from_prompt: Option<usize> = None;
    let mut i = 0;

    while i < args.len() {
        match args[i] {
            "--up-to-prompt" => {
                i += 1;
                let val = args.get(i).ok_or_else(|| usage_error(command, usage))?;
                let n: usize = val.parse().map_err(|_| {
                    command_error("Prompt number must be a positive integer.", command, usage)
                })?;
                if n == 0 {
                    return Err(command_error(
                        "Prompt number must be a positive integer.",
                        command,
                        usage,
                    ));
                }
                up_to_prompt = Some(n);
            }
            "--from-prompt" => {
                i += 1;
                let val = args.get(i).ok_or_else(|| usage_error(command, usage))?;
                let n: usize = val.parse().map_err(|_| {
                    command_error("Prompt number must be a positive integer.", command, usage)
                })?;
                if n == 0 {
                    return Err(command_error(
                        "Prompt number must be a positive integer.",
                        command,
                        usage,
                    ));
                }
                from_prompt = Some(n);
            }
            _ => {
                return Err(command_error(&format!("Usage: {usage}"), command, usage));
            }
        }
        i += 1;
    }

    if up_to_prompt.is_some() && from_prompt.is_some() {
        return Err(command_error(
            "Cannot use both --up-to-prompt and --from-prompt at the same time.",
            command,
            usage,
        ));
    }

    Ok(SlashCommand::Compact {
        mode: PartialCompactMode::from_options(up_to_prompt, from_prompt),
    })
}

fn optional_single_arg(
    command: &str,
    args: &[&str],
    argument_hint: &str,
) -> Result<Option<String>, SlashCommandParseError> {
    match args {
        [] => Ok(None),
        [value] => Ok(Some((*value).to_string())),
        _ => Err(usage_error(command, argument_hint)),
    }
}

fn require_remainder(
    command: &str,
    remainder: Option<String>,
    argument_hint: &str,
) -> Result<String, SlashCommandParseError> {
    remainder.ok_or_else(|| usage_error(command, argument_hint))
}

fn parse_permissions_mode(args: &[&str]) -> Result<Option<String>, SlashCommandParseError> {
    let mode = optional_single_arg(
        "permissions",
        args,
        "[read-only|workspace-write|danger-full-access]",
    )?;
    if let Some(mode) = mode {
        if matches!(
            mode.as_str(),
            "read-only" | "workspace-write" | "danger-full-access"
        ) {
            return Ok(Some(mode));
        }
        return Err(command_error(
            &format!(
                "Unsupported /permissions mode '{mode}'. Use read-only, workspace-write, or danger-full-access."
            ),
            "permissions",
            "/permissions [read-only|workspace-write|danger-full-access]",
        ));
    }

    Ok(None)
}

fn parse_clear_args(args: &[&str]) -> Result<bool, SlashCommandParseError> {
    match args {
        [] => Ok(false),
        ["--confirm"] => Ok(true),
        [unexpected] => Err(command_error(
            &format!("Unsupported /clear argument '{unexpected}'. Use /clear or /clear --confirm."),
            "clear",
            "/clear [--confirm]",
        )),
        _ => Err(usage_error("clear", "[--confirm]")),
    }
}

fn parse_config_section(args: &[&str]) -> Result<Option<String>, SlashCommandParseError> {
    let section = optional_single_arg("config", args, "[env|hooks|model|plugins]")?;
    if let Some(section) = section {
        if matches!(section.as_str(), "env" | "hooks" | "model" | "plugins") {
            return Ok(Some(section));
        }
        return Err(command_error(
            &format!("Unsupported /config section '{section}'. Use env, hooks, model, or plugins."),
            "config",
            "/config [env|hooks|model|plugins]",
        ));
    }

    Ok(None)
}

fn parse_session_command(args: &[&str]) -> Result<SlashCommand, SlashCommandParseError> {
    match args {
        [] => Ok(SlashCommand::Session {
            action: None,
            target: None,
        }),
        ["list"] => Ok(SlashCommand::Session {
            action: Some("list".to_string()),
            target: None,
        }),
        ["list", ..] => Err(usage_error("session", "[list|switch <session-id>|fork [branch-name]|delete <session-id> [--force]]")),
        ["switch"] => Err(usage_error("session switch", "<session-id>")),
        ["switch", target] => Ok(SlashCommand::Session {
            action: Some("switch".to_string()),
            target: Some((*target).to_string()),
        }),
        ["switch", ..] => Err(command_error(
            "Unexpected arguments for /session switch.",
            "session",
            "/session switch <session-id>",
        )),
        ["fork"] => Ok(SlashCommand::Session {
            action: Some("fork".to_string()),
            target: None,
        }),
        ["fork", target] => Ok(SlashCommand::Session {
            action: Some("fork".to_string()),
            target: Some((*target).to_string()),
        }),
        ["fork", ..] => Err(command_error(
            "Unexpected arguments for /session fork.",
            "session",
            "/session fork [branch-name]",
        )),
        ["delete"] => Err(usage_error("session delete", "<session-id> [--force]")),
        ["delete", target] => Ok(SlashCommand::Session {
            action: Some("delete".to_string()),
            target: Some((*target).to_string()),
        }),
        ["delete", target, "--force"] => Ok(SlashCommand::Session {
            action: Some("delete-force".to_string()),
            target: Some((*target).to_string()),
        }),
        ["delete", _target, unexpected] => Err(command_error(
            &format!(
                "Unsupported /session delete flag '{unexpected}'. Use --force to skip confirmation."
            ),
            "session",
            "/session delete <session-id> [--force]",
        )),
        ["delete", ..] => Err(command_error(
            "Unexpected arguments for /session delete.",
            "session",
            "/session delete <session-id> [--force]",
        )),
        [action, ..] => Err(command_error(
            &format!(
                "Unknown /session action '{action}'. Use list, switch <session-id>, fork [branch-name], or delete <session-id> [--force]."
            ),
            "session",
            "/session [list|switch <session-id>|fork [branch-name]|delete <session-id> [--force]]",
        )),
    }
}

fn parse_mcp_command(args: &[&str]) -> Result<SlashCommand, SlashCommandParseError> {
    match args {
        [] => Ok(SlashCommand::Mcp {
            action: None,
            target: None,
        }),
        ["list"] => Ok(SlashCommand::Mcp {
            action: Some("list".to_string()),
            target: None,
        }),
        ["list", ..] => Err(usage_error("mcp list", "")),
        ["show"] => Err(usage_error("mcp show", "<server>")),
        ["show", target] => Ok(SlashCommand::Mcp {
            action: Some("show".to_string()),
            target: Some((*target).to_string()),
        }),
        ["show", ..] => Err(command_error(
            "Unexpected arguments for /mcp show.",
            "mcp",
            "/mcp show <server>",
        )),
        ["help" | "-h" | "--help"] => Ok(SlashCommand::Mcp {
            action: Some("help".to_string()),
            target: None,
        }),
        [action, ..] => Err(command_error(
            &format!("Unknown /mcp action '{action}'. Use list, show <server>, or help."),
            "mcp",
            "/mcp [list|show <server>|help]",
        )),
    }
}

fn parse_plugin_command(args: &[&str]) -> Result<SlashCommand, SlashCommandParseError> {
    match args {
        [] => Ok(SlashCommand::Plugins {
            action: None,
            target: None,
        }),
        ["list"] => Ok(SlashCommand::Plugins {
            action: Some("list".to_string()),
            target: None,
        }),
        ["list", ..] => Err(usage_error("plugin list", "")),
        ["install"] => Err(usage_error("plugin install", "<path>")),
        ["install", target @ ..] => Ok(SlashCommand::Plugins {
            action: Some("install".to_string()),
            target: Some(target.join(" ")),
        }),
        ["enable"] => Err(usage_error("plugin enable", "<name>")),
        ["enable", target] => Ok(SlashCommand::Plugins {
            action: Some("enable".to_string()),
            target: Some((*target).to_string()),
        }),
        ["enable", ..] => Err(command_error(
            "Unexpected arguments for /plugin enable.",
            "plugin",
            "/plugin enable <name>",
        )),
        ["disable"] => Err(usage_error("plugin disable", "<name>")),
        ["disable", target] => Ok(SlashCommand::Plugins {
            action: Some("disable".to_string()),
            target: Some((*target).to_string()),
        }),
        ["disable", ..] => Err(command_error(
            "Unexpected arguments for /plugin disable.",
            "plugin",
            "/plugin disable <name>",
        )),
        ["uninstall"] => Err(usage_error("plugin uninstall", "<id>")),
        ["uninstall", target] => Ok(SlashCommand::Plugins {
            action: Some("uninstall".to_string()),
            target: Some((*target).to_string()),
        }),
        ["uninstall", ..] => Err(command_error(
            "Unexpected arguments for /plugin uninstall.",
            "plugin",
            "/plugin uninstall <id>",
        )),
        ["update"] => Err(usage_error("plugin update", "<id>")),
        ["update", target] => Ok(SlashCommand::Plugins {
            action: Some("update".to_string()),
            target: Some((*target).to_string()),
        }),
        ["update", ..] => Err(command_error(
            "Unexpected arguments for /plugin update.",
            "plugin",
            "/plugin update <id>",
        )),
        [action, ..] => Err(command_error(
            &format!(
                "Unknown /plugin action '{action}'. Use list, install <path>, enable <name>, disable <name>, uninstall <id>, or update <id>."
            ),
            "plugin",
            "/plugin [list|install <path>|enable <name>|disable <name>|uninstall <id>|update <id>]",
        )),
    }
}

fn parse_list_or_help_args(
    command: &str,
    args: Option<String>,
) -> Result<Option<String>, SlashCommandParseError> {
    match normalize_optional_args(args.as_deref()) {
        None | Some("list" | "help" | "-h" | "--help") => Ok(args),
        Some(unexpected) => Err(command_error(
            &format!(
                "Unexpected arguments for /{command}: {unexpected}. Use /{command}, /{command} list, or /{command} help."
            ),
            command,
            &format!("/{command} [list|help]"),
        )),
    }
}

fn parse_skills_args(args: Option<&str>) -> Result<Option<String>, SlashCommandParseError> {
    let Some(args) = normalize_optional_args(args) else {
        return Ok(None);
    };

    if matches!(args, "list" | "help" | "-h" | "--help") {
        return Ok(Some(args.to_string()));
    }

    if args == "install" {
        return Err(command_error(
            "Usage: /skills install <path>",
            "skills",
            "/skills install <path>",
        ));
    }

    if let Some(target) = args.strip_prefix("install").map(str::trim) {
        if !target.is_empty() {
            return Ok(Some(format!("install {target}")));
        }
    }

    Ok(Some(args.to_string()))
}

fn usage_error(command: &str, argument_hint: &str) -> SlashCommandParseError {
    let usage = format!("/{command} {argument_hint}");
    let usage = usage.trim_end().to_string();
    command_error(
        &format!("Usage: {usage}"),
        command_root_name(command),
        &usage,
    )
}

fn command_error(message: &str, command: &str, usage: &str) -> SlashCommandParseError {
    let detail = render_slash_command_help_detail(command)
        .map(|detail| format!("\n\n{detail}"))
        .unwrap_or_default();
    SlashCommandParseError::new(format!("{message}\n  Usage            {usage}{detail}"))
}

fn remainder_after_command(input: &str, command: &str) -> Option<String> {
    input
        .trim()
        .strip_prefix(&format!("/{command}"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}
