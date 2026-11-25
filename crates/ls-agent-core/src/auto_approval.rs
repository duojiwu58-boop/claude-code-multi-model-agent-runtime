//! AgyStudio approval pacing, adapted to live LS interactions (no ACP runtime).
mod pacing;

use crate::{InteractionDecision, InteractionSubmitRequest, PendingInteraction};
use pacing::{Pacer, RequestProfile, ScheduleContext};
use std::collections::HashSet;
use std::time::Instant;

#[derive(Default)]
pub(crate) struct AutoApproval {
    pub enabled: bool,
    pacer: Pacer,
    scheduled: Option<(PendingInteraction, Instant)>,
    attempted: HashSet<(String, u64)>,
}

impl AutoApproval {
    pub fn begin_turn(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.scheduled = None;
        self.pacer.note_activity(Instant::now());
    }

    pub fn disable(&mut self) {
        self.enabled = false;
        self.scheduled = None;
    }

    pub fn next(
        &mut self,
        pending: Option<PendingInteraction>,
        now: Instant,
    ) -> Option<InteractionSubmitRequest> {
        let Some(p) = pending.filter(|p| self.enabled && p.supported) else {
            self.scheduled = None;
            return None;
        };
        let decision = match p.kind.as_str() {
            "permission" => InteractionDecision::Permission {
                allow: true,
                scope: ls_wire::PermissionScope::Once,
            },
            "runCommand" => InteractionDecision::RunCommand { confirm: true },
            "filePermission" => InteractionDecision::FilePermission { allow: true },
            _ => {
                self.scheduled = None;
                return None;
            }
        };
        let key = (p.trajectory_id.clone(), p.step_index);
        if self.attempted.contains(&key) {
            self.scheduled = None;
            return None;
        }
        if !self.scheduled.as_ref().is_some_and(|(old, _)| old == &p) {
            let packet = serde_json::json!({"params":{"toolCall":{
                "name": p.spec.get(&p.kind).and_then(|v| v.get("toolName")).and_then(|v| v.as_str()).unwrap_or(&p.kind),
                "rawInput": p.spec
            }}});
            let profile = RequestProfile::from_packet(&packet, None);
            let delay = self
                .pacer
                .sample_delay(&profile, now, ScheduleContext::Normal);
            self.scheduled = Some((p, now + delay));
            return None;
        }
        if now < self.scheduled.as_ref().unwrap().1 {
            return None;
        }
        self.scheduled = None;
        self.attempted.insert(key); // even rejected/unknown attempts require a manual decision
        self.pacer.note_decision(now);
        Some(InteractionSubmitRequest {
            conversation_id: p.conversation_id,
            trajectory_id: p.trajectory_id,
            step_index: p.step_index,
            kind: p.kind,
            decision,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    fn pending(kind: &str) -> PendingInteraction {
        PendingInteraction {
            conversation_id: "c".into(),
            trajectory_id: "t".into(),
            step_index: 1,
            kind: kind.into(),
            supported: true,
            spec: serde_json::json!({kind:{}}),
        }
    }
    #[test]
    fn delays_once_cancels_stale_and_never_answers_questions() {
        let mut a = AutoApproval::default();
        a.begin_turn(true);
        let now = Instant::now();
        let p = pending("runCommand");
        assert!(a.next(Some(p.clone()), now).is_none());
        assert!(a.next(None, now).is_none());
        assert!(
            a.next(Some(p.clone()), now + Duration::from_secs(4))
                .is_none()
        );
        assert!(
            a.next(Some(p.clone()), now + Duration::from_secs(8))
                .is_some()
        );
        assert!(a.next(Some(p), now + Duration::from_secs(12)).is_none());
        for kind in ["askQuestion", "elicitation", "unknown"] {
            assert!(a.next(Some(pending(kind)), now).is_none());
            assert!(
                a.next(Some(pending(kind)), now + Duration::from_secs(4))
                    .is_none()
            );
        }
        a.begin_turn(false);
        assert!(a.next(Some(pending("permission")), now).is_none());
    }
}
