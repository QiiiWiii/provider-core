use provider_core::{
    ProviderKind, ProviderQuotaView, QuotaAmount, QuotaGroupScope, QuotaMetric, QuotaMetricKind,
    QuotaPeriodKind, QuotaUnit, QuotaWindowEstimate,
};
use provider_usage::{AccountWindowUsage, TimeRange, UsdAtoms};

use super::ManagementState;

const MIN_USED_PERCENT_HUNDREDTHS: u64 = 500;
const MS_PER_SECOND: i64 = 1000;

pub(super) async fn attach_quota_estimates(
    state: &ManagementState,
    mut quota: ProviderQuotaView,
) -> ProviderQuotaView {
    let Some(usage) = state.usage.as_ref() else {
        return quota;
    };
    let Some(snapshot) = quota.snapshot.as_mut() else {
        return quota;
    };
    if !matches!(snapshot.provider, ProviderKind::Grok | ProviderKind::Codex) {
        return quota;
    }

    let account_id = snapshot.account_id.clone();
    let provider = snapshot.provider;
    let as_of = snapshot.last_observed_at.unwrap_or(snapshot.fetched_at);
    let Some(metric) = primary_usage_metric(snapshot) else {
        return quota;
    };
    let Some(range) = estimate_range(provider, metric, as_of) else {
        return quota;
    };
    let Some(used_hundredths) = used_percent_hundredths(metric) else {
        return quota;
    };
    let Ok(observed) = usage.query.account_window_usage(&account_id, range).await else {
        return quota;
    };
    metric.estimate = window_estimate(range, used_hundredths, observed);
    quota
}

fn primary_usage_metric(
    snapshot: &mut provider_core::ProviderQuotaSnapshot,
) -> Option<&mut QuotaMetric> {
    let group = snapshot
        .groups
        .iter_mut()
        .find(|group| group.scope == QuotaGroupScope::Aggregate)?;
    group
        .metrics
        .iter_mut()
        .find(|metric| metric.kind == QuotaMetricKind::Usage && metric.unit == QuotaUnit::Percent)
}

fn estimate_range(provider: ProviderKind, metric: &QuotaMetric, as_of: i64) -> Option<TimeRange> {
    if metric.kind != QuotaMetricKind::Usage || metric.unit != QuotaUnit::Percent {
        return None;
    }
    let period = metric.period.as_ref()?;
    let (start_secs, end_secs) = match provider {
        ProviderKind::Codex => {
            if period.kind != QuotaPeriodKind::Rolling {
                return None;
            }
            let duration = period.duration_seconds.filter(|value| *value > 0)?;
            let ends_at = period.ends_at?;
            let starts_at = ends_at.checked_sub(duration)?;
            (starts_at, as_of.min(ends_at))
        }
        ProviderKind::Grok => {
            if !matches!(
                period.kind,
                QuotaPeriodKind::Weekly | QuotaPeriodKind::Monthly
            ) {
                return None;
            }
            let starts_at = period.starts_at?;
            let ends_at = period.ends_at.unwrap_or(as_of);
            (starts_at, as_of.min(ends_at))
        }
        ProviderKind::Antigravity
        | ProviderKind::OpenAiCompatible
        | ProviderKind::AnthropicCompatible => return None,
    };
    let from_ms = start_secs.checked_mul(MS_PER_SECOND)?;
    let to_ms = end_secs.checked_mul(MS_PER_SECOND)?;
    TimeRange::new(from_ms, to_ms).ok()
}

fn used_percent_hundredths(metric: &QuotaMetric) -> Option<u64> {
    let used = match metric.used.as_ref()? {
        QuotaAmount::Integer(value) if *value >= 0 => *value as f64,
        QuotaAmount::Decimal(value) => *value,
        QuotaAmount::DecimalString(value) => value.parse().ok()?,
        QuotaAmount::Integer(_) => return None,
    };
    if !used.is_finite() || used < 5.0 || used > 100.0 {
        return None;
    }
    let hundredths = (used * 100.0).round();
    if hundredths < MIN_USED_PERCENT_HUNDREDTHS as f64 || hundredths > 10_000.0 {
        return None;
    }
    Some(hundredths as u64)
}

fn window_estimate(
    range: TimeRange,
    used_hundredths: u64,
    observed: AccountWindowUsage,
) -> Option<QuotaWindowEstimate> {
    let estimated_limit_tokens = scale_observed(u128::from(observed.tokens), used_hundredths)
        .and_then(|value| u64::try_from(value).ok());
    let estimated_limit_cost_usd = observed.complete_cost_atoms().and_then(|atoms| {
        let estimated = scale_observed(atoms.as_atoms().try_into().ok()?, used_hundredths)?;
        let estimated = i128::try_from(estimated).ok()?;
        Some(UsdAtoms::from_atoms(estimated).to_decimal_string())
    });
    if estimated_limit_tokens.is_none() && estimated_limit_cost_usd.is_none() {
        return None;
    }
    Some(QuotaWindowEstimate {
        window_start: range.from_ms / MS_PER_SECOND,
        window_end: range.to_ms / MS_PER_SECOND,
        observed_tokens: (observed.tokens > 0).then_some(observed.tokens),
        estimated_limit_tokens,
        observed_cost_usd: observed
            .complete_cost_atoms()
            .filter(|atoms| atoms.as_atoms() > 0)
            .map(UsdAtoms::to_decimal_string),
        estimated_limit_cost_usd,
    })
}

