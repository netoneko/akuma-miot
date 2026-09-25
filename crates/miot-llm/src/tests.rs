//! What can be checked without a network: how an [`Llm`] is configured.

use super::*;

#[test]
fn glm_thinks_at_low_effort_unless_told_otherwise() {
    let glm = Llm::glm("token", "glm-5.3");
    assert_eq!(glm.reasoning(), Some(GLM_REASONING));
    assert_eq!(glm.with_reasoning("high").unwrap().reasoning(), Some("high"));
    assert_eq!(Llm::glm("token", "glm-5.3").with_reasoning("default").unwrap().reasoning(), None, "default sends nothing");
    assert!(Llm::glm("token", "glm-5.3").with_reasoning("turbo").is_err());
    assert_eq!(Llm::local("http://127.0.0.1:8081", "qwen3:4b").reasoning(), None, "a local model is left alone");
}
