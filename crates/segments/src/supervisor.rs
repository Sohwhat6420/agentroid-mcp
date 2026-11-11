//! The hub's supervisor: admits a segment before it is sent, then checks the phone's report.
//!
//! The report is what `android/runner` `SegmentRunner` produces (`Report.java`); it travels over the link as the
//! result of the request `runner.report {segment_id}` (see RUNNER-PHONE-REPORT.md, glue). Checks:
//! - the segment is known, for this device and this epoch; a report from an older epoch is refused (`stale_epoch`);
//! - each step is reported at most once (`duplicate_step`), only steps of the segment (`unknown_step`), in order;
//! - every `done` step says its post-condition was verified (`post_not_verified`);
//! - no step ran after the segment expired (`ran_after_expiry`);
//! - `completed` means every step is `done`; a second report for the same segment is a `duplicate_report`.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::classes::{Phase, SafeClasses};
use crate::segment::{verify, Refusal, Segment, SignedSegment, VerifyContext};
use crate::time;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepReport {
    pub id: String,
    /// done | failed | refused | interrupted
    pub status: String,
    pub post_verified: bool,
    /// The phone's own `safety_decision.decision` for the step.
    pub decision: String,
    /// Phone wall clock, Unix ms, when the step finished (or was refused).
    pub ts_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// What a `read` step read (the node's text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    pub v: u32,
    pub t: String,
    pub segment_id: String,
    pub run_id: String,
    pub device: String,
    pub epoch: u64,
    /// completed | stopped_human | refused | failed | expired | stale_epoch
    pub outcome: String,
    pub steps: Vec<StepReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    UnknownSegment(String),
    WrongDevice(String),
    StaleEpoch { report: u64, current: u64 },
    RunMismatch,
    DuplicateReport,
    DuplicateStep(String),
    UnknownStep(String),
    OutOfOrder(String),
    PostNotVerified(String),
    RanAfterExpiry(String),
    IncompleteButCompleted(usize),
    BadStatus(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub findings: Vec<Finding>,
    pub outcome: String,
}

impl Verdict {
    pub fn ok(&self) -> bool {
        self.findings.is_empty()
    }
}

/// One supervisor per hub process (one epoch).
pub struct Supervisor {
    epoch: u64,
    classes: SafeClasses,
    phase: Phase,
    sent: BTreeMap<String, Segment>,
    reported: BTreeSet<String>,
    /// Admitted here but refused by the phone (or never sent): nothing ran, so they are not running.
    refused: BTreeSet<String>,
}

impl Supervisor {
    pub fn new(epoch: u64, classes: SafeClasses, phase: Phase) -> Supervisor {
        Supervisor {
            epoch,
            classes,
            phase,
            sent: BTreeMap::new(),
            reported: BTreeSet::new(),
            refused: BTreeSet::new(),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Before sending: the same checks the phone makes (signature with our own key, epoch, expiry, classes). A
    /// stale-epoch or expired segment is refused here, so it never reaches the phone.
    pub fn admit(
        &mut self,
        signed: &SignedSegment,
        hub_spki: &[u8],
        device: &str,
        now: i64,
    ) -> Result<(), Refusal> {
        let ctx = VerifyContext {
            hub_spki,
            device,
            current_epoch: self.epoch,
            now,
        };
        verify(signed, &ctx, &self.classes, self.phase)?;
        // A segment id runs once. One this supervisor already sent is running, finished, or of unknown outcome (a
        // runner timeout leaves it so): sending it again could run it twice. Only an id the phone (or the hub's own
        // send) REFUSED, so that nothing ran, may be sent again.
        let id = &signed.segment.segment_id;
        if self.sent.contains_key(id) && !self.refused.contains(id) {
            return Err(Refusal::Duplicate(id.clone()));
        }
        // an id that was refused earlier and is sent again is running again
        self.refused.remove(&signed.segment.segment_id);
        self.sent
            .insert(signed.segment.segment_id.clone(), signed.segment.clone());
        Ok(())
    }

    /// The phone refused the segment, or it was never sent: nothing ran. `admit` happens before the send, so
    /// without this mark a refused segment would count as running until it expires, and its cached answer would be
    /// pinned for that long. A report for it can still be checked (the phone queues one for most refusals).
    pub fn refuse(&mut self, segment_id: &str) {
        if self.sent.contains_key(segment_id) {
            self.refused.insert(segment_id.to_string());
        }
    }

    /// True while a segment sent by this supervisor can still be running or reported on: it has not
    /// been reported, was not refused, and it has not expired (`now` is epoch seconds). Pruning never touches these,
    /// and the hub's response cache is told to keep their answers.
    pub fn is_running(&self, segment_id: &str, now: i64) -> bool {
        match self.sent.get(segment_id) {
            Some(seg) => {
                !self.reported.contains(segment_id)
                    && !self.refused.contains(segment_id)
                    && Self::expires_s(seg) > now
            }
            None => false,
        }
    }

    fn expires_s(seg: &Segment) -> i64 {
        time::parse_utc(&seg.expires).unwrap_or(i64::MIN)
    }

    /// How many sent segments the supervisor remembers (running or not).
    pub fn remembered(&self) -> usize {
        self.sent.len()
    }

    /// Forgets finished segments, so a long-lived hub does not keep every segment it ever sent.
    /// A segment is finished once it has been reported or refused or has expired. Two passes, as the caches
    /// do: first every finished segment whose expiry is more than `max_age_s` ago, then, if more
    /// than `max_count` are still remembered, the finished ones with the oldest expiry. A running
    /// segment is never dropped, by age or by count. Returns how many were forgotten.
    pub fn prune(&mut self, now: i64, max_age_s: i64, max_count: usize) -> usize {
        let finished = |s: &Self, id: &str, seg: &Segment| {
            s.reported.contains(id) || s.refused.contains(id) || Self::expires_s(seg) <= now
        };
        let mut gone: Vec<String> = self
            .sent
            .iter()
            .filter(|(id, seg)| {
                finished(self, id, seg) && Self::expires_s(seg) < now.saturating_sub(max_age_s)
            })
            .map(|(id, _)| id.clone())
            .collect();
        let left = self.sent.len() - gone.len();
        if left > max_count {
            let mut rest: Vec<(i64, &String)> = self
                .sent
                .iter()
                .filter(|(id, seg)| finished(self, id, seg) && !gone.contains(id))
                .map(|(id, seg)| (Self::expires_s(seg), id))
                .collect();
            rest.sort();
            gone.extend(
                rest.into_iter()
                    .take(left - max_count)
                    .map(|(_, id)| id.clone()),
            );
        }
        for id in &gone {
            self.sent.remove(id);
            self.reported.remove(id);
            self.refused.remove(id);
        }
        // a report id with no segment left (cannot happen today) would never be freed otherwise
        self.reported.retain(|id| self.sent.contains_key(id));
        self.refused.retain(|id| self.sent.contains_key(id));
        gone.len()
    }

    /// Check one report from the phone.
    pub fn check_report(&mut self, r: &Report) -> Verdict {
        let mut f = Vec::new();
        let outcome = r.outcome.clone();
        if r.epoch < self.epoch {
            f.push(Finding::StaleEpoch {
                report: r.epoch,
                current: self.epoch,
            });
            return Verdict {
                findings: f,
                outcome,
            };
        }
        let Some(seg) = self.sent.get(&r.segment_id) else {
            f.push(Finding::UnknownSegment(r.segment_id.clone()));
            return Verdict {
                findings: f,
                outcome,
            };
        };
        if r.epoch != seg.epoch {
            f.push(Finding::StaleEpoch {
                report: r.epoch,
                current: seg.epoch,
            });
        }
        if r.device != seg.device {
            f.push(Finding::WrongDevice(r.device.clone()));
        }
        if r.run_id != seg.run_id {
            f.push(Finding::RunMismatch);
        }
        if !self.reported.insert(r.segment_id.clone()) {
            f.push(Finding::DuplicateReport);
        }
        let expires_ms = time::parse_utc(&seg.expires)
            .map(|t| t * 1000)
            .unwrap_or(i64::MIN);
        let order: BTreeMap<&str, usize> = seg
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id.as_str(), i))
            .collect();
        let mut seen = BTreeSet::new();
        let mut last: Option<usize> = None;
        let mut done = 0usize;
        for s in &r.steps {
            if !seen.insert(s.id.as_str()) {
                f.push(Finding::DuplicateStep(s.id.clone()));
                continue;
            }
            let Some(&pos) = order.get(s.id.as_str()) else {
                f.push(Finding::UnknownStep(s.id.clone()));
                continue;
            };
            if last.is_some_and(|l| pos <= l) {
                f.push(Finding::OutOfOrder(s.id.clone()));
            }
            last = Some(pos);
            match s.status.as_str() {
                "done" => {
                    done += 1;
                    if !s.post_verified {
                        f.push(Finding::PostNotVerified(s.id.clone()));
                    }
                    if s.ts_ms > expires_ms {
                        f.push(Finding::RanAfterExpiry(s.id.clone()));
                    }
                }
                "failed" | "refused" | "interrupted" => {}
                other => f.push(Finding::BadStatus(other.to_string())),
            }
        }
        if r.outcome == "completed" && done != seg.steps.len() {
            f.push(Finding::IncompleteButCompleted(seg.steps.len() - done));
        }
        Verdict {
            findings: f,
            outcome,
        }
    }
}
