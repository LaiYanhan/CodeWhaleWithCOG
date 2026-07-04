use super::types::Recommendation;

pub fn compact_recommendation_text(recommendations: &[Recommendation]) -> String {
    recommendations
        .iter()
        .map(|recommendation| {
            format!(
                "- {} ({:.2}): {}",
                recommendation.entity.qualified_name,
                recommendation.score,
                recommendation
                    .evidence
                    .first()
                    .map(|evidence| evidence.reason.as_str())
                    .unwrap_or("related evidence")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
