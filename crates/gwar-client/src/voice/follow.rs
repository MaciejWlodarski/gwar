//! Which device each side of the audio belongs on while devices come and go.
//!
//! "System default" follows the operating system's default device. A device chosen by name
//! is used while it is present; when it disappears the side falls back to the default, and
//! it returns to the chosen device when that comes back. These are pure decisions, testable
//! without hardware: `io` feeds them what the operating system reports and opens the
//! streams they ask for.

use std::time::{Duration, Instant};

/// One direction of the audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Input,
    Output,
}

/// What the operating system offers on one side right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Offer {
    /// The system default device's name; `None` when there is no device at all.
    pub default: Option<String>,
    pub names: Vec<String>,
}

/// A device to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// The device the user chose, by name.
    Named(String),
    /// The system default, named as it was when opened.
    Default(Option<String>),
}

/// The decision for the current offer.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    Keep,
    /// Open this in place of the side's stream, if any.
    Open(Route),
    /// Nothing to open: the side has no device.
    Missing,
}

/// What to tell the user after a stream opened.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Opened {
    /// Audio moved to this device without the user choosing it.
    pub switched: Option<String>,
    /// A failure was reported earlier and is over now.
    pub recovered: bool,
}

/// Retries of a device that failed to open (or kept dying) start here and double up to [`RETRY_MAX`].
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(8);
/// A stream lost sooner than this after opening counts as a failure, so a broken device is not
/// reopened in a tight loop.
const FLAPPING: Duration = Duration::from_secs(2);

struct Failure {
    /// The offer the failure happened with; a different offer is tried at once. `None` holds any offer.
    offer: Option<Offer>,
    retry_at: Instant,
    backoff: Duration,
}

/// A chosen device that failed to open: the default stands in until `until`.
struct Bench {
    name: String,
    until: Instant,
    backoff: Duration,
}

/// One side's device state.
pub struct Follow {
    wanted: Option<String>,
    /// The operating system moves an open default stream to a new default by itself.
    reroutes: bool,
    open: Option<Route>,
    opened_at: Option<Instant>,
    /// The system default when the open stream was opened: a loss after the default changed
    /// is the change, not a broken device.
    opened_default: Option<String>,
    /// The last route opened since the user chose `wanted`; a change of kind is announced.
    last: Option<Route>,
    failure: Option<Failure>,
    bench: Option<Bench>,
    /// A failure was reported and not followed by a successful open yet.
    reported: bool,
    /// Microphone access is denied: not retried until the engine restarts.
    blocked: bool,
}

impl Follow {
    pub fn new(wanted: Option<String>, reroutes: bool) -> Self {
        Self {
            wanted,
            reroutes,
            open: None,
            opened_at: None,
            opened_default: None,
            last: None,
            failure: None,
            bench: None,
            reported: false,
            blocked: false,
        }
    }

    /// The user chose another device (`None`: the system default). True if that changes anything.
    pub fn want(&mut self, wanted: Option<String>) -> bool {
        if self.wanted == wanted {
            return false;
        }
        self.wanted = wanted;
        // The user's own choice is not announced, and is tried at once.
        self.last = None;
        self.failure = None;
        self.bench = None;
        true
    }

    fn target(&self, offer: &Offer, now: Instant) -> Route {
        match &self.wanted {
            Some(name)
                if offer.names.contains(name)
                    && !self.bench.as_ref().is_some_and(|b| b.name == *name && now < b.until) =>
            {
                Route::Named(name.clone())
            }
            _ => Route::Default(offer.default.clone()),
        }
    }

