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

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_color::PixelFormat;
    use photocraft_geom::Rect;
    use serde_json::json;

    #[test]
    fn cached_results_commit_once_through_transform_and_warp_history() {
        for warp in [false, true] {
            let mut s = Session::new();
            s.execute("file.new", json!({"width":64,"height":48})).unwrap();
            s.execute("layer.new.layer", json!({})).unwrap();
            s.edit("source", |doc, active| {
                let layer = doc.layer_mut(active.unwrap()).unwrap();
                layer.surface_mut().unwrap().fill_rect(Rect::new(8, 9, 20, 22), &[1., 0., 0., 1.]);
                layer.mask = Some(photocraft_doc::LayerMask::reveal_all());
                layer.mask.as_mut().unwrap().density = 0.0;
                Ok(())
            })
            .unwrap();
            let st = s.active().unwrap();
            let (document, id, before, history) = (st.doc.id, st.active_layer.unwrap(), st.doc.clone(), st.history.past_len());
            let mut pixels = Surface::new(PixelFormat::RGBA8);
            pixels.fill_rect(Rect::new(31, 15, 42, 28), &[0., 0., 1., 1.]);
            let mut mask = Surface::with_default(PixelFormat::GRAY8, &[1.]);
            mask.fill_rect(Rect::new(33, 16, 37, 21), &[0.]);
            s.stage_cached_transform(CachedCommit { document, pixels: HashMap::from([(id, pixels.clone())]), masks: HashMap::from([(id, mask.clone())]) })
                .unwrap();
            assert_eq!(s.active().unwrap().doc.layer(id).unwrap().surface(), before.layer(id).unwrap().surface(), "staging cannot mutate source pixels");
            let (command, params) = if warp {
                ("edit.transform.warp", json!({"layer":id.0,"rect":[8,9,20,22],"style":"arc","bend":25}))
            } else {
                ("edit.transform", json!({"layer":id.0,"matrix":[1,0,0,1,3,4]}))
            };
            s.execute(command, params).unwrap();
            let st = s.active().unwrap();
            let layer = st.doc.layer(id).unwrap();
            assert_eq!(layer.surface(), Some(&pixels), "the resident native result is authoritative");
            assert_eq!(layer.mask.as_ref().unwrap().surface, mask);
            assert_eq!(layer.mask.as_ref().unwrap().density, 0.0);
            assert_eq!(st.history.past_len(), history + 1);
            assert!(s.take_cached_transform().is_none());
            s.undo();
            assert_eq!(s.active().unwrap().doc.layer(id).unwrap().surface(), before.layer(id).unwrap().surface());
            assert_eq!(s.active().unwrap().doc.layer(id).unwrap().mask.as_ref().unwrap().surface, before.layer(id).unwrap().mask.as_ref().unwrap().surface);
        }
    }
}
