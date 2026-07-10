use super::cog_adapter::CogAdapter;
use super::types::TrajectoryEvent;

pub fn resolve_event_entity(
    mut event: TrajectoryEvent,
    adapter: &impl CogAdapter,
) -> TrajectoryEvent {
    if event.entity_ref.is_some() {
        return event;
    }
    let Some(path) = event.file_path.as_deref() else {
        return event;
    };

    let entity = match event.line_range {
        Some(range) => adapter.resolve_location(path, range.start),
        None => {
            let candidates = adapter.resolve_file(path);
            select_payload_entity(&event.payload, &candidates)
                .cloned()
                .or_else(|| candidates.into_iter().next())
        }
    };

    if let Some(entity) = entity {
        event.confidence = entity.confidence;
        event.entity_ref = Some(entity);
    }
    event
}

fn select_payload_entity<'a>(
    payload: &serde_json::Value,
    candidates: &'a [super::types::EntityRef],
) -> Option<&'a super::types::EntityRef> {
    let text = payload.to_string();
    candidates
        .iter()
        .filter(|entity| {
            let short_name = entity
                .qualified_name
                .rsplit("::")
                .next()
                .unwrap_or(&entity.qualified_name);
            short_name.len() >= 3 && text.contains(short_name)
        })
        .max_by_key(|entity| entity.qualified_name.matches("::").count())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::cog_recommender::cog_adapter::StaticCogAdapter;
    use crate::cog_recommender::types::{EntityKind, EntityRef, TrajectoryKind};

    #[test]
    fn patch_symbol_selects_matching_entity_in_file() {
        let adapter = StaticCogAdapter::new().with_file(
            "src/service.py",
            EntityRef::new("inventory::service").with_kind(EntityKind::Module),
        );
        let adapter = adapter.with_file(
            "src/service.py",
            EntityRef::new("inventory::service::classify_stock").with_kind(EntityKind::Function),
        );
        let event = TrajectoryEvent {
            id: "event".into(),
            raw_event_id: "raw".into(),
            session_id: "session".into(),
            kind: TrajectoryKind::EditEntity,
            entity_ref: None,
            file_path: Some("src/service.py".into()),
            line_range: None,
            payload: json!({"path": "src/service.py", "patch": "def classify_stock(quantity):"}),
            confidence: 0.0,
        };

        let resolved = resolve_event_entity(event, &adapter);
        assert_eq!(
            resolved.entity_ref.unwrap().qualified_name,
            "inventory::service::classify_stock"
        );
    }
}
