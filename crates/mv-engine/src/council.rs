//! Council — several vendors on one question, at once, and the
//! disagreement kept.
//!
//! §3.6 states the value in one sentence that shapes every decision
//! here: "comparing different model perspectives beats picking the
//! single strongest model everywhere," and then the line that makes it
//! a build rather than a preference: **the disagreement is the
//! product**. This module does not vote, rank, or synthesize one answer
//! out of several — it seats a roster of *distinct vendors*, launches
//! each member through [`crate::runner`] (already merged), and hands
//! back every answer with its seat attached. Averaging them into one
//! answer would destroy the only thing a council produces that a single
//! Seer (the renamed Oracle) does not; §3.6 names the synthesizer as the
//! Nexus — the session already holding the context that made the
//! question worth asking.
//!
//! ## Shrink, never substitute
//!
//! §12.1, and the rule [`seat`] exists to enforce: if one vendor is
//! unavailable, run a smaller council rather than replacing it with a
//! second seat of a vendor already present — three Claude councillors
//! is one opinion rendered three times. [`MINIMUM_COUNCILLORS`] is the
//! floor below which a roster is not a council at all, just a Seer under
//! the wrong name.
//!
//! ## A narrow `Selector`, now bound to the real one
//!
//! This module depends on [`Selector`] — the one narrow capability a
//! council actually needs: "pick the next seat for this question,
//! excluding these vendors," and "is this already-seated seat still
//! usable." Both are exactly what `AccountSelector.select` and
//! `AccountSelector._availability` already give an ordinary dispatch in
//! Python's `council.py`. `crate::selector`'s chain-walking landed on
//! `main` after this module was first written against the narrow trait;
//! [`ChainSelector`] is the binding, delegating every call to
//! `selector::select` rather than re-deriving availability/eligibility
//! rules that could drift from it. [`Selector`] itself stays in place —
//! it is what keeps the roster/quorum logic above testable with a plain
//! fake, independent of the real selector's much larger `SelectParams`/
//! `BreakerQuery` surface.
//!
//! One gap remains: building each seat's brief/task directory (Python's
//! `DispatchPlanner.card_from_selection`) is the dispatch planner's job
//! (`crate::dispatch`), not council's, so [`convene`] takes one
//! already-prepared [`crate::runner::DispatchStore`] per active seat
//! rather than building one itself. `crate::dispatch` landed alongside
//! the selector but exposes planning for one ordinary dispatch, not a
//! "build N stores for a seated roster" entry point; wiring that up is
//! for whoever first needs `council::convene` for real.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use mv_config::roles::Role;

use crate::dispatch;
use crate::gateway::TaskStatus;
use crate::runner::{self, DispatchStore, Herdr, RunnerError, Timing};
use crate::seatstate;
use crate::selector;

/// Below this many distinct vendors, it is not a council — §12.1: "two
/// distinct vendors is a valid council; two seats of the same vendor is
/// not." One seat is a Seer, a perfectly good thing to dispatch under
/// its own name.
pub const MINIMUM_COUNCILLORS: usize = 2;

/// One account, already resolved to a vendor, selected for a council
/// seat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seat {
    pub vendor: String,
    pub account: String,
}

/// Why the selector's chain produced nothing more: a real refusal
/// (fenced, draining, over its own cap) or simply "nothing left to try."
/// Kept only to explain a roster that came out too small — the loop in
/// [`seat`] treats chain exhaustion as the normal way it ends.
pub type SelectError = String;

/// What a council needs from the real account selector (see the module
/// docs for why this stays narrow rather than depending on the
/// selector's concrete type, which has not landed in this checkout).
pub trait Selector {
    /// Select the next available seat for a council on `question`,
    /// excluding every vendor already in `exclude_vendors`. `question`
    /// lets a real implementation measure this exact question's
    /// rendered length against a candidate's `max_brief_chars`, the same
    /// way a single dispatch's brief would be checked; `None` is the
    /// "never blocks on length" default every caller but a council's
    /// own `plan` already gets.
    fn select(
        &mut self,
        question: Option<&str>,
        exclude_vendors: &HashSet<String>,
    ) -> Result<Seat, SelectError>;

    /// Re-check one already-seated seat's availability at convene time,
    /// after planning but before starting it — the same gap
    /// `recheck_seat_cap` closes for an ordinary dispatch. `Ok(())`
    /// means still usable; `Err` carries the reason it is not (fenced,
    /// draining, human-disabled, an unclaimed half-open pool, ...).
    fn recheck(&mut self, seat: &Seat) -> Result<(), String>;
}

/// Binds the real, landed `crate::selector` chain-walker to the narrow
/// [`Selector`] council needs. [`Selector::select`] re-runs
/// `selector::select` over the whole configured `chain` with the vendors
/// seated so far excluded — exactly the loop [`seat`] already drives —
/// and [`Selector::recheck`] re-runs it over that one seat alone,
/// excluding nothing, for a fresh verdict under today's breaker/quota/cap
/// state. `question`'s raw character length stands in for the rendered
/// brief length `max_brief_chars` is actually measured against (Python's
/// `council.py:235-250`): a real caller with the dispatch planner's brief
/// renderer in hand should measure that instead and call
/// `selector::select` itself for a closer eligibility check.
///
/// Every full [`selector::Selection`] made along the way — including its
/// half-open trial token — is kept and retrievable by account name via
/// [`ChainSelector::selection`]: whoever builds this seat's
/// `DispatchStore` needs that token; council itself has no reason to
/// carry it.
pub struct ChainSelector<'a> {
    chain: &'a [selector::Seat],
    project: &'a str,
    reviewer_role: &'a str,
    nexus_role: &'a str,
    floors: selector::QuotaFloors,
    policy: selector::FallbackPolicy,
    task_id: &'a str,
    breaker: &'a dyn selector::BreakerQuery,
    now: Instant,
    selections: HashMap<String, selector::Selection>,
}

impl<'a> ChainSelector<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chain: &'a [selector::Seat],
        project: &'a str,
        reviewer_role: &'a str,
        nexus_role: &'a str,
        floors: selector::QuotaFloors,
        policy: selector::FallbackPolicy,
        task_id: &'a str,
        breaker: &'a dyn selector::BreakerQuery,
        now: Instant,
    ) -> Self {
        Self {
            chain,
            project,
            reviewer_role,
            nexus_role,
            floors,
            policy,
            task_id,
            breaker,
            now,
            selections: HashMap::new(),
        }
    }

    /// The full selection made for `account` — including its half-open
    /// trial token — once seating has produced it.
    pub fn selection(&self, account: &str) -> Option<&selector::Selection> {
        self.selections.get(account)
    }
}

impl Selector for ChainSelector<'_> {
    fn select(
        &mut self,
        question: Option<&str>,
        exclude_vendors: &HashSet<String>,
    ) -> Result<Seat, SelectError> {
        let excluded: Vec<String> = exclude_vendors.iter().cloned().collect();
        let params = selector::SelectParams {
            role: "council",
            project: self.project,
            reviewer_role: self.reviewer_role,
            nexus_role: self.nexus_role,
            exclude_vendors: &excluded,
            task_len_chars: question.map(str::len).unwrap_or(0),
            floors: self.floors,
            policy: self.policy,
            task_id: self.task_id,
            trial_token: None,
        };
        let selection = selector::select(self.chain, &params, self.breaker, self.now)
            .map_err(|error| error.message)?;
        let seat = Seat {
            vendor: selection.seat.vendor.clone(),
            account: selection.seat.name.clone(),
        };
        self.selections.insert(seat.account.clone(), selection);
        Ok(seat)
    }

    fn recheck(&mut self, seat: &Seat) -> Result<(), String> {
        let Some(candidate) = self.chain.iter().find(|s| s.name == seat.account) else {
            return Err("seat is no longer in the configured chain".into());
        };
        // Critic finding critic-safety.md#2: a `trial_token: None` here
        // never matches the claim `select` minted for this exact seat
        // moments ago (`availability`'s `trial_token.is_some_and(...)`,
        // `selector.rs`), so the council would drop a half-open seat it
        // just seated. Thread the stored claim back in so the owning
        // seat's own trial still reads as usable here.
        let trial_token = self
            .selections
            .get(&seat.account)
            .and_then(|selection| selection.trial_token.as_deref());
        let params = selector::SelectParams {
            role: "council",
            project: self.project,
            reviewer_role: self.reviewer_role,
            nexus_role: self.nexus_role,
            exclude_vendors: &[],
            task_len_chars: 0,
            floors: self.floors,
            policy: self.policy,
            task_id: self.task_id,
            trial_token,
        };
        selector::select(
            std::slice::from_ref(candidate),
            &params,
            self.breaker,
            self.now,
        )
        .map(|_| ())
        .map_err(|error| error.message)
    }
}

/// One seat, its selection, and what came back.
#[derive(Debug, Clone)]
pub struct Councillor {
    pub seat: Seat,
    /// The specialist's own terminal status, once dispatch settled with
    /// a usable result. `None` until then.
    pub status: Option<TaskStatus>,
    /// Set when this seat failed while the others may have succeeded.
    pub error: String,
    /// Set when this seat was deliberately omitted after a convene-time
    /// recheck, rather than failing during dispatch.
    pub skipped: bool,
    /// Set when the agent settled without ever writing a terminal
    /// status file.
    pub unanswered: bool,
    /// Whether seating this councillor skipped at least one chain entry
    /// first — Python's `Selection.outcome` (`accounts.py:1546-1549`):
    /// `CHAIN_PRIMARY` only when nothing was skipped to reach this
    /// account, `CHAIN_FALLBACK` otherwise. Each later seat's own walk
    /// restarts from the top of the chain with every already-seated
    /// vendor excluded (`council.seat`'s "shrink, never substitute"), so
    /// those earlier vendors' chain entries are themselves skipped
    /// (`SkipCause.VENDOR_EXCLUDED`) before this seat's account is
    /// reached — in practice this is `false` for at most the first
    /// seated councillor and `true` for every one after it. `false` by
    /// default; a caller that built the roster through
    /// [`ChainSelector`] sets it from that selector's own
    /// retained [`selector::Selection`] once seating finishes.
    pub is_fallback: bool,
    /// The chain-walk reason this account was chosen — Python's own
    /// `_dispatch_reason`-equivalent text ("first entry in ... chain for
    /// ...", or "entry N ...; skipped ..." once [`is_fallback`](Self::is_fallback)
    /// is set), carried onto the routing log's `reason=` field the same way
    /// an ordinary dispatch's does. Empty by default; populated the same
    /// way [`is_fallback`](Self::is_fallback) is.
    pub reason: String,
}