    pub fn plan(&mut self, offer: &Offer, now: Instant) -> Plan {
        if self.blocked {
            return Plan::Keep;
        }
        let target = self.target(offer, now);
        match &mut self.open {
            Some(Route::Default(name)) if self.reroutes && matches!(target, Route::Default(Some(_))) => {
                name.clone_from(&offer.default);
                self.opened_default.clone_from(&offer.default);
                Plan::Keep
            }
            // The default stream is already on the chosen device: relabel it instead of reopening.
            // Not where a default stream moves with the default by itself.
            Some(open)
                if !self.reroutes
                    && matches!((&*open, &target), (Route::Default(Some(a)), Route::Named(b)) if a == b) =>
            {
                *open = target.clone();
                self.last = Some(target);
                Plan::Keep
            }
            Some(open) if *open == target => Plan::Keep,
            // Nothing better to move to; a stream that died reports itself.
            Some(_) if target == Route::Default(None) => Plan::Keep,
            // The move failed with this offer (see `stayed`): the current stream plays on for now.
            Some(_) if self.failure.as_ref().is_some_and(|f| now < f.retry_at && f.offer.as_ref() == Some(offer)) => {
                Plan::Keep
            }
            Some(_) => Plan::Open(target),
            None => {
                let held = self.failure.as_ref().is_some_and(|f| {
                    now < f.retry_at && f.offer.as_ref().is_none_or(|failed_with| failed_with == offer)
                });
                if held {
                    Plan::Keep
                } else if target == Route::Default(None) {
                    Plan::Missing
                } else {
                    Plan::Open(target)
                }
            }
        }
    }

    pub fn opened(&mut self, route: Route, offer: &Offer, now: Instant) -> Opened {
        let switched = match (&self.last, &route) {
            (Some(Route::Named(_)), Route::Default(Some(name))) | (Some(Route::Default(_)), Route::Named(name)) => {
                Some(name.clone())
            }
            _ => None,
        };
        if matches!(route, Route::Named(_)) {
            self.bench = None;
        }
        self.open = Some(route.clone());
        self.opened_at = Some(now);
        self.opened_default.clone_from(&offer.default);
        self.last = Some(route);
        // A stream that kept dying is trusted again only once it survives (see `settle`).
        let probation = self.failure.as_ref().is_some_and(|f| f.offer.is_none());
        if probation {
            return Opened { switched, recovered: false };
        }
        self.failure = None;
        Opened { switched, recovered: std::mem::take(&mut self.reported) }
    }

    /// The side's stream was closed to open another (the user's choice closes first).
    pub fn closed(&mut self) {
        self.open = None;
    }

    /// The chosen device did not open (busy, still negotiating a Bluetooth profile, ...): the
    /// default stands in, and the chosen device is tried again later. Not a failure to report.
    pub fn bench(&mut self, name: &str, now: Instant) {
        let backoff = match &self.bench {
            Some(b) if b.name == name => (b.backoff * 2).min(RETRY_MAX),
            _ => RETRY_FIRST,
        };
        self.bench = Some(Bench { name: name.to_owned(), until: now + backoff, backoff });
        // Nothing runs: the stand-in is announced. A stream still running just stays.
        if self.open.is_none() {
            self.last = Some(Route::Named(name.to_owned()));
        }
    }

    /// A move made by itself did not open, while the stream it would replace still plays: that
    /// one stays, and the move is tried again later. Not a failure to report.
    pub fn stayed(&mut self, offer: &Offer, now: Instant) {
        let backoff = match &self.failure {
            Some(f) if f.offer.as_ref() == Some(offer) => (f.backoff * 2).min(RETRY_MAX),
            _ => RETRY_FIRST,
        };
        self.failure = Some(Failure { offer: Some(offer.clone()), retry_at: now + backoff, backoff });
    }

    /// A stream reopened after it kept dying has stayed up. True if a reported failure is over.
    pub fn settle(&mut self, now: Instant) -> bool {
        let probation = self.open.is_some() && self.failure.as_ref().is_some_and(|f| f.offer.is_none());
        if !probation || self.opened_at.is_some_and(|at| now.saturating_duration_since(at) < FLAPPING) {
            return false;
        }
        self.failure = None;
        std::mem::take(&mut self.reported)
    }

