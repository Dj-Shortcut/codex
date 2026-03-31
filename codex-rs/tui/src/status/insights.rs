use chrono::DateTime;
use chrono::Duration as ChronoDuration;
use chrono::Local;
use chrono::NaiveDate;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;

use super::rate_limits::RateLimitSnapshotDisplay;
use super::rate_limits::RateLimitWindowDisplay;

const HIGH_BURN_RATE_TOKENS_PER_MIN: f64 = 4_000.0;
const HIGH_CONTEXT_USED_PERCENT: i64 = 80;
const MAX_OBSERVABILITY_FILES: usize = 500;
const MAX_OBSERVABILITY_LINES_PER_FILE: usize = 20_000;

#[derive(Debug, Clone, Default)]
pub(crate) struct CompactStatusInsights {
    pub burn_rate_tpm: Option<f64>,
    pub eta_to_limit: Option<ChronoDuration>,
    pub reset_countdown: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ModelUsageSummary {
    pub model: String,
    pub tokens: i64,
    pub cached_input_tokens: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct DailyUsageSummary {
    pub day: NaiveDate,
    pub tokens: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct FullObservabilityData {
    pub today_tokens: i64,
    pub seven_day_tokens: i64,
    pub thirty_day_tokens: i64,
    pub all_time_tokens: i64,
    pub active_days: usize,
    pub current_streak_days: usize,
    pub longest_streak_days: usize,
    pub model_usage: Vec<ModelUsageSummary>,
    pub recent_days: Vec<DailyUsageSummary>,
    pub scanned_files: usize,
}

pub(crate) fn compute_compact_status_insights(
    total_usage: &TokenUsage,
    token_info: Option<&TokenUsageInfo>,
    rate_limits: &[RateLimitSnapshotDisplay],
    now: DateTime<Local>,
    session_started_at: Option<DateTime<Local>>,
) -> CompactStatusInsights {
    let burn_rate_tpm = session_started_at
        .and_then(|start| calculate_burn_rate(total_usage.blended_total(), start, now));
    let eta_to_limit = estimate_eta_to_limit(rate_limits, now);
    let reset_countdown = format_reset_countdown(rate_limits, now);
    let mut warnings = vec![];

    if max_used_percent(rate_limits).is_some_and(|used| used >= 80.0) {
        warnings.push("usage above 80% of limit".to_string());
    }
    if burn_rate_tpm.is_some_and(|rate| rate >= HIGH_BURN_RATE_TOKENS_PER_MIN) {
        warnings.push("high burn rate".to_string());
    }
    if token_info
        .and_then(|info| info.model_context_window.map(|window| (info, window)))
        .is_some_and(|(info, window)| {
            let remaining = info
                .last_token_usage
                .percent_of_context_window_remaining(window);
            let used = 100 - remaining;
            used >= HIGH_CONTEXT_USED_PERCENT
        })
    {
        warnings.push("high context usage".to_string());
    }

    CompactStatusInsights {
        burn_rate_tpm,
        eta_to_limit,
        reset_countdown,
        warnings,
    }
}

pub(crate) fn calculate_burn_rate(
    total_tokens: i64,
    started_at: DateTime<Local>,
    now: DateTime<Local>,
) -> Option<f64> {
    if total_tokens <= 0 {
        return None;
    }
    let elapsed_seconds = now.signed_duration_since(started_at).num_seconds();
    if elapsed_seconds < 60 {
        return None;
    }
    let elapsed_minutes = elapsed_seconds as f64 / 60.0;
    Some(total_tokens as f64 / elapsed_minutes)
}

fn format_reset_countdown(
    rate_limits: &[RateLimitSnapshotDisplay],
    now: DateTime<Local>,
) -> Option<String> {
    let now_ts = now.timestamp();
    let mut nearest_reset: Option<i64> = None;

    for window in iter_windows(rate_limits) {
        if let Some(resets_at) = window.resets_at_unix
            && resets_at > now_ts
            && nearest_reset.is_none_or(|existing| resets_at < existing)
        {
            nearest_reset = Some(resets_at);
        }
    }

    let reset = nearest_reset?;
    let seconds = reset - now_ts;
    let duration = ChronoDuration::seconds(seconds);
    let local_reset = DateTime::from_timestamp(reset, 0)?.with_timezone(&Local);
    Some(format!(
        "resets in {} ({})",
        format_duration(duration),
        local_reset.format("%H:%M")
    ))
}

fn estimate_eta_to_limit(
    rate_limits: &[RateLimitSnapshotDisplay],
    now: DateTime<Local>,
) -> Option<ChronoDuration> {
    iter_windows(rate_limits)
        .into_iter()
        .filter_map(|window| eta_for_window(window, now))
        .min()
}

fn eta_for_window(window: &RateLimitWindowDisplay, now: DateTime<Local>) -> Option<ChronoDuration> {
    let used_percent = window.used_percent.clamp(0.0, 100.0);
    if !(0.0..100.0).contains(&used_percent) {
        return None;
    }

    let window_minutes = window.window_minutes?;
    let reset_at = window.resets_at_unix?;
    let seconds_until_reset = reset_at - now.timestamp();
    if seconds_until_reset <= 0 {
        return None;
    }

    let minutes_until_reset = seconds_until_reset as f64 / 60.0;
    let elapsed_minutes = window_minutes as f64 - minutes_until_reset;
    if elapsed_minutes <= 0.5 {
        return None;
    }

    let percent_per_minute = used_percent / elapsed_minutes;
    if percent_per_minute <= 0.0 {
        return None;
    }

    let remaining_percent = 100.0 - used_percent;
    let eta_minutes = remaining_percent / percent_per_minute;
    if !eta_minutes.is_finite() || eta_minutes <= 0.0 {
        return None;
    }

    Some(ChronoDuration::seconds((eta_minutes * 60.0).round() as i64))
}

fn max_used_percent(rate_limits: &[RateLimitSnapshotDisplay]) -> Option<f64> {
    iter_windows(rate_limits)
        .into_iter()
        .map(|window| window.used_percent)
        .max_by(|a, b| a.total_cmp(b))
}

fn iter_windows(rate_limits: &[RateLimitSnapshotDisplay]) -> Vec<&RateLimitWindowDisplay> {
    let mut windows = vec![];
    for snapshot in rate_limits {
        if let Some(primary) = snapshot.primary.as_ref() {
            windows.push(primary);
        }
        if let Some(secondary) = snapshot.secondary.as_ref() {
            windows.push(secondary);
        }
    }
    windows
}

fn format_duration(duration: ChronoDuration) -> String {
    let mut seconds = duration.num_seconds().max(0);
    let days = seconds / 86_400;
    seconds -= days * 86_400;
    let hours = seconds / 3_600;
    seconds -= hours * 3_600;
    let minutes = seconds / 60;

    if days > 0 {
        return format!("{days}d {hours}h");
    }
    if hours > 0 {
        return format!("{hours}h {minutes}m");
    }
    format!("{minutes}m")
}

pub(crate) fn compute_full_observability(
    codex_home: &Path,
    now: DateTime<Local>,
) -> Option<FullObservabilityData> {
    let mut files = vec![];
    collect_rollout_files(codex_home.join("sessions").as_path(), &mut files);
    collect_rollout_files(codex_home.join("archived_sessions").as_path(), &mut files);
    if files.is_empty() {
        return None;
    }

    sort_rollout_files_by_recency(&mut files);
    if files.len() > MAX_OBSERVABILITY_FILES {
        files.truncate(MAX_OBSERVABILITY_FILES);
    }

    let mut by_day: BTreeMap<NaiveDate, i64> = BTreeMap::new();
    let mut by_model: BTreeMap<String, ModelUsageSummary> = BTreeMap::new();

    for path in files.iter() {
        parse_rollout_usage(path, &mut by_day, &mut by_model);
    }

    if by_day.is_empty() {
        return None;
    }

    let today = now.date_naive();
    let mut all_time_tokens = 0i64;
    let mut today_tokens = 0i64;
    let mut seven_day_tokens = 0i64;
    let mut thirty_day_tokens = 0i64;

    for (day, tokens) in &by_day {
        let value = (*tokens).max(0);
        all_time_tokens += value;
        let delta_days = today.signed_duration_since(*day).num_days();
        if delta_days == 0 {
            today_tokens += value;
        }
        if (0..7).contains(&delta_days) {
            seven_day_tokens += value;
        }
        if (0..30).contains(&delta_days) {
            thirty_day_tokens += value;
        }
    }

    if all_time_tokens <= 0 {
        return None;
    }

    let active_days = by_day.values().filter(|tokens| **tokens > 0).count();
    let (current_streak_days, longest_streak_days) = compute_streaks(&by_day, today);
    let mut model_usage: Vec<ModelUsageSummary> = by_model.into_values().collect();
    model_usage.sort_by(|a, b| b.tokens.cmp(&a.tokens).then(a.model.cmp(&b.model)));

    let recent_days = (0..7)
        .rev()
        .map(|offset| {
            let day = today - ChronoDuration::days(offset);
            DailyUsageSummary {
                day,
                tokens: *by_day.get(&day).unwrap_or(&0),
            }
        })
        .collect();

    Some(FullObservabilityData {
        today_tokens,
        seven_day_tokens,
        thirty_day_tokens,
        all_time_tokens,
        active_days,
        current_streak_days,
        longest_streak_days,
        model_usage,
        recent_days,
        scanned_files: files.len(),
    })
}

fn compute_streaks(by_day: &BTreeMap<NaiveDate, i64>, today: NaiveDate) -> (usize, usize) {
    let active_dates: Vec<NaiveDate> = by_day
        .iter()
        .filter_map(|(day, tokens)| (*tokens > 0).then_some(*day))
        .collect();
    if active_dates.is_empty() {
        return (0, 0);
    }

    let mut longest = 1usize;
    let mut current_run = 1usize;
    for window in active_dates.windows(2) {
        if let [a, b] = window {
            if b.signed_duration_since(*a).num_days() == 1 {
                current_run += 1;
            } else {
                longest = longest.max(current_run);
                current_run = 1;
            }
        }
    }
    longest = longest.max(current_run);

    let mut current_streak = 0usize;
    let mut cursor = today;
    loop {
        if by_day.get(&cursor).copied().unwrap_or(0) > 0 {
            current_streak += 1;
            cursor -= ChronoDuration::days(1);
        } else {
            break;
        }
    }

    (current_streak, longest)
}

fn collect_rollout_files(root: &Path, files: &mut Vec<PathBuf>) {
    if !root.exists() {
        return;
    }

    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollout_files(path.as_path(), files);
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if file_name.starts_with("rollout-") && file_name.ends_with(".jsonl") {
            files.push(path);
        }
    }
}

fn sort_rollout_files_by_recency(files: &mut [PathBuf]) {
    files.sort_by(|a, b| {
        rollout_file_recency_key(b)
            .cmp(&rollout_file_recency_key(a))
            .then_with(|| a.cmp(b))
    });
}

fn rollout_file_recency_key(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let core = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    Some(core.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TokenSnapshotKey {
    model: String,
    total_input_tokens: i64,
    total_cached_input_tokens: i64,
    total_output_tokens: i64,
    total_reasoning_output_tokens: i64,
    total_tokens: i64,
    last_input_tokens: i64,
    last_cached_input_tokens: i64,
    last_output_tokens: i64,
    last_reasoning_output_tokens: i64,
    last_total_tokens: i64,
}

fn parse_rollout_usage(
    path: &Path,
    by_day: &mut BTreeMap<NaiveDate, i64>,
    by_model: &mut BTreeMap<String, ModelUsageSummary>,
) {
    let Ok(file) = File::open(path) else {
        return;
    };
    let reader = BufReader::new(file);
    let mut model_for_turn = "unknown".to_string();
    let mut seen_snapshots: HashSet<TokenSnapshotKey> = HashSet::new();

    for (idx, line) in reader.lines().enumerate() {
        if idx >= MAX_OBSERVABILITY_LINES_PER_FILE {
            break;
        }
        let Ok(line) = line else {
            continue;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(rollout_line) = serde_json::from_str::<RolloutLine>(trimmed) else {
            continue;
        };
        let RolloutLine { timestamp, item } = rollout_line;

        match item {
            RolloutItem::TurnContext(turn_context) => {
                if !turn_context.model.trim().is_empty() {
                    model_for_turn = turn_context.model;
                }
            }
            RolloutItem::EventMsg(EventMsg::TokenCount(token_count)) => {
                let Some(info) = token_count.info else {
                    continue;
                };
                let snapshot_key = TokenSnapshotKey {
                    model: model_for_turn.clone(),
                    total_input_tokens: info.total_token_usage.input_tokens,
                    total_cached_input_tokens: info.total_token_usage.cached_input_tokens,
                    total_output_tokens: info.total_token_usage.output_tokens,
                    total_reasoning_output_tokens: info.total_token_usage.reasoning_output_tokens,
                    total_tokens: info.total_token_usage.total_tokens,
                    last_input_tokens: info.last_token_usage.input_tokens,
                    last_cached_input_tokens: info.last_token_usage.cached_input_tokens,
                    last_output_tokens: info.last_token_usage.output_tokens,
                    last_reasoning_output_tokens: info.last_token_usage.reasoning_output_tokens,
                    last_total_tokens: info.last_token_usage.total_tokens,
                };
                if !seen_snapshots.insert(snapshot_key) {
                    continue;
                }
                let usage = info.last_token_usage;
                let tokens = usage.blended_total().max(0);
                if tokens == 0 {
                    continue;
                }
                let day = parse_rollout_line_day(timestamp.as_str())
                    .or_else(|| infer_day_from_file_name(path))
                    .unwrap_or_else(|| Local::now().date_naive());

                *by_day.entry(day).or_insert(0) += tokens;
                let entry =
                    by_model
                        .entry(model_for_turn.clone())
                        .or_insert_with(|| ModelUsageSummary {
                            model: model_for_turn.clone(),
                            tokens: 0,
                            cached_input_tokens: 0,
                        });
                entry.tokens += tokens;
                entry.cached_input_tokens += usage.cached_input();
            }
            _ => {}
        }
    }
}

fn parse_rollout_line_day(timestamp: &str) -> Option<NaiveDate> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|dt| dt.with_timezone(&Local).date_naive())
}

fn infer_day_from_file_name(path: &Path) -> Option<NaiveDate> {
    let name = path.file_name()?.to_str()?;
    let core = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    let date = core.get(..10)?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::SandboxPolicy;
    use codex_protocol::protocol::TokenCountEvent;
    use pretty_assertions::assert_eq;
    use std::io::Write;
    use std::path::PathBuf;
    use tempfile::NamedTempFile;

    #[test]
    fn calculates_burn_rate_tokens_per_minute() {
        let now = Local
            .with_ymd_and_hms(2026, 1, 10, 12, 0, 0)
            .single()
            .expect("valid timestamp");
        let started = now - ChronoDuration::minutes(10);
        let rate = calculate_burn_rate(/*total_tokens*/ 12_000, started, now).expect("burn rate");
        assert_eq!(rate.round() as i64, 1_200);
    }

    #[test]
    fn compute_streaks_tracks_current_and_longest() {
        let today = NaiveDate::from_ymd_opt(2026, 1, 10).expect("date");
        let mut by_day = BTreeMap::new();
        by_day.insert(today - ChronoDuration::days(4), 100);
        by_day.insert(today - ChronoDuration::days(3), 100);
        by_day.insert(today - ChronoDuration::days(1), 100);
        by_day.insert(today, 100);

        let (current, longest) = compute_streaks(&by_day, today);
        assert_eq!(current, 2);
        assert_eq!(longest, 2);
    }

    #[test]
    fn sort_rollout_files_prioritizes_newest_timestamped_names() {
        let mut files = vec![
            PathBuf::from("sessions/2026/01/09/rollout-2026-01-09T12-00-00-000000000Z.jsonl"),
            PathBuf::from("sessions/2026/01/10/rollout-2026-01-10T01-00-00-000000000Z.jsonl"),
            PathBuf::from("sessions/2026/01/08/rollout-2026-01-08T20-00-00-000000000Z.jsonl"),
        ];

        sort_rollout_files_by_recency(&mut files);

        assert_eq!(
            files,
            vec![
                PathBuf::from("sessions/2026/01/10/rollout-2026-01-10T01-00-00-000000000Z.jsonl"),
                PathBuf::from("sessions/2026/01/09/rollout-2026-01-09T12-00-00-000000000Z.jsonl"),
                PathBuf::from("sessions/2026/01/08/rollout-2026-01-08T20-00-00-000000000Z.jsonl"),
            ]
        );
    }

    #[test]
    fn sort_rollout_files_falls_back_for_non_standard_names() {
        let mut files = vec![
            PathBuf::from("sessions/rollout-2026-01-10T01-00-00-000000000Z.jsonl"),
            PathBuf::from("sessions/not-a-rollout-file.jsonl"),
            PathBuf::from("sessions/rollout-2026-01-11T01-00-00-000000000Z.jsonl"),
        ];

        sort_rollout_files_by_recency(&mut files);

        assert_eq!(
            files,
            vec![
                PathBuf::from("sessions/rollout-2026-01-11T01-00-00-000000000Z.jsonl"),
                PathBuf::from("sessions/rollout-2026-01-10T01-00-00-000000000Z.jsonl"),
                PathBuf::from("sessions/not-a-rollout-file.jsonl"),
            ]
        );
    }

    #[test]
    fn parse_rollout_usage_deduplicates_repeated_token_snapshots() {
        let mut rollout = NamedTempFile::new().expect("temp rollout file");

        let turn_context = RolloutLine {
            timestamp: "2026-01-10T12:00:00Z".to_string(),
            item: RolloutItem::TurnContext(codex_protocol::protocol::TurnContextItem {
                turn_id: Some("turn-1".to_string()),
                trace_id: None,
                cwd: PathBuf::from("/workspace/tests"),
                current_date: None,
                timezone: None,
                approval_policy: AskForApproval::OnRequest,
                sandbox_policy: SandboxPolicy::DangerFullAccess,
                network: None,
                model: "alpha-model".to_string(),
                personality: None,
                collaboration_mode: None,
                realtime_active: None,
                effort: None,
                summary: codex_protocol::config_types::ReasoningSummary::Auto,
                user_instructions: None,
                developer_instructions: None,
                final_output_json_schema: None,
                truncation_policy: None,
            }),
        };
        let token_count = RolloutLine {
            timestamp: "2026-01-10T12:00:05Z".to_string(),
            item: RolloutItem::EventMsg(EventMsg::TokenCount(TokenCountEvent {
                info: Some(TokenUsageInfo {
                    total_token_usage: TokenUsage {
                        input_tokens: 260,
                        cached_input_tokens: 100,
                        output_tokens: 140,
                        reasoning_output_tokens: 0,
                        total_tokens: 400,
                    },
                    last_token_usage: TokenUsage {
                        input_tokens: 260,
                        cached_input_tokens: 100,
                        output_tokens: 140,
                        reasoning_output_tokens: 0,
                        total_tokens: 400,
                    },
                    model_context_window: None,
                }),
                rate_limits: None,
            })),
        };

        writeln!(
            rollout,
            "{}",
            serde_json::to_string(&turn_context).expect("serialize turn context")
        )
        .expect("write turn context");
        writeln!(
            rollout,
            "{}",
            serde_json::to_string(&token_count).expect("serialize token count")
        )
        .expect("write token count");
        writeln!(
            rollout,
            "{}",
            serde_json::to_string(&token_count).expect("serialize duplicated token count")
        )
        .expect("write duplicated token count");

        let mut by_day: BTreeMap<NaiveDate, i64> = BTreeMap::new();
        let mut by_model: BTreeMap<String, ModelUsageSummary> = BTreeMap::new();
        parse_rollout_usage(rollout.path(), &mut by_day, &mut by_model);

        let day = NaiveDate::from_ymd_opt(2026, 1, 10).expect("date");
        assert_eq!(by_day.get(&day), Some(&300));
        assert_eq!(by_model.get("alpha-model").map(|m| m.tokens), Some(300));
        assert_eq!(
            by_model.get("alpha-model").map(|m| m.cached_input_tokens),
            Some(100)
        );
    }
}
