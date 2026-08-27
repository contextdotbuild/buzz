//! Stable composite-cursor pagination for NIP-AE memory queries.

use std::collections::HashSet;

use nostr::{Event, EventId, PublicKey};

use crate::client::BuzzClient;
use crate::error::CliError;

use buzz_core::kind::KIND_AGENT_ENGRAM;

const PAGE_LIMIT: usize = 1_000;
const MAX_PAGES: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemoryCursor {
    until: u64,
    before_id: String,
}

#[derive(Debug)]
struct PageAccumulator {
    events: Vec<Event>,
    seen_event_ids: HashSet<EventId>,
    seen_cursors: HashSet<(u64, EventId)>,
}

impl PageAccumulator {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            seen_event_ids: HashSet::new(),
            seen_cursors: HashSet::new(),
        }
    }

    fn absorb(&mut self, page: Vec<Event>) -> Result<Option<MemoryCursor>, CliError> {
        let page_len = page.len();
        let next_cursor = page.last().map(|event| {
            (
                event.created_at.as_secs(),
                event.id,
                MemoryCursor {
                    until: event.created_at.as_secs(),
                    before_id: event.id.to_hex(),
                },
            )
        });

        let before_len = self.events.len();
        for event in page {
            if self.seen_event_ids.insert(event.id) {
                self.events.push(event);
            }
        }

        if page_len < PAGE_LIMIT {
            return Ok(None);
        }
        let Some((until, id, cursor)) = next_cursor else {
            return Ok(None);
        };
        if self.events.len() == before_len || !self.seen_cursors.insert((until, id)) {
            return Err(CliError::Other(
                "relay memory pagination made no progress; composite cursor support is required"
                    .into(),
            ));
        }
        Ok(Some(cursor))
    }
}

pub(super) async fn query_all_agent_engrams(
    client: &BuzzClient,
    agent: &PublicKey,
    owner: &PublicKey,
) -> Result<Vec<Event>, CliError> {
    let mut accumulator = PageAccumulator::new();
    let mut cursor: Option<MemoryCursor> = None;

    for _ in 0..MAX_PAGES {
        let mut filter = serde_json::json!({
            "kinds": [KIND_AGENT_ENGRAM],
            "authors": [agent.to_hex()],
            "#p": [owner.to_hex()],
            "limit": PAGE_LIMIT,
        });
        if let Some(cursor) = &cursor {
            filter["until"] = serde_json::json!(cursor.until);
            filter["before_id"] = serde_json::json!(cursor.before_id);
        }

        let raw = client.query(&filter).await?;
        let page: Vec<Event> = serde_json::from_str(&raw).map_err(|error| {
            CliError::Other(format!(
                "relay memory query returned invalid events: {error}"
            ))
        })?;
        cursor = accumulator.absorb(page)?;
        if cursor.is_none() {
            return Ok(accumulator.events);
        }
    }

    Err(CliError::Other(format!(
        "relay memory query exceeded the bounded {MAX_PAGES}-page traversal"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use buzz_core::engram::{build_event, Body};
    use nostr::Keys;

    #[test]
    fn more_than_1000_same_second_schedule_heads_use_composite_cursor() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let mut events = (0..1_005)
            .map(|index| {
                build_event(
                    &agent,
                    &owner.public_key(),
                    &Body::Memory {
                        slug: format!("mem/buzz-follow-through/item-{index:04}"),
                        value: Some("{}".into()),
                    },
                    1_787_772_400,
                )
                .expect("build schedule-shaped engram")
            })
            .collect::<Vec<_>>();
        events.sort_by_key(|event| event.id.to_bytes());

        let mut accumulator = PageAccumulator::new();
        let cursor = accumulator
            .absorb(events[..PAGE_LIMIT].to_vec())
            .expect("first page")
            .expect("full page continues");
        assert_eq!(cursor.until, 1_787_772_400);
        assert_eq!(cursor.before_id, events[PAGE_LIMIT - 1].id.to_hex());

        assert!(accumulator
            .absorb(events[PAGE_LIMIT..].to_vec())
            .expect("tail page")
            .is_none());
        assert_eq!(accumulator.events.len(), 1_005);
        assert_eq!(
            accumulator
                .events
                .iter()
                .map(|event| event.id)
                .collect::<HashSet<_>>()
                .len(),
            1_005
        );
    }

    #[test]
    fn repeated_full_page_fails_instead_of_silently_truncating() {
        let agent = Keys::generate();
        let owner = Keys::generate();
        let event = build_event(
            &agent,
            &owner.public_key(),
            &Body::Memory {
                slug: "mem/buzz-follow-through/repeated".into(),
                value: Some("{}".into()),
            },
            1_787_772_400,
        )
        .expect("build engram");
        let page = vec![event; PAGE_LIMIT];
        let mut accumulator = PageAccumulator::new();
        accumulator.absorb(page.clone()).expect("first page");
        let error = accumulator
            .absorb(page)
            .expect_err("must reject no progress");
        assert!(error
            .to_string()
            .contains("composite cursor support is required"));
    }
}
