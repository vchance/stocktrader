use st_core::{Event, EventSource};

/// Replays an in-memory set of events in `available_at` order. Ties keep their input order, so
/// merging bars, news and fundamentals from different loaders is just concatenating the vectors.
pub struct VecEventSource {
    events: std::vec::IntoIter<Event>,
}

impl VecEventSource {
    pub fn new(mut events: Vec<Event>) -> Self {
        events.sort_by_key(|e| e.available_at);
        Self { events: events.into_iter() }
    }
}

impl EventSource for VecEventSource {
    fn next_event(&mut self) -> Option<Event> {
        self.events.next()
    }
}
