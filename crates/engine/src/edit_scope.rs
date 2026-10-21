//! Shared target resolution for cached canvas edits.

use photocraft_doc::{Document, LayerId};
use serde_json::Value;

use crate::{EngineError, Result};

/// Resolve the scope without changing the user's layer selection. Groups remain roots so their
/// masks and children move together, and a selected descendant is never processed twice.
pub fn roots(doc: &Document, selected: &[LayerId], active: Option<LayerId>, scope: &str) -> Vec<LayerId> {
    let ids = match scope {
        "all" => doc.layers.iter().map(|l| l.id).collect(),
        "group" => selected
            .iter()
            .copied()
            .chain(active.filter(|_| selected.is_empty()))
            .map(|id| {
                let Some(path) = doc.path_of(id) else { return id };
                // Artboards are independent canvases; a group scope stays inside its artboard.
                let start = if path.get(..1).and_then(|p| doc.layer_at(p)).is_some_and(|l| l.artboard().is_some()) { 2 } else { 1 };
                (start..=path.len())
                    .filter_map(|n| path.get(..n).and_then(|p| doc.layer_at(p)))
                    .find(|l| l.is_group() && l.artboard().is_none())
                    .map_or(id, |l| l.id)
            })
            .collect(),
        _ => {
            if selected.is_empty() {
                active.into_iter().collect()
            } else {
                selected.to_vec()
            }
        }
    };
    unique_roots(doc, &ids)
}

fn unique_roots(doc: &Document, ids: &[LayerId]) -> Vec<LayerId> {
    let mut roots = crate::layer_multi_cmds::top_level(doc, ids);
    let mut seen = Vec::new();
    roots.retain(|id| {
        if seen.contains(id) {
            false
        } else {
            seen.push(*id);
            true
        }
    });
    roots
}

/// Explicit frozen roots carried by a preview to its one atomic commit.
pub(crate) fn explicit(doc: &Document, p: &Value) -> Result<Option<Vec<LayerId>>> {
    let Some(value) = p.get("layers") else { return Ok(None) };
    let values = value.as_array().ok_or_else(|| bad("`layers` must be an array"))?;
    if values.is_empty() {
        return Err(bad("`layers` must not be empty"));
    }
    let ids = values
        .iter()
        .map(|v| {
            let id = v.as_u64().map(LayerId).ok_or_else(|| bad("`layers` must contain layer IDs"))?;
            doc.layer(id).ok_or(EngineError::NoLayer(id))?;
            Ok(id)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(unique_roots(doc, &ids)))
}

fn bad(msg: &str) -> EngineError {
    EngineError::BadParams { cmd: "edit.transform".into(), msg: msg.into() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Session;
    use serde_json::json;

    #[test]
    fn selected_siblings_resolve_to_one_group_root() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width":16,"height":16})).unwrap();
        s.execute("layer.new.layer", json!({})).unwrap();
        let a = s.active().unwrap().active_layer.unwrap();
        s.execute("layer.new.layer", json!({})).unwrap();
        let b = s.active().unwrap().active_layer.unwrap();
        s.execute("layer.select", json!({"layer":a.0,"mode":"add"})).unwrap();
        let g = LayerId(s.execute("layer.new.groupFromLayers", json!({})).unwrap()["layer"].as_u64().unwrap());
        let doc = &s.active().unwrap().doc;
        assert_eq!(roots(doc, &[a, b], Some(a), "group"), vec![g]);
        assert_eq!(explicit(doc, &json!({"layers":[g.0,a.0,g.0,b.0]})).unwrap(), Some(vec![g]));
    }
}
