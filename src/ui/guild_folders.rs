//! Server folders, read from the Discord account and shown in the server rail.
//!
//! Discord keeps them in the account's settings (protobuf, with the older JSON settings
//! as a fallback). The rail is one flat list: a folder row, then its servers while open.

use std::cell::RefCell;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use slint::{Color, Image, Model, ModelRc, VecModel};

use crate::{AppWindow, GuildItem};

const DEFAULT_FOLDER_COLOR: u32 = 0x5865F2;
const PREVIEW_ICONS: usize = 4;
const USER_SETTINGS_GUILD_FOLDERS: u64 = 14;

/// One entry of the account's server list. Without an id it is a lone server.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub color: Option<u32>,
    pub guild_ids: Vec<String>,
}

impl Folder {
    fn is_real(&self) -> bool {
        !self.id.is_empty()
    }
}

pub fn has_real_folders(folders: &[Folder]) -> bool {
    folders.iter().any(Folder::is_real)
}

/// Server ids in the order the account lists them.
pub fn flattened_ids(folders: &[Folder]) -> Vec<String> {
    folders.iter().flat_map(|f| f.guild_ids.iter().cloned()).collect()
}

pub fn parse_json(settings: &serde_json::Value) -> Vec<Folder> {
    let Some(entries) = settings["guild_folders"].as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .map(|entry| Folder {
            id: entry["id"]
                .as_u64()
                .map(|n| n.to_string())
                .or_else(|| entry["id"].as_str().map(str::to_owned))
                .unwrap_or_default(),
            name: entry["name"].as_str().unwrap_or("").to_owned(),
            color: entry["color"].as_u64().map(|c| c as u32),
            guild_ids: entry["guild_ids"]
                .as_array()
                .map(|ids| ids.iter().filter_map(|id| id.as_str().map(str::to_owned)).collect())
                .unwrap_or_default(),
        })
        .collect()
}

enum Field<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32,
}

