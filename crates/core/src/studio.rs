//! Open cloud places in Roblox Studio through its registered roblox-studio: protocol, the same
//! link the Creator Hub's Edit in Studio button uses.

use std::io;
use std::time::Duration;

use serde_json::Value;

use crate::config::Place;
use crate::https::{self, HttpsError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reference {
    Place(u64),
    Universe(u64),
}

/// A bare place ID, game link, or Creator Hub experience or place link (most specific first:
/// /places/<id>, /games/<id>, ?placeId=<id> or &placeId=<id>, then /experiences/<id>).
pub fn parse_reference(text: &str) -> Option<Reference> {
    let text = text.trim();
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
        return text.parse().ok().map(Reference::Place);
    }
    number_after(text, "/places/", "")
        .map(Reference::Place)
        .or_else(|| number_after(text, "/games/", "").map(Reference::Place))
        .or_else(|| number_after(text, "placeId=", "?&").map(Reference::Place))
        .or_else(|| number_after(text, "/experiences/", "").map(Reference::Universe))
}

/// The first number right after `marker` in `text`. A nonempty `preceded_by` lists the characters
/// allowed just before the marker.
fn number_after(text: &str, marker: &str, preceded_by: &str) -> Option<u64> {
    text.match_indices(marker).find_map(|(at, _)| {
        if !preceded_by.is_empty() && !text[..at].ends_with(|c| preceded_by.contains(c)) {
            return None;
        }
        let rest = &text[at + marker.len()..];
        rest[..rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len())].parse().ok()
    })
}

#[derive(Debug)]
pub enum ResolveError {
    /// Unusable input, or Roblox does not know the ID (4xx).
    Invalid(String),
    /// Roblox could not be reached or failed (5xx, network).
    Unreachable(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Invalid(message) | ResolveError::Unreachable(message) => f.write_str(message),
        }
    }
}

/// Look up the place and universe IDs Studio needs. An experience link opens its start place.
pub fn resolve_place(text: &str) -> Result<Place, ResolveError> {
    resolve_with(text, |url| https::get_json(url, "application/json", Duration::from_secs(10)))
}

fn resolve_with(text: &str, fetch: impl Fn(&str) -> Result<Value, HttpsError>) -> Result<Place, ResolveError> {
    let reference = parse_reference(text)
        .ok_or_else(|| ResolveError::Invalid("Paste a place ID, game link, or Creator Hub link".into()))?;
    let (kind, number, url, key) = match reference {
        Reference::Place(id) => {
            ("place", id, format!("https://apis.roblox.com/universes/v1/places/{id}/universe"), "universeId")
        }
        Reference::Universe(id) => {
            ("universe", id, format!("https://develop.roblox.com/v1/universes/{id}"), "rootPlaceId")
        }
    };
    let unknown = || ResolveError::Invalid(format!("Roblox does not recognize {kind} {number}"));
    let data = fetch(&url).map_err(|error| match error {
        HttpsError::Status(400 | 404) => unknown(),
        error => ResolveError::Unreachable(error.to_string()),
    })?;
    let other = data.get(key).and_then(Value::as_u64).filter(|id| *id > 0).ok_or_else(unknown)?;
    match reference {
        Reference::Place(place_id) if place_id > 0 => Ok(Place { place_id, universe_id: other }),
        Reference::Universe(universe_id) if universe_id > 0 => Ok(Place { place_id: other, universe_id }),
        _ => Err(unknown()),
    }
}

/// The experience's name, as its creator set it.
pub fn experience_name(universe_id: u64) -> Result<String, HttpsError> {
    let url = format!("https://develop.roblox.com/v1/universes/{universe_id}");
    let data = https::get_json(&url, "application/json", Duration::from_secs(10))?;
    data.get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| HttpsError::Transport("The experience has no name".into()))
}

/// Another Studio MCP server set up in Claude Code or Codex, such as robloxstudio-mcp or Roblox's
/// built-in one, as "<name> in <agent>". It gives agents tools like Rojo Manager's Studio tools.
pub fn other_studio_mcp() -> Option<String> {
    let home = std::env::home_dir()?;
    let claude: Option<Value> =
        std::fs::read(home.join(".claude.json")).ok().and_then(|data| serde_json::from_slice(&data).ok());
    if let Some(claude) = claude.as_ref() {
        // User servers, then each folder's own.
        let projects = claude.get("projects").and_then(Value::as_object).into_iter().flat_map(|p| p.values());
        for servers in std::iter::once(claude).chain(projects).filter_map(|t| t.get("mcpServers")?.as_object()) {
            if let Some(name) = servers.iter().find(|(name, server)| studio_server(name, server)).map(|(n, _)| n) {
                return Some(format!("{name} in Claude Code"));
            }
        }
    }
    let codex = std::fs::read_to_string(home.join(".codex").join("config.toml")).ok();
    let codex: Option<Value> = codex.and_then(|text| toml::from_str(&text).ok());
    let servers = codex.as_ref().and_then(|codex| codex.get("mcp_servers")?.as_object())?;
    let name = servers.iter().find(|(name, server)| studio_server(name, server)).map(|(name, _)| name)?;
    Some(format!("{name} in Codex"))
}

