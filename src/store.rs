//! Durable room state: one JSON file per room under `DATA_DIR/rooms/`.
//!
//! Writes are atomic (temp file + rename) so a crash mid-write never leaves a
//! truncated room on disk. Rooms are loaded lazily on first access.

use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Serialize};
use tokio::fs;

#[derive(Clone)]
pub struct RoomStore {
    root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl RoomStore {
    pub async fn open(data_dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let root = data_dir.as_ref().join("rooms");
        fs::create_dir_all(&root).await?;
        Ok(Self { root })
    }

    fn path_for(&self, room_id: u32) -> PathBuf {
        self.root.join(format!("{room_id}.json"))
    }

    pub async fn load<T: DeserializeOwned>(&self, room_id: u32) -> Result<Option<T>, StoreError> {
        match fs::read(self.path_for(room_id)).await {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub async fn save<T: Serialize>(&self, room_id: u32, value: &T) -> Result<(), StoreError> {
        let final_path = self.path_for(room_id);
        let tmp_path = self.root.join(format!("{room_id}.json.tmp"));
        let bytes = serde_json::to_vec(value)?;
        fs::write(&tmp_path, &bytes).await?;
        fs::rename(&tmp_path, &final_path).await?;
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<u32>, StoreError> {
        let mut ids = Vec::new();
        let mut dir = fs::read_dir(&self.root).await?;
        while let Some(entry) = dir.next_entry().await? {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(stem) = name.strip_suffix(".json") {
                if let Ok(id) = stem.parse::<u32>() {
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Doc {
        revision: u64,
        text: String,
    }

    #[tokio::test]
    async fn roundtrip_and_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let store = RoomStore::open(dir.path()).await.unwrap();
        assert_eq!(store.load::<Doc>(5).await.unwrap(), None);

        let a = Doc {
            revision: 1,
            text: "a".into(),
        };
        store.save(5, &a).await.unwrap();
        assert_eq!(store.load::<Doc>(5).await.unwrap(), Some(a));

        let b = Doc {
            revision: 2,
            text: "b".into(),
        };
        store.save(5, &b).await.unwrap();
        assert_eq!(store.load::<Doc>(5).await.unwrap(), Some(b));
        assert_eq!(store.list().await.unwrap(), vec![5]);
        assert!(!dir.path().join("rooms/5.json.tmp").exists());
    }
}
