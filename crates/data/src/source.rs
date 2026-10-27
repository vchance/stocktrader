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

/// Interleaves two time-ordered sources by `available_at`. On equal timestamps `first` goes first, so
/// e.g. bars precede attention stamped at the same instant.
pub struct MergeSource<A, B> {
    first: A,
    second: B,
    next_first: Option<Event>,
    next_second: Option<Event>,
    started: bool,
}

impl<A: EventSource, B: EventSource> MergeSource<A, B> {
    pub fn new(first: A, second: B) -> Self {
        Self { first, second, next_first: None, next_second: None, started: false }
    }
}

impl<A: EventSource, B: EventSource> EventSource for MergeSource<A, B> {
    fn next_event(&mut self) -> Option<Event> {
        if !self.started {
            self.started = true;
            self.next_first = self.first.next_event();
            self.next_second = self.second.next_event();
        }
        let take_first = match (&self.next_first, &self.next_second) {
            (None, None) => return None,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(a), Some(b)) => a.available_at <= b.available_at,
        };
        if take_first {
            std::mem::replace(&mut self.next_first, self.first.next_event())
        } else {
            std::mem::replace(&mut self.next_second, self.second.next_event())
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, TimeZone, Utc};
    use rust_decimal::Decimal;
    use st_core::{EventKind, NewsItem, Symbol};

    use super::*;
    use crate::daily_bar;

    const ONE: Decimal = Decimal::ONE;

    fn news(hour: u32) -> Event {
        Event {
            available_at: Utc.with_ymd_and_hms(2026, 1, 5, hour, 0, 0).unwrap(),
            kind: EventKind::News(NewsItem { symbols: vec![], source: "t".into(), headline: hour.to_string(), url: None }),
        }
    }

    #[test]
    fn interleaves_by_time_with_first_winning_ties() {
        let day = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
        // The bar closes at 21:00 UTC.
        let bar = Event::bar(daily_bar(Symbol::new("A"), day, ONE, ONE, ONE, ONE, ONE));
        let mut merged = MergeSource::new(VecEventSource::new(vec![bar]), VecEventSource::new(vec![news(22), news(21), news(3)]));
        let order: Vec<String> = std::iter::from_fn(|| merged.next_event())
            .map(|e| match e.kind {
                EventKind::Bar(_) => "bar".to_string(),
                EventKind::News(n) => n.headline,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(order, vec!["3", "bar", "21", "22"]);
    }
}
