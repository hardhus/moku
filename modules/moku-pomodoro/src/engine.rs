//! Persisted `[modules.pomodoro]` config: named cycle-plan profiles (each
//! a `Block` tree, from `moku-pomodoro-daemon`), which one starts by
//! default, whether phase transitions notify, and how much countdown
//! detail the TUI shows. Read/write through the same
//! `MokuConfig.modules`/`resolve_module_config` machinery every other
//! module uses — see `moku-core/src/config/schema.rs`.

use std::time::Duration;

use moku_pomodoro_daemon::{Block, PhaseKind, Plan, RepeatCount};
use serde::{Deserialize, Serialize};

use crate::model::SimplePresetForm;

/// Reserved name for the always-available, never-persisted fallback plan
/// — a completely unconfigured install can still `moku pomodoro start`
/// (or select it explicitly: `moku pomodoro start classic`) with zero
/// setup. Never appears in `PomodoroConfig.profiles`.
pub const CLASSIC_PROFILE_NAME: &str = "classic";

/// 25 min work / 5 min break, forever — the simplest possible shape, used
/// only when the user hasn't configured any real profile yet.
pub fn classic_plan() -> Plan {
    vec![Block::Repeat {
        count: RepeatCount::Infinite,
        blocks: vec![
            Block::Phase {
                kind: PhaseKind::Work,
                minutes: 25,
                label: None,
            },
            Block::Phase {
                kind: PhaseKind::Break,
                minutes: 5,
                label: None,
            },
        ],
    }]
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    pub name: String,
    pub plan: Plan,
}

/// How much countdown precision the Status screen shows. Cycled with
/// the `toggle_detail` action (default key `t`). `Full`'s digits are
/// computed from the daemon's genuinely full-precision
/// `PhaseSnapshot.remaining_secs` (`f64`, never truncated) — see
/// `format`'s doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DetailLevel {
    Coarse,
    #[default]
    Seconds,
    Centiseconds,
    Full,
}

impl DetailLevel {
    pub fn next(self) -> DetailLevel {
        match self {
            DetailLevel::Coarse => DetailLevel::Seconds,
            DetailLevel::Seconds => DetailLevel::Centiseconds,
            DetailLevel::Centiseconds => DetailLevel::Full,
            DetailLevel::Full => DetailLevel::Coarse,
        }
    }

    /// How often the TUI should re-query the daemon and redraw while this
    /// tier is active. The daemon is asked fresh every tick regardless of
    /// tier — a local named-pipe/socket round trip is sub-millisecond, so
    /// there's no need for client-side interpolation between queries just
    /// to avoid "hammering" it; ticking faster is exactly what makes the
    /// higher tiers' digits look (and actually be) live.
    pub fn tick_interval(self) -> Duration {
        match self {
            DetailLevel::Coarse => Duration::from_secs(15),
            DetailLevel::Seconds => Duration::from_secs(1),
            DetailLevel::Centiseconds => Duration::from_millis(100),
            DetailLevel::Full => Duration::from_millis(33),
        }
    }

