//! The recursive `Block`/`Plan` tree and its stack-based cursor
//! interpreter — the single data model meant to serve every cycle
//! complexity level, from "work/break forever, no long break" up through
//! arbitrarily nested repeat groups and fully explicit finite sequences
//! (plan §"Core data model").
//!
//! `Cursor::advance` is the one authoritative "move to the next leaf
//! phase" algorithm — both natural phase-timer expiry and a user-triggered
//! skip call it identically (the daemon worker decides, at the call site,
//! whether to also send a notification).

use serde::de::Visitor;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseKind {
    Work,
    Break,
    LongBreak,
}

impl PhaseKind {
    pub fn label(&self) -> &'static str {
        match self {
            PhaseKind::Work => "Work",
            PhaseKind::Break => "Break",
            PhaseKind::LongBreak => "Long break",
        }
    }
}

/// How many times a `Repeat` block's own `blocks` list should run.
/// Serializes as a plain TOML/JSON integer for `Finite`, or the string
/// `"infinite"` for `Infinite` — see the manual `Serialize`/`Deserialize`
/// impls below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatCount {
    Finite(u32),
    Infinite,
}

impl Serialize for RepeatCount {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            RepeatCount::Finite(n) => serializer.serialize_u32(*n),
            RepeatCount::Infinite => serializer.serialize_str("infinite"),
        }
    }
}

struct RepeatCountVisitor;

impl<'de> Visitor<'de> for RepeatCountVisitor {
    type Value = RepeatCount;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a non-negative integer or the string \"infinite\"")
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
        u32::try_from(v)
            .map(RepeatCount::Finite)
            .map_err(|_| E::custom("repeat count out of range"))
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
        if v < 0 {
            return Err(E::custom("repeat count cannot be negative"));
        }
        self.visit_u64(v as u64)
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        if v.eq_ignore_ascii_case("infinite") || v.eq_ignore_ascii_case("inf") {
            Ok(RepeatCount::Infinite)
        } else {
            Err(E::custom(format!(
                "expected \"infinite\" or an integer, got {v:?}"
            )))
        }
    }
}

impl<'de> Deserialize<'de> for RepeatCount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(RepeatCountVisitor)
    }
}

/// One node of a cycle plan: either a leaf phase to run for `minutes`, or a
/// group of sub-blocks repeated `count` times. Nesting `Repeat` inside
/// `Repeat` is how "3x(work/break) -> long break, forever" or "2 of those
/// big groups -> an even longer break" get expressed; a plan with no
/// `Repeat` at all (just a flat list of `Phase`s) is how a fully custom,
/// naturally-ending sequence (e.g. a break that shrinks every cycle) gets
/// expressed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Phase {
        kind: PhaseKind,
        minutes: u32,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        label: Option<String>,
    },
    Repeat {
        count: RepeatCount,
        blocks: Vec<Block>,
    },
}