fn scale_observed(observed: u128, used_hundredths: u64) -> Option<u128> {
    if observed == 0 || used_hundredths == 0 {
        return None;
    }
    observed
        .checked_mul(10_000)?
        .checked_add(u128::from(used_hundredths) / 2)?
        .checked_div(u128::from(used_hundredths))
}

#[cfg(test)]
mod tests {
    use provider_core::{QuotaPeriod, QuotaPeriodKind};

    use super::*;

    fn percent_metric(used: f64, period: QuotaPeriod) -> QuotaMetric {
        QuotaMetric {
            key: "primary".to_owned(),
            kind: QuotaMetricKind::Usage,
            unit: QuotaUnit::Percent,
            used: Some(QuotaAmount::Decimal(used)),
            remaining: Some(QuotaAmount::Decimal((100.0 - used).max(0.0))),
            limit: Some(QuotaAmount::Decimal(100.0)),
            period: Some(period),
            breakdown: Vec::new(),
            estimate: None,
        }
    }

    #[test]
    fn codex_rolling_window_starts_at_reset_minus_duration() {
        let metric = percent_metric(
            20.0,
            QuotaPeriod {
                kind: QuotaPeriodKind::Rolling,
                starts_at: None,
                ends_at: Some(1_800),
                duration_seconds: Some(1_800),
            },
        );
        let range = estimate_range(ProviderKind::Codex, &metric, 1_200).expect("range");
        assert_eq!(range.from_ms, 0);
        assert_eq!(range.to_ms, 1_200_000);
    }

    #[test]
    fn grok_weekly_window_uses_period_start() {
        let metric = percent_metric(
            40.0,
            QuotaPeriod {
                kind: QuotaPeriodKind::Weekly,
                starts_at: Some(100),
                ends_at: Some(700),
                duration_seconds: None,
            },
        );
        let range = estimate_range(ProviderKind::Grok, &metric, 250).expect("range");
        assert_eq!(range.from_ms, 100_000);
        assert_eq!(range.to_ms, 250_000);
    }

    #[test]
    fn antigravity_and_tiny_percent_do_not_estimate() {
        let metric = percent_metric(
            20.0,
            QuotaPeriod {
                kind: QuotaPeriodKind::Rolling,
                starts_at: None,
                ends_at: Some(1_800),
                duration_seconds: Some(1_800),
            },
        );
        assert!(estimate_range(ProviderKind::Antigravity, &metric, 1_200).is_none());
        let tiny = percent_metric(
            4.9,
            QuotaPeriod {
                kind: QuotaPeriodKind::Weekly,
                starts_at: Some(1),
                ends_at: Some(100),
                duration_seconds: None,
            },
        );
        assert!(used_percent_hundredths(&tiny).is_none());
    }

    #[test]
    fn implied_limit_is_observed_over_used_percent() {
        let range = TimeRange::new(0, 5 * 60 * 60 * 1000).expect("range");
        let estimate = window_estimate(
            range,
            2_000,
            AccountWindowUsage {
                tokens: 10,
                dispatched_attempts: 1,
                complete_cost_attempts: 1,
                cost: provider_usage::CostTotals {
                    atoms: Some(UsdAtoms::from_atoms(2_000_000)),
                },
            },
        )
        .expect("estimate");
        assert_eq!(estimate.estimated_limit_tokens, Some(50));
        assert_eq!(
            estimate.estimated_limit_cost_usd.as_deref(),
            Some("0.00000010000000")
        );
        assert_eq!(estimate.observed_tokens, Some(10));
    }

    #[test]
    fn only_the_list_quota_window_is_selected() {
        let mut snapshot = provider_core::ProviderQuotaSnapshot {
            account_id: "account-1".to_owned(),
            provider: ProviderKind::Codex,
            fetched_at: 1_200,
            last_observed_at: None,
            groups: vec![provider_core::QuotaGroup {
                key: "codex".to_owned(),
                scope: QuotaGroupScope::Aggregate,
                audience: provider_core::QuotaGroupAudience::Shared,
                attributes: Default::default(),
                metrics: vec![
                    percent_metric(
                        20.0,
                        QuotaPeriod {
                            kind: QuotaPeriodKind::Rolling,
                            starts_at: None,
                            ends_at: Some(1_800),
                            duration_seconds: Some(18_000),
                        },
                    ),
                    {
                        let mut weekly = percent_metric(
                            40.0,
                            QuotaPeriod {
                                kind: QuotaPeriodKind::Rolling,
                                starts_at: None,
                                ends_at: Some(8_000),
                                duration_seconds: Some(604_800),
                            },
                        );
                        weekly.key = "secondary".to_owned();
                        weekly
                    },
                ],
            }],
            warnings: Vec::new(),
        };
        let selected = primary_usage_metric(&mut snapshot).expect("primary");
        assert_eq!(selected.key, "primary");
        assert_eq!(
            selected
                .period
                .as_ref()
                .and_then(|period| period.duration_seconds),
            Some(18_000)
        );
    }

    #[test]
    fn missing_tokens_and_incomplete_cost_yield_no_estimate() {
        let range = TimeRange::new(0, 1000).expect("range");
        assert!(
            window_estimate(
                range,
                2_000,
                AccountWindowUsage {
                    tokens: 0,
                    dispatched_attempts: 2,
                    complete_cost_attempts: 1,
                    cost: provider_usage::CostTotals {
                        atoms: Some(UsdAtoms::from_atoms(2_000_000)),
                    },
                },
            )
            .is_none()
        );
    }
}