    /// Formats a genuinely full-precision remaining-seconds value (see
    /// `moku-pomodoro-daemon`'s `PhaseSnapshot.remaining_secs`, computed
    /// via `Duration::as_secs_f64()`, never truncated to whole seconds)
    /// at this tier's precision. Because the *input* already carries real
    /// sub-second precision and is re-fetched every tick, every tier
    /// below `Full` is just "print fewer of the real digits" — none of
    /// them are ever backfilled or simulated.
    pub fn format(self, remaining_secs: f64) -> String {
        let remaining_secs = remaining_secs.max(0.0);
        match self {
            DetailLevel::Coarse => {
                format!("~{} min", (remaining_secs / 60.0).ceil() as i64)
            }
            DetailLevel::Seconds => {
                let total = remaining_secs.floor() as u64;
                format!("{:02}:{:02}", total / 60, total % 60)
            }
            DetailLevel::Centiseconds => {
                let total_ms = (remaining_secs * 1000.0).round() as u64;
                let mins = total_ms / 60_000;
                let secs = (total_ms / 1000) % 60;
                let centis = (total_ms % 1000) / 10;
                format!("{mins:02}:{secs:02}.{centis:02}")
            }
            DetailLevel::Full => format!("{remaining_secs:.9}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PomodoroConfig {
    pub profiles: Vec<Profile>,
    pub default_profile: Option<String>,
    pub notify: bool,
    pub detail: DetailLevel,
}

impl Default for PomodoroConfig {
    fn default() -> Self {
        Self {
            profiles: Vec::new(),
            default_profile: None,
            notify: true,
            detail: DetailLevel::default(),
        }
    }
}

/// Resolves which plan a `start` (CLI or TUI "quick start") should run —
/// the single code path both `moku pomodoro start [NAME]` and the
/// Profile List screen's Enter key go through, so they can never drift.
///
/// - `name` given: that profile (case-sensitive match, matching how it's
///   displayed/typed), or the reserved `"classic"` fallback, or an error
///   listing the known names.
/// - `name` omitted: `default_profile` if set and still present, else the
///   first configured profile, else the built-in classic plan.
pub fn resolve_start_plan(config: &PomodoroConfig, name: Option<&str>) -> Result<(String, Plan), String> {
    if let Some(n) = name {
        if n == CLASSIC_PROFILE_NAME {
            return Ok((CLASSIC_PROFILE_NAME.to_string(), classic_plan()));
        }
        return config
            .profiles
            .iter()
            .find(|p| p.name == n)
            .map(|p| (p.name.clone(), p.plan.clone()))
            .ok_or_else(|| {
                if config.profiles.is_empty() {
                    format!(
                        "No profile named '{n}'. No profiles are configured yet — set one up from the Pomodoro screen in the TUI, or use '{CLASSIC_PROFILE_NAME}'."
                    )
                } else {
                    let known: Vec<&str> = config.profiles.iter().map(|p| p.name.as_str()).collect();
                    format!("No profile named '{n}'. Known profiles: {}", known.join(", "))
                }
            });
    }

    if let Some(default_name) = &config.default_profile
        && let Some(p) = config.profiles.iter().find(|p| &p.name == default_name)
    {
        return Ok((p.name.clone(), p.plan.clone()));
    }
    if let Some(p) = config.profiles.first() {
        return Ok((p.name.clone(), p.plan.clone()));
    }
    Ok((CLASSIC_PROFILE_NAME.to_string(), classic_plan()))
}

/// Turns a simple-mode form into one of the canonical `Block` tree shapes
/// — the Quick Create flow's starting point for a brand-new profile,
/// which is then dropped into the full tree editor for further tweaks
/// (never a dead end — see `modules/moku-pomodoro`'s crate-level design).
pub fn build_plan_from_simple_form(form: &SimplePresetForm) -> Plan {
    let inner = vec![
        Block::Phase {
            kind: PhaseKind::Work,
            minutes: form.work_minutes.max(1),
            label: None,
        },
        Block::Phase {
            kind: PhaseKind::Break,
            minutes: form.break_minutes.max(1),
            label: None,
        },
    ];

    let body: Vec<Block> = if form.enable_long_break {
        vec![
            Block::Repeat {
                count: RepeatCount::Finite(form.cycles_before_long_break.max(1)),
                blocks: inner,
            },
            Block::Phase {
                kind: PhaseKind::LongBreak,
                minutes: form.long_break_minutes.max(1),
                label: None,
            },
        ]
    } else {
        inner
    };

    let count = if form.infinite {
        RepeatCount::Infinite
    } else {
        RepeatCount::Finite(form.total_repeats.max(1))
    };

    vec![Block::Repeat { count, blocks: body }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use moku_core::MokuConfig;

    fn profile(name: &str, plan: Plan) -> Profile {
        Profile {
            name: name.to_string(),
            plan,
        }
    }

    #[test]
    fn test_default_config_when_module_section_missing() {
        let config = MokuConfig::default();
        let pomodoro: PomodoroConfig = config.resolve_module_config("pomodoro");
        assert_eq!(pomodoro, PomodoroConfig::default());
        assert!(pomodoro.notify);
        assert_eq!(pomodoro.detail, DetailLevel::Seconds);
        assert!(pomodoro.profiles.is_empty());
        assert!(pomodoro.default_profile.is_none());
    }

    #[test]
    fn test_multi_profile_config_round_trips_through_toml() {
        let config = PomodoroConfig {
            profiles: vec![
                profile("basic", classic_plan()),
                profile(
                    "kitap",
                    vec![Block::Repeat {
                        count: RepeatCount::Infinite,
                        blocks: vec![
                            Block::Phase {
                                kind: PhaseKind::Work,
                                minutes: 30,
                                label: Some("reading".to_string()),
                            },
                            Block::Phase {
                                kind: PhaseKind::Break,
                                minutes: 10,
                                label: None,
                            },
                        ],
                    }],
                ),
            ],
            default_profile: Some("basic".to_string()),
            notify: false,
            detail: DetailLevel::Full,
        };

        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            pomodoro: PomodoroConfig,
        }
        let toml_str = toml::to_string_pretty(&Wrapper { pomodoro: config.clone() }).unwrap();
        assert!(toml_str.contains("[[pomodoro.profiles]]"));
        assert!(toml_str.contains("name = \"kitap\""));

        let parsed: Wrapper = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.pomodoro, config);
    }

    #[test]
    fn test_resolve_start_plan_by_name() {
        let config = PomodoroConfig {
            profiles: vec![profile("basic", classic_plan()), profile("kitap", classic_plan())],
            ..PomodoroConfig::default()
        };
        let (name, _) = resolve_start_plan(&config, Some("kitap")).unwrap();
        assert_eq!(name, "kitap");
    }

    #[test]
    fn test_resolve_start_plan_unknown_name_lists_known_profiles() {
        let config = PomodoroConfig {
            profiles: vec![profile("basic", classic_plan())],
            ..PomodoroConfig::default()
        };
        let err = resolve_start_plan(&config, Some("nope")).unwrap_err();
        assert!(err.contains("basic"));
    }

    #[test]
    fn test_resolve_start_plan_classic_reserved_name_always_available() {
        let config = PomodoroConfig::default();
        let (name, plan) = resolve_start_plan(&config, Some(CLASSIC_PROFILE_NAME)).unwrap();
        assert_eq!(name, CLASSIC_PROFILE_NAME);
        assert_eq!(plan, classic_plan());
    }

    #[test]
    fn test_resolve_start_plan_no_name_uses_default_profile() {
        let config = PomodoroConfig {
            profiles: vec![profile("basic", classic_plan()), profile("kitap", classic_plan())],
            default_profile: Some("kitap".to_string()),
            ..PomodoroConfig::default()
        };
        let (name, _) = resolve_start_plan(&config, None).unwrap();
        assert_eq!(name, "kitap");
    }

    #[test]
    fn test_resolve_start_plan_no_name_no_default_uses_first_profile() {
        let config = PomodoroConfig {
            profiles: vec![profile("basic", classic_plan()), profile("kitap", classic_plan())],
            ..PomodoroConfig::default()
        };
        let (name, _) = resolve_start_plan(&config, None).unwrap();
        assert_eq!(name, "basic");
    }

    #[test]
    fn test_resolve_start_plan_no_profiles_falls_back_to_classic() {
        let config = PomodoroConfig::default();
        let (name, plan) = resolve_start_plan(&config, None).unwrap();
        assert_eq!(name, CLASSIC_PROFILE_NAME);
        assert_eq!(plan, classic_plan());
    }

    #[test]
    fn test_resolve_start_plan_stale_default_profile_falls_back_to_first() {
        let config = PomodoroConfig {
            profiles: vec![profile("basic", classic_plan())],
            default_profile: Some("deleted-profile".to_string()),
            ..PomodoroConfig::default()
        };
        let (name, _) = resolve_start_plan(&config, None).unwrap();
        assert_eq!(name, "basic");
    }

    #[test]
    fn test_detail_level_cycles_through_all_four_tiers() {
        assert_eq!(DetailLevel::Coarse.next(), DetailLevel::Seconds);
        assert_eq!(DetailLevel::Seconds.next(), DetailLevel::Centiseconds);
        assert_eq!(DetailLevel::Centiseconds.next(), DetailLevel::Full);
        assert_eq!(DetailLevel::Full.next(), DetailLevel::Coarse);
    }

    #[test]
    fn test_detail_level_format_shapes() {
        assert_eq!(DetailLevel::Coarse.format(754.0), "~13 min");
        assert_eq!(DetailLevel::Seconds.format(754.0), "12:34");
        assert_eq!(DetailLevel::Centiseconds.format(754.567), "12:34.56");
        assert!(DetailLevel::Full.format(754.567891234).starts_with("754.567891"));
    }

    #[test]
    fn test_detail_level_format_never_shows_negative_time() {
        assert_eq!(DetailLevel::Seconds.format(-5.0), "00:00");
    }

    #[test]
    fn test_detail_level_tick_interval_gets_faster_at_higher_tiers() {
        assert!(DetailLevel::Coarse.tick_interval() > DetailLevel::Seconds.tick_interval());
        assert!(DetailLevel::Seconds.tick_interval() > DetailLevel::Centiseconds.tick_interval());
        assert!(DetailLevel::Centiseconds.tick_interval() > DetailLevel::Full.tick_interval());
    }

    #[test]
    fn test_simple_form_without_long_break_is_a_single_infinite_repeat() {
        let form = SimplePresetForm {
            work_minutes: 45,
            break_minutes: 15,
            enable_long_break: false,
            infinite: true,
            ..SimplePresetForm::default()
        };
        let plan = build_plan_from_simple_form(&form);
        assert_eq!(plan.len(), 1);
        let Block::Repeat { count, blocks } = &plan[0] else {
            panic!("expected a single Repeat block");
        };
        assert_eq!(*count, RepeatCount::Infinite);
        assert_eq!(blocks.len(), 2, "no long break means just work+break");
    }

    #[test]
    fn test_simple_form_with_long_break_nests_an_inner_repeat() {
        let form = SimplePresetForm {
            work_minutes: 45,
            break_minutes: 15,
            enable_long_break: true,
            cycles_before_long_break: 3,
            long_break_minutes: 30,
            infinite: true,
            ..SimplePresetForm::default()
        };
        let plan = build_plan_from_simple_form(&form);
        let Block::Repeat { blocks, .. } = &plan[0] else {
            panic!("expected outer Repeat");
        };
        assert_eq!(blocks.len(), 2, "inner repeat + long break");
        assert!(matches!(blocks[0], Block::Repeat { .. }));
        assert!(matches!(
            blocks[1],
            Block::Phase {
                kind: PhaseKind::LongBreak,
                ..
            }
        ));
    }

    #[test]
    fn test_simple_form_finite_uses_total_repeats() {
        let form = SimplePresetForm {
            infinite: false,
            total_repeats: 4,
            ..SimplePresetForm::default()
        };
        let plan = build_plan_from_simple_form(&form);
        let Block::Repeat { count, .. } = &plan[0] else {
            panic!("expected Repeat");
        };
        assert_eq!(*count, RepeatCount::Finite(4));
    }

    #[test]
    fn test_every_simple_form_shape_passes_validate() {
        for form in [
            SimplePresetForm::default(),
            SimplePresetForm {
                enable_long_break: true,
                ..SimplePresetForm::default()
            },
            SimplePresetForm {
                infinite: false,
                total_repeats: 2,
                ..SimplePresetForm::default()
            },
            SimplePresetForm {
                enable_long_break: true,
                infinite: false,
                total_repeats: 2,
                ..SimplePresetForm::default()
            },
        ] {
            let plan = build_plan_from_simple_form(&form);
            assert!(
                moku_pomodoro_daemon::validate(&plan).is_ok(),
                "form {form:?} produced an invalid plan"
            );
        }
    }
}