/// The top-level sequence, run once through (any looping is expressed by
/// wrapping the relevant part in a `Block::Repeat { count: Infinite, .. }`
/// node, not by the `Plan` type itself).
pub type Plan = Vec<Block>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    Empty,
    ZeroMinutePhase,
    NoReachablePhase,
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::Empty => write!(f, "plan has no blocks"),
            PlanError::ZeroMinutePhase => write!(f, "a phase has 0 minutes"),
            PlanError::NoReachablePhase => write!(
                f,
                "plan has no reachable phase (only empty/zero-count repeats)"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// Rejects a plan that could never produce a real running timer: no
/// blocks at all, a phase with 0 minutes, or a plan whose every branch is
/// an empty/zero-count `Repeat` so no leaf `Phase` is ever reachable.
pub fn validate(plan: &Plan) -> Result<(), PlanError> {
    if plan.is_empty() {
        return Err(PlanError::Empty);
    }

    fn check(blocks: &[Block]) -> Result<bool, PlanError> {
        let mut reachable = false;
        for b in blocks {
            match b {
                Block::Phase { minutes, .. } => {
                    if *minutes == 0 {
                        return Err(PlanError::ZeroMinutePhase);
                    }
                    reachable = true;
                }
                Block::Repeat { count, blocks } => {
                    if matches!(count, RepeatCount::Finite(0)) {
                        continue;
                    }
                    if check(blocks)? {
                        reachable = true;
                    }
                }
            }
        }
        Ok(reachable)
    }

    if !check(plan)? {
        return Err(PlanError::NoReachablePhase);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    /// -1 means "not yet entered this level's blocks for the current
    /// pass" — every real position is >= 0. Using -1 instead of
    /// `Option<usize>` lets a freshly-pushed frame and a reset-for-another-
    /// pass frame share the exact same "increment, then inspect" step as
    /// every other frame, with no separate first-entry special case.
    index: isize,
    /// Passes left (including the one in progress) for this frame's own
    /// `blocks` list. Meaningless for the root frame (level 0 always runs
    /// its top-level list exactly once — the root exhausting is what
    /// `AdvanceOutcome::Complete` means).
    remaining: RepeatCount,
}

/// The phase the cursor is currently sitting on.
#[derive(Debug, Clone, Copy)]
pub struct CurrentPhase<'a> {
    pub kind: PhaseKind,
    pub minutes: u32,
    pub label: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvanceOutcome {
    Next,
    Complete,
}

/// A depth-first walk position over a `Plan`'s `Block` tree — a stack of
/// `Frame`s, one per currently-open `Repeat` level (the root frame walks
/// the top-level `Plan` itself). Deliberately holds no reference to the
/// `Plan` it walks (every method takes `plan: &Plan` explicitly) so it can
/// be stored and mutated independently of the plan's own storage in the
/// daemon's `RuntimeState`.
#[derive(Debug, Clone)]
pub struct Cursor {
    stack: Vec<Frame>,
}

/// Resolves the `Vec<Block>` that `stack[level]` walks, by following each
/// ancestor frame's already-fixed `index` down into its own `Repeat`
/// node's `blocks` field. Only ever called with indices for levels whose
/// frames were pushed by `advance` itself, so every ancestor index is
/// guaranteed to currently point at a `Block::Repeat`.
fn siblings_at<'a>(plan: &'a [Block], stack: &[Frame], level: usize) -> &'a [Block] {
    let mut current: &[Block] = plan;
    for frame in &stack[..level] {
        match &current[frame.index as usize] {
            Block::Repeat { blocks, .. } => current = blocks,
            Block::Phase { .. } => {
                unreachable!("a non-root frame's owning index always points at a Repeat block")
            }
        }
    }
    current
}

impl Cursor {
    /// Builds a cursor positioned at `plan`'s first reachable leaf phase.
    /// `None` if the plan has none (an all-empty/zero-count tree) —
    /// callers are expected to have already run `validate(plan)` to rule
    /// this out ahead of time, but `start` doesn't assume that.
    pub fn start(plan: &Plan) -> Option<Cursor> {
        let mut cursor = Cursor {
            stack: vec![Frame {
                index: -1,
                remaining: RepeatCount::Finite(1),
            }],
        };
        match cursor.advance(plan) {
            AdvanceOutcome::Next => Some(cursor),
            AdvanceOutcome::Complete => None,
        }
    }

    /// Re-runs `start` and replaces `self` with the result, discarding all
    /// progress including any partially-consumed repeat counts. `None`
    /// (leaving `self` untouched) only if the plan has no reachable phase
    /// at all.
    pub fn reset(&mut self, plan: &Plan) -> Option<()> {
        let fresh = Self::start(plan)?;
        *self = fresh;
        Some(())
    }

    pub fn is_complete(&self) -> bool {
        self.stack.is_empty()
    }

