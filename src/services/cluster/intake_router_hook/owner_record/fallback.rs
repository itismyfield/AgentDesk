//! Input-time routing primitives; runtime routing remains disconnected.

pub(crate) mod notice;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Home<'a> {
    Known(&'a str),
    Unknown,
    Conflict,
    StorageUnknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Availability {
    Available,
    Unavailable,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Held {
    NoGateway,
    HomeUnknown,
    HomeConflict,
    StorageUnknown,
    InvalidPair,
    AvailabilityUnknown,
    NeitherAvailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target<'a> {
    Selected(&'a str),
    Held(Held),
}

pub(crate) struct Snapshot<'a> {
    pub home: Home<'a>,
    pub pair: [&'a str; 2],
    pub availability: [Availability; 2],
    pub gateway: Option<&'a str>,
}

pub(crate) fn choose_target<'a>(snapshot: &Snapshot<'a>) -> Target<'a> {
    if snapshot.gateway.is_none() {
        return Target::Held(Held::NoGateway);
    }
    let home = match snapshot.home {
        Home::Known(home) => home,
        Home::Unknown => return Target::Held(Held::HomeUnknown),
        Home::Conflict => return Target::Held(Held::HomeConflict),
        Home::StorageUnknown => return Target::Held(Held::StorageUnknown),
    };
    let [a, b] = snapshot.pair;
    if a.trim().is_empty() || b.trim().is_empty() || a == b {
        return Target::Held(Held::InvalidPair);
    }
    let home_index = if home == a {
        0
    } else if home == b {
        1
    } else {
        return Target::Held(Held::HomeUnknown);
    };
    match snapshot.availability[home_index] {
        Availability::Available => Target::Selected(snapshot.pair[home_index]),
        Availability::Unknown => Target::Held(Held::AvailabilityUnknown),
        Availability::Unavailable => match snapshot.availability[1 - home_index] {
            Availability::Available => Target::Selected(snapshot.pair[1 - home_index]),
            Availability::Unavailable => Target::Held(Held::NeitherAvailable),
            Availability::Unknown => Target::Held(Held::AvailabilityUnknown),
        },
    }
}
