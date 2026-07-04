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
        None => adapter.resolve_file(path).into_iter().next(),
    };

    if let Some(entity) = entity {
        event.confidence = entity.confidence;
        event.entity_ref = Some(entity);
    }
    event
}