    /// The leaf phase the cursor currently sits on, or `None` once
    /// `is_complete()`.
    pub fn current_phase<'a>(&self, plan: &'a Plan) -> Option<CurrentPhase<'a>> {
        let level = self.stack.len().checked_sub(1)?;
        let siblings = siblings_at(plan, &self.stack, level);
        let idx = self.stack[level].index;
        if idx < 0 {
            return None;
        }
        match siblings.get(idx as usize)? {
            Block::Phase {
                kind,
                minutes,
                label,
            } => Some(CurrentPhase {
                kind: *kind,
                minutes: *minutes,
                label: label.as_deref(),
            }),
            Block::Repeat { .. } => None,
        }
    }

    /// Moves to the next leaf phase, walking through/out of any `Repeat`
    /// nesting as needed. See the module doc comment — this is the single
    /// algorithm both natural phase expiry and a user "skip" call into.
    pub fn advance(&mut self, plan: &Plan) -> AdvanceOutcome {
        loop {
            let Some(level) = self.stack.len().checked_sub(1) else {
                return AdvanceOutcome::Complete;
            };
            let siblings = siblings_at(plan, &self.stack, level);
            self.stack[level].index += 1;
            let idx = self.stack[level].index;

            if idx >= 0 && (idx as usize) < siblings.len() {
                match &siblings[idx as usize] {
                    Block::Phase { .. } => return AdvanceOutcome::Next,
                    Block::Repeat { count, .. } => {
                        if matches!(count, RepeatCount::Finite(0)) {
                            // Contributes nothing — leave this level's
                            // index where it is; the next loop pass
                            // advances past it to the following sibling.
                            continue;
                        }
                        self.stack.push(Frame {
                            index: -1,
                            remaining: *count,
                        });
                        continue;
                    }
                }
            }

            // Ran off the end of this level's blocks for the current pass.
            if level == 0 {
                self.stack.clear();
                return AdvanceOutcome::Complete;
            }
            match &mut self.stack[level].remaining {
                RepeatCount::Finite(n) => {
                    if *n <= 1 {
                        self.stack.pop();
                    } else {
                        *n -= 1;
                        self.stack[level].index = -1;
                    }
                }
                RepeatCount::Infinite => {
                    self.stack[level].index = -1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(minutes: u32) -> Block {
        Block::Phase {
            kind: PhaseKind::Work,
            minutes,
            label: None,
        }
    }
    fn brk(minutes: u32) -> Block {
        Block::Phase {
            kind: PhaseKind::Break,
            minutes,
            label: None,
        }
    }
    fn long_break(minutes: u32) -> Block {
        Block::Phase {
            kind: PhaseKind::LongBreak,
            minutes,
            label: None,
        }
    }
    fn repeat(count: RepeatCount, blocks: Vec<Block>) -> Block {
        Block::Repeat { count, blocks }
    }

    fn phases(mut cursor: Cursor, plan: &Plan, n: usize) -> Vec<Option<(PhaseKind, u32)>> {
        let mut out = Vec::new();
        for _ in 0..n {
            match cursor.current_phase(plan) {
                Some(p) => out.push(Some((p.kind, p.minutes))),
                None => out.push(None),
            }
            cursor.advance(plan);
        }
        out
    }

    // --- TOML round-trip: highest-risk unknown, verified first. ---

    #[test]
    fn test_standard_pomodoro_plan_round_trips_through_toml() {
        let plan: Plan = vec![repeat(
            RepeatCount::Infinite,
            vec![
                repeat(RepeatCount::Finite(3), vec![work(45), brk(15)]),
                long_break(30),
            ],
        )];

        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            plan: Plan,
        }
        let wrapper = Wrapper { plan: plan.clone() };
        let toml_str = toml::to_string_pretty(&wrapper).expect("serialize");
        assert!(toml_str.contains("count = \"infinite\""));
        assert!(toml_str.contains("count = 3"));

        let parsed: Wrapper = toml::from_str(&toml_str).expect("deserialize");
        assert_eq!(parsed.plan, plan);
    }

    #[test]
    fn test_repeat_count_rejects_bogus_string() {
        #[derive(Debug, Deserialize)]
        struct Wrapper {
            #[allow(dead_code)]
            count: RepeatCount,
        }
        let err = toml::from_str::<Wrapper>("count = \"bogus\"").unwrap_err();
        assert!(err.to_string().contains("infinite"));
    }

    // --- Cursor walk: the four required example shapes. ---

    #[test]
    fn test_simplest_infinite_work_break_never_completes() {
        let plan: Plan = vec![repeat(RepeatCount::Infinite, vec![work(45), brk(15)])];
        let cursor = Cursor::start(&plan).expect("reachable");
        let seq = phases(cursor, &plan, 50);
        for (i, p) in seq.iter().enumerate() {
            let expected = if i % 2 == 0 {
                (PhaseKind::Work, 45)
            } else {
                (PhaseKind::Break, 15)
            };
            assert_eq!(*p, Some(expected), "at index {i}");
        }
    }

    #[test]
    fn test_standard_nested_pomodoro_cycles_through_long_break_and_resets_inner_count() {
        let plan: Plan = vec![repeat(
            RepeatCount::Infinite,
            vec![
                repeat(RepeatCount::Finite(3), vec![work(45), brk(15)]),
                long_break(30),
            ],
        )];
        let cursor = Cursor::start(&plan).expect("reachable");
        let seq = phases(cursor, &plan, 14);
        let expected = [
            (PhaseKind::Work, 45),
            (PhaseKind::Break, 15),
            (PhaseKind::Work, 45),
            (PhaseKind::Break, 15),
            (PhaseKind::Work, 45),
            (PhaseKind::Break, 15),
            (PhaseKind::LongBreak, 30),
            // outer loop restarts, inner 3-count resets
            (PhaseKind::Work, 45),
            (PhaseKind::Break, 15),
            (PhaseKind::Work, 45),
            (PhaseKind::Break, 15),
            (PhaseKind::Work, 45),
            (PhaseKind::Break, 15),
            (PhaseKind::LongBreak, 30),
        ];
        for (i, e) in expected.iter().enumerate() {
            assert_eq!(seq[i], Some(*e), "at index {i}");
        }
    }

    #[test]
    fn test_doubly_nested_two_big_cycles_then_one_long_break_completes() {
        // "2 big cycles" (each: 3x work/break -> 30min break) then one
        // final 60min break, then the plan ends.
        let big_cycle = repeat(
            RepeatCount::Finite(3),
            vec![work(45), brk(15)],
        );
        let group = repeat(RepeatCount::Finite(2), vec![big_cycle, long_break(30)]);
        let plan: Plan = vec![group, long_break(60)];

        let cursor = Cursor::start(&plan).expect("reachable");
        let seq = phases(cursor, &plan, 17);
        let expected = [
            Some((PhaseKind::Work, 45)),
            Some((PhaseKind::Break, 15)),
            Some((PhaseKind::Work, 45)),
            Some((PhaseKind::Break, 15)),
            Some((PhaseKind::Work, 45)),
            Some((PhaseKind::Break, 15)),
            Some((PhaseKind::LongBreak, 30)),
            Some((PhaseKind::Work, 45)),
            Some((PhaseKind::Break, 15)),
            Some((PhaseKind::Work, 45)),
            Some((PhaseKind::Break, 15)),
            Some((PhaseKind::Work, 45)),
            Some((PhaseKind::Break, 15)),
            Some((PhaseKind::LongBreak, 30)),
            Some((PhaseKind::LongBreak, 60)),
            None, // Complete
            None, // still Complete — idempotent
        ];
        assert_eq!(seq, expected);
    }

    #[test]
    fn test_fully_explicit_finite_decreasing_break_list_completes_in_order() {
        // No Repeat nodes at all: 8 manually-written work/break pairs with
        // a shrinking break, then the plan just ends.
        let mut plan: Plan = Vec::new();
        for i in 0..8u32 {
            plan.push(work(40));
            plan.push(brk(20 - i * 2));
        }

        let cursor = Cursor::start(&plan).expect("reachable");
        let seq = phases(cursor, &plan, 17);
        for i in 0..8usize {
            assert_eq!(seq[2 * i], Some((PhaseKind::Work, 40)), "work {i}");
            assert_eq!(
                seq[2 * i + 1],
                Some((PhaseKind::Break, 20 - i as u32 * 2)),
                "break {i}"
            );
        }
        assert_eq!(seq[16], None, "plan should be complete after 16 phases");
    }

    #[test]
    fn test_reset_mid_walk_discards_progress_including_partial_repeat_counts() {
        let plan: Plan = vec![repeat(RepeatCount::Finite(3), vec![work(45), brk(15)])];
        let mut cursor = Cursor::start(&plan).expect("reachable");
        // Consume most of the plan first.
        for _ in 0..4 {
            cursor.advance(&plan);
        }
        assert!(cursor.current_phase(&plan).is_some());

        cursor.reset(&plan).expect("reachable");
        assert_eq!(
            cursor.current_phase(&plan).map(|p| (p.kind, p.minutes)),
            Some((PhaseKind::Work, 45)),
            "reset should return to the very first phase"
        );
    }

    #[test]
    fn test_skip_and_natural_expiry_share_the_same_advance_call() {
        // No separate code path exists for "skip" vs "expired" — both are
        // literally just a call to advance(); this test only documents
        // that fact by exercising the shared function directly.
        let plan: Plan = vec![work(1), brk(1)];
        let mut cursor = Cursor::start(&plan).expect("reachable");
        assert_eq!(
            cursor.current_phase(&plan).map(|p| p.kind),
            Some(PhaseKind::Work)
        );
        assert_eq!(cursor.advance(&plan), AdvanceOutcome::Next);
        assert_eq!(
            cursor.current_phase(&plan).map(|p| p.kind),
            Some(PhaseKind::Break)
        );
    }

    #[test]
    fn test_validate_rejects_empty_plan() {
        assert_eq!(validate(&Vec::new()), Err(PlanError::Empty));
    }

    #[test]
    fn test_validate_rejects_zero_minute_phase() {
        let plan: Plan = vec![work(0)];
        assert_eq!(validate(&plan), Err(PlanError::ZeroMinutePhase));
    }

    #[test]
    fn test_validate_rejects_unreachable_plan() {
        let plan: Plan = vec![repeat(RepeatCount::Finite(0), vec![work(45)])];
        assert_eq!(validate(&plan), Err(PlanError::NoReachablePhase));
    }

    #[test]
    fn test_validate_accepts_all_four_example_shapes() {
        let simplest: Plan = vec![repeat(RepeatCount::Infinite, vec![work(45), brk(15)])];
        assert!(validate(&simplest).is_ok());

        let standard: Plan = vec![repeat(
            RepeatCount::Infinite,
            vec![
                repeat(RepeatCount::Finite(3), vec![work(45), brk(15)]),
                long_break(30),
            ],
        )];
        assert!(validate(&standard).is_ok());

        let doubly_nested: Plan = vec![
            repeat(
                RepeatCount::Finite(2),
                vec![
                    repeat(RepeatCount::Finite(3), vec![work(45), brk(15)]),
                    long_break(30),
                ],
            ),
            long_break(60),
        ];
        assert!(validate(&doubly_nested).is_ok());

        let mut explicit: Plan = Vec::new();
        for i in 0..8u32 {
            explicit.push(work(40));
            explicit.push(brk(20 - i * 2));
        }
        assert!(validate(&explicit).is_ok());
    }

    #[test]
    fn test_finite_zero_repeat_is_skipped_entirely() {
        let plan: Plan = vec![
            work(10),
            repeat(RepeatCount::Finite(0), vec![brk(999)]),
            brk(5),
        ];
        let cursor = Cursor::start(&plan).expect("reachable");
        let seq = phases(cursor, &plan, 3);
        assert_eq!(seq[0], Some((PhaseKind::Work, 10)));
        assert_eq!(seq[1], Some((PhaseKind::Break, 5)));
        assert_eq!(seq[2], None);
    }
}
