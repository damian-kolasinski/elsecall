//! A request's properties from every layer that may set them, in precedence
//! order: command-line flags, then the caller's environment, then the first label
//! rule of `.elsecall.toml` that matches the label, then defaults. The result says
//! where each property came from, so `explain` can show it.
use crate::{
    admission_cli::{
        environment::CallerEnvironment,
        options::{Declared, UrgencyChoice},
    },
    consumer_configuration::{
        ConsumerConfiguration, LabelRule, RulePlacement, RuleUrgency, WhenUnavailable,
    },
    failure::Result,
    scheduling::{
        request::{AdmissionRequest, Kind, Placement, Urgency, invalid},
        settings::{AppliedRule, Settings, Source},
        timestamp::Timestamp,
    },
};

/// Everything a request's properties may come from.
pub struct Layers<'a> {
    pub kind: Kind,
    /// The lease holder; runs belong to the requesting process.
    pub holder_pid: Option<u32>,
    pub cli: &'a Declared,
    /// The placement flags of `run`; leases are always pinned.
    pub placement: Option<Placement>,
    pub when_unavailable: Option<WhenUnavailable>,
    pub environment: &'a CallerEnvironment,
    pub configuration: &'a ConsumerConfiguration,
}

#[derive(Debug, PartialEq)]
pub struct Resolved {
    /// The validated request, with `settings` saying where each property came from.
    pub request: AdmissionRequest,
    pub when_unavailable: WhenUnavailable,
}

/// The first value present, with its source; `Default` when none is.
fn first<T, const N: usize>(candidates: [(Option<T>, Source); N]) -> (Option<T>, Source) {
    candidates
        .into_iter()
        .find(|(value, _)| value.is_some())
        .unwrap_or((None, Source::Default))
}

fn named(rule: &LabelRule) -> String {
    format!(
        "rule {} (match = {:?}) in .elsecall.toml",
        rule.number, rule.pattern
    )
}

