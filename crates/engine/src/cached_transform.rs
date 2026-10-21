//! Resident preview results become one normal engine edit at the explicit commit boundary.
use crate::{EngineError, Result, Session};
use photocraft_doc::{DocId, LayerId};
use photocraft_raster::Surface;
use std::collections::HashMap;
#[derive(Clone)]
pub struct CachedCommit {
    pub document: DocId,
    pub pixels: HashMap<LayerId, Surface>,
    /// Original mask values, before density and Properties feather.
    pub masks: HashMap<LayerId, Surface>,
}
impl Session {
    pub fn stage_cached_transform(&mut self, commit: CachedCommit) -> Result<()> {
        if self.active().is_none_or(|s| s.doc.id != commit.document) {
            return Err(EngineError::Other("the cached transform belongs to a different document".into()));
        }
        self.cached_transform_commit = Some(commit);
        Ok(())
    }
    pub fn take_cached_transform(&mut self) -> Option<CachedCommit> {
        self.cached_transform_commit.take()
    }
}