/// Whether an MCP server entry runs robloxstudio-mcp or Roblox's built-in Studio MCP (Roblox/mcp.bat),
/// judged by its name, command, and arguments.
fn studio_server(name: &str, server: &Value) -> bool {
    let args = server.get("args").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
    let parts: Vec<&str> =
        std::iter::once(name).chain(server.get("command").and_then(Value::as_str)).chain(args).collect();
    let text = parts.join(" ").to_lowercase().replace('\\', "/").replace(['-', '_'], "");
    !text.contains("rojomanager") && (text.contains("robloxstudio") || text.contains("roblox/mcp"))
}

pub fn launch_uri(place: &Place) -> String {
    format!(
        "roblox-studio:1+launchmode:edit+task:EditPlace+placeId:{}+universeId:{}",
        place.place_id, place.universe_id
    )
}

/// Hand the place to Studio. Errors when Studio is not installed.
pub fn open_place(place: &Place) -> io::Result<()> {
    crate::shell::open(&launch_uri(place))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn references_come_from_ids_and_roblox_links() {
        let hub = "https://create.roblox.com/dashboard/creations/experiences/2222222222";
        let (place_link, overview) = (format!("{hub}/places/111111111111111/configure"), format!("{hub}/overview"));
        for (text, expected) in [
            (" 111111111111111 ", Some(Reference::Place(111111111111111))),
            ("https://www.roblox.com/games/111111111111111/Test-Game", Some(Reference::Place(111111111111111))),
            (place_link.as_str(), Some(Reference::Place(111111111111111))),
            (overview.as_str(), Some(Reference::Universe(2222222222))),
            ("https://www.roblox.com/home?placeId=42", Some(Reference::Place(42))),
            ("https://www.roblox.com/home?xplaceId=7&placeId=42", Some(Reference::Place(42))),
            ("https://www.roblox.com/games/", None),
            ("Test Game", None),
        ] {
            assert_eq!(parse_reference(text), expected, "{text}");
        }
    }

    #[test]
    fn places_and_experiences_resolve_to_both_ids() {
        let place = resolve_with("111111111111111", |url| {
            assert!(url.ends_with("/places/111111111111111/universe"));
            Ok(json!({"universeId": 2222222222u64}))
        });
        assert_eq!(place.unwrap(), Place { place_id: 111111111111111, universe_id: 2222222222 });
        let experience = resolve_with("https://create.roblox.com/dashboard/creations/experiences/2222222222", |_| {
            Ok(json!({"rootPlaceId": 33333333333333u64}))
        });
        assert_eq!(experience.unwrap(), Place { place_id: 33333333333333, universe_id: 2222222222 });
    }

    #[test]
    fn unknown_places_are_invalid_and_outages_are_not() {
        for reply in [json!({"universeId": null}), json!({"universeId": true}), json!({"universeId": 0}), json!([])] {
            let error = resolve_with("1", |_| Ok(reply.clone())).unwrap_err();
            assert!(
                matches!(error, ResolveError::Invalid(ref m) if m == "Roblox does not recognize place 1"),
                "{reply}"
            );
        }
        assert!(matches!(resolve_with("1", |_| Err(HttpsError::Status(404))), Err(ResolveError::Invalid(_))));
        assert!(matches!(resolve_with("1", |_| Err(HttpsError::Status(503))), Err(ResolveError::Unreachable(_))));
        let not_fetched = resolve_with("not a place", |_| panic!("no lookup without a reference"));
        assert!(matches!(not_fetched, Err(ResolveError::Invalid(_))));
    }

    #[test]
    fn launch_uri_matches_the_creator_hub_edit_link() {
        assert_eq!(
            launch_uri(&Place { place_id: 2, universe_id: 1 }),
            "roblox-studio:1+launchmode:edit+task:EditPlace+placeId:2+universeId:1"
        );
    }

    #[test]
    fn studio_mcp_servers_are_recognized_by_name_or_command() {
        assert!(studio_server("tools", &json!({"command": "C:/bin/robloxstudio-mcp.exe"})));
        assert!(studio_server("studio", &json!({"command": r"C:\Users\a\AppData\Local/Roblox/mcp.bat"})));
        assert!(studio_server("Roblox_Studio", &json!({})));
        assert!(!studio_server("rojo-manager", &json!({"command": r"C:\Roblox\RojoManager\RojoManagerMCP.exe"})));
        assert!(!studio_server("blender", &json!({"command": "uvx", "args": ["blender-mcp"]})));
    }
}
