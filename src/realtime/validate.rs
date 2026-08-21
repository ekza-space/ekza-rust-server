//! Room state schema + hard limits. Anything the server re-broadcasts to other
//! browsers is validated here first; this is the only place that decides what
//! a "link" or "model" may point at.

use serde::{Deserialize, Serialize};

pub const MAX_OBJECTS: usize = 200;
pub const MAX_ID_LEN: usize = 64;
pub const MAX_LABEL_LEN: usize = 64;
pub const MAX_COLOR_LEN: usize = 16;
pub const MAX_URL_LEN: usize = 512;
pub const MAX_FILE_NAME_LEN: usize = 128;
pub const MAX_ENV_LEN: usize = 64;
pub const MAX_COORD: f32 = 10_000.0;

pub const ALLOWED_KINDS: &[&str] = &["sphere", "cube", "torus", "cone", "link", "model"];

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomAssetInstance {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub position: Vec<f32>,
    pub rotation: Vec<f32>,
    pub scale: Vec<f32>,
    pub color: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_in_new_tab: Option<bool>,
    /// Remote model location (`https://` or `ipfs://`). Inline `data:` URLs are refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_data_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_file_name: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomProgramState {
    pub version: u8,
    pub environment_id: String,
    pub objects: Vec<RoomAssetInstance>,
    pub updated_at: u64,
}

impl RoomProgramState {
    pub fn empty(now_ms: u64) -> Self {
        Self {
            version: 1,
            environment_id: "cosmic-grass".to_string(),
            objects: Vec::new(),
            updated_at: now_ms,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("unsupported state version {0}")]
    Version(u8),
    #[error("environmentId too long")]
    Environment,
    #[error("too many objects ({0} > {MAX_OBJECTS})")]
    TooManyObjects(usize),
    #[error("object {0}: {1}")]
    Object(usize, &'static str),
}

fn check_vec3(v: &[f32]) -> bool {
    v.len() == 3 && v.iter().all(|n| n.is_finite() && n.abs() <= MAX_COORD)
}

fn allowed_link(url: &str) -> bool {
    url.len() <= MAX_URL_LEN
        && (url.starts_with("https://") || url.starts_with("http://"))
        && !url.chars().any(char::is_control)
}

fn allowed_model(url: &str) -> bool {
    url.len() <= MAX_URL_LEN
        && (url.starts_with("https://") || url.starts_with("ipfs://"))
        && !url.chars().any(char::is_control)
}

pub fn validate_room_state(state: &RoomProgramState) -> Result<(), ValidationError> {
    if state.version != 1 {
        return Err(ValidationError::Version(state.version));
    }
    if state.environment_id.len() > MAX_ENV_LEN {
        return Err(ValidationError::Environment);
    }
    if state.objects.len() > MAX_OBJECTS {
        return Err(ValidationError::TooManyObjects(state.objects.len()));
    }
    for (i, o) in state.objects.iter().enumerate() {
        let err = |m| ValidationError::Object(i, m);
        if o.id.is_empty() || o.id.len() > MAX_ID_LEN {
            return Err(err("bad id"));
        }
        if !ALLOWED_KINDS.contains(&o.kind.as_str()) {
            return Err(err("unknown kind"));
        }
        if o.label.len() > MAX_LABEL_LEN {
            return Err(err("label too long"));
        }
        if o.color.len() > MAX_COLOR_LEN {
            return Err(err("color too long"));
        }
        if !check_vec3(&o.position) || !check_vec3(&o.rotation) || !check_vec3(&o.scale) {
            return Err(err("bad transform"));
        }
        if let Some(url) = &o.link_url {
            if !url.is_empty() && !allowed_link(url) {
                return Err(err("link must be http(s) and <= 512 chars"));
            }
        }
        if let Some(url) = &o.model_data_url {
            if !allowed_model(url) {
                return Err(err("model must be https:// or ipfs:// (no inline data)"));
            }
        }
        if let Some(name) = &o.model_file_name {
            if name.len() > MAX_FILE_NAME_LEN {
                return Err(err("model file name too long"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str) -> RoomAssetInstance {
        RoomAssetInstance {
            id: "a".into(),
            kind: kind.into(),
            label: "x".into(),
            position: vec![0.0, 1.0, 2.0],
            rotation: vec![0.0, 0.0, 0.0],
            scale: vec![1.0, 1.0, 1.0],
            color: "#fff".into(),
            link_url: None,
            open_in_new_tab: None,
            model_data_url: None,
            model_file_name: None,
        }
    }

    fn state(objects: Vec<RoomAssetInstance>) -> RoomProgramState {
        RoomProgramState {
            version: 1,
            environment_id: "studio-grid".into(),
            objects,
            updated_at: 0,
        }
    }

    #[test]
    fn accepts_sane_state() {
        let mut link = obj("link");
        link.link_url = Some("https://ekza.io".into());
        let mut model = obj("model");
        model.model_data_url = Some("ipfs://bafy".into());
        assert_eq!(
            validate_room_state(&state(vec![obj("cube"), link, model])),
            Ok(())
        );
    }

    #[test]
    fn rejects_javascript_links_and_inline_models() {
        let mut link = obj("link");
        link.link_url = Some("javascript:alert(1)".into());
        assert!(validate_room_state(&state(vec![link])).is_err());

        let mut model = obj("model");
        model.model_data_url = Some("data:model/gltf-binary;base64,AAAA".into());
        assert!(validate_room_state(&state(vec![model])).is_err());
    }

    #[test]
    fn rejects_garbage_transforms_kinds_and_size() {
        let mut bad = obj("cube");
        bad.position = vec![f32::NAN, 0.0, 0.0];
        assert!(validate_room_state(&state(vec![bad])).is_err());
        assert!(validate_room_state(&state(vec![obj("exploit")])).is_err());
        let many = (0..=MAX_OBJECTS).map(|_| obj("cube")).collect();
        assert!(matches!(
            validate_room_state(&state(many)),
            Err(ValidationError::TooManyObjects(_))
        ));
        let mut s = state(vec![]);
        s.version = 2;
        assert!(validate_room_state(&s).is_err());
    }
}