    /// Opening failed, or there was nothing to open. True if the user should be told.
    pub fn failed(&mut self, offer: &Offer, now: Instant, denied: bool) -> bool {
        self.open = None;
        let newly_blocked = denied && !self.blocked;
        self.blocked |= denied;
        let backoff = match &self.failure {
            Some(f) if f.offer.as_ref() == Some(offer) => (f.backoff * 2).min(RETRY_MAX),
            _ => RETRY_FIRST,
        };
        self.failure = Some(Failure { offer: Some(offer.clone()), retry_at: now + backoff, backoff });
        !std::mem::replace(&mut self.reported, true) || newly_blocked
    }

    /// The open stream stopped working; `offer` is what the system offers now. True if the user
    /// should be told: it keeps dying right after opening, so it is held back for a while.
    pub fn lost(&mut self, offer: &Offer, now: Instant) -> bool {
        let open = self.open.take();
        // A device that left, or a default that changed (Windows invalidates default streams),
        // explains the loss: that is no sign of a broken device.
        let moved = self.opened_default != offer.default
            || matches!(&open, Some(Route::Named(name)) if !offer.names.contains(name));
        let flapping = !moved && self.opened_at.is_some_and(|at| now.saturating_duration_since(at) < FLAPPING);
        if !flapping {
            self.failure = None;
            return false;
        }
        let backoff = self.failure.as_ref().map_or(RETRY_FIRST, |f| (f.backoff * 2).min(RETRY_MAX));
        self.failure = Some(Failure { offer: None, retry_at: now + backoff, backoff });
        !std::mem::replace(&mut self.reported, true)
    }

    /// Microphone access was denied while the stream ran. True if the user should be told.
    pub fn denied(&mut self) -> bool {
        self.open = None;
        self.reported = true;
        !std::mem::replace(&mut self.blocked, true)
    }