impl Councillor {
    fn new(seat: Seat) -> Self {
        Self {
            seat,
            status: None,
            error: String::new(),
            skipped: false,
            unanswered: false,
            is_fallback: false,
            reason: String::new(),
        }
    }
}

/// A seated council and its outcome. There is deliberately no vote,
/// ranking, reducer, or synthesized answer anywhere on this type — see
/// the module docs.
#[derive(Debug, Clone, Default)]
pub struct Council {
    pub question: String,
    pub councillors: Vec<Councillor>,
}

impl Council {
    /// Distinct vendors represented in the roster, sorted for display.
    pub fn vendors(&self) -> Vec<String> {
        let mut vendors: Vec<String> = self
            .councillors
            .iter()
            .map(|c| c.seat.vendor.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        vendors.sort();
        vendors
    }

    /// Seats whose dispatch produced a usable terminal status.
    pub fn answered(&self) -> impl Iterator<Item = &Councillor> {
        self.councillors
            .iter()
            .filter(|c| c.status.is_some() && c.error.is_empty())
    }

    /// Seats that failed during dispatch, excluding seats that were
    /// deliberately skipped or that merely settled without an answer —
    /// those are reported separately rather than folded into "failed."
    pub fn failed(&self) -> impl Iterator<Item = &Councillor> {
        self.councillors
            .iter()
            .filter(|c| !c.error.is_empty() && !c.skipped && !c.unanswered)
    }

    /// Whether enough distinct vendors actually **answered**. Seating a
    /// council and having it answer are different facts: two seats that
    /// both died leave a council that was validly convened and produced
    /// nothing, and reporting that as a council would be the exact
    /// failure §3.6 exists to prevent.
    pub fn is_quorate(&self) -> bool {
        self.answered()
            .map(|c| c.seat.vendor.as_str())
            .collect::<HashSet<_>>()
            .len()
            >= MINIMUM_COUNCILLORS
    }
}

/// A council that cannot be seated (or stayed seated), with the reason a
/// human can act on.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CouncilTooSmallError(String);

impl CouncilTooSmallError {
    fn new(seated: &[Seat], reason: &str) -> Self {
        let mut vendors: Vec<&str> = seated.iter().map(|s| s.vendor.as_str()).collect();
        vendors.sort_unstable();
        vendors.dedup();
        let vendors = if vendors.is_empty() {
            "none".to_string()
        } else {
            vendors.join(", ")
        };
        let reason_clause = if reason.is_empty() {
            String::new()
        } else {
            format!("{reason} ")
        };
        CouncilTooSmallError(format!(
            "a council needs at least {MINIMUM_COUNCILLORS} distinct vendors and only \
             {} could be seated ({vendors}). {reason_clause}A {} is seated for \
             disagreement, so a seat is never filled with a second account of a \
             vendor already present — three seats of one vendor is one opinion \
             rendered three times. Either make another vendor available, or dispatch \
             {} and call it what it is.",
            seated.len(),
            Role::Council.display_name(),
            Role::Seer.display_name(),
        ))
    }
}

/// Resolve the roster: at most one available seat per vendor, in chain
/// order. Calls [`Selector::select`] repeatedly with the vendors already
/// seated excluded — that is what makes "shrink, never substitute" fall
/// out of the selector's own chain-walking rather than a second copy of
/// its availability and eligibility rules.
///
/// # Errors
/// [`CouncilTooSmallError`] if fewer than [`MINIMUM_COUNCILLORS`]
/// distinct vendors can be seated.
pub fn seat(
    selector: &mut impl Selector,
    question: &str,
) -> Result<Vec<Seat>, CouncilTooSmallError> {
    let mut seated: Vec<Seat> = Vec::new();
    let mut excluded: HashSet<String> = HashSet::new();
    let last_reason = loop {
        match selector.select(Some(question), &excluded) {
            Ok(seat) => {
                excluded.insert(seat.vendor.clone());
                seated.push(seat);
            }
            // Exhausting the chain is the normal way this loop ends —
            // every remaining entry is either unavailable or of a vendor
            // already seated. Python's own `council.py:269`: `str(exc).
            // replace("\n", " ")` — the reason is kept only to explain a
            // council that came out too small, folded onto `a council
            // needs at least ...`'s own single line.
            Err(reason) => break reason.replace('\n', " "),
        }
    };
    if seated.len() < MINIMUM_COUNCILLORS {
        return Err(CouncilTooSmallError::new(&seated, &last_reason));
    }
    Ok(seated)
}

/// Seat a council and record one councillor per seat. Dispatches
/// nothing — see the module docs for why building each seat's brief and
/// task directory is left to the dispatch planner rather than done here.
///
/// # Errors
/// [`CouncilTooSmallError`], via [`seat`].
pub fn plan(
    selector: &mut impl Selector,
    question: impl Into<String>,
) -> Result<Council, CouncilTooSmallError> {
    let question = question.into();
    let seated = seat(selector, &question)?;
    Ok(Council {
        question,
        councillors: seated.into_iter().map(Councillor::new).collect(),
    })
}

/// Check the whole council against capacity, once, rather than once per
/// seat — checking per seat would let N seats racing each see capacity
/// and all proceed, or let the first two fit and the third be refused,
/// leaving a council convened without the vendor that made it worth
/// convening. Delegates to [`dispatch::check_fleet_capacity`] (landed
/// alongside the selector), which covers both `capacity_or_raise` and
/// `project_slots_or_raise` from `council.py` in one call: the
/// fleet-wide `max_concurrent` cap, this project's own ceiling, and the
/// cross-project floor-borrowing check. `inputs.batch_count` is
/// overwritten with this council's own active-seat count — the same
/// whole-batch shape Python's two separate functions each insisted on.
/// Every other field (fleet/project open counts, floors, other
/// projects' gaps) is a Herdr/config-listing concern that belongs to the
/// caller, read under the same fleet lock the per-seat starts in
/// [`convene`] will use.
pub fn capacity_or_raise(
    council: &Council,
    mut inputs: dispatch::FleetCapacityInputs,
) -> Result<(), dispatch::DispatchError> {
    inputs.batch_count = council.councillors.iter().filter(|c| !c.skipped).count() as u32;
    dispatch::check_fleet_capacity(&inputs)
}

/// Skip seats that became unavailable after planning. `Selector::select`
/// runs before any fleet lock, so a planned seat can be fenced,
/// draining, or human-disabled before `convene` starts. A failed seat is
/// retained in the roster with `skipped = true` so the caller can report
/// the missing opinion; it is never started and never rebound to a
/// second account of its vendor.
///
/// # Errors
/// [`CouncilTooSmallError`] if skipping leaves fewer than
/// [`MINIMUM_COUNCILLORS`] distinct vendors still active.
pub fn recheck_seat_availability(
    council: &mut Council,
    selector: &mut impl Selector,
) -> Result<(), CouncilTooSmallError> {
    let mut skipped_notes: Vec<String> = Vec::new();
    for councillor in council.councillors.iter_mut().filter(|c| !c.skipped) {
        if let Err(reason) = selector.recheck(&councillor.seat) {
            councillor.skipped = true;
            councillor.error = format!("SKIPPED at convene time: {reason}");
            skipped_notes.push(format!("{} ({reason})", councillor.seat.account));
        }
    }
    too_small_after_skipping(
        council,
        &skipped_notes,
        "Seat(s) unavailable by convene time",
    )
}

/// Skip every councillor whose seat is at its open-agent cap right now.
/// `cap` is read from `mv_state`'s seatstate store (already landed,
/// [`crate::seatstate::get_cap`]); `open_counts` is the fleet's current
/// per-seat open-agent count, or `None` when that listing could not be
/// read — the same fail-closed posture an ordinary dispatch's
/// `recheck_seat_cap` already takes: an unreadable listing treats every
/// capped seat as full rather than guessing it has room. A full seat is
/// skipped, never rebound to a second account of its vendor.
///
/// `open_counts` is keyed by account **name** (`councillor.seat.account`),
/// matching Python `recheck_seat_caps` (`council.py:508`
/// `counts.get(councillor.account, 0)`) against `open_agent_counts`
/// account-name keys. A map keyed by seat short code would read every
/// cap as 0 open (critic-grok3 row 5).
///
/// # Errors
/// [`CouncilTooSmallError`] if skipping leaves fewer than
/// [`MINIMUM_COUNCILLORS`] distinct vendors still active.
pub fn recheck_seat_caps(
    council: &mut Council,
    config_dir: &Path,
    open_counts: Option<&HashMap<String, u32>>,
) -> Result<(), CouncilTooSmallError> {
    let mut full: Vec<String> = Vec::new();
    for councillor in council.councillors.iter_mut().filter(|c| !c.skipped) {
        let Some(cap) = seatstate::get_cap(config_dir, &councillor.seat.account) else {
            continue;
        };
        let open_now = match open_counts {
            None => cap,
            Some(counts) => counts.get(&councillor.seat.account).copied().unwrap_or(0),
        };
        if open_now < cap {
            continue;
        }
        councillor.skipped = true;
        councillor.error =
            format!("SKIPPED at convene time: at its open-agent cap ({open_now}/{cap} open)");
        full.push(format!(
            "{} ({open_now}/{cap} open)",
            councillor.seat.account
        ));
    }
    too_small_after_skipping(
        council,
        &full,
        "Seat(s) at their open-agent cap by convene time",
    )
}

fn too_small_after_skipping(
    council: &Council,
    notes: &[String],
    heading: &str,
) -> Result<(), CouncilTooSmallError> {
    if notes.is_empty() {
        return Ok(());
    }
    let active: Vec<Seat> = council
        .councillors
        .iter()
        .filter(|c| !c.skipped)
        .map(|c| c.seat.clone())
        .collect();
    let distinct_vendors = active
        .iter()
        .map(|s| s.vendor.as_str())
        .collect::<HashSet<_>>()
        .len();
    if distinct_vendors < MINIMUM_COUNCILLORS {
        return Err(CouncilTooSmallError::new(
            &active,
            &format!("{heading}: {}.", notes.join(", ")),
        ));
    }
    Ok(())
}

/// The exact CLI-facing text for a convened council: no vote, no
/// ranking, nothing synthesized — only how many councillors answered,
/// across how many vendors, and whether that clears the quorum floor.
/// Kept here, not in a clap command, because it is a judgment about the
/// council's own data rather than argument parsing.
pub fn quorum_report(council: &Council) -> String {
    let answered: Vec<&Councillor> = council.answered().collect();
    let vendors = answered
        .iter()
        .map(|c| c.seat.vendor.as_str())
        .collect::<HashSet<_>>()
        .len();
    if council.is_quorate() {
        format!(
            "\n{} councillors answered across {vendors} vendors. Nothing was \
             synthesized: read them yourself and keep where they disagree — that is \
             what a council produces that one agent cannot.",
            answered.len()
        )
    } else {
        format!(
            "\nOnly {} councillor(s) answered, across {vendors} vendor(s). That is not \
             a council — read the results as individual opinions, or run it again \
             when the missing vendors are back.",
            answered.len()
        )
    }
}

/// What became of one councillor's dispatch. Kept separate from mutating
/// [`Councillor`] directly so the common-case mapping is one pure
/// function ([`apply_outcome`]), testable without a real Herdr
/// round-trip.
enum SeatOutcome {
    Answered(TaskStatus),
    Unanswered(String),
    Failed(String),
}

fn apply_outcome(councillor: &mut Councillor, outcome: SeatOutcome) {
    match outcome {
        SeatOutcome::Answered(status) => councillor.status = Some(status),
        SeatOutcome::Unanswered(error) => {
            councillor.unanswered = true;
            councillor.error = error;
        }
        SeatOutcome::Failed(error) => councillor.error = error,
    }
}

fn error_kind(error: &RunnerError) -> &'static str {
    match error {
        RunnerError::Refused(_) => "Refused",
        RunnerError::Herdr(_) => "Herdr",
        RunnerError::Io(_) => "Io",
        RunnerError::NeedsAttention { .. } => "NeedsAttention",
    }
}

