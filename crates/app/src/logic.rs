//! Pure view logic: how engine state maps onto labels, buttons, and validation. Kept out of the
//! views so it can be tested without a window.

use std::collections::HashSet;

use rojo_manager_core::config::Settings;
use rojo_manager_core::manager::{ProjectView, ServerState};
use rojo_manager_core::tooling::{StudioPlugin, bridge_outdated};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusColor {
    Idle,
    Yellow,
    Green,
    Blue,
    Red,
}

/// What the main row button does in each state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainAction {
    Start,
    Stop,
    Cancel,
    Kill,
}

impl MainAction {
    pub fn label(self) -> &'static str {
        match self {
            MainAction::Start => "Start",
            MainAction::Stop => "Stop",
            MainAction::Cancel => "Cancel",
            MainAction::Kill => "Kill",
        }
    }
}

pub struct RowStatus {
    pub label: &'static str,
    pub color: StatusColor,
    pub action: MainAction,
    pub tip: &'static str,
}

pub fn row_status(state: ServerState) -> RowStatus {
    let (label, color, action, tip) = match state {
        ServerState::Off => ("Stopped", StatusColor::Idle, MainAction::Start, "Server stopped."),
        ServerState::Starting => ("Starting", StatusColor::Yellow, MainAction::Cancel, "Rojo is starting."),
        ServerState::Serving => ("Serving", StatusColor::Yellow, MainAction::Stop, "Rojo is waiting for a connection."),
        ServerState::Connected => ("Connected", StatusColor::Green, MainAction::Stop, "Rojo has an active connection."),
        ServerState::External => (
            "Existing",
            StatusColor::Blue,
            MainAction::Stop,
            "Rojo started outside this manager, or kept running through a manager restart.",
        ),
        ServerState::Blocked => (
            "Port busy",
            StatusColor::Red,
            MainAction::Kill,
            "Another application is using this port. Kill it, or change this project's port.",
        ),
    };
    RowStatus { label, color, action, tip }
}

/// A server this app is running or launching; stopping or removing it affects Rojo.
pub fn is_active(state: ServerState) -> bool {
    !matches!(state, ServerState::Off | ServerState::Blocked)
}

pub fn can_start_all(projects: &[ProjectView]) -> bool {
    projects.iter().any(|p| p.state == ServerState::Off)
}

pub fn can_stop_all(projects: &[ProjectView]) -> bool {
    projects.iter().any(|p| is_active(p.state))
}

/// Footer line: "3 projects · 2 running · 1 connected".
pub fn summary(projects: &[ProjectView]) -> String {
    tally(&projects.iter().map(|p| p.state).collect::<Vec<_>>(), "project")
}

/// An experience header's line: "2 places · 1 running".
pub fn experience_summary(projects: &[&ProjectView]) -> String {
    tally(&projects.iter().map(|p| p.state).collect::<Vec<_>>(), "place")
}

fn tally(states: &[ServerState], noun: &str) -> String {
    let running = states.iter().filter(|state| state.running()).count();
    let connected = states.iter().filter(|state| **state == ServerState::Connected).count();
    let plural = if states.len() == 1 { "" } else { "s" };
    let mut parts = vec![format!("{} {noun}{plural}", states.len()), format!("{running} running")];
    if connected > 0 {
        parts.push(format!("{connected} connected"));
    }
    parts.join("  ·  ")
}

/// One entry of the project list.
pub enum ListItem<'a> {
    Project(&'a ProjectView),
    /// Two or more projects linked to places of one experience, listed where its first one is.
    Experience {
        universe_id: u64,
        name: String,
        projects: Vec<&'a ProjectView>,
    },
}

/// The list in project order; with `group`, places of the same experience gather under one header.
pub fn list_items(projects: &[ProjectView], group: bool) -> Vec<ListItem<'_>> {
    let universe = |p: &ProjectView| p.place.map(|place| place.universe_id).filter(|_| group);
    let (mut items, mut grouped) = (Vec::new(), HashSet::new());
    for project in projects {
        let Some(id) = universe(project) else {
            items.push(ListItem::Project(project));
            continue;
        };
        if grouped.contains(&id) {
            continue;
        }
        let members: Vec<&ProjectView> = projects.iter().filter(|p| universe(p) == Some(id)).collect();
        if members.len() < 2 {
            items.push(ListItem::Project(project));
            continue;
        }
        grouped.insert(id);
        let name = members.iter().find_map(|p| p.experience.clone()).unwrap_or_else(|| format!("Experience {id}"));
        items.push(ListItem::Experience { universe_id: id, name, projects: members });
    }
    items
}