fn read_varint(buf: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *buf.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// The fields of a protobuf message; `None` when it is malformed.
fn read_fields(buf: &[u8]) -> Option<Vec<(u64, Field<'_>)>> {
    let mut fields = Vec::new();
    let mut at = 0;
    while at < buf.len() {
        let key = read_varint(buf, &mut at)?;
        let field = match key & 7 {
            0 => Field::Varint(read_varint(buf, &mut at)?),
            1 => {
                let end = at.checked_add(8)?;
                let value = u64::from_le_bytes(buf.get(at..end)?.try_into().ok()?);
                at = end;
                Field::Fixed64(value)
            }
            2 => {
                let len = usize::try_from(read_varint(buf, &mut at)?).ok()?;
                let end = at.checked_add(len)?;
                let bytes = buf.get(at..end)?;
                at = end;
                Field::Bytes(bytes)
            }
            5 => {
                at = at.checked_add(4).filter(|end| *end <= buf.len())?;
                Field::Fixed32
            }
            _ => return None,
        };
        fields.push((key >> 3, field));
    }
    Some(fields)
}

/// The value inside a protobuf wrapper message (Int64Value, UInt64Value).
fn wrapped_varint(buf: &[u8]) -> Option<u64> {
    read_fields(buf)?.into_iter().find_map(|(number, field)| match (number, field) {
        (1, Field::Varint(value)) => Some(value),
        _ => None,
    })
}

fn wrapped_string(buf: &[u8]) -> Option<String> {
    read_fields(buf)?.into_iter().find_map(|(number, field)| match (number, field) {
        (1, Field::Bytes(text)) => std::str::from_utf8(text).ok().map(str::to_owned),
        _ => None,
    })
}

fn parse_proto_folder(buf: &[u8]) -> Option<Folder> {
    let mut folder = Folder::default();
    for (number, field) in read_fields(buf)? {
        match (number, field) {
            (1, Field::Bytes(ids)) => folder.guild_ids.extend(
                ids.chunks_exact(8)
                    .map(|id| u64::from_le_bytes(id.try_into().unwrap_or([0; 8])).to_string()),
            ),
            (1, Field::Fixed64(id)) => folder.guild_ids.push(id.to_string()),
            (2, Field::Bytes(id)) => {
                folder.id = wrapped_varint(id).filter(|n| *n != 0).map(|n| n.to_string()).unwrap_or_default();
            }
            (3, Field::Bytes(name)) => folder.name = wrapped_string(name).unwrap_or_default(),
            (4, Field::Bytes(color)) => folder.color = wrapped_varint(color).map(|c| c as u32),
            _ => {}
        }
    }
    Some(folder)
}

/// The folders in the account's settings (`PreloadedUserSettings`).
pub fn parse_proto(settings: &[u8]) -> Vec<Folder> {
    let Some(fields) = read_fields(settings) else {
        return Vec::new();
    };
    let mut folders = Vec::new();
    for (number, field) in fields {
        let (USER_SETTINGS_GUILD_FOLDERS, Field::Bytes(section)) = (number, field) else {
            continue;
        };
        for (inner, entry) in read_fields(section).unwrap_or_default() {
            if let (1, Field::Bytes(entry)) = (inner, entry) {
                folders.extend(parse_proto_folder(entry));
            }
        }
    }
    folders
}

#[derive(Debug, PartialEq, Eq)]
enum Row {
    Guild { guild: usize, folder: Option<usize> },
    Folder(usize),
}

/// Servers missing from the account's list come first, as on Discord.
fn layout(folders: &[Folder], guild_ids: &[&str], expanded: &HashSet<String>) -> Vec<Row> {
    let listed: HashSet<&str> = folders.iter().flat_map(|f| f.guild_ids.iter().map(String::as_str)).collect();
    let position = |id: &str| guild_ids.iter().position(|g| *g == id);
    let mut rows: Vec<Row> = guild_ids
        .iter()
        .enumerate()
        .filter(|(_, id)| !listed.contains(**id))
        .map(|(guild, _)| Row::Guild { guild, folder: None })
        .collect();

    for (index, folder) in folders.iter().enumerate() {
        let members: Vec<usize> = folder.guild_ids.iter().filter_map(|id| position(id)).collect();
        if !folder.is_real() {
            rows.extend(members.into_iter().map(|guild| Row::Guild { guild, folder: None }));
        } else if !members.is_empty() {
            rows.push(Row::Folder(index));
            if expanded.contains(&folder.id) {
                rows.extend(members.into_iter().map(|guild| Row::Guild { guild, folder: Some(index) }));
            }
        }
    }
    rows
}

#[derive(Default)]
struct Rail {
    guilds: Vec<GuildItem>,
    folders: Vec<Folder>,
    expanded: HashSet<String>,
}

thread_local! {
    static RAIL: RefCell<Rail> = RefCell::new(Rail::default());
}

fn folder_color(folder: &Folder) -> Color {
    let rgb = folder.color.filter(|c| *c != 0).unwrap_or(DEFAULT_FOLDER_COLOR);
    Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

fn folder_row(rail: &Rail, index: usize, active: &str) -> GuildItem {
    let folder = &rail.folders[index];
    let members = || rail.guilds.iter().filter(|g| folder.guild_ids.iter().any(|id| id == g.id.as_str()));
    let previews: Vec<Image> = members()
        .filter(|g| g.has_image)
        .take(PREVIEW_ICONS)
        .map(|g| g.icon_image.clone())
        .collect();
    GuildItem {
        id: format!("folder:{}", folder.id).into(),
        name: folder.name.as_str().into(),
        is_folder: true,
        folder_id: folder.id.as_str().into(),
        folder_color: folder_color(folder),
        is_expanded: rail.expanded.contains(&folder.id),
        contains_active: members().any(|g| g.id == active),
        previews: ModelRc::new(VecModel::from(previews)),
        ..Default::default()
    }
}

fn render(ui: &AppWindow, rail: &Rail) {
    let ids: Vec<&str> = rail.guilds.iter().map(|g| g.id.as_str()).collect();
    let active = ui.get_active_guild_id();
    let rows: Vec<GuildItem> = layout(&rail.folders, &ids, &rail.expanded)
        .into_iter()
        .map(|row| match row {
            Row::Folder(index) => folder_row(rail, index, &active),
            Row::Guild { guild, folder } => {
                let mut item = rail.guilds[guild].clone();
                if let Some(index) = folder {
                    item.in_folder = true;
                    item.folder_color = folder_color(&rail.folders[index]);
                }
                item
            }
        })
        .collect();
    ui.set_guild_drag_enabled(!has_real_folders(&rail.folders));
    ui.set_guilds(ModelRc::new(VecModel::from(rows)));
}

/// Shows these servers, keeping the current folders.
pub fn show(ui: &AppWindow, guilds: Vec<GuildItem>) {
    RAIL.with_borrow_mut(|rail| {
        rail.guilds = guilds;
        render(ui, rail);
    });
}

/// Shows these servers inside the account's folders.
pub fn show_with_folders(ui: &AppWindow, guilds: Vec<GuildItem>, folders: Vec<Folder>) {
    RAIL.with_borrow_mut(|rail| {
        rail.guilds = guilds;
        rail.folders = if has_real_folders(&folders) { folders } else { Vec::new() };
        render(ui, rail);
    });
}

/// Another account, or none: nothing of the previous rail stays.
pub fn clear(ui: &AppWindow) {
    RAIL.with_borrow_mut(|rail| {
        *rail = Rail::default();
        render(ui, rail);
    });
}

pub fn toggle(ui: &AppWindow, folder_id: &str) {
    RAIL.with_borrow_mut(|rail| {
        if !rail.expanded.remove(folder_id) {
            rail.expanded.insert(folder_id.to_owned());
        }
        render(ui, rail);
    });
}

pub fn set_icon(ui: &AppWindow, guild_id: &str, icon: Image) {
    RAIL.with_borrow_mut(|rail| {
        if let Some(item) = rail.guilds.iter_mut().find(|g| g.id == guild_id) {
            item.has_image = true;
            item.icon_image = icon;
            render(ui, rail);
        }
    });
}

/// The open server changed: a closed folder marks that it holds it.
pub fn refresh_active(ui: &AppWindow) {
    let active = ui.get_active_guild_id();
    let rows = ui.get_guilds();
    RAIL.with_borrow(|rail| {
        for row in 0..rows.row_count() {
            let Some(mut item) = rows.row_data(row).filter(|item| item.is_folder) else {
                continue;
            };
            let Some(folder) = rail.folders.iter().find(|f| f.id == item.folder_id.as_str()) else {
                continue;
            };
            let holds = folder.guild_ids.iter().any(|id| id == active.as_str());
            if item.contains_active != holds {
                item.contains_active = holds;
                rows.set_row_data(row, item);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrapped(tag: u8, value: u8) -> Vec<u8> {
        vec![tag, 2, 0x08, value]
    }

    fn folder_bytes() -> Vec<u8> {
        let mut folder = vec![0x0A, 16];
        folder.extend_from_slice(&11u64.to_le_bytes());
        folder.extend_from_slice(&22u64.to_le_bytes());
        folder.extend(wrapped(0x12, 7));
        folder.extend_from_slice(&[0x1A, 5, 0x0A, 3, b'F', b'u', b'n']);
        folder.extend(wrapped(0x22, 9));
        folder
    }

    #[test]
    fn reads_folders_from_the_protobuf_settings() {
        let folder = folder_bytes();
        let mut section = vec![0x0A, folder.len() as u8];
        section.extend(folder);
        let mut settings = vec![0x72, section.len() as u8];
        settings.extend(section);

        assert_eq!(
            parse_proto(&settings),
            vec![Folder { id: "7".into(), name: "Fun".into(), color: Some(9), guild_ids: vec!["11".into(), "22".into()] }]
        );
    }

    #[test]
    fn malformed_settings_give_no_folders() {
        assert!(parse_proto(&[0x72, 200, 1]).is_empty());
        assert!(parse_proto(&[0xFF]).is_empty());
    }

    #[test]
    fn reads_folders_from_the_json_settings() {
        let settings = serde_json::json!({ "guild_folders": [
            { "id": 5, "name": "Games", "color": 255, "guild_ids": ["1", "2"] },
            { "id": null, "name": null, "color": null, "guild_ids": ["3"] }
        ]});
        let folders = parse_json(&settings);
        assert_eq!(folders[0].id, "5");
        assert_eq!(folders[0].guild_ids, ["1", "2"]);
        assert!(!folders[1].is_real());
        assert!(has_real_folders(&folders));
    }

    #[test]
    fn a_closed_folder_hides_its_servers_and_an_open_one_shows_them() {
        let folders = vec![
            Folder { guild_ids: vec!["a".into()], ..Default::default() },
            Folder { id: "9".into(), guild_ids: vec!["b".into(), "c".into()], ..Default::default() },
        ];
        let ids = ["new", "a", "b", "c"];
        let closed = layout(&folders, &ids, &HashSet::new());
        assert_eq!(closed, vec![
            Row::Guild { guild: 0, folder: None },
            Row::Guild { guild: 1, folder: None },
            Row::Folder(1),
        ]);
        let open = layout(&folders, &ids, &HashSet::from(["9".to_string()]));
        assert_eq!(open.len(), 5);
        assert_eq!(open[3], Row::Guild { guild: 2, folder: Some(1) });
    }

    #[test]
    fn a_folder_with_no_joined_servers_is_not_shown() {
        let folders = vec![Folder { id: "9".into(), guild_ids: vec!["gone".into()], ..Default::default() }];
        assert!(layout(&folders, &["a"], &HashSet::new()).iter().all(|row| !matches!(row, Row::Folder(_))));
    }
}
