//! Data model for Mermaid `requirementDiagram` diagrams.
//!
//! A requirement diagram models formal requirements, real-world elements, and
//! the relationships between them. Requirements carry an id, descriptive text,
//! and optional risk/verify-method metadata. Elements represent real-world
//! artefacts (code, documents, subsystems). Relationships link requirements and
//! elements via typed arcs (contains, copies, derives, satisfies, verifies,
//! refines, traces).
//!
//! Example source:
//!
//! ```text
//! requirementDiagram
//!
//!     requirement test_req {
//!         id: 1
//!         text: the test text.
//!         risk: high
//!         verifymethod: test
//!     }
//!
//!     element test_entity {
//!         type: simulation
//!     }
//!
//!     test_entity - satisfies -> test_req
//! ```
//!
//! Constructed by [`crate::parser::requirement_diagram::parse`] and consumed by
//! [`crate::render::requirement_diagram::render`].
//!
//! ## Phase 1 limitations
//!
//! - Custom styling/colours are not supported.
//! - `accDescr` / `accTitle` accessibility metadata is silently ignored.
//! - Layout is naive left-to-right; no crossing-minimisation is attempted.

// ---------------------------------------------------------------------------
// Enumerations
// ---------------------------------------------------------------------------

/// The formal type of a requirement node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RequirementKind {
    /// Generic `requirement` block.
    #[default]
    Requirement,
    /// `functionalRequirement` block.
    Functional,
    /// `interfaceRequirement` block.
    Interface,
    /// `performanceRequirement` block.
    Performance,
    /// `physicalRequirement` block.
    Physical,
    /// `designConstraint` block.
    DesignConstraint,
}

impl RequirementKind {
    /// Canonical display label used as the stereotype header in rendered boxes.
    pub fn label(self) -> &'static str {
        match self {
            RequirementKind::Requirement => "requirement",
            RequirementKind::Functional => "functionalRequirement",
            RequirementKind::Interface => "interfaceRequirement",
            RequirementKind::Performance => "performanceRequirement",
            RequirementKind::Physical => "physicalRequirement",
            RequirementKind::DesignConstraint => "designConstraint",
        }
    }
}

/// Risk classification on a requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    Low,
    Medium,
    High,
}

/// Method by which a requirement can be verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMethod {
    Analysis,
    Inspection,
    Test,
    Demonstration,
}

/// The semantic type of a relationship arc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationshipKind {
    Contains,
    Copies,
    Derives,
    Satisfies,
    Verifies,
    Refines,
    Traces,
}

impl RelationshipKind {
    /// Short display label used in relationship arrows.
    pub fn label(self) -> &'static str {
        match self {
            RelationshipKind::Contains => "contains",
            RelationshipKind::Copies => "copies",
            RelationshipKind::Derives => "derives",
            RelationshipKind::Satisfies => "satisfies",
            RelationshipKind::Verifies => "verifies",
            RelationshipKind::Refines => "refines",
            RelationshipKind::Traces => "traces",
        }
    }
}

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

/// A single requirement node in the diagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requirement {
    /// Formal category (requirement, functionalRequirement, …).
    pub kind: RequirementKind,
    /// Identifier name used to reference this requirement in relationships.
    pub name: String,
    /// Numeric or alphanumeric id (e.g. `"1"`, `"1.2.1"`).
    pub id: String,
    /// Human-readable requirement text.
    pub text: String,
    /// Optional risk classification.
    pub risk: Option<Risk>,
    /// Optional verification method.
    pub verify_method: Option<VerifyMethod>,
}

/// A real-world element (artefact, system, document) referenced in the diagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    /// Identifier name used to reference this element in relationships.
    pub name: String,
    /// Free-form element type (e.g. `"simulation"`, `"word doc"`).
    pub kind: String,
    /// Optional reference URL or document path.
    pub docref: Option<String>,
}

/// A directed relationship arc between two nodes (requirements or elements).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequirementRelationship {
    /// Name of the source node.
    pub source: String,
    /// Name of the target node.
    pub target: String,
    /// Semantic kind of the relationship.
    pub kind: RelationshipKind,
}

// ---------------------------------------------------------------------------
// Top-level diagram
// ---------------------------------------------------------------------------

/// A parsed `requirementDiagram` diagram.
///
/// Constructed by [`crate::parser::requirement_diagram::parse`] and consumed by
/// [`crate::render::requirement_diagram::render`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequirementDiagram {
    pub requirements: Vec<Requirement>,
    pub elements: Vec<Element>,
    pub relationships: Vec<RequirementRelationship>,
}

impl RequirementDiagram {
    /// Total number of nodes (requirements + elements) in the diagram.
    pub fn total_items(&self) -> usize {
        self.requirements.len() + self.elements.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_diagram_is_empty() {
        let d = RequirementDiagram::default();
        assert!(d.requirements.is_empty());
        assert!(d.elements.is_empty());
        assert!(d.relationships.is_empty());
        assert_eq!(d.total_items(), 0);
    }

    #[test]
    fn total_items_counts_requirements_and_elements() {
        let d = RequirementDiagram {
            requirements: vec![
                Requirement {
                    kind: RequirementKind::Requirement,
                    name: "req1".to_string(),
                    id: "1".to_string(),
                    text: "some text".to_string(),
                    risk: Some(Risk::High),
                    verify_method: Some(VerifyMethod::Test),
                },
                Requirement {
                    kind: RequirementKind::Functional,
                    name: "req2".to_string(),
                    id: "2".to_string(),
                    text: "other text".to_string(),
                    risk: None,
                    verify_method: None,
                },
            ],
            elements: vec![Element {
                name: "elem1".to_string(),
                kind: "simulation".to_string(),
                docref: Some("docs/elem1".to_string()),
            }],
            relationships: vec![RequirementRelationship {
                source: "elem1".to_string(),
                target: "req1".to_string(),
                kind: RelationshipKind::Satisfies,
            }],
        };
        assert_eq!(d.total_items(), 3); // 2 requirements + 1 element
        assert_eq!(d.relationships.len(), 1);
    }

    #[test]
    fn equality_holds_for_identical_diagrams() {
        let a = RequirementDiagram {
            requirements: vec![Requirement {
                kind: RequirementKind::Performance,
                name: "perf_req".to_string(),
                id: "3".to_string(),
                text: "perf text".to_string(),
                risk: Some(Risk::Medium),
                verify_method: Some(VerifyMethod::Demonstration),
            }],
            elements: vec![],
            relationships: vec![],
        };
        let b = a.clone();
        assert_eq!(a, b);

        let c = RequirementDiagram {
            requirements: vec![Requirement {
                kind: RequirementKind::Interface,
                name: "other".to_string(),
                id: "4".to_string(),
                text: "interface text".to_string(),
                risk: None,
                verify_method: Some(VerifyMethod::Analysis),
            }],
            elements: vec![],
            relationships: vec![],
        };
        assert_ne!(a, c);
    }
}
