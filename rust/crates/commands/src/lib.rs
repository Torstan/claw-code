mod handlers;
mod help;
mod parse;
mod registry;
mod shared_args;
mod simplify;
mod spec;

pub use handlers::{
    classify_skills_slash_command, handle_agents_slash_command, handle_agents_slash_command_json,
    handle_mcp_slash_command, handle_mcp_slash_command_json, handle_plugins_slash_command,
    handle_skills_slash_command, handle_skills_slash_command_json, handle_slash_command,
    render_plugins_report, resolve_skill_invocation, resolve_skill_path, PluginsCommandResult,
    SkillSlashDispatch, SlashCommandResult,
};
pub use help::{
    render_slash_command_help, render_slash_command_help_detail,
    render_slash_command_help_filtered, resume_supported_slash_commands, suggest_slash_commands,
};
pub use parse::{validate_slash_command_input, SlashCommand, SlashCommandParseError};
pub use registry::{CommandManifestEntry, CommandRegistry, CommandSource};
pub use simplify::build_simplify_prompt;
pub use spec::{slash_command_specs, SlashCommandSpec};

#[cfg(test)]
pub(crate) use handlers::{
    install_skill_into, load_agents_from_roots, load_skills_from_roots, parse_skill_frontmatter,
    render_agents_report, render_agents_report_json, render_mcp_report_for,
    render_mcp_report_json_for, render_skill_install_report, render_skills_report,
    render_skills_report_json, DefinitionSource, SkillOrigin, SkillRoot,
};

#[cfg(test)]
mod tests;
