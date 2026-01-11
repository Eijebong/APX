use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Copy, Clone, Debug)]
pub struct Permissions {
    /// permission for the `release` command
    pub release: CommandPermission,
    /// permission for the `collect` command
    pub collect: CommandPermission,
    /// permission for the `remaining` command
    pub remaining: RemainingCommandPermission,
}

#[repr(u8)]
#[derive(Serialize_repr, Deserialize_repr, Copy, Clone, Debug)]
pub enum CommandPermission {
    Disabled = 0b000,    // 0, completely disables access
    Enabled = 0b001,     // 1, allows manual use
    Goal = 0b010,        // 2, allows manual use after goal completion
    Auto = 0b110,        // 6, forces use after goal completion, only works for release and collect
    AutoEnabled = 0b111, // 7, forces use after goal completion, allows manual use any time
}

#[repr(u8)]
#[derive(Serialize_repr, Deserialize_repr, Copy, Clone, Debug)]
pub enum RemainingCommandPermission {
    Disabled = 0b000, // 0, completely disables access
    Enabled = 0b001,  // 1, allows manual use
    Goal = 0b010,     // 2, allows manual use after goal completion
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VersionWithClass {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
    #[serde(rename = "class")]
    pub class_name: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct RoomInfo {
    cmd: String,
    password: bool,
    games: Vec<String>,
    tags: Vec<String>,
    version: VersionWithClass,
    generator_version: VersionWithClass,
    permissions: Permissions,
    #[serde(default)]
    hint_cost: u32,
    #[serde(default)]
    location_check_points: u32,
    #[serde(default)]
    datapackage_checksums: serde_json::Value,
    #[serde(default)]
    seed_name: String,
    #[serde(default)]
    time: f64,
}

impl RoomInfo {
    pub fn set_password(&mut self, password: bool) {
        self.password = password;
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Connect {
    pub cmd: String,
    pub password: String,
    pub name: String,
    pub version: Version,
    pub tags: Vec<String>,
    pub uuid: String,
    #[serde(default)]
    pub game: String,
    #[serde(default)]
    pub slot: u32,
    #[serde(default)]
    pub items_handling: u8,
    #[serde(default)]
    pub slot_data: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SlotInfo {
    pub name: String,
    pub game: String,
    #[serde(rename = "type")]
    pub type_: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_members: Option<Vec<u32>>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Connected {
    pub cmd: String,
    pub team: u32,
    pub slot: u32,
    pub players: Vec<NetworkPlayer>,
    pub missing_locations: Vec<i64>,
    pub checked_locations: Vec<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot_data: Option<serde_json::Value>,
    pub slot_info: HashMap<String, SlotInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint_points: Option<i32>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NetworkPlayer {
    pub team: u32,
    pub slot: u32,
    pub alias: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ConnectionRefused {
    pub cmd: String,
    pub errors: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PrintJSON {
    pub cmd: String,
    pub data: Vec<JSONMessagePart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub type_: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

impl PrintJSON {
    pub fn new(text: &str) -> Self {
        PrintJSON {
            cmd: "PrintJSON".to_string(),
            data: vec![JSONMessagePart {
                text: text.to_string(),
                type_: None,
                color: None,
            }],
            type_: None,
            tags: Vec::new(),
        }
    }

    pub fn with_color(text: &str, color: &str) -> Self {
        PrintJSON {
            cmd: "PrintJSON".to_string(),
            data: vec![JSONMessagePart {
                text: text.to_string(),
                type_: Some("color".to_string()),
                color: Some(color.to_string()),
            }],
            type_: None,
            tags: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct JSONMessagePart {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "type")]
    pub type_: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Say {
    pub cmd: String,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Bounced {
    pub cmd: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub data: serde_json::Value,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SlotPasswordInfo {
    pub slot_number: u32,
    pub player_name: String,
    pub password: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GetDataPackage {
    pub cmd: String,
    #[serde(default)]
    pub games: Vec<String>,
}