pub fn resolve(layers: Layers<'_>, now: Timestamp) -> Result<Resolved> {
    let Layers {
        kind,
        holder_pid,
        cli,
        placement,
        when_unavailable,
        environment,
        configuration,
    } = layers;
    let rule = cli
        .label
        .as_deref()
        .and_then(|label| configuration.rule_for(label));
    let (size, size_source) = first([
        (cli.size, Source::Cli),
        (rule.and_then(|r| r.size), Source::Rule),
    ]);

    let rule_urgency = rule.and_then(|r| r.urgency);
    let (choice, urgency_source) = match (cli.urgency, rule_urgency) {
        (Some(choice), _) => (choice, Source::Cli),
        (None, Some(RuleUrgency::Interactive { .. })) => (UrgencyChoice::Interactive, Source::Rule),
        (None, Some(RuleUrgency::Deferrable { .. })) => (UrgencyChoice::Deferrable, Source::Rule),
        (None, None) => (UrgencyChoice::Interactive, Source::Default),
    };
    let mut settings = Settings {
        rule: rule.map(|rule| AppliedRule {
            number: rule.number,
            pattern: rule.pattern.clone(),
        }),
        size: size_source,
        urgency: urgency_source,
        hold_limit: None,
        deadline: None,
        at_deadline: None,
        placement: Source::Default,
    };
    let urgency = match choice {
        UrgencyChoice::Interactive => {
            if cli.deadline.is_some() || cli.at_deadline.is_some() {
                return Err(invalid(
                    "--deadline and --at-deadline apply only with --deferrable.",
                ));
            }
            let rule_limit = match rule_urgency {
                Some(RuleUrgency::Interactive { hold_limit }) => hold_limit,
                _ => None,
            };
            let (limit, source) = first([
                (cli.hold_limit, Source::Cli),
                (environment.hold_limit, Source::Environment),
                (rule_limit, Source::Rule),
            ]);
            settings.hold_limit = Some(source);
            Urgency::Interactive {
                hold_limit_ms: limit
                    .map(|limit| limit.as_millis().min(u128::from(u64::MAX)) as u64),
            }
        }
        UrgencyChoice::Deferrable => {
            if cli.hold_limit.is_some() {
                return Err(invalid(match (urgency_source, rule) {
                    (Source::Rule, Some(rule)) => format!(
                        "--hold-limit applies to interactive work, and {} makes this label deferrable; add --interactive to wait as interactive work.",
                        named(rule)
                    ),
                    _ => {
                        "--hold-limit applies to interactive work; deferrable work uses --deadline."
                            .into()
                    }
                }));
            }
            let (rule_deadline, rule_action) = match rule_urgency {
                Some(RuleUrgency::Deferrable {
                    deadline,
                    at_deadline,
                }) => (Some(now.after(deadline)), Some(at_deadline)),
                _ => (None, None),
            };
            let (deadline, deadline_source) =
                first([(cli.deadline, Source::Cli), (rule_deadline, Source::Rule)]);
            let (action, action_source) =
                first([(cli.at_deadline, Source::Cli), (rule_action, Source::Rule)]);
            let (Some(deadline), Some(at_deadline)) = (deadline, action) else {
                return Err(invalid(
                    "Deferrable work needs --deadline and --at-deadline start|skip.",
                ));
            };
            settings.deadline = Some(deadline_source);
            settings.at_deadline = Some(action_source);
            Urgency::Deferrable {
                deadline,
                at_deadline,
            }
        }
    };

    let (placement, placement_source) = match (kind, placement, rule.and_then(|r| r.placement)) {
        // A lease holds capacity here for the caller's own work, whatever its rule says.
        (Kind::Lease, _, _) => (Placement::Pinned, Source::Default),
        (Kind::Run, Some(placement), _) => (placement, Source::Cli),
        (Kind::Run, None, Some(RulePlacement::Pinned)) => (Placement::Pinned, Source::Rule),
        (Kind::Run, None, Some(RulePlacement::Placeable)) => (Placement::Placeable, Source::Rule),
        (Kind::Run, None, None) => (Placement::Pinned, Source::Default),
    };
    settings.placement = placement_source;
    if let (Urgency::Deferrable { .. }, Placement::Placeable, Source::Rule, Some(rule)) =
        (&urgency, &placement, placement_source, rule)
    {
        return Err(invalid(format!(
            "Deferrable work is pinned only, and {} makes this label placeable; add --pinned.",
            named(rule)
        )));
    }

    let request = AdmissionRequest {
        kind,
        holder_pid,
        size,
        label: cli.label.clone(),
        workload: cli.workload.clone(),
        placement,
        urgency,
        workspace: None,
        settings: Some(settings),
    };
    // Validate before contacting the service, so mistakes fail fast.
    request.clone().validate(now)?;
    Ok(Resolved {
        request,
        when_unavailable: self::when_unavailable(when_unavailable, environment, configuration),
    })
}

