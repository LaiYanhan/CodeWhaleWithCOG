use super::types::{Candidate, Evidence};

pub struct CandidateSources;

impl CandidateSources {
    pub fn from_evidence(trigger_event_id: &str, evidence: Vec<Evidence>) -> Vec<Candidate> {
        evidence
            .into_iter()
            .map(|evidence| Candidate {
                entity: evidence.target.clone(),
                trigger_event_id: trigger_event_id.to_string(),
                evidence: vec![evidence],
            })
            .collect()
    }
}