    /// Whether something should be checked again without a notification: the side is
    /// closed, on the default while the chosen device is away, or on probation.
    pub fn waiting(&self) -> bool {
        !self.blocked
            && (self.open.is_none()
                || self.failure.is_some()
                || (self.wanted.is_some() && matches!(self.open, Some(Route::Default(_)))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(default: Option<&str>, names: &[&str]) -> Offer {
        Offer { default: default.map(str::to_owned), names: names.iter().map(|n| (*n).to_owned()).collect() }
    }

    fn default(name: &str) -> Route {
        Route::Default(Some(name.to_owned()))
    }

    fn named(name: &str) -> Route {
        Route::Named(name.to_owned())
    }

    fn open(follow: &mut Follow, offer: &Offer, now: Instant) -> Opened {
        let Plan::Open(route) = follow.plan(offer, now) else { panic!("expected an open for {offer:?}") };
        follow.opened(route, offer, now)
    }

    #[test]
    fn the_system_default_is_followed_quietly() {
        let now = Instant::now();
        let mut follow = Follow::new(None, false);
        let mac = offer(Some("MacBook"), &["MacBook"]);
        assert_eq!(follow.plan(&mac, now), Plan::Open(default("MacBook")));
        assert_eq!(follow.opened(default("MacBook"), &mac, now), Opened::default());
        assert_eq!(follow.plan(&offer(Some("MacBook"), &["MacBook", "Headset"]), now), Plan::Keep);
        let headset = offer(Some("Headset"), &["MacBook", "Headset"]);
        assert_eq!(follow.plan(&headset, now), Plan::Open(default("Headset")));
        assert_eq!(follow.opened(default("Headset"), &headset, now), Opened::default());
        assert!(!follow.waiting());
    }

    #[test]
    fn a_rerouting_default_stream_is_kept() {
        let now = Instant::now();
        let mut follow = Follow::new(None, true);
        open(&mut follow, &offer(Some("Speakers"), &["Speakers"]), now);
        assert_eq!(follow.plan(&offer(Some("AirPods"), &["Speakers", "AirPods"]), now), Plan::Keep);
        // The default disappearing altogether is the stream's own error to report.
        assert_eq!(follow.plan(&offer(None, &[]), now), Plan::Keep);
    }

    #[test]
    fn a_chosen_device_that_disappears_falls_back_and_returns() {
        let mut now = Instant::now();
        let both = offer(Some("MacBook"), &["MacBook", "Headset"]);
        let mut follow = Follow::new(Some("Headset".into()), false);
        assert_eq!(open(&mut follow, &both, now), Opened::default());
        assert!(!follow.waiting());

        now += Duration::from_secs(60);
        let unplugged = offer(Some("MacBook"), &["MacBook"]);
        assert!(!follow.lost(&unplugged, now));
        assert_eq!(follow.plan(&unplugged, now), Plan::Open(default("MacBook")));
        assert_eq!(follow.opened(default("MacBook"), &unplugged, now).switched.as_deref(), Some("MacBook"));
        assert!(follow.waiting(), "watches for the headset to come back");
        assert_eq!(follow.plan(&unplugged, now), Plan::Keep);

        assert_eq!(follow.plan(&both, now), Plan::Open(named("Headset")));
        assert_eq!(follow.opened(named("Headset"), &both, now).switched.as_deref(), Some("Headset"));
        assert!(!follow.waiting());
    }

    #[test]
    fn a_disappearance_noticed_through_the_device_list_falls_back_too() {
        let now = Instant::now();
        let mut follow = Follow::new(Some("Headset".into()), false);
        open(&mut follow, &offer(Some("MacBook"), &["MacBook", "Headset"]), now);
        assert_eq!(follow.plan(&offer(Some("MacBook"), &["MacBook"]), now), Plan::Open(default("MacBook")));
    }

    #[test]
    fn a_chosen_device_missing_at_the_start_falls_back_quietly() {
        let now = Instant::now();
        let mut follow = Follow::new(Some("Headset".into()), false);
        assert_eq!(open(&mut follow, &offer(Some("MacBook"), &["MacBook"]), now), Opened::default());
        // Its return is still announced: audio moves without the user doing anything.
        let back = open(&mut follow, &offer(Some("MacBook"), &["MacBook", "Headset"]), now);
        assert_eq!(back.switched.as_deref(), Some("Headset"));
    }

    #[test]
    fn the_users_own_choice_is_not_announced() {
        let now = Instant::now();
        let both = offer(Some("MacBook"), &["MacBook", "Headset"]);
        let mut follow = Follow::new(None, false);
        open(&mut follow, &both, now);
        assert!(!follow.want(None));
        assert!(follow.want(Some("Headset".into())));
        assert_eq!(open(&mut follow, &both, now), Opened::default());
        assert!(follow.want(None));
        assert_eq!(open(&mut follow, &both, now), Opened::default());
        assert!(follow.want(Some("Headset".into())));
        assert_eq!(open(&mut follow, &both, now), Opened::default());
        // The chosen device is the default already open: relabelled, not reopened.
        assert!(follow.want(None));
        assert_eq!(open(&mut follow, &both, now), Opened::default());
        assert!(follow.want(Some("MacBook".into())));
        assert_eq!(follow.plan(&both, now), Plan::Keep);
        assert!(!follow.waiting(), "on the chosen device");
        // A named stream does not follow the default: back to "System default" reopens.
        assert!(follow.want(None));
        assert_eq!(follow.plan(&both, now), Plan::Open(default("MacBook")));
    }

    #[test]
    fn a_rerouting_default_stream_is_not_relabelled_as_the_chosen_device() {
        let now = Instant::now();
        let both = offer(Some("Speakers"), &["Speakers", "Headphones"]);
        let mut follow = Follow::new(None, true);
        open(&mut follow, &both, now);
        assert!(follow.want(Some("Speakers".into())));
        assert_eq!(follow.plan(&both, now), Plan::Open(named("Speakers")));
    }

    #[test]
    fn a_failing_device_is_reported_once_and_retried_with_backoff() {
        let start = Instant::now();
        let mics = offer(Some("MacBook"), &["MacBook"]);
        let mut follow = Follow::new(None, false);
        assert_eq!(follow.plan(&mics, start), Plan::Open(default("MacBook")));
        assert!(follow.failed(&mics, start, false));
        assert!(follow.waiting());
        assert_eq!(follow.plan(&mics, start + Duration::from_millis(500)), Plan::Keep);
        let again = start + RETRY_FIRST;
        assert_eq!(follow.plan(&mics, again), Plan::Open(default("MacBook")));
        assert!(!follow.failed(&mics, again, false), "told once");
        assert_eq!(follow.plan(&mics, again + RETRY_FIRST), Plan::Keep, "the wait doubles");
        // A new device is tried at once.
        let plugged = offer(Some("Headset"), &["MacBook", "Headset"]);
        assert_eq!(follow.plan(&plugged, again), Plan::Open(default("Headset")));
        assert_eq!(follow.opened(default("Headset"), &plugged, again), Opened { switched: None, recovered: true });
    }

    #[test]
    fn no_device_is_reported_once_until_one_appears() {
        let now = Instant::now();
        let none = offer(None, &[]);
        let mut follow = Follow::new(None, false);
        assert_eq!(follow.plan(&none, now), Plan::Missing);
        assert!(follow.failed(&none, now, false));
        assert_eq!(follow.plan(&none, now), Plan::Keep);
        assert_eq!(follow.plan(&none, now + RETRY_FIRST), Plan::Missing);
        assert!(!follow.failed(&none, now + RETRY_FIRST, false));
        let opened = open(&mut follow, &offer(Some("USB mic"), &["USB mic"]), now + RETRY_FIRST);
        assert!(opened.recovered);
    }

    #[test]
    fn a_stream_that_keeps_dying_is_held_back() {
        let now = Instant::now();
        let mics = offer(Some("MacBook"), &["MacBook"]);
        let mut follow = Follow::new(None, false);
        open(&mut follow, &mics, now);
        let soon = now + Duration::from_millis(100);
        assert!(follow.lost(&mics, soon), "told when it keeps dying");
        assert_eq!(follow.plan(&offer(Some("Headset"), &["Headset"]), soon), Plan::Keep, "held for any device");
        let later = soon + RETRY_FIRST;
        assert!(!open(&mut follow, &mics, later).recovered, "not before it stays up");
        assert!(!follow.lost(&mics, later + Duration::from_millis(100)), "told once");
        let last = later + Duration::from_millis(100) + RETRY_FIRST;
        assert_eq!(follow.plan(&mics, last), Plan::Keep, "the wait doubles");
        let last = last + RETRY_FIRST;
        open(&mut follow, &mics, last);
        assert!(follow.waiting());
        assert!(!follow.settle(last + Duration::from_millis(100)));
        assert!(follow.settle(last + FLAPPING), "recovered once it stays up");
        assert!(!follow.waiting());
    }

    #[test]
    fn denied_access_stops_retrying() {
        let now = Instant::now();
        let mics = offer(Some("MacBook"), &["MacBook"]);
        let mut follow = Follow::new(None, false);
        follow.plan(&mics, now);
        assert!(follow.failed(&mics, now, true));
        assert!(!follow.waiting());
        assert_eq!(follow.plan(&mics, now + RETRY_MAX), Plan::Keep);

        let mut running = Follow::new(None, false);
        open(&mut running, &mics, now);
        assert!(running.denied());
        assert!(!running.denied());
        assert_eq!(running.plan(&mics, now + RETRY_MAX), Plan::Keep);
    }

    #[test]
    fn a_default_change_shortly_after_opening_is_no_sign_of_a_broken_device() {
        // Windows reports a default change as the default stream's loss.
        let now = Instant::now();
        let mut follow = Follow::new(None, false);
        open(&mut follow, &offer(Some("Speakers"), &["Speakers"]), now);
        let soon = now + Duration::from_millis(300);
        let headphones = offer(Some("Headphones"), &["Speakers", "Headphones"]);
        assert!(!follow.lost(&headphones, soon), "not reported");
        assert_eq!(follow.plan(&headphones, soon), Plan::Open(default("Headphones")), "not held back");
        assert_eq!(follow.opened(default("Headphones"), &headphones, soon), Opened::default());
        // And again within the window: still a change, not instability.
        let back = offer(Some("Speakers"), &["Speakers", "Headphones"]);
        assert!(!follow.lost(&back, soon + Duration::from_millis(300)));
        assert_eq!(follow.plan(&back, soon + Duration::from_millis(300)), Plan::Open(default("Speakers")));
    }

    #[test]
    fn a_chosen_device_unplugged_shortly_after_opening_is_no_sign_of_a_broken_device() {
        let now = Instant::now();
        let mut follow = Follow::new(Some("Headset".into()), false);
        open(&mut follow, &offer(Some("MacBook"), &["MacBook", "Headset"]), now);
        let unplugged = offer(Some("MacBook"), &["MacBook"]);
        assert!(!follow.lost(&unplugged, now + Duration::from_millis(500)));
        assert_eq!(follow.plan(&unplugged, now + Duration::from_millis(500)), Plan::Open(default("MacBook")));
    }

    #[test]
    fn a_chosen_device_that_does_not_open_leaves_the_default_in_place() {
        let now = Instant::now();
        let both = offer(Some("MacBook"), &["MacBook", "Headset"]);
        let mut follow = Follow::new(Some("Headset".into()), false);
        open(&mut follow, &offer(Some("MacBook"), &["MacBook"]), now);
        // The headset comes back but does not open yet (Bluetooth still negotiating).
        assert_eq!(follow.plan(&both, now), Plan::Open(named("Headset")));
        follow.bench("Headset", now);
        assert_eq!(follow.plan(&both, now), Plan::Keep, "the default stream stays");
        assert_eq!(follow.plan(&both, now + RETRY_FIRST), Plan::Open(named("Headset")), "tried again");
        follow.bench("Headset", now + RETRY_FIRST);
        assert_eq!(follow.plan(&both, now + RETRY_FIRST * 2), Plan::Keep, "the wait doubles");
        let later = now + RETRY_FIRST * 3;
        let back = open(&mut follow, &both, later);
        assert_eq!(back.switched.as_deref(), Some("Headset"));
        assert!(!follow.waiting());
    }

    #[test]
    fn a_chosen_device_that_does_not_open_with_nothing_running_announces_the_default() {
        let now = Instant::now();
        let both = offer(Some("MacBook"), &["MacBook", "Headset"]);
        let mut follow = Follow::new(Some("Headset".into()), false);
        assert_eq!(follow.plan(&both, now), Plan::Open(named("Headset")));
        follow.bench("Headset", now);
        let stand_in = open(&mut follow, &both, now);
        assert_eq!(stand_in, Opened { switched: Some("MacBook".into()), recovered: false });
        assert!(follow.waiting());
    }

    #[test]
    fn a_new_default_that_does_not_open_leaves_the_working_stream() {
        let now = Instant::now();
        let mut follow = Follow::new(None, false);
        open(&mut follow, &offer(Some("MacBook"), &["MacBook"]), now);
        let usb = offer(Some("USB"), &["MacBook", "USB"]);
        assert_eq!(follow.plan(&usb, now), Plan::Open(default("USB")));
        follow.stayed(&usb, now);
        assert!(follow.waiting());
        assert_eq!(follow.plan(&usb, now), Plan::Keep);
        assert_eq!(follow.plan(&usb, now + RETRY_FIRST), Plan::Open(default("USB")));
        follow.stayed(&usb, now + RETRY_FIRST);
        assert_eq!(follow.plan(&usb, now + RETRY_FIRST * 2), Plan::Keep, "the wait doubles");
        assert_eq!(open(&mut follow, &usb, now + RETRY_FIRST * 3), Opened::default(), "nothing was reported");
        assert!(!follow.waiting());
    }
}