/// Start every active (not skipped) councillor, then collect what each
/// says. One seat failing never cancels the others: a council of three
/// where one vendor dies still has two vendors disagreeing, which is the
/// product; the failure is recorded on that seat and reported alongside
/// (§3.6).
///
/// Two phases, not one, and that split is scout2-gaps.md #23
/// (`council.py:581-705`'s `_fleet_lock` span): every seat is *started*
/// — [`runner::start_dispatch`] through pane creation, until the agent
/// is countable — on this one thread, under one fleet lock taken once
/// via the first store. Inner [`runner::dispatch`] must not re-enter
/// that lock (Python `DispatchRunner(config=None)`): a reentrant
/// `held: None` guard would leave this outer acquisition held through
/// `setup_lane` / `wait_ready` / `deliver_prompt` (critic-grok3 row 4).
/// Each seat's start therefore uses [`runner::FleetLock::AlreadyHeld`].
/// The lock is dropped the moment every seat has started, *before* any
/// councillor is prompted (`council.py:701-702`). Only *then* does the
/// prompt phase run, then every started seat's wait for an actual
/// answer, concurrently, via `std::thread::scope`.
///
/// Each councillor's `S: DispatchStore` must already be bound to its own
/// brief/task directory/worktree (the dispatch planner's job, not this
/// module's — see the module docs); `stores` and `herdrs` are one per
/// currently-active seat, in the same order
/// `council.councillors.iter().filter(|c| !c.skipped)` yields. A real
/// caller typically passes one [`crate::runner::ProcessHerdr`] per seat
/// — cheap to construct, and every seat still reaches the one real
/// Herdr fleet through its own call.
///
/// # Panics
/// If `stores` or `herdrs` does not have exactly one entry per active
/// seat — a mismatch here is a caller bug, not a council outcome.
pub fn convene<S, H>(
    council: &mut Council,
    stores: Vec<S>,
    herdrs: Vec<H>,
    current_pane: &str,
    timing: &Timing,
    timeout: Duration,
) where
    S: DispatchStore + Send,
    H: Herdr + Send,
{
    let result: Result<(), std::convert::Infallible> = convene_with_admission(
        council,
        stores,
        herdrs,
        current_pane,
        timing,
        timeout,
        |_| Ok(()),
    );
    match result {
        Ok(()) => {}
        Err(never) => match never {},
    }
}

