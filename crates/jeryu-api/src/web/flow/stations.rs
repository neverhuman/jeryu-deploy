//! The stations of the delivery line: the route every lot travels.
//!
//! The nine steps group the twelve `GET /api/v1/trace` stages into what a
//! person reasons about, in a fixed order (`docs/flow.md`, "Stations"). Both
//! Flow modes read this list, so the Live report and the Layout view cannot
//! disagree about what the line is.

use serde::Serialize;

/// One step of the route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Station {
    Queue,
    Work,
    Shift,
    Review,
    Gate,
    Merge,
    Pin,
    Stage,
    Deploy,
}

impl Station {
    /// Every station in route order: what the report lists and what an ETA
    /// sums over. Product code names a station, never the list.
    pub(crate) const ALL: [Self; 9] = [
        Self::Queue,
        Self::Work,
        Self::Shift,
        Self::Review,
        Self::Gate,
        Self::Merge,
        Self::Pin,
        Self::Stage,
        Self::Deploy,
    ];

    /// The wire id, also the `station` column of `flow_ledger`.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Queue => "queue",
            Self::Work => "work",
            Self::Shift => "shift",
            Self::Review => "review",
            Self::Gate => "gate",
            Self::Merge => "merge",
            Self::Pin => "pin",
            Self::Stage => "stage",
            Self::Deploy => "deploy",
        }
    }

    /// What a reader is shown.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Queue => "Queue",
            Self::Work => "Work",
            Self::Shift => "Shift",
            Self::Review => "Review",
            Self::Gate => "Gate",
            Self::Merge => "Merge",
            Self::Pin => "Pin",
            Self::Stage => "Stage",
            Self::Deploy => "Deploy",
        }
    }

    /// Its place on the route, 1 for the first station.
    pub(crate) fn order(self) -> u8 {
        Self::ALL
            .iter()
            .position(|station| *station == self)
            .map_or(0, |index| u8::try_from(index + 1).unwrap_or(u8::MAX))
    }

    /// Whether the station's tool has the lot from the moment it arrives.
    ///
    /// Most stations have a start signal: a slot claims the todo, a runner
    /// beats, the queue enqueues. Shift and Stage have none (`docs/flow.md`
    /// leaves their "Starts when" empty) because their tool is a timer that
    /// is already running: the lot is in its hands on arrival. Their queue
    /// time is therefore zero and the whole visit is process time, rather
    /// than both being unknown and taking every ETA down the line with them.
    pub(crate) fn starts_on_arrival(self) -> bool {
        matches!(self, Self::Shift | Self::Stage)
    }

    /// The station a stored `flow_ledger` row names, or `None` for an id no
    /// station answers to (a row an older or newer binary wrote).
    pub(crate) fn parse(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|station| station.id() == id)
    }
}