/// The project's tool badges the person chose to show.
pub fn visible_badges<'a>(badges: &'a [String], settings: &Settings) -> Vec<&'a str> {
    let shown = |badge: &str| match badge {
        "Wally" => settings.show_wally,
        "Rokit" => settings.show_rokit,
        "Aftman" => settings.show_aftman,
        _ => false,
    };
    badges.iter().map(String::as_str).filter(|badge| shown(badge)).collect()
}

pub fn badge_tip(badge: &str) -> &'static str {
    if badge == "Wally" {
        "Install this project's Wally packages"
    } else {
        "Install this project's pinned tools (Rokit also supports Aftman)"
    }
}

/// Port editor input: whole numbers 1-65535 only.
pub fn parse_port(text: &str) -> Result<u16, &'static str> {
    let text = text.trim();
    match text.parse::<u16>() {
        Ok(port) if port > 0 && text.bytes().all(|b| b.is_ascii_digit()) => Ok(port),
        _ => Err("Port must be 1-65535"),
    }
}

/// Split a path so the parent can truncate while the folder name (with its separator) stays visible.
pub fn split_path(path: &str) -> (&str, &str) {
    let trimmed = path.trim_end_matches(['\\', '/']);
    match trimmed.rfind(['\\', '/']) {
        Some(cut) => trimmed.split_at(cut),
        None => ("", trimmed),
    }
}

pub const LOG_CAP: usize = 2000;

/// Append lines newer than the last one held (backlog and live events overlap), keeping the newest `cap`.
pub fn merge_log(lines: &mut Vec<(u64, String)>, added: impl IntoIterator<Item = (u64, String)>, cap: usize) {
    let last = lines.last().map(|(seq, _)| *seq);
    lines.extend(added.into_iter().filter(|(seq, _)| last.is_none_or(|last| *seq > last)));
    if lines.len() > cap {
        lines.drain(..lines.len() - cap);
    }
}

/// The Studio plugin row in Settings > Agents.
#[derive(Debug, PartialEq, Eq)]
pub struct PluginRow {
    pub hint: String,
    /// The hint is a problem to fix.
    pub warn: bool,
    pub button: &'static str,
    /// The button is the suggested next step.
    pub primary: bool,
}

/// `rojo` is the installed Rojo's `--version` output, once read.
pub fn plugin_row(plugin: &StudioPlugin, rojo: Option<&str>, installing: bool) -> PluginRow {
    let row = |hint: String, warn, button, primary| PluginRow { hint, warn, button, primary };
    if installing {
        return row("Installing. Follow the terminal window.".into(), false, "Installing", false);
    }
    match plugin {
        StudioPlugin::Missing => {
            row("Lets agents connect and disconnect Rojo in Studio.".into(), false, "Install", true)
        }
        StudioPlugin::Official => row(
            "The plain Rojo plugin is installed. Add the bridge so agents can use it.".into(),
            false,
            "Add bridge",
            true,
        ),
        StudioPlugin::Bridge { built } => match (bridge_outdated(plugin, rojo), built) {
            (Some(hint), _) => row(hint, true, "Update", true),
            (None, built) => row(
                format!(
                    "Installed for Rojo {}. Updating Rojo here updates it too.",
                    built.as_ref().map_or("", |built| built.rojo.as_str())
                ),
                false,
                "Reinstall",
                false,
            ),
        },
    }
}