/// [`convene`], plus an `admit` callback that runs under the same fleet
/// lock, before any seat starts — Python `council.py:650-657`:
/// `recheck_seat_caps`, `recheck_seat_availability`, `capacity_or_raise`
/// (and `project_slots_or_raise`, folded into `capacity_or_raise` here).
/// `admit` may skip seats; leftover stores for those seats are dropped
/// and never started. An `Err` from `admit` starts nobody and is
/// returned to the caller.
///
/// # Panics
/// If `stores` or `herdrs` does not have exactly one entry per seat that
/// was active when this function was called.
pub fn convene_with_admission<S, H, E>(
    council: &mut Council,
    mut stores: Vec<S>,
    herdrs: Vec<H>,
    current_pane: &str,
    timing: &Timing,
    timeout: Duration,
    admit: impl FnOnce(&mut Council) -> Result<(), E>,
) -> Result<(), E>
where
    S: DispatchStore + Send,
    H: Herdr + Send,
{
    let store_for: Vec<usize> = council
        .councillors
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.skipped)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(store_for.len(), stores.len(), "one store per active seat");
    assert_eq!(store_for.len(), herdrs.len(), "one herdr per active seat");

    let (mut stores, mut herdrs, started) = {
        let fleet_lock = stores.first_mut().map(DispatchStore::lock_fleet);
        match fleet_lock {
            Some(Err(error)) => {
                let message = error.to_string();
                let started = stores
                    .iter()
                    .map(|_| Err(RunnerError::Refused(format!("fleet lock: {message}"))))
                    .collect();
                (stores, herdrs, started)
            }
            held => {
                let _held = held;
                admit(council)?;
                let mut kept_stores = Vec::new();
                let mut kept_herdrs = Vec::new();
                for ((idx, store), herdr) in store_for.into_iter().zip(stores).zip(herdrs) {
                    if council.councillors[idx].skipped {
                        continue;
                    }
                    kept_stores.push(store);
                    kept_herdrs.push(herdr);
                }
                let started: Vec<Result<runner::StartedDispatch, RunnerError>> = kept_stores
                    .iter_mut()
                    .zip(kept_herdrs.iter_mut())
                    .map(|(store, herdr)| {
                        runner::start_dispatch(
                            store,
                            herdr,
                            current_pane,
                            timing,
                            runner::FleetLock::AlreadyHeld,
                        )
                    })
                    .collect();
                (kept_stores, kept_herdrs, started)
            }
        }
    };

    let started: Vec<Result<runner::Dispatched, RunnerError>> = started
        .into_iter()
        .zip(stores.iter_mut())
        .zip(herdrs.iter_mut())
        .map(|((started, store), herdr)| match started {
            Ok(started) => runner::prompt_dispatch(store, herdr, started, timing),
            Err(error) => Err(error),
        })
        .collect();

    let active: Vec<&mut Councillor> = council
        .councillors
        .iter_mut()
        .filter(|c| !c.skipped)
        .collect();
    assert_eq!(
        active.len(),
        started.len(),
        "one start result per still-active seat"
    );

    std::thread::scope(|scope| {
        for ((councillor, result), herdr) in active.into_iter().zip(started).zip(herdrs.iter_mut())
        {
            scope.spawn(move || {
                let outcome = match result {
                    Ok(dispatched) => {
                        match runner::wait_for_result(herdr, &dispatched, timeout, timing.poll) {
                            Ok((Some(status), _)) => {
                                // Python `runner.py:2906-2907`:
                                // `close_pane_when_done` defaults true and
                                // `council.py`'s own `DispatchRunner().
                                // dispatch(...)` never turns it off, so each
                                // councillor's pane is closed (and its row
                                // tidied) the moment it settles DONE, not
                                // left open until a later sweep.
                                if status == TaskStatus::Done {
                                    runner::teardown(herdr, &dispatched.pane, current_pane);
                                }
                                SeatOutcome::Answered(status)
                            }
                            Ok((None, _)) | Err(_) => SeatOutcome::Unanswered(
                                "MISSING ANSWER — agent settled without writing a usable status file"
                                    .into(),
                            ),
                        }
                    }
                    Err(error) => SeatOutcome::Failed(format!("{}: {error}", error_kind(&error))),
                };
                apply_outcome(councillor, outcome);
            });
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, OnceLock};
    use std::time::SystemTime;

    use mv_herdr::argv::{self, HerdrCommand};
    use mv_herdr::envelope::CommandResult;
    use serde_json::{json, Value};

    // --- Fakes for roster/quorum logic -------------------------------

    /// A chain of seats, consumed in order, that models
    /// `AccountSelector.select`'s shape closely enough for roster tests:
    /// skip anything already excluded, skip anything marked unavailable,
    /// otherwise hand out the next entry and never offer it again.
    struct FakeSelector {
        chain: Vec<(Seat, bool)>,
    }

    impl FakeSelector {
        fn new(chain: &[(&str, &str, bool)]) -> Self {
            Self {
                chain: chain
                    .iter()
                    .map(|(account, vendor, available)| {
                        (
                            Seat {
                                vendor: (*vendor).to_string(),
                                account: (*account).to_string(),
                            },
                            *available,
                        )
                    })
                    .collect(),
            }
        }
    }

    impl Selector for FakeSelector {
        fn select(
            &mut self,
            _question: Option<&str>,
            exclude_vendors: &HashSet<String>,
        ) -> Result<Seat, SelectError> {
            let index = self.chain.iter().position(|(seat, available)| {
                *available && !exclude_vendors.contains(&seat.vendor)
            });
            match index {
                Some(index) => Ok(self.chain.remove(index).0),
                None => Err("chain exhausted".into()),
            }
        }

        fn recheck(&mut self, seat: &Seat) -> Result<(), String> {
            if self
                .chain
                .iter()
                .any(|(candidate, available)| candidate.account == seat.account && !available)
            {
                return Err("unavailable".into());
            }
            Ok(())
        }
    }

    fn seat_triplet<'a>(account: &'a str, vendor: &'a str) -> (&'a str, &'a str, bool) {
        (account, vendor, true)
    }

    #[test]
    fn a_council_seats_one_account_per_vendor() {
        let mut selector = FakeSelector::new(&[
            seat_triplet("anthropic", "claude"),
            seat_triplet("openai", "chatgpt"),
            seat_triplet("xai", "grok"),
        ]);

        let seated = seat(&mut selector, "question").unwrap();

        assert_eq!(
            seated
                .iter()
                .map(|s| s.account.as_str())
                .collect::<Vec<_>>(),
            ["anthropic", "openai", "xai"]
        );
        let mut vendors: Vec<_> = seated.iter().map(|s| s.vendor.as_str()).collect();
        vendors.sort_unstable();
        assert_eq!(vendors, ["chatgpt", "claude", "grok"]);
    }

    #[test]
    fn a_second_seat_of_a_seated_vendor_is_never_taken() {
        let mut selector = FakeSelector::new(&[
            seat_triplet("claude-a", "claude"),
            seat_triplet("claude-b", "claude"),
            seat_triplet("xai", "grok"),
        ]);

        let seated = seat(&mut selector, "question").unwrap();

        assert_eq!(seated.len(), 2);
        let vendors: HashSet<_> = seated.iter().map(|s| s.vendor.as_str()).collect();
        assert_eq!(vendors, HashSet::from(["claude", "grok"]));
        assert!(!seated.iter().any(|s| s.account == "claude-b"));
    }

    #[test]
    fn an_unavailable_vendor_shrinks_the_council_rather_than_substituting() {
        // With grok down, an ordinary chain would fall through to the next
        // entry (another claude). A council must not: shrink to two
        // vendors instead of reporting three seats from one vendor pair.
        let mut selector = FakeSelector::new(&[
            ("claude-a", "claude", true),
            ("openai", "chatgpt", true),
            ("xai", "grok", false),
            ("claude-b", "claude", true),
        ]);

        let seated = seat(&mut selector, "question").unwrap();

        let vendors: HashSet<_> = seated.iter().map(|s| s.vendor.as_str()).collect();
        assert_eq!(vendors, HashSet::from(["claude", "chatgpt"]));
        assert!(!seated.iter().any(|s| s.account == "claude-b"));
    }

    #[test]
    fn one_vendor_is_refused_because_it_is_an_oracle() {
        let mut selector = FakeSelector::new(&[
            seat_triplet("claude-a", "claude"),
            seat_triplet("claude-b", "claude"),
        ]);

        let error = seat(&mut selector, "question").unwrap_err();

        assert!(error.to_string().contains("at least 2 distinct vendors"));
        assert!(error.to_string().contains(Role::Seer.display_name()));
    }

    #[test]
    fn two_vendors_is_a_valid_council() {
        let mut selector = FakeSelector::new(&[
            seat_triplet("claude-a", "claude"),
            seat_triplet("openai", "chatgpt"),
        ]);

        assert_eq!(seat(&mut selector, "question").unwrap().len(), 2);
    }

    #[test]
    fn plan_builds_one_councillor_per_seated_vendor_and_dispatches_nothing() {
        let mut selector = FakeSelector::new(&[
            seat_triplet("claude-a", "claude"),
            seat_triplet("openai", "chatgpt"),
        ]);

        let council = plan(&mut selector, "what should we do?").unwrap();

        assert_eq!(council.question, "what should we do?");
        assert_eq!(council.councillors.len(), 2);
        assert!(council
            .councillors
            .iter()
            .all(|c| c.status.is_none() && !c.skipped));
    }

    // --- Quorum / vendors / answered / failed -------------------------

    fn councillor(vendor: &str, account: &str) -> Councillor {
        Councillor::new(Seat {
            vendor: vendor.into(),
            account: account.into(),
        })
    }

    #[test]
    fn a_council_where_only_one_vendor_answers_is_not_quorate() {
        let mut council = Council {
            question: "q".into(),
            councillors: vec![
                councillor("claude", "anthropic"),
                councillor("chatgpt", "openai"),
            ],
        };
        council.councillors[0].status = Some(TaskStatus::Done);
        council.councillors[1].error = "DispatchError: timed out".into();

        assert_eq!(council.answered().count(), 1);
        assert_eq!(council.failed().count(), 1);
        assert!(!council.is_quorate());
    }

    #[test]
    fn two_distinct_vendors_answering_is_quorate() {
        let mut council = Council {
            question: "q".into(),
            councillors: vec![
                councillor("claude", "anthropic"),
                councillor("chatgpt", "openai"),
            ],
        };
        council.councillors[0].status = Some(TaskStatus::Done);
        council.councillors[1].status = Some(TaskStatus::Blocked);

        assert!(council.is_quorate());
        assert_eq!(
            council.vendors(),
            vec!["chatgpt".to_string(), "claude".to_string()]
        );
    }

    #[test]
    fn a_blocked_seat_without_status_is_missing_not_an_answer() {
        let mut c = councillor("grok", "xai");
        apply_outcome(
            &mut c,
            SeatOutcome::Unanswered(
                "MISSING ANSWER — agent settled without writing a usable status file".into(),
            ),
        );

        assert!(c.unanswered);
        assert!(c.status.is_none());
        assert!(!c.error.is_empty());
        // Unanswered is reported separately from "failed."
        let council = Council {
            question: "q".into(),
            councillors: vec![c],
        };
        assert_eq!(council.failed().count(), 0);
        assert_eq!(council.answered().count(), 0);
    }

    #[test]
    fn a_skipped_seat_is_neither_answered_nor_failed() {
        let mut c = councillor("grok", "xai");
        c.skipped = true;
        c.error = "SKIPPED at convene time: fenced".into();
        let council = Council {
            question: "q".into(),
            councillors: vec![c],
        };
        assert_eq!(council.failed().count(), 0);
        assert_eq!(council.answered().count(), 0);
    }

    // --- capacity / caps / availability rechecks -----------------------

    fn two_seat_council() -> Council {
        Council {
            question: "q".into(),
            councillors: vec![
                councillor("claude", "anthropic"),
                councillor("chatgpt", "openai"),
            ],
        }
    }

    fn cap_inputs(max_concurrent: u32, fleet_open: u32) -> dispatch::FleetCapacityInputs {
        dispatch::FleetCapacityInputs {
            max_concurrent,
            fleet_open,
            project_open: 0,
            project_min_slots: 0,
            project_max_slots: None,
            other_floor_gaps: Vec::new(),
            batch_count: 0, // overwritten by `capacity_or_raise`
        }
    }

    #[test]
    fn the_cap_is_checked_for_the_whole_council_at_once() {
        let council = two_seat_council();
        assert!(capacity_or_raise(&council, cap_inputs(9, 7)).is_ok());
        let error = capacity_or_raise(&council, cap_inputs(9, 8)).unwrap_err();
        assert!(matches!(
            error,
            dispatch::DispatchError::FleetCapExceeded { running: 8, cap: 9 }
        ));
        assert!(error.to_string().contains("max_concurrent"));
    }

    #[test]
    fn capacity_or_raise_does_not_count_an_already_skipped_seat() {
        let mut council = two_seat_council();
        council.councillors[1].skipped = true;
        // Only one active seat now, so the same fleet_open/cap that would
        // have refused two seats no longer does.
        assert!(capacity_or_raise(&council, cap_inputs(9, 8)).is_ok());
    }

    #[test]
    fn capacity_or_raise_also_catches_the_projects_own_ceiling() {
        // `dispatch::check_fleet_capacity` folds in `project_slots_or_raise`
        // from `council.py` — a project ceiling below `max_concurrent`
        // refuses even when the fleet as a whole has room.
        let council = two_seat_council();
        let mut inputs = cap_inputs(9, 0);
        inputs.project_open = 0;
        inputs.project_max_slots = Some(1);
        let error = capacity_or_raise(&council, inputs).unwrap_err();
        assert!(matches!(
            error,
            dispatch::DispatchError::ProjectCeilingExceeded {
                open: 0,
                ceiling: 1
            }
        ));
    }

    #[test]
    fn recheck_seat_caps_leaves_an_uncapped_council_alone_without_asking_herdr() {
        let mut council = two_seat_council();
        let dir = tempfile::tempdir().unwrap();
        // No seatstate entries written: every seat reads as uncapped.
        recheck_seat_caps(&mut council, dir.path(), None).unwrap();
        assert!(council.councillors.iter().all(|c| !c.skipped));
    }

    #[test]
    fn a_capped_seat_shrinks_a_larger_council_rather_than_being_started() {
        let mut council = Council {
            question: "q".into(),
            councillors: vec![
                councillor("claude", "anthropic"),
                councillor("chatgpt", "openai"),
                councillor("grok", "xai"),
            ],
        };
        let dir = tempfile::tempdir().unwrap();
        mv_state::seatstate_store::save(
            dir.path(),
            &mv_state::seatstate_store::SeatMap::from([(
                "openai".to_string(),
                mv_state::seatstate_store::SeatEntry {
                    cap: Some(1),
                    ..Default::default()
                },
            )]),
        )
        .unwrap();
        let mut counts = HashMap::new();
        counts.insert("openai".to_string(), 1);

        recheck_seat_caps(&mut council, dir.path(), Some(&counts)).unwrap();

        assert!(council.councillors[1].skipped);
        assert!(council.councillors[1].error.contains("open-agent cap"));
        assert!(!council.councillors[0].skipped);
        assert!(!council.councillors[2].skipped);
    }

    #[test]
    fn capped_seats_that_would_leave_fewer_than_two_vendors_raise_too_small() {
        let mut council = two_seat_council();
        let dir = tempfile::tempdir().unwrap();
        mv_state::seatstate_store::save(
            dir.path(),
            &mv_state::seatstate_store::SeatMap::from([(
                "openai".to_string(),
                mv_state::seatstate_store::SeatEntry {
                    cap: Some(1),
                    ..Default::default()
                },
            )]),
        )
        .unwrap();
        let mut counts = HashMap::new();
        counts.insert("openai".to_string(), 1);

        let error = recheck_seat_caps(&mut council, dir.path(), Some(&counts)).unwrap_err();
        assert!(error.to_string().contains("open-agent cap"));
    }

    /// Critic-grok3 row 5: `open_counts` is keyed by account name, the
    /// same key Python's `recheck_seat_caps` uses (`council.py:508`). A
    /// short-code map (CLI `live_open_counts` before row 2) must not
    /// trip the cap; an account-name map must.
    #[test]
    fn recheck_seat_caps_looks_up_account_name_not_short() {
        let dir = tempfile::tempdir().unwrap();
        mv_state::seatstate_store::save(
            dir.path(),
            &mv_state::seatstate_store::SeatMap::from([(
                "claude-a".to_string(),
                mv_state::seatstate_store::SeatEntry {
                    cap: Some(1),
                    ..Default::default()
                },
            )]),
        )
        .unwrap();

        let mut short_keyed = Council {
            question: "q".into(),
            councillors: vec![
                councillor("claude", "claude-a"),
                councillor("chatgpt", "codex-b"),
                councillor("grok", "grok-c"),
            ],
        };
        let mut shorts = HashMap::new();
        shorts.insert("ca".to_string(), 1);
        recheck_seat_caps(&mut short_keyed, dir.path(), Some(&shorts)).unwrap();
        assert!(
            !short_keyed.councillors[0].skipped,
            "a short-keyed map must not trip an account-name lookup"
        );

        let mut name_keyed = Council {
            question: "q".into(),
            councillors: vec![
                councillor("claude", "claude-a"),
                councillor("chatgpt", "codex-b"),
                councillor("grok", "grok-c"),
            ],
        };
        let mut names = HashMap::new();
        names.insert("claude-a".to_string(), 1);
        recheck_seat_caps(&mut name_keyed, dir.path(), Some(&names)).unwrap();
        assert!(
            name_keyed.councillors[0].skipped,
            "an account-name map must trip the matching seat's cap"
        );
        assert!(!name_keyed.councillors[1].skipped);
        assert!(!name_keyed.councillors[2].skipped);
    }

    #[test]
    fn an_unreadable_open_count_listing_treats_a_capped_seat_as_full() {
        let mut council = two_seat_council();
        let dir = tempfile::tempdir().unwrap();
        mv_state::seatstate_store::save(
            dir.path(),
            &mv_state::seatstate_store::SeatMap::from([(
                "openai".to_string(),
                mv_state::seatstate_store::SeatEntry {
                    cap: Some(5),
                    ..Default::default()
                },
            )]),
        )
        .unwrap();

        // `open_counts: None` — the listing itself could not be read.
        let error = recheck_seat_caps(&mut council, dir.path(), None).unwrap_err();
        assert!(error.to_string().contains("open-agent cap"));
    }

    struct FakeRecheck {
        unavailable: HashSet<String>,
    }
    impl Selector for FakeRecheck {
        fn select(&mut self, _: Option<&str>, _: &HashSet<String>) -> Result<Seat, SelectError> {
            unreachable!("recheck tests never call select")
        }
        fn recheck(&mut self, seat: &Seat) -> Result<(), String> {
            if self.unavailable.contains(&seat.account) {
                return Err("draining: below floor".into());
            }
            Ok(())
        }
    }

    #[test]
    fn convene_rechecks_availability_and_skips_a_seat_disabled_after_planning() {
        // Three seats, so skipping one still leaves two distinct vendors —
        // a shrink, not a refusal.
        let mut council = Council {
            question: "q".into(),
            councillors: vec![
                councillor("claude", "anthropic"),
                councillor("chatgpt", "openai"),
                councillor("grok", "xai"),
            ],
        };
        let mut selector = FakeRecheck {
            unavailable: HashSet::from(["openai".to_string()]),
        };

        recheck_seat_availability(&mut council, &mut selector).unwrap();

        assert!(council.councillors[1].skipped);
        assert!(council.councillors[1].error.contains("draining"));
        assert!(!council.councillors[0].skipped);
        assert!(!council.councillors[2].skipped);
    }

    #[test]
    fn recheck_seat_availability_raises_too_small_when_it_would_shrink_below_the_floor() {
        let mut council = two_seat_council();
        let mut selector = FakeRecheck {
            unavailable: HashSet::from(["openai".to_string()]),
        };
        // Only two seats total; disabling one leaves a single vendor.
        let error = recheck_seat_availability(&mut council, &mut selector).unwrap_err();
        assert!(error
            .to_string()
            .contains("Seat(s) unavailable by convene time"));
    }

    // --- quorum_report text ---------------------------------------------

    #[test]
    fn quorum_report_names_nothing_synthesized_when_quorate() {
        let mut council = two_seat_council();
        council.councillors[0].status = Some(TaskStatus::Done);
        council.councillors[1].status = Some(TaskStatus::Done);

        let text = quorum_report(&council);
        assert!(text.contains("2 councillors answered across 2 vendors"));
        assert!(text.contains("Nothing was synthesized"));
    }

    #[test]
    fn quorum_report_refuses_to_call_one_answer_a_council() {
        let mut council = two_seat_council();
        council.councillors[0].status = Some(TaskStatus::Done);

        let text = quorum_report(&council);
        assert!(text.contains("Only 1 councillor(s) answered, across 1 vendor(s)"));
        assert!(text.contains("That is not a council"));
    }

    // --- ChainSelector, bound to the real `selector::select` ------------

    struct AlwaysClosed;
    impl selector::BreakerQuery for AlwaysClosed {
        fn status(&self, _pool: &str) -> selector::BreakerStatus {
            selector::BreakerStatus::CLOSED
        }
        fn claim_trial(&self, _pool: &str, _task_id: &str, _token: Option<&str>) -> Option<String> {
            None
        }
    }

    fn selector_seat(name: &str, vendor: &str) -> selector::Seat {
        selector::Seat {
            name: name.to_string(),
            vendor: vendor.to_string(),
            pool: format!("{vendor}:{name}"),
            kind: "claude".to_string(),
            available: true,
            unavailable_reason: None,
            unavailable_until: None,
            lent: true,
            has_model: true,
            roles_allowed: None,
            max_brief_chars: None,
            has_command: true,
            cap: None,
            open_count: None,
            quota_primary_percent: None,
            quota_secondary_percent: None,
            local_alive: None,
        }
    }

    #[test]
    fn chain_selector_seats_one_per_vendor_via_the_real_selector() {
        let chain = vec![
            selector_seat("anthropic", "claude"),
            selector_seat("openai", "chatgpt"),
            selector_seat("xai", "grok"),
        ];
        let breaker = AlwaysClosed;
        let mut adapter = ChainSelector::new(
            &chain,
            "sandbox",
            "critic",
            "nexus",
            selector::QuotaFloors::default(),
            selector::FallbackPolicy::Degrade,
            "20261102-council-test",
            &breaker,
            Instant::now(),
        );

        let seated = seat(&mut adapter, "what should we build next?").unwrap();

        assert_eq!(
            seated
                .iter()
                .map(|s| s.account.as_str())
                .collect::<Vec<_>>(),
            ["anthropic", "openai", "xai"]
        );
        // The full `Selection` (half-open trial token included) stays
        // retrievable for whoever builds this seat's `DispatchStore`.
        assert!(adapter.selection("anthropic").is_some());
        assert!(adapter.selection("nobody").is_none());
    }

    #[test]
    fn chain_selector_recheck_reflects_an_open_breaker() {
        struct Fenced;
        impl selector::BreakerQuery for Fenced {
            fn status(&self, pool: &str) -> selector::BreakerStatus {
                if pool == "chatgpt:openai" {
                    selector::BreakerStatus {
                        open: true,
                        half_open: false,
                        reset_at: selector::ResetAt::NoAutomaticReset,
                        reset_display: None,
                        trial_claimed_by: None,
                    }
                } else {
                    selector::BreakerStatus::CLOSED
                }
            }
            fn claim_trial(&self, _: &str, _: &str, _: Option<&str>) -> Option<String> {
                None
            }
        }
        let chain = vec![
            selector_seat("anthropic", "claude"),
            selector_seat("openai", "chatgpt"),
        ];
        let breaker = Fenced;
        let mut adapter = ChainSelector::new(
            &chain,
            "sandbox",
            "critic",
            "nexus",
            selector::QuotaFloors::default(),
            selector::FallbackPolicy::Degrade,
            "20261102-council-test",
            &breaker,
            Instant::now(),
        );

        let openai = Seat {
            vendor: "chatgpt".into(),
            account: "openai".into(),
        };
        let anthropic = Seat {
            vendor: "claude".into(),
            account: "anthropic".into(),
        };
        assert!(adapter.recheck(&openai).is_err());
        assert!(adapter.recheck(&anthropic).is_ok());
    }

    /// Critic finding critic-safety.md#2: `recheck` used to build its
    /// `SelectParams` with `trial_token: None`, which never matches the
    /// claim `select` minted for this exact seat moments earlier
    /// (`availability`'s `trial_token.is_some_and(|t| claim.task_id ==
    /// task_id && claim.token == t)`, `selector.rs`) — so a council would
    /// drop the very half-open seat it just seated. A fake breaker that
    /// actually claims (unlike every other fake in this module, which
    /// either never fences or never claims) reproduces the self-conflict:
    /// `select` must succeed on the half-open seat, and `recheck` on that
    /// same seat right after must also succeed, not read it as held by
    /// "another dispatch."
    #[test]
    fn chain_selector_recheck_accepts_the_trial_it_just_claimed() {
        use std::cell::RefCell;

        struct HalfOpenClaiming {
            claimed: RefCell<Option<selector::TrialClaim>>,
        }
        impl selector::BreakerQuery for HalfOpenClaiming {
            fn status(&self, pool: &str) -> selector::BreakerStatus {
                if pool != "chatgpt:openai" {
                    return selector::BreakerStatus::CLOSED;
                }
                selector::BreakerStatus {
                    open: true,
                    half_open: true,
                    reset_at: selector::ResetAt::None,
                    reset_display: None,
                    trial_claimed_by: self.claimed.borrow().clone(),
                }
            }
            fn claim_trial(
                &self,
                _pool: &str,
                task_id: &str,
                token: Option<&str>,
            ) -> Option<String> {
                let mut claimed = self.claimed.borrow_mut();
                if let Some(existing) = claimed.as_ref() {
                    return if existing.task_id == task_id && Some(existing.token.as_str()) == token
                    {
                        Some(existing.token.clone())
                    } else {
                        None
                    };
                }
                let token = token.map(str::to_string).unwrap_or_else(|| "tok".into());
                *claimed = Some(selector::TrialClaim {
                    task_id: task_id.to_string(),
                    token: token.clone(),
                });
                Some(token)
            }
        }

        let chain = vec![selector_seat("openai", "chatgpt")];
        let breaker = HalfOpenClaiming {
            claimed: RefCell::new(None),
        };
        let mut adapter = ChainSelector::new(
            &chain,
            "sandbox",
            "critic",
            "nexus",
            selector::QuotaFloors::default(),
            selector::FallbackPolicy::Degrade,
            "20261102-council-test",
            &breaker,
            Instant::now(),
        );

        let seat = adapter
            .select(Some("question"), &HashSet::new())
            .expect("half-open, unclaimed pool is selectable");
        assert!(
            adapter.selection("openai").unwrap().trial_token.is_some(),
            "select must have actually claimed the trial"
        );

        assert!(
            adapter.recheck(&seat).is_ok(),
            "recheck must accept the trial this exact council just claimed"
        );
    }

    // --- convene, against a real fake-herdr -----------------------------

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Sandbox(PathBuf);
    impl Sandbox {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "mv-council-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fake_binary() -> &'static Path {
        static BINARY: OnceLock<PathBuf> = OnceLock::new();
        BINARY.get_or_init(|| {
            // Nested `cargo test --workspace -j N` lets other crates rebuild
            // `fake-herdr` into the shared `target/` artifact while this
            // crate is already exec'ing it. Rewriting a binary that another
            // process is exec'ing returns ETXTBSY; `wait_ready` here has a
            // zero readiness budget, so that single failed spawn marks the
            // seat failed and flakes
            // `a_quorate_council_runs_every_seat_through_the_runner_and_collects_its_answer`.
            // Ask Cargo for a finished artifact (its lock serializes the
            // rebuild), then copy it to a process-private path so later
            // workspace rebuilds cannot touch the file we exec.
            let output = Command::new(env!("CARGO"))
                .args([
                    "build",
                    "-p",
                    "mv-herdr",
                    "--bin",
                    "fake-herdr",
                    "--locked",
                    "--message-format=json",
                ])
                .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
                .output()
                .expect("build fake-herdr with Cargo");
            assert!(
                output.status.success(),
                "fake-herdr build failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let artifact = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find_map(|message| {
                    (message["reason"] == "compiler-artifact"
                        && message["target"]["name"] == "fake-herdr")
                        .then(|| message["executable"].as_str().map(PathBuf::from))
                        .flatten()
                })
                .expect("Cargo must report the fake-herdr executable");
            assert!(
                artifact.is_file(),
                "fake-herdr must exist; never fall back to real Herdr"
            );
            let isolated =
                std::env::temp_dir().join(format!("mv-council-fake-herdr-{}", std::process::id()));
            fs::copy(&artifact, &isolated).expect("isolate fake-herdr from target/");
            fs::set_permissions(&isolated, fs::Permissions::from_mode(0o700)).unwrap();
            isolated
        })
    }

    fn quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }

    struct Store {
        launch: runner::PreparedLaunch,
        fail_prepare: bool,
    }
    impl DispatchStore for Store {
        type Guard = ();
        fn lock_fleet(&mut self) -> Result<(), RunnerError> {
            Ok(())
        }
        fn lock_project(&mut self) -> Result<(), RunnerError> {
            Ok(())
        }
        fn prepare(&mut self) -> Result<runner::PreparedLaunch, RunnerError> {
            if self.fail_prepare {
                return Err(RunnerError::Refused(
                    "seat unavailable at start time".into(),
                ));
            }
            fs::create_dir_all(&self.launch.cwd)?;
            Ok(self.launch.clone())
        }
        fn unfinished_agent_names(&mut self) -> Result<HashSet<String>, RunnerError> {
            Ok(HashSet::new())
        }
        fn record_started(
            &mut self,
            _: &runner::PreparedLaunch,
            _: &str,
            _: SystemTime,
        ) -> Result<(), RunnerError> {
            Ok(())
        }
        fn setup_lane(&mut self, _: &runner::PreparedLaunch) -> Result<(), RunnerError> {
            Ok(())
        }
        fn release_trial(&mut self) -> Result<(), RunnerError> {
            Ok(())
        }
        fn warn(&mut self, _: &str) {}
    }

    struct Fake {
        script: PathBuf,
        state: PathBuf,
        task_dir: PathBuf,
        fail: Option<&'static str>,
        drop_status_after_prompt: bool,
        hold: Duration,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
        // Rendezvous point proving concurrency without depending on
        // wall-clock timing: every seat blocks here until the whole party
        // has arrived, so overlap holds under any amount of host
        // contention, not just contention short enough to fit inside a
        // fixed sleep. `None` for fixtures that don't need the proof.
        //
        // Triggered on each seat's *second* `agent get` call, not its
        // first: scout2-gaps.md #23 made every seat's own `runner::
        // dispatch` (pane creation through prompt delivery) run in chain
        // order on one thread, under one held fleet lock, so the first
        // `agent get` — `deliver_prompt_to`'s own landing-confirmation poll,
        // still inside `dispatch` — no longer overlaps across seats by
        // design, and gating on it here deadlocks the very property this
        // fixture exists to prove (seat 2 never starts dispatching until
        // seat 1's `dispatch` call returns). The *second* `agent get` per
        // seat is `wait_for_result`'s own first poll, made only once every
        // seat has already been dispatched — the one phase still
        // guaranteed to run concurrently.
        barrier: Option<Arc<Barrier>>,
        get_calls_seen: std::cell::Cell<u32>,
    }
    impl Herdr for Fake {
        fn call(
            &mut self,
            command: &HerdrCommand,
            timeout: Duration,
        ) -> Result<CommandResult, RunnerError> {
            let action = command.argv[2].as_str();
            if action == "get" {
                self.get_calls_seen.set(self.get_calls_seen.get() + 1);
            }
            if action == "get" && self.get_calls_seen.get() == 2 {
                let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_in_flight.fetch_max(now, Ordering::SeqCst);
                if let Some(barrier) = &self.barrier {
                    barrier.wait();
                } else if !self.hold.is_zero() {
                    std::thread::sleep(self.hold);
                }
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
            }
            let result = if self.fail == Some(action) {
                CommandResult {
                    argv: command.argv.clone(),
                    returncode: 1,
                    stdout: String::new(),
                    stderr: "injected failure".into(),
                    payload: None,
                    protocol_error: None,
                }
            } else {
                let mut redirected = command.clone();
                redirected.argv[0] = self.script.to_string_lossy().into_owned();
                mv_herdr::run::run(&redirected, Some(timeout))?
            };
            if action == "start" {
                // Real Herdr panes have visible text; the fake binary does
                // not populate any by default. A dispatch's readiness wait
                // reads the pane, so give it something harmless to see —
                // the same trick `runner.rs`'s own fixture uses.
                let mut state: Value =
                    serde_json::from_slice(&fs::read(&self.state).unwrap()).unwrap();
                let pane = state["agents"].as_array().unwrap().last().unwrap()["pane_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                state["text"][pane] = json!("Ready >");
                fs::write(&self.state, serde_json::to_vec(&state).unwrap()).unwrap();
            }
            if action == "prompt" && self.drop_status_after_prompt {
                let _ = fs::remove_file(self.task_dir.join("status"));
            }
            Ok(result)
        }
    }

    fn timing() -> Timing {
        Timing {
            readiness: Duration::ZERO,
            prompt: Duration::ZERO,
            poll: Duration::ZERO,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn seat_fixture(
        name: &str,
        vendor: &str,
        fail: Option<&'static str>,
        fail_prepare: bool,
        drop_status_after_prompt: bool,
        hold: Duration,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
        barrier: Option<Arc<Barrier>>,
    ) -> (Sandbox, Store, Fake) {
        let sandbox = Sandbox::new();
        let state = sandbox.0.join("herdr.json");
        let script = sandbox.0.join("isolated-herdr");
        let config = sandbox.0.join("config");
        let runtime = sandbox.0.join("runtime");
        fs::create_dir(&config).unwrap();
        fs::create_dir(&runtime).unwrap();
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nexport HERDR_BIN_PATH={}\nexport MV_FAKE_HERDR_STATE={}\nexport HOME={}\nexport HERDR_PLUGIN_CONFIG_DIR={}\nexport HERDR_PLUGIN_STATE_DIR={}\nexec {} \"$@\"\n",
                quote(&fake_binary().to_string_lossy()),
                quote(&state.to_string_lossy()),
                quote(&sandbox.0.to_string_lossy()),
                quote(&config.to_string_lossy()),
                quote(&runtime.to_string_lossy()),
                quote(&fake_binary().to_string_lossy()),
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let task_id = format!("20261102-council-{name}");
        let launch = runner::PreparedLaunch {
            project_root: sandbox.0.clone(),
            cwd: sandbox.0.join("writer's lane"),
            task_id: task_id.clone(),
            agent_name: format!("example-{name}"),
            role: "council".into(),
            kind: "codex".into(),
            model: "test-model".into(),
            seat: format!("seat-{name}"),
            brief: format!("council question, seat {vendor}\n"),
            native_args: argv::native_args("test-model", "codex", None),
            environment: vec![("CODEX_HOME".into(), config.to_string_lossy().into_owned())],
            clear_auth: vec!["OPENAI_API_KEY".into()],
            command: None,
        };
        let task_dir = launch.cwd.join(".multiverse/tasks").join(&task_id);
        let store = Store {
            launch,
            fail_prepare,
        };
        let fake = Fake {
            script,
            state,
            task_dir,
            fail,
            drop_status_after_prompt,
            hold,
            in_flight,
            max_in_flight,
            barrier,
            get_calls_seen: std::cell::Cell::new(0),
        };
        (sandbox, store, fake)
    }

    #[test]
    fn a_quorate_council_runs_every_seat_through_the_runner_and_collects_its_answer() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let (_s1, store1, fake1) = seat_fixture(
            "a",
            "claude",
            None,
            false,
            false,
            Duration::ZERO,
            in_flight.clone(),
            max_in_flight.clone(),
            None,
        );
        let (_s2, store2, fake2) = seat_fixture(
            "b",
            "chatgpt",
            None,
            false,
            false,
            Duration::ZERO,
            in_flight,
            max_in_flight,
            None,
        );
        let mut council = Council {
            question: "q".into(),
            councillors: vec![
                councillor("claude", "anthropic"),
                councillor("chatgpt", "openai"),
            ],
        };

        convene(
            &mut council,
            vec![store1, store2],
            vec![fake1, fake2],
            "w52:p1",
            &timing(),
            Duration::from_secs(5),
        );

        assert!(council.is_quorate());
        assert!(council
            .councillors
            .iter()
            .all(|c| c.status == Some(TaskStatus::Done)));
        assert_eq!(council.failed().count(), 0);
    }

    #[test]
    fn one_failed_seat_does_not_sink_the_others() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let (_s1, store1, fake1) = seat_fixture(
            "a",
            "claude",
            None,
            false,
            false,
            Duration::ZERO,
            in_flight.clone(),
            max_in_flight.clone(),
            None,
        );
        let (_s2, store2, fake2) = seat_fixture(
            "b",
            "chatgpt",
            None,
            true, // fails at prepare time, before anything is started
            false,
            Duration::ZERO,
            in_flight.clone(),
            max_in_flight.clone(),
            None,
        );
        let (_s3, store3, fake3) = seat_fixture(
            "c",
            "grok",
            None,
            false,
            false,
            Duration::ZERO,
            in_flight,
            max_in_flight,
            None,
        );
        let mut council = Council {
            question: "q".into(),
            councillors: vec![
                councillor("claude", "anthropic"),
                councillor("chatgpt", "openai"),
                councillor("grok", "xai"),
            ],
        };

        convene(
            &mut council,
            vec![store1, store2, store3],
            vec![fake1, fake2, fake3],
            "w52:p1",
            &timing(),
            Duration::from_secs(5),
        );

        assert_eq!(council.failed().count(), 1);
        assert!(council.councillors[1]
            .error
            .contains("seat unavailable at start time"));
        assert!(!council.councillors[1].unanswered);
        assert!(!council.councillors[1].skipped);
        // The two healthy seats still answered, and that alone is quorate.
        assert!(council.is_quorate());
        assert_eq!(council.answered().count(), 2);
    }

    #[test]
    fn a_seat_that_settles_without_writing_status_is_unanswered_not_failed() {
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let (_s1, store1, fake1) = seat_fixture(
            "a",
            "claude",
            None,
            false,
            false,
            Duration::ZERO,
            in_flight.clone(),
            max_in_flight.clone(),
            None,
        );
        let (_s2, store2, fake2) = seat_fixture(
            "b",
            "chatgpt",
            None,
            false,
            true, // settles, but its status file is missing
            Duration::ZERO,
            in_flight,
            max_in_flight,
            None,
        );
        let mut council = Council {
            question: "q".into(),
            councillors: vec![
                councillor("claude", "anthropic"),
                councillor("chatgpt", "openai"),
            ],
        };

        convene(
            &mut council,
            vec![store1, store2],
            vec![fake1, fake2],
            "w52:p1",
            &timing(),
            Duration::from_millis(200),
        );

        assert_eq!(council.councillors[0].status, Some(TaskStatus::Done));
        assert!(council.councillors[1].unanswered);
        assert!(council.councillors[1].error.contains("MISSING ANSWER"));
        assert_eq!(council.failed().count(), 0);
        assert!(!council.is_quorate());
    }

    #[test]
    fn councillors_wait_for_their_result_concurrently_rather_than_one_after_another() {
        // Scout2-gaps.md #23 made every seat's *start* (pane creation
        // through prompt delivery) run in chain order on one thread, under
        // one held fleet lock — so seats are no longer expected to overlap
        // there. The wait phase, which only begins once every seat has
        // started, is the one phase still guaranteed to run concurrently;
        // this test proves that phase, not the (now intentionally
        // sequential) start phase a version of this test proved before #23.
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        // A rendezvous, not a sleep: each seat blocks on its first `agent
        // get` poll (inside `wait_for_result`) until all three have
        // arrived, so overlap is guaranteed rather than merely likely under
        // contention from other tests running concurrently in this same
        // process. A fixed sleep (tried first) flaked under heavy host
        // load, where thread scheduling delays could exceed any duration
        // short of making the whole suite slow.
        let barrier = Arc::new(Barrier::new(3));
        let (_s1, store1, fake1) = seat_fixture(
            "a",
            "claude",
            None,
            false,
            false,
            Duration::ZERO,
            in_flight.clone(),
            max_in_flight.clone(),
            Some(barrier.clone()),
        );
        let (_s2, store2, fake2) = seat_fixture(
            "b",
            "chatgpt",
            None,
            false,
            false,
            Duration::ZERO,
            in_flight.clone(),
            max_in_flight.clone(),
            Some(barrier.clone()),
        );
        let (_s3, store3, fake3) = seat_fixture(
            "c",
            "grok",
            None,
            false,
            false,
            Duration::ZERO,
            in_flight,
            max_in_flight.clone(),
            Some(barrier),
        );
        let mut council = Council {
            question: "q".into(),
            councillors: vec![
                councillor("claude", "anthropic"),
                councillor("chatgpt", "openai"),
                councillor("grok", "xai"),
            ],
        };

        convene(
            &mut council,
            vec![store1, store2, store3],
            vec![fake1, fake2, fake3],
            "w52:p1",
            &timing(),
            Duration::from_secs(30),
        );

        // The decisive signal: if the wait phase ran one seat after
        // another, at most one could ever be inside its first `agent get`
        // call at a time. Wall-clock timing is not used here — it is too
        // noisy under a test binary that runs many tests (and their own
        // subprocesses) concurrently.
        assert!(
            max_in_flight.load(Ordering::SeqCst) >= 2,
            "no two seats were ever in flight together — looks sequential"
        );
        assert!(council.is_quorate());
    }

    /// Scout2-gaps.md #23's actual claim: the fleet lock spans the whole
    /// batch, not just one seat, so nothing outside this council can spend
    /// capacity between its capacity check and its last seat starting.
    /// Proved directly rather than through timing: while `convene` is still
    /// working through its (deliberately slowed) first seat, a foreign
    /// thread's independent attempt to acquire the *same* fleet lock path
    /// must be refused — if `convene` only held the lock per seat (the old
    /// shape), a foreign acquirer landing between seat 1 and seat 2 would
    /// succeed instead.
    #[test]
    fn convene_holds_one_fleet_lock_across_every_seats_start() {
        struct LockingStore {
            lock_path: PathBuf,
            project_lock_path: PathBuf,
            delay: Duration,
        }
        impl DispatchStore for LockingStore {
            type Guard = mv_core::lock::LockGuard;
            fn lock_fleet(&mut self) -> Result<Self::Guard, RunnerError> {
                mv_core::lock::acquire(&self.lock_path, "fleet", Duration::from_secs(5))
                    .map_err(|e| RunnerError::Refused(e.to_string()))
            }
            fn lock_project(&mut self) -> Result<Self::Guard, RunnerError> {
                mv_core::lock::acquire(&self.project_lock_path, "project", Duration::from_secs(5))
                    .map_err(|e| RunnerError::Refused(e.to_string()))
            }
            fn prepare(&mut self) -> Result<runner::PreparedLaunch, RunnerError> {
                // Stand in for real pane-creation work that takes a moment —
                // long enough for the probe thread below to land its attempt
                // while this seat (and so the whole batch) is still inside
                // the held fleet lock.
                std::thread::sleep(self.delay);
                Err(RunnerError::Refused(
                    "stop before any real pane work".into(),
                ))
            }
            fn unfinished_agent_names(&mut self) -> Result<HashSet<String>, RunnerError> {
                unreachable!("prepare always fails first")
            }
            fn record_started(
                &mut self,
                _: &runner::PreparedLaunch,
                _: &str,
                _: SystemTime,
            ) -> Result<(), RunnerError> {
                unreachable!("prepare always fails first")
            }
            fn setup_lane(&mut self, _: &runner::PreparedLaunch) -> Result<(), RunnerError> {
                unreachable!("prepare always fails first")
            }
            fn release_trial(&mut self) -> Result<(), RunnerError> {
                Ok(())
            }
            fn warn(&mut self, _: &str) {}
        }

        struct NeverCalled;
        impl Herdr for NeverCalled {
            fn call(
                &mut self,
                _command: &HerdrCommand,
                _timeout: Duration,
            ) -> Result<CommandResult, RunnerError> {
                panic!(
                    "herdr must never be reached — `prepare` fails before `dispatch` \
                     touches it"
                )
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("fleet.lock");

        let mut council = Council {
            question: "q".into(),
            councillors: vec![
                councillor("claude", "anthropic"),
                councillor("chatgpt", "openai"),
            ],
        };
        let stores = vec![
            LockingStore {
                lock_path: lock_path.clone(),
                project_lock_path: dir.path().join("project-a.lock"),
                delay: Duration::from_millis(300),
            },
            LockingStore {
                lock_path: lock_path.clone(),
                project_lock_path: dir.path().join("project-b.lock"),
                delay: Duration::ZERO,
            },
        ];
        let herdrs = vec![NeverCalled, NeverCalled];

        let probe_path = lock_path.clone();
        let probe = std::thread::spawn(move || {
            // Land inside seat 1's artificial delay, well before seat 2 (or
            // anything else) could plausibly have the lock.
            std::thread::sleep(Duration::from_millis(100));
            mv_core::lock::acquire(&probe_path, "foreign", Duration::from_millis(100))
        });

        convene(
            &mut council,
            stores,
            herdrs,
            "w52:p1",
            &timing(),
            Duration::from_secs(5),
        );

        let probe_result = probe.join().unwrap();
        assert!(
            matches!(probe_result, Err(mv_core::lock::LockError::Timeout(_))),
            "a foreign acquire of this council's own fleet lock must be refused \
             while convene is still starting its batch, not merely while one seat \
             is mid-dispatch: acquired = {}",
            probe_result.is_ok()
        );
        // Both seats failed at `prepare` (by design, to keep this test fast
        // and Herdr-free) — never skipped, never answered.
        assert_eq!(council.failed().count(), 2);
        assert_eq!(council.answered().count(), 0);
    }

    /// Critic-grok3 row 4: inner start must not call `lock_fleet` again.
    /// A reentrant acquire (`held: None`) would leave the outer guard as
    /// the one that actually unlocks, and that guard used to live across
    /// prompt delivery. Python's `DispatchRunner(config=None)` skips the
    /// inner lock entirely (`council.py:591-606`).
    #[test]
    fn convene_does_not_reenter_the_fleet_lock_for_inner_starts() {
        struct CountingLockStore {
            lock_path: PathBuf,
            project_lock_path: PathBuf,
            lock_count: Arc<AtomicUsize>,
        }
        impl DispatchStore for CountingLockStore {
            type Guard = mv_core::lock::LockGuard;
            fn lock_fleet(&mut self) -> Result<Self::Guard, RunnerError> {
                self.lock_count.fetch_add(1, Ordering::SeqCst);
                mv_core::lock::acquire(&self.lock_path, "fleet", Duration::from_secs(5))
                    .map_err(|e| RunnerError::Refused(e.to_string()))
            }
            fn lock_project(&mut self) -> Result<Self::Guard, RunnerError> {
                mv_core::lock::acquire(&self.project_lock_path, "project", Duration::from_secs(5))
                    .map_err(|e| RunnerError::Refused(e.to_string()))
            }
            fn prepare(&mut self) -> Result<runner::PreparedLaunch, RunnerError> {
                Err(RunnerError::Refused(
                    "stop before any real pane work".into(),
                ))
            }
            fn unfinished_agent_names(&mut self) -> Result<HashSet<String>, RunnerError> {
                unreachable!("prepare always fails first")
            }
            fn record_started(
                &mut self,
                _: &runner::PreparedLaunch,
                _: &str,
                _: SystemTime,
            ) -> Result<(), RunnerError> {
                unreachable!("prepare always fails first")
            }
            fn setup_lane(&mut self, _: &runner::PreparedLaunch) -> Result<(), RunnerError> {
                unreachable!("prepare always fails first")
            }
            fn release_trial(&mut self) -> Result<(), RunnerError> {
                Ok(())
            }
            fn warn(&mut self, _: &str) {}
        }

        struct NeverCalled;
        impl Herdr for NeverCalled {
            fn call(
                &mut self,
                _command: &HerdrCommand,
                _timeout: Duration,
            ) -> Result<CommandResult, RunnerError> {
                panic!("herdr must never be reached — `prepare` fails before start touches it")
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("fleet.lock");
        let lock_count = Arc::new(AtomicUsize::new(0));
        let mut council = Council {
            question: "q".into(),
            councillors: vec![
                councillor("claude", "anthropic"),
                councillor("chatgpt", "openai"),
            ],
        };
        let stores = vec![
            CountingLockStore {
                lock_path: lock_path.clone(),
                project_lock_path: dir.path().join("project-a.lock"),
                lock_count: lock_count.clone(),
            },
            CountingLockStore {
                lock_path,
                project_lock_path: dir.path().join("project-b.lock"),
                lock_count: lock_count.clone(),
            },
        ];

        convene(
            &mut council,
            stores,
            vec![NeverCalled, NeverCalled],
            "w52:p1",
            &timing(),
            Duration::from_secs(5),
        );

        assert_eq!(
            lock_count.load(Ordering::SeqCst),
            1,
            "only the outer convene acquire should call lock_fleet; inner starts must use AlreadyHeld"
        );
        assert_eq!(council.failed().count(), 2);
    }

    /// Critic-grok3 row 3: Python holds `_fleet_lock` across
    /// `recheck_seat_caps` / `recheck_seat_availability` / capacity, then
    /// start (`council.py:650-657`). `admit` runs inside that same
    /// acquisition, so a foreign lock attempt mid-admit must time out.
    #[test]
    fn convene_holds_fleet_lock_across_admission_rechecks() {
        struct LockingStore {
            lock_path: PathBuf,
            project_lock_path: PathBuf,
        }
        impl DispatchStore for LockingStore {
            type Guard = mv_core::lock::LockGuard;
            fn lock_fleet(&mut self) -> Result<Self::Guard, RunnerError> {
                mv_core::lock::acquire(&self.lock_path, "fleet", Duration::from_secs(5))
                    .map_err(|e| RunnerError::Refused(e.to_string()))
            }
            fn lock_project(&mut self) -> Result<Self::Guard, RunnerError> {
                mv_core::lock::acquire(&self.project_lock_path, "project", Duration::from_secs(5))
                    .map_err(|e| RunnerError::Refused(e.to_string()))
            }
            fn prepare(&mut self) -> Result<runner::PreparedLaunch, RunnerError> {
                Err(RunnerError::Refused(
                    "stop before any real pane work".into(),
                ))
            }
            fn unfinished_agent_names(&mut self) -> Result<HashSet<String>, RunnerError> {
                unreachable!("prepare always fails first")
            }
            fn record_started(
                &mut self,
                _: &runner::PreparedLaunch,
                _: &str,
                _: SystemTime,
            ) -> Result<(), RunnerError> {
                unreachable!("prepare always fails first")
            }
            fn setup_lane(&mut self, _: &runner::PreparedLaunch) -> Result<(), RunnerError> {
                unreachable!("prepare always fails first")
            }
            fn release_trial(&mut self) -> Result<(), RunnerError> {
                Ok(())
            }
            fn warn(&mut self, _: &str) {}
        }

        struct NeverCalled;
        impl Herdr for NeverCalled {
            fn call(
                &mut self,
                _command: &HerdrCommand,
                _timeout: Duration,
            ) -> Result<CommandResult, RunnerError> {
                panic!("herdr must never be reached — admit/prepare fail first")
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("fleet.lock");
        let mut council = Council {
            question: "q".into(),
            councillors: vec![
                councillor("claude", "anthropic"),
                councillor("chatgpt", "openai"),
            ],
        };
        let stores = vec![
            LockingStore {
                lock_path: lock_path.clone(),
                project_lock_path: dir.path().join("project-a.lock"),
            },
            LockingStore {
                lock_path: lock_path.clone(),
                project_lock_path: dir.path().join("project-b.lock"),
            },
        ];

        // The probe must not attempt its acquire on a blind guess at how
        // long admission takes to start — under CI scheduling delays a
        // fixed pre-sleep can land after admission (and the fleet lock
        // with it) has already finished, passing for the wrong reason.
        // Instead the probe waits for admission to actually begin, which
        // is also the instant the fleet lock is known held.
        let (admitting_tx, admitting_rx) = std::sync::mpsc::channel::<()>();

        let probe_path = lock_path.clone();
        let probe = std::thread::spawn(move || {
            admitting_rx.recv().unwrap();
            mv_core::lock::acquire(&probe_path, "foreign", Duration::from_millis(80))
        });

        convene_with_admission(
            &mut council,
            stores,
            vec![NeverCalled, NeverCalled],
            "w52:p1",
            &timing(),
            Duration::from_secs(5),
            |_| {
                admitting_tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(200));
                Ok::<(), std::convert::Infallible>(())
            },
        )
        .unwrap();

        let probe_result = probe.join().unwrap();
        assert!(
            matches!(probe_result, Err(mv_core::lock::LockError::Timeout(_))),
            "a foreign acquire must be refused while admit is still running under the fleet lock"
        );
    }
}
