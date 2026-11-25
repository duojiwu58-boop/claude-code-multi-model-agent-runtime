//! Human-like natural approval pacing.
//!
//! Computes non-blocking per-request approval delays modeled after an experienced
//! human developer glancing at a permission card before clicking Allow:
//! - scales by tool class (read / edit / execute / other) and content/diff size
//! - uses a right-skewed log-normal distribution instead of robotic fixed timers
//! - adapts to recent interaction rhythm (active flow vs. cold refocus after idle)
//! - injects occasional brief hesitation pauses without multi-second stalls
//! - staggers concurrent requests strictly one by one

use serde_json::Value;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MIN_DELAY_MS: f64 = 320.0;
const MIN_STAGGER_MS: f64 = 260.0;
const MAX_DELAY_MS: f64 = 2_800.0;
const MAX_SCAN_CHARS: usize = 64 * 1024;
const MAX_SCAN_NODES: usize = 256;

static SEED_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ToolClass {
    Read,
    Edit,
    Execute,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RequestProfile {
    pub class: ToolClass,
    pub content_chars: usize,
    pub content_lines: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Retain upstream contexts; LS policy changes apply on the next turn.
pub(super) enum ScheduleContext {
    /// Normal arrival of a permission request.
    Normal,
    /// User just turned auto-approve ON while this card was already visible.
    JustEnabled,
    /// Another permission card was just approved; this card surfaced next.
    ConsecutiveQueued,
}

impl RequestProfile {
    pub(super) fn from_packet(packet: &Value, session_tool: Option<&Value>) -> Self {
        let tc = &packet["params"]["toolCall"];
        let kind = non_empty_str(tc.get("kind"))
            .or_else(|| session_tool.and_then(|t| non_empty_str(t.get("kind"))));
        let name = non_empty_str(tc.get("name"))
            .or_else(|| session_tool.and_then(|t| non_empty_str(t.get("name"))));
        let content = tc.get("content").filter(|v| !v.is_null()).or_else(|| {
            session_tool
                .and_then(|t| t.get("content"))
                .filter(|v| !v.is_null())
        });
        let has_diff = content.is_some_and(contains_diff_block);

        let class = classify_tool(kind, name, has_diff);

        let mut chars = 0usize;
        let mut lines = 0usize;
        let mut budget = MAX_SCAN_NODES;

        if let Some(title) = non_empty_str(tc.get("title"))
            .or_else(|| session_tool.and_then(|t| non_empty_str(t.get("title"))))
        {
            accumulate_text(title, &mut chars, &mut lines);
        }
        if let Some(value) = content {
            accumulate_value(value, &mut chars, &mut lines, &mut budget, 0);
        }
        let raw_input = tc
            .get("rawInput")
            .filter(|v| !v.is_null())
            .or_else(|| tc.get("args").filter(|v| !v.is_null()))
            .or_else(|| {
                session_tool
                    .and_then(|t| t.get("rawInput"))
                    .filter(|v| !v.is_null())
            })
            .or_else(|| {
                session_tool
                    .and_then(|t| t.get("args"))
                    .filter(|v| !v.is_null())
            });
        if let Some(value) = raw_input {
            accumulate_value(value, &mut chars, &mut lines, &mut budget, 0);
        }

        Self {
            class,
            content_chars: chars.min(MAX_SCAN_CHARS),
            content_lines: lines,
        }
    }
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn contains_diff_block(value: &Value) -> bool {
    value
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item["type"] == "diff"))
}

fn classify_tool(kind: Option<&str>, name: Option<&str>, has_diff: bool) -> ToolClass {
    if let Some(kind) = kind {
        match kind.to_ascii_lowercase().as_str() {
            "execute" | "delete" | "fetch" => return ToolClass::Execute,
            "edit" | "move" => return ToolClass::Edit,
            "read" | "search" | "think" => return ToolClass::Read,
            _ => {}
        }
    }
    if let Some(name) = name {
        let lower = name.to_ascii_lowercase();
        if [
            "run",
            "exec",
            "shell",
            "bash",
            "cmd",
            "powershell",
            "pwsh",
            "terminal",
            "command",
            "delete",
            "remove",
            "rm_",
            "fetch",
            "http",
            "curl",
            "browser",
            "install",
            "deploy",
            "push",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
        {
            return ToolClass::Execute;
        }
        if [
            "write", "edit", "patch", "replace", "create", "rename", "move", "apply", "format",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
        {
            return ToolClass::Edit;
        }
        if [
            "read", "list", "find", "grep", "search", "view", "inspect", "stat", "cat", "glob",
            "get_",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
        {
            return ToolClass::Read;
        }
    }
    if has_diff {
        return ToolClass::Edit;
    }
    ToolClass::Other
}

fn accumulate_text(text: &str, chars: &mut usize, lines: &mut usize) {
    if *chars >= MAX_SCAN_CHARS {
        return;
    }
    let remaining = MAX_SCAN_CHARS - *chars;
    let slice = if text.len() > remaining {
        let mut end = remaining;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    } else {
        text
    };
    *chars = chars.saturating_add(slice.chars().count());
    let newlines = slice.as_bytes().iter().filter(|&&b| b == b'\n').count();
    *lines = lines.saturating_add(if slice.is_empty() { 0 } else { newlines + 1 });
}

fn accumulate_value(
    value: &Value,
    chars: &mut usize,
    lines: &mut usize,
    budget: &mut usize,
    depth: usize,
) {
    if *budget == 0 || *chars >= MAX_SCAN_CHARS || depth > 6 {
        return;
    }
    *budget -= 1;
    match value {
        Value::String(s) => accumulate_text(s, chars, lines),
        Value::Array(items) => {
            for item in items {
                accumulate_value(item, chars, lines, budget, depth + 1);
                if *budget == 0 || *chars >= MAX_SCAN_CHARS {
                    break;
                }
            }
        }
        Value::Object(map) => {
            for (_, v) in map {
                accumulate_value(v, chars, lines, budget, depth + 1);
                if *budget == 0 || *chars >= MAX_SCAN_CHARS {
                    break;
                }
            }
        }
        _ => {}
    }
}

/// Lightweight deterministic SplitMix64 PRNG (zero external crate dependencies).
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn from_entropy() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let low = nanos as u64;
        let high = (nanos >> 64) as u64;
        let seq = SEED_COUNTER.fetch_add(1, Ordering::Relaxed);
        let stack_addr = (&seq as *const u64) as usize as u64;
        let seed = low
            ^ high.rotate_left(17)
            ^ seq.wrapping_mul(0x9e37_79b9_7f4a_7c15)
            ^ stack_addr.rotate_left(31);
        Self::new(seed)
    }

    fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_add(0x9e37_79b9_7f4a_7c15),
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform f64 in the open interval (0.0, 1.0).
    fn next_unit(&mut self) -> f64 {
        let bits = self.next_u64() >> 11; // 53 bits
        ((bits as f64) + 0.5) / ((1u64 << 53) as f64)
    }

    /// Clamped standard normal sample via Box-Muller transform.
    fn next_normal(&mut self) -> f64 {
        let u1 = self.next_unit();
        let u2 = self.next_unit();
        let z = (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos();
        z.clamp(-2.5, 2.5)
    }

    fn lognormal(&mut self, median: f64, sigma: f64) -> f64 {
        median * (sigma * self.next_normal()).exp()
    }
}

pub(super) struct Pacer {
    rng: SplitMix64,
    last_decision_at: Option<Instant>,
    last_activity_at: Instant,
    streak: u32,
    last_was_long_pause: bool,
}

impl Default for Pacer {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

impl Pacer {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            rng: SplitMix64::from_entropy(),
            last_decision_at: None,
            last_activity_at: now,
            streak: 0,
            last_was_long_pause: false,
        }
    }

    #[cfg(test)]
    pub(super) fn with_seed(seed: u64, now: Instant) -> Self {
        Self {
            rng: SplitMix64::new(seed),
            last_decision_at: None,
            last_activity_at: now,
            streak: 0,
            last_was_long_pause: false,
        }
    }

    /// Record non-approval user interaction (e.g. submitting a prompt or toggling settings).
    pub(super) fn note_activity(&mut self, now: Instant) {
        self.last_activity_at = now;
    }

    /// Record a completed approval decision (automatic or manual) to drive flow/refocus rhythm.
    pub(super) fn note_decision(&mut self, now: Instant) {
        if self
            .last_decision_at
            .is_some_and(|prev| now.saturating_duration_since(prev) <= Duration::from_secs(15))
        {
            self.streak = self.streak.saturating_add(1).min(8);
        } else {
            self.streak = 1;
        }
        self.last_decision_at = Some(now);
        self.last_activity_at = now;
    }

    pub(super) fn sample_delay(
        &mut self,
        profile: &RequestProfile,
        now: Instant,
        context: ScheduleContext,
    ) -> Duration {
        // 1. Base reaction + glance time by tool class (experienced developer profile).
        let (base_median_ms, sigma) = match profile.class {
            ToolClass::Read => (450.0, 0.24),
            ToolClass::Edit => (750.0, 0.24),
            ToolClass::Execute => (950.0, 0.24),
            ToolClass::Other => (700.0, 0.24),
        };
        let base_ms = self.rng.lognormal(base_median_ms, sigma);

        // 2. Logarithmic reading increment from diff / command / args volume.
        let chars = profile.content_chars as f64;
        let lines = profile.content_lines as f64;
        let raw_reading_ms = match profile.class {
            ToolClass::Read => (1.0 + chars / 240.0).ln() * 65.0,
            ToolClass::Edit => {
                (1.0 + chars / 180.0).ln() * 110.0 + (1.0 + lines / 12.0).ln() * 85.0
            }
            ToolClass::Execute => {
                (1.0 + chars / 90.0).ln() * 130.0 + (1.0 + lines / 6.0).ln() * 70.0
            }
            ToolClass::Other => (1.0 + chars / 180.0).ln() * 90.0,
        };
        let reading_cap_ms = match profile.class {
            ToolClass::Read => 180.0,
            ToolClass::Edit => 500.0,
            ToolClass::Execute => 460.0,
            ToolClass::Other => 320.0,
        };
        let reading_jitter = 0.85 + 0.30 * self.rng.next_unit();
        let reading_ms = (raw_reading_ms * reading_jitter).min(reading_cap_ms);

        // 3. Context rhythm: flow streak vs. cold refocus vs. just-enabled / queued card.
        let (rhythm_factor, refocus_ms, min_ms) = match context {
            ScheduleContext::JustEnabled => (0.72, 0.0, MIN_STAGGER_MS),
            ScheduleContext::ConsecutiveQueued => (0.68, 0.0, MIN_STAGGER_MS),
            ScheduleContext::Normal => {
                let since_decision = self
                    .last_decision_at
                    .map(|t| now.saturating_duration_since(t));
                let since_activity = now.saturating_duration_since(self.last_activity_at);
                let idle = since_decision.map_or(since_activity, |d| d.min(since_activity));
                if since_decision.is_some_and(|d| d < Duration::from_secs(15)) {
                    let factor = match self.streak {
                        0 | 1 => 0.90,
                        2 => 0.83,
                        _ => 0.76,
                    };
                    (factor, 0.0, MIN_DELAY_MS)
                } else if idle >= Duration::from_secs(60) {
                    let mut refocus = self.rng.lognormal(320.0, 0.28).clamp(180.0, 700.0);
                    if idle >= Duration::from_secs(180) {
                        refocus *= 1.2;
                    }
                    (1.0, refocus, MIN_DELAY_MS)
                } else {
                    (1.0, 0.0, MIN_DELAY_MS)
                }
            }
        };

        // 4. Occasional brief human hesitation without multi-second stalls.
        // Avoid stacking two pauses back-to-back.
        let pause_ms = if self.last_was_long_pause
            || matches!(
                context,
                ScheduleContext::JustEnabled | ScheduleContext::ConsecutiveQueued
            ) {
            self.last_was_long_pause = false;
            0.0
        } else {
            let roll = self.rng.next_unit();
            if roll < 0.05 {
                self.last_was_long_pause = true;
                self.rng.lognormal(620.0, 0.25).clamp(380.0, 1_050.0)
            } else {
                0.0
            }
        };

        let total_ms = ((base_ms + reading_ms) * rhythm_factor + refocus_ms + pause_ms)
            .clamp(min_ms, MAX_DELAY_MS);
        Duration::from_millis(total_ms.round() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_tool_kinds_names_and_session_tool_fallback() {
        let read_pkt = json!({
            "method": "session/request_permission",
            "params": { "toolCall": { "toolCallId": "t1" } }
        });
        let session_read = json!({
            "kind": "read",
            "name": "read_file",
            "title": "File sentinel.txt",
            "rawInput": { "Path": "sentinel.txt" }
        });
        let read_profile = RequestProfile::from_packet(&read_pkt, Some(&session_read));
        assert_eq!(read_profile.class, ToolClass::Read);
        assert!(read_profile.content_chars > 0);

        let edit_pkt = json!({
            "method": "session/request_permission",
            "params": {
                "toolCall": {
                    "name": "write_file",
                    "content": [{ "type": "diff", "path": "a.rs", "newText": "line1\nline2\nline3\n" }]
                }
            }
        });
        let edit_profile = RequestProfile::from_packet(&edit_pkt, None);
        assert_eq!(edit_profile.class, ToolClass::Edit);
        assert!(edit_profile.content_lines >= 3);

        let exec_pkt = json!({
            "method": "session/request_permission",
            "params": {
                "toolCall": {
                    "kind": "execute",
                    "name": "run_command",
                    "rawInput": { "CommandLine": "cargo test --workspace" }
                }
            }
        });
        let exec_profile = RequestProfile::from_packet(&exec_pkt, None);
        assert_eq!(exec_profile.class, ToolClass::Execute);
    }

    #[test]
    fn natural_pacing_stays_within_human_bounds_and_scales_by_tool_and_idle() {
        let base = Instant::now();
        let read_small = RequestProfile {
            class: ToolClass::Read,
            content_chars: 24,
            content_lines: 1,
        };
        let edit_large = RequestProfile {
            class: ToolClass::Edit,
            content_chars: 4_000,
            content_lines: 120,
        };
        let exec_cmd = RequestProfile {
            class: ToolClass::Execute,
            content_chars: 140,
            content_lines: 2,
        };

        let sample_mean = |profile: RequestProfile, offset: Duration| -> (f64, u64, u64, usize) {
            let mut pacer = Pacer::with_seed(42, base);
            let mut sum = 0u64;
            let mut min = u64::MAX;
            let mut max = 0u64;
            let mut brief_pauses = 0usize;
            for _ in 0..400 {
                let d = pacer
                    .sample_delay(&profile, base + offset, ScheduleContext::Normal)
                    .as_millis() as u64;
                assert!(d >= MIN_DELAY_MS as u64, "too fast: {d}ms");
                assert!(d <= MAX_DELAY_MS as u64, "too slow: {d}ms");
                sum += d;
                min = min.min(d);
                max = max.max(d);
                if d >= 1_650 {
                    brief_pauses += 1;
                }
            }
            (sum as f64 / 400.0, min, max, brief_pauses)
        };

        let (read_mean, _, read_max, _) = sample_mean(read_small, Duration::from_secs(5));
        let (edit_mean, _, _, edit_pauses) = sample_mean(edit_large, Duration::from_secs(5));
        let (exec_mean, _, _, _) = sample_mean(exec_cmd, Duration::from_secs(5));
        let (cold_read_mean, _, _, _) = sample_mean(read_small, Duration::from_secs(90));

        assert!(
            read_mean > 380.0 && read_mean < 750.0,
            "read_mean={read_mean}"
        );
        assert!(
            edit_mean > read_mean + 500.0,
            "edit_mean={edit_mean} vs read_mean={read_mean}"
        );
        assert!(
            exec_mean > read_mean + 450.0,
            "exec_mean={exec_mean} vs read_mean={read_mean}"
        );
        assert!(
            cold_read_mean > read_mean + 220.0,
            "cold_read_mean={cold_read_mean} vs read_mean={read_mean}"
        );
        assert!(
            read_max >= 900,
            "expected occasional brief hesitation, max={read_max}"
        );
        assert!(
            edit_pauses > 10,
            "expected brief hesitation pauses on edits, count={edit_pauses}"
        );
    }

    #[test]
    fn flow_streak_speeds_up_gently_without_going_robotic() {
        let base = Instant::now();
        let profile = RequestProfile {
            class: ToolClass::Edit,
            content_chars: 300,
            content_lines: 10,
        };
        let mut fresh = Pacer::with_seed(7, base);
        let mut streak = Pacer::with_seed(7, base);
        let mut t = base;
        for _ in 0..4 {
            t += Duration::from_secs(3);
            streak.note_decision(t);
        }

        let mut fresh_sum = 0u64;
        let mut streak_sum = 0u64;
        for _ in 0..200 {
            let f = fresh
                .sample_delay(
                    &profile,
                    base + Duration::from_secs(5),
                    ScheduleContext::Normal,
                )
                .as_millis() as u64;
            let s = streak
                .sample_delay(
                    &profile,
                    t + Duration::from_secs(3),
                    ScheduleContext::Normal,
                )
                .as_millis() as u64;
            assert!(s >= MIN_DELAY_MS as u64);
            fresh_sum += f;
            streak_sum += s;
        }
        assert!(
            streak_sum < fresh_sum,
            "streak should be gently faster than cold baseline"
        );
    }
}