/// "Rojo 7.4.4" -> "7.4.4": the `--version` output without the tool name.
pub fn version_number(output: &str) -> &str {
    let output = output.trim();
    output.split_once(char::is_whitespace).map_or(output, |(_, rest)| rest.trim_start())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rojo_manager_core::tooling::BRIDGE_VERSION;

    fn project(state: ServerState) -> ProjectView {
        ProjectView {
            name: "A".into(),
            path: "C:\\Games\\A".into(),
            project_file: "default.project.json".into(),
            port: 34872,
            autostart: false,
            auto_connect: false,
            place: None,
            sourcemap: false,
            experience: None,
            state,
            badges: vec![],
        }
    }

    #[test]
    fn places_of_one_experience_share_a_header_where_the_first_one_was() {
        let linked = |name: &str, universe_id, experience: Option<&str>| ProjectView {
            name: name.into(),
            place: Some(rojo_manager_core::config::Place { place_id: 1, universe_id }),
            experience: experience.map(Into::into),
            ..project(ServerState::Off)
        };
        let projects = [
            project(ServerState::Off),
            linked("Lobby", 7, None),
            linked("Solo", 8, None),
            linked("Arena", 7, Some("Game")),
        ];
        let shape = |group| -> Vec<String> {
            list_items(&projects, group)
                .iter()
                .map(|item| match item {
                    ListItem::Project(p) => p.name.clone(),
                    ListItem::Experience { name, projects, .. } => {
                        format!("{name}: {}", projects.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "))
                    }
                })
                .collect()
        };
        assert_eq!(shape(true), ["A", "Game: Lobby, Arena", "Solo"]);
        assert_eq!(shape(false), ["A", "Lobby", "Solo", "Arena"]);
    }

    #[test]
    fn summary_counts_running_and_connected() {
        let projects = [project(ServerState::Serving), project(ServerState::Connected), project(ServerState::Off)];
        assert_eq!(summary(&projects), "3 projects  ·  2 running  ·  1 connected");
        assert_eq!(summary(&projects[2..]), "1 project  ·  0 running");
    }

    #[test]
    fn start_and_stop_all_follow_states() {
        assert!(can_start_all(&[project(ServerState::Off)]));
        assert!(!can_start_all(&[project(ServerState::Blocked)]));
        assert!(can_stop_all(&[project(ServerState::Starting)]));
        assert!(!can_stop_all(&[project(ServerState::Off), project(ServerState::Blocked)]));
    }

    #[test]
    fn blocked_rows_offer_kill() {
        assert_eq!(row_status(ServerState::Blocked).action, MainAction::Kill);
        assert_eq!(row_status(ServerState::Starting).action.label(), "Cancel");
    }

    #[test]
    fn ports_are_whole_numbers_in_range() {
        assert_eq!(parse_port(" 34872 "), Ok(34872));
        for text in ["0", "65536", "-1", "+80", "8.5", "", "abc"] {
            assert!(parse_port(text).is_err(), "{text}");
        }
    }

    #[test]
    fn paths_split_before_the_folder() {
        assert_eq!(split_path("C:\\Games\\Lobby\\"), ("C:\\Games", "\\Lobby"));
        assert_eq!(split_path("Lobby"), ("", "Lobby"));
    }

    #[test]
    fn logs_merge_by_sequence_and_cap() {
        let mut lines = vec![(1, "a".to_string()), (2, "b".to_string())];
        merge_log(&mut lines, [(2, "b".to_string()), (3, "c".to_string()), (4, "d".to_string())], 3);
        assert_eq!(lines.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(), [2, 3, 4]);
    }

    #[test]
    fn badges_follow_settings() {
        let settings = Settings { show_wally: false, ..Settings::default() };
        assert_eq!(visible_badges(&["Wally".into(), "Rokit".into()], &settings), ["Rokit"]);
    }

    #[test]
    fn the_plugin_row_asks_for_an_update_when_rojo_or_the_bridge_changed() {
        let bridge = |rojo: &str, bridge| StudioPlugin::Bridge {
            built: Some(rojo_manager_core::tooling::BridgeBuild { rojo: rojo.into(), bridge }),
        };
        let row = plugin_row(&bridge("7.6.1", BRIDGE_VERSION), Some("Rojo 7.7.0"), false);
        assert_eq!(
            (row.hint.as_str(), row.button, row.warn),
            (
                "The Studio bridge was built for Rojo 7.6.1, but Rojo 7.7.0 is installed. Update to match.",
                "Update",
                true
            )
        );
        assert_eq!(plugin_row(&bridge("7.7.0", BRIDGE_VERSION - 1), Some("Rojo 7.7.0"), false).button, "Update");
        assert_eq!(plugin_row(&bridge("7.7.0", BRIDGE_VERSION), Some("Rojo 7.7.0"), false).button, "Reinstall");
        assert_eq!(plugin_row(&bridge("7.7.0", BRIDGE_VERSION), None, false).button, "Reinstall");
    }

    #[test]
    fn version_drops_the_tool_name() {
        assert_eq!(version_number("Rojo 7.4.4\n"), "7.4.4");
        assert_eq!(version_number("7.4.4"), "7.4.4");
    }
}
