use crate::spec::{slash_command_specs, SlashCommandSpec};

fn find_slash_command_spec(name: &str) -> Option<&'static SlashCommandSpec> {
    slash_command_specs().iter().find(|spec| {
        spec.name.eq_ignore_ascii_case(name)
            || spec
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(name))
    })
}

pub(crate) fn command_root_name(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or(command)
}

fn slash_command_usage(spec: &SlashCommandSpec) -> String {
    match spec.argument_hint {
        Some(argument_hint) => format!("/{} {argument_hint}", spec.name),
        None => format!("/{}", spec.name),
    }
}

fn slash_command_detail_lines(spec: &SlashCommandSpec) -> Vec<String> {
    let mut lines = vec![format!("/{}", spec.name)];
    lines.push(format!("  Summary          {}", spec.summary));
    lines.push(format!("  Usage            {}", slash_command_usage(spec)));
    lines.push(format!(
        "  Category         {}",
        slash_command_category(spec.name)
    ));
    if !spec.aliases.is_empty() {
        lines.push(format!(
            "  Aliases          {}",
            spec.aliases
                .iter()
                .map(|alias| format!("/{alias}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if spec.resume_supported {
        lines.push("  Resume           Supported with --resume SESSION.jsonl".to_string());
    }
    lines
}

#[must_use]
pub fn render_slash_command_help_detail(name: &str) -> Option<String> {
    find_slash_command_spec(name).map(|spec| slash_command_detail_lines(spec).join("\n"))
}

#[must_use]
pub fn resume_supported_slash_commands() -> Vec<&'static SlashCommandSpec> {
    slash_command_specs()
        .iter()
        .filter(|spec| spec.resume_supported)
        .collect()
}

fn slash_command_category(name: &str) -> &'static str {
    match name {
        "help" | "status" | "cost" | "resume" | "session" | "version" | "login" | "logout"
        | "usage" | "stats" | "rename" | "clear" | "compact" | "history" | "tokens" | "cache"
        | "exit" | "summary" | "tag" | "thinkback" | "copy" | "share" | "feedback" | "rewind"
        | "pin" | "unpin" | "bookmarks" | "context" | "files" | "focus" | "unfocus" | "retry"
        | "stop" | "undo" => "Session",
        "model" | "permissions" | "config" | "memory" | "theme" | "vim" | "voice" | "color"
        | "effort" | "fast" | "brief" | "output-style" | "keybindings" | "privacy-settings"
        | "stickers" | "language" | "profile" | "max-tokens" | "temperature" | "system-prompt"
        | "api-key" | "terminal-setup" | "notifications" | "telemetry" | "providers" | "env"
        | "project" | "reasoning" | "budget" | "rate-limit" | "workspace" | "reset" | "ide"
        | "desktop" | "upgrade" => "Config",
        "debug-tool-call" | "doctor" | "sandbox" | "diagnostics" | "tool-details" | "changelog"
        | "metrics" => "Debug",
        _ => "Tools",
    }
}

fn format_slash_command_help_line(spec: &SlashCommandSpec) -> String {
    let name = slash_command_usage(spec);
    let alias_suffix = if spec.aliases.is_empty() {
        String::new()
    } else {
        format!(
            " (aliases: {})",
            spec.aliases
                .iter()
                .map(|alias| format!("/{alias}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let resume = if spec.resume_supported {
        " [resume]"
    } else {
        ""
    };
    format!("  {name:<66} {}{alias_suffix}{resume}", spec.summary)
}

fn levenshtein_distance(left: &str, right: &str) -> usize {
    if left == right {
        return 0;
    }
    if left.is_empty() {
        return right.chars().count();
    }
    if right.is_empty() {
        return left.chars().count();
    }

    let right_chars = right.chars().collect::<Vec<_>>();
    let mut previous = (0..=right_chars.len()).collect::<Vec<_>>();
    let mut current = vec![0; right_chars.len() + 1];

    for (left_index, left_char) in left.chars().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_char) in right_chars.iter().enumerate() {
            let substitution_cost = usize::from(left_char != *right_char);
            current[right_index + 1] = (current[right_index] + 1)
                .min(previous[right_index + 1] + 1)
                .min(previous[right_index] + substitution_cost);
        }
        previous.clone_from(&current);
    }

    previous[right_chars.len()]
}

#[must_use]
pub fn suggest_slash_commands(input: &str, limit: usize) -> Vec<String> {
    let query = input.trim().trim_start_matches('/').to_ascii_lowercase();
    if query.is_empty() || limit == 0 {
        return Vec::new();
    }

    let mut suggestions = slash_command_specs()
        .iter()
        .filter_map(|spec| {
            let best = std::iter::once(spec.name)
                .chain(spec.aliases.iter().copied())
                .map(str::to_ascii_lowercase)
                .map(|candidate| {
                    let prefix_rank =
                        if candidate.starts_with(&query) || query.starts_with(&candidate) {
                            0
                        } else if candidate.contains(&query) || query.contains(&candidate) {
                            1
                        } else {
                            2
                        };
                    let distance = levenshtein_distance(&candidate, &query);
                    (prefix_rank, distance)
                })
                .min();

            best.and_then(|(prefix_rank, distance)| {
                if prefix_rank <= 1 || distance <= 2 {
                    Some((prefix_rank, distance, spec.name.len(), spec.name))
                } else {
                    None
                }
            })
        })
        .collect::<Vec<_>>();

    suggestions.sort_unstable();
    suggestions
        .into_iter()
        .map(|(_, _, _, name)| format!("/{name}"))
        .take(limit)
        .collect()
}

#[must_use]
/// Render the slash-command help section, optionally excluding stub commands
/// (commands that are registered in the spec list but not yet implemented).
/// Pass an empty slice to include all commands.
pub fn render_slash_command_help_filtered(exclude: &[&str]) -> String {
    let mut lines = vec![
        "Slash commands".to_string(),
        "  Start here        /status, /diff, /agents, /skills, /commit".to_string(),
        "  [resume]          also works with --resume SESSION.jsonl".to_string(),
        String::new(),
    ];

    let categories = ["Session", "Tools", "Config", "Debug"];

    for category in categories {
        lines.push(category.to_string());
        for spec in slash_command_specs()
            .iter()
            .filter(|spec| slash_command_category(spec.name) == category)
            .filter(|spec| !exclude.contains(&spec.name))
        {
            lines.push(format_slash_command_help_line(spec));
        }
        lines.push(String::new());
    }

    lines
        .into_iter()
        .rev()
        .skip_while(String::is_empty)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn render_slash_command_help() -> String {
    let mut lines = vec![
        "Slash commands".to_string(),
        "  Start here        /status, /diff, /agents, /skills, /commit".to_string(),
        "  [resume]          also works with --resume SESSION.jsonl".to_string(),
        String::new(),
    ];

    let categories = ["Session", "Tools", "Config", "Debug"];

    for category in categories {
        lines.push(category.to_string());
        for spec in slash_command_specs()
            .iter()
            .filter(|spec| slash_command_category(spec.name) == category)
        {
            lines.push(format_slash_command_help_line(spec));
        }
        lines.push(String::new());
    }

    lines.push("Keyboard shortcuts".to_string());
    lines.push("  Up/Down              Navigate prompt history".to_string());
    lines.push("  Tab                  Complete commands, modes, and recent sessions".to_string());
    lines.push("  Ctrl-C               Clear input (or exit on empty prompt)".to_string());
    lines.push("  Shift+Enter/Ctrl+J   Insert a newline".to_string());

    lines
        .into_iter()
        .rev()
        .skip_while(String::is_empty)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}