/// What to do when the service cannot be reached: the flag, else
/// `ELSECALL_WHEN_UNAVAILABLE`, else `[admission] when_unavailable`, else fail.
pub fn when_unavailable(
    flag: Option<WhenUnavailable>,
    environment: &CallerEnvironment,
    configuration: &ConsumerConfiguration,
) -> WhenUnavailable {
    flag.or(environment.when_unavailable)
        .or(configuration.when_unavailable)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduling::{capacity::Cores, request::AtDeadline};
    use std::time::Duration;

    fn now() -> Timestamp {
        Timestamp::from_millis(1_000_000)
    }

    fn configuration() -> ConsumerConfiguration {
        ConsumerConfiguration::parse(
            r#"
            [admission]
            when_unavailable = "run"
            [[labels]]
            match = "build *"
            size = 4
            hold_limit = "5m"
            placement = "placeable"
            [[labels]]
            match = "warm *"
            size = 2
            urgency = "deferrable"
            deadline = "30m"
            at_deadline = "start"
            [[labels]]
            match = "*"
            size = 1.5
            "#,
        )
        .unwrap()
    }

    struct Case {
        kind: Kind,
        cli: Declared,
        placement: Option<Placement>,
        when_unavailable: Option<WhenUnavailable>,
        environment: CallerEnvironment,
        configuration: ConsumerConfiguration,
    }
    impl Default for Case {
        fn default() -> Self {
            Self {
                kind: Kind::Run,
                cli: Declared::default(),
                placement: None,
                when_unavailable: None,
                environment: CallerEnvironment::default(),
                configuration: configuration(),
            }
        }
    }
    fn label(text: &str) -> Declared {
        Declared {
            label: Some(text.into()),
            ..Default::default()
        }
    }
    fn resolve_case(case: Case) -> Result<Resolved> {
        resolve(
            Layers {
                kind: case.kind,
                holder_pid: (case.kind == Kind::Lease).then_some(1),
                cli: &case.cli,
                placement: case.placement,
                when_unavailable: case.when_unavailable,
                environment: &case.environment,
                configuration: &case.configuration,
            },
            now(),
        )
    }
    fn settings(resolved: &Resolved) -> &Settings {
        resolved.request.settings.as_ref().unwrap()
    }

    #[test]
    fn the_first_matching_rule_supplies_what_the_caller_left_out() {
        let build = resolve_case(Case {
            cli: label("build App"),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(build.request.size, Some(Cores::from_millis(4000)));
        assert_eq!(build.request.placement, Placement::Placeable);
        assert_eq!(
            build.request.urgency,
            Urgency::Interactive {
                hold_limit_ms: Some(300_000)
            }
        );
        assert_eq!(
            settings(&build),
            &Settings {
                rule: Some(AppliedRule {
                    number: 1,
                    pattern: "build *".into()
                }),
                size: Source::Rule,
                urgency: Source::Rule,
                hold_limit: Some(Source::Rule),
                deadline: None,
                at_deadline: None,
                placement: Source::Rule,
            }
        );
        assert_eq!(build.when_unavailable, WhenUnavailable::Run);

        let warm = resolve_case(Case {
            cli: label("warm Cache"),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            warm.request.urgency,
            Urgency::Deferrable {
                deadline: now().after(Duration::from_secs(1800)),
                at_deadline: AtDeadline::Start
            }
        );
        assert_eq!(settings(&warm).deadline, Some(Source::Rule));
        assert_eq!(settings(&warm).hold_limit, None);
        assert_eq!(warm.request.placement, Placement::Pinned);
        assert_eq!(settings(&warm).placement, Source::Default);

        let other = resolve_case(Case {
            cli: label("lint"),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(settings(&other).rule.as_ref().unwrap().number, 3);
        assert_eq!(settings(&other).urgency, Source::Default);
    }

    #[test]
    fn without_a_label_or_a_match_defaults_apply() {
        let resolved = resolve_case(Case {
            configuration: ConsumerConfiguration::default(),
            cli: label("build App"),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resolved.request.size, None);
        assert_eq!(resolved.request.placement, Placement::Pinned);
        assert_eq!(
            resolved.request.urgency,
            Urgency::Interactive {
                hold_limit_ms: None
            }
        );
        assert_eq!(
            settings(&resolved),
            &Settings {
                rule: None,
                size: Source::Default,
                urgency: Source::Default,
                hold_limit: Some(Source::Default),
                deadline: None,
                at_deadline: None,
                placement: Source::Default,
            }
        );
        assert_eq!(resolved.when_unavailable, WhenUnavailable::Fail);
        let unlabelled = resolve_case(Case::default()).unwrap();
        assert_eq!(settings(&unlabelled).rule, None, "rules match labels only");
    }

    #[test]
    fn flags_override_the_environment_which_overrides_rules() {
        let environment = CallerEnvironment {
            hold_limit: Some(Duration::from_secs(60)),
            when_unavailable: Some(WhenUnavailable::Fail),
            ..Default::default()
        };
        let from_environment = resolve_case(Case {
            cli: label("build App"),
            environment,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            from_environment.request.urgency,
            Urgency::Interactive {
                hold_limit_ms: Some(60_000)
            }
        );
        assert_eq!(
            settings(&from_environment).hold_limit,
            Some(Source::Environment)
        );
        assert_eq!(from_environment.when_unavailable, WhenUnavailable::Fail);

        let from_flags = resolve_case(Case {
            cli: Declared {
                size: Some(Cores::from_millis(2000)),
                hold_limit: Some(Duration::from_secs(10)),
                ..label("build App")
            },
            placement: Some(Placement::Pinned),
            when_unavailable: Some(WhenUnavailable::Run),
            environment,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(from_flags.request.size, Some(Cores::from_millis(2000)));
        assert_eq!(from_flags.request.placement, Placement::Pinned);
        assert_eq!(
            from_flags.request.urgency,
            Urgency::Interactive {
                hold_limit_ms: Some(10_000)
            }
        );
        let sources = settings(&from_flags);
        assert_eq!(
            (sources.size, sources.hold_limit, sources.placement),
            (Source::Cli, Some(Source::Cli), Source::Cli)
        );
        assert_eq!(from_flags.when_unavailable, WhenUnavailable::Run);
    }

    #[test]
    fn urgency_flags_override_a_rules_urgency() {
        let interactive = resolve_case(Case {
            cli: Declared {
                urgency: Some(UrgencyChoice::Interactive),
                ..label("warm Cache")
            },
            environment: CallerEnvironment {
                hold_limit: Some(Duration::from_secs(45)),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            interactive.request.urgency,
            Urgency::Interactive {
                hold_limit_ms: Some(45_000)
            }
        );
        assert_eq!(settings(&interactive).urgency, Source::Cli);
        assert_eq!(interactive.request.size, Some(Cores::from_millis(2000)));

        let sooner = resolve_case(Case {
            cli: Declared {
                deadline: Some(now().after(Duration::from_secs(60))),
                ..label("warm Cache")
            },
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            sooner.request.urgency,
            Urgency::Deferrable {
                deadline: now().after(Duration::from_secs(60)),
                at_deadline: AtDeadline::Start
            },
            "a deadline flag overrides the rule's deadline and keeps its action"
        );
        let sources = settings(&sooner);
        assert_eq!(
            (sources.deadline, sources.at_deadline),
            (Some(Source::Cli), Some(Source::Rule))
        );

        let deferred_build = resolve_case(Case {
            cli: Declared {
                urgency: Some(UrgencyChoice::Deferrable),
                deadline: Some(now().after(Duration::from_secs(60))),
                at_deadline: Some(AtDeadline::Skip),
                ..label("build App")
            },
            placement: Some(Placement::Pinned),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(settings(&deferred_build).hold_limit, None);
    }

    #[test]
    fn the_environment_hold_limit_never_makes_deferrable_work_invalid() {
        let warm = resolve_case(Case {
            cli: label("warm Cache"),
            environment: CallerEnvironment {
                hold_limit: Some(Duration::from_secs(45)),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        assert!(matches!(warm.request.urgency, Urgency::Deferrable { .. }));
    }

    #[test]
    fn contradictions_fail_with_a_hint_that_names_the_rule() {
        let failure = |case: Case| resolve_case(case).unwrap_err();
        let hold = failure(Case {
            cli: Declared {
                hold_limit: Some(Duration::from_secs(60)),
                ..label("warm Cache")
            },
            ..Default::default()
        });
        assert_eq!(hold.code, "invalid_request");
        assert!(
            hold.message
                .contains("rule 2 (match = \"warm *\") in .elsecall.toml")
                && hold.message.contains("--interactive"),
            "{}",
            hold.message
        );
        let placeable = failure(Case {
            cli: Declared {
                urgency: Some(UrgencyChoice::Deferrable),
                deadline: Some(now().after(Duration::from_secs(60))),
                at_deadline: Some(AtDeadline::Skip),
                ..label("build App")
            },
            ..Default::default()
        });
        assert!(
            placeable.message.contains("rule 1") && placeable.message.contains("--pinned"),
            "{}",
            placeable.message
        );
        let incomplete = failure(Case {
            cli: Declared {
                urgency: Some(UrgencyChoice::Deferrable),
                deadline: Some(now().after(Duration::from_secs(60))),
                ..label("lint")
            },
            ..Default::default()
        });
        assert!(incomplete.message.contains("--at-deadline"));
        let stray = failure(Case {
            cli: Declared {
                deadline: Some(now().after(Duration::from_secs(60))),
                ..label("build App")
            },
            ..Default::default()
        });
        assert!(stray.message.contains("only with --deferrable"));
    }

    #[test]
    fn leases_stay_pinned_whatever_the_rule_says() {
        let lease = resolve_case(Case {
            kind: Kind::Lease,
            cli: label("build App"),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(lease.request.placement, Placement::Pinned);
        assert_eq!(lease.request.size, Some(Cores::from_millis(4000)));
        assert_eq!(settings(&lease).placement, Source::Default);
    }
}
