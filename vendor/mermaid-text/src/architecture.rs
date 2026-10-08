//! Data model for Mermaid `architecture-beta` diagrams.
//!
//! An architecture diagram describes services (rectangular nodes) optionally
//! collected into named groups (cluster boxes), with directed or undirected
//! edges between them. Ports on edges indicate which side of a service box
//! the connection attaches to.
//!
//! Example source:
//!
//! ```text
//! architecture-beta
//!     group api(cloud)[API]
//!
//!     service db(database)[Database] in api
//!     service disk1(disk)[Storage] in api
//!     service disk2(disk)[Storage] in api
//!     service server(server)[Server] in api
//!
//!     db:L -- R:server
//!     disk1:T -- B:server
//!     disk2:T -- B:db
//! ```
//!
//! Constructed by [`crate::parser::architecture::parse`] and consumed by
//! [`crate::render::architecture::render`].
//!
//! ## Current limitations
//!
//! - Icon names are parsed but not rendered (records them for future
//!   icon-library integration).
//! - Junction nodes (`junction(jid)`) are silently skipped.
//! - `accDescr` / `accTitle` are silently skipped.
//! - Port specifiers (`L`/`R`/`T`/`B`) on edges are stored but ignored by the
//!   renderer — spatial port-aware attachment is deferred to Path B.

/// Which side of a service box an edge attaches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Port {
    /// Left side of the box.
    Left,
    /// Right side of the box.
    Right,
    /// Top side of the box.
    Top,
    /// Bottom side of the box.
    Bottom,
}

impl Port {
    /// Single-character abbreviation used in Mermaid source (`L`, `R`, `T`, `B`).
    pub fn abbreviation(self) -> char {
        match self {
            Port::Left => 'L',
            Port::Right => 'R',
            Port::Top => 'T',
            Port::Bottom => 'B',
        }
    }
}

/// A named group (cluster) in an `architecture-beta` diagram.
///
/// Groups act as visual containers for services. In Phase 1 the group is
/// rendered as a labeled border box; the icon name is stored for future use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchGroup {
    /// Identifier used to reference this group from service declarations.
    pub id: String,
    /// Optional icon name (e.g. `cloud`, `database`). Parsed but not rendered in Phase 1.
    pub icon: Option<String>,
    /// Optional display label shown in the group header box.
    pub label: Option<String>,
}

/// A service node in an `architecture-beta` diagram.
///
/// Services are the primary entities of an architecture diagram. Each service
/// optionally belongs to a group (via `in <group_id>`). Top-level services
/// not assigned to any group are rendered outside the group boxes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchService {
    /// Unique identifier for this service.
    pub id: String,
    /// Optional icon name (e.g. `server`, `disk`). Parsed but not rendered in Phase 1.
    pub icon: Option<String>,
    /// Optional display label shown inside the service box.
    pub label: Option<String>,
    /// The id of the group this service belongs to, or `None` for top-level services.
    pub group: Option<String>,
}

impl ArchService {
    /// The text to display inside the service box.
    ///
    /// Returns `label` when present, otherwise falls back to `id`.
    pub fn display_label(&self) -> &str {
        match &self.label {
            Some(l) if !l.is_empty() => l.as_str(),
            _ => &self.id,
        }
    }
}

/// A directed or undirected edge between two services in an
/// `architecture-beta` diagram.
///
/// Port specifiers (`L`/`R`/`T`/`B`) indicate which side of the service box
/// the edge connects to. Simple `-->` edges have no port specifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchEdge {
    /// Identifier of the source service.
    pub source: String,
    /// Port on the source service side, if specified.
    pub source_port: Option<Port>,
    /// Identifier of the target service.
    pub target: String,
    /// Port on the target service side, if specified.
    pub target_port: Option<Port>,
    /// Optional edge label (not widely used in the Mermaid spec but captured for completeness).
    pub label: Option<String>,
}

/// A parsed `architecture-beta` diagram.
///
/// Constructed by [`crate::parser::architecture::parse`] and consumed by
/// [`crate::render::architecture::render`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Architecture {
    /// Groups (cluster boxes) declared in the diagram.
    pub groups: Vec<ArchGroup>,
    /// Services (node boxes) declared in the diagram.
    pub services: Vec<ArchService>,
    /// Edges between services.
    pub edges: Vec<ArchEdge>,
}

impl Architecture {
    /// Total number of groups in the diagram.
    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// Total number of services in the diagram.
    pub fn service_count(&self) -> usize {
        self.services.len()
    }

    /// Total number of edges in the diagram.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Look up a group by its `id`. Returns `None` if no group has that id.
    pub fn find_group(&self, id: &str) -> Option<&ArchGroup> {
        self.groups.iter().find(|g| g.id == id)
    }

    /// Look up a service by its `id`. Returns `None` if no service has that id.
    pub fn find_service(&self, id: &str) -> Option<&ArchService> {
        self.services.iter().find(|s| s.id == id)
    }

    /// Return services that belong to the given group id.
    pub fn services_in_group<'a>(&'a self, group_id: &str) -> Vec<&'a ArchService> {
        self.services
            .iter()
            .filter(|s| s.group.as_deref() == Some(group_id))
            .collect()
    }

    /// Return top-level services that do not belong to any group.
    pub fn top_level_services(&self) -> Vec<&ArchService> {
        self.services.iter().filter(|s| s.group.is_none()).collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_architecture_is_empty() {
        let arch = Architecture::default();
        assert_eq!(arch.group_count(), 0);
        assert_eq!(arch.service_count(), 0);
        assert_eq!(arch.edge_count(), 0);
    }

    #[test]
    fn arch_service_display_label_falls_back_to_id() {
        let with_label = ArchService {
            id: "db".to_string(),
            icon: None,
            label: Some("Database".to_string()),
            group: None,
        };
        assert_eq!(with_label.display_label(), "Database");

        let no_label = ArchService {
            id: "db".to_string(),
            icon: None,
            label: None,
            group: None,
        };
        assert_eq!(no_label.display_label(), "db");

        let empty_label = ArchService {
            id: "srv".to_string(),
            icon: None,
            label: Some(String::new()),
            group: None,
        };
        assert_eq!(empty_label.display_label(), "srv");
    }

    #[test]
    fn equality_holds_for_identical_diagrams() {
        let a = Architecture {
            groups: vec![ArchGroup {
                id: "api".to_string(),
                icon: Some("cloud".to_string()),
                label: Some("API".to_string()),
            }],
            services: vec![ArchService {
                id: "db".to_string(),
                icon: Some("database".to_string()),
                label: Some("Database".to_string()),
                group: Some("api".to_string()),
            }],
            edges: vec![ArchEdge {
                source: "db".to_string(),
                source_port: Some(Port::Left),
                target: "server".to_string(),
                target_port: Some(Port::Right),
                label: None,
            }],
        };
        let b = a.clone();
        assert_eq!(a, b);

        let c = Architecture::default();
        assert_ne!(a, c);
    }

    #[test]
    fn port_abbreviations_are_correct() {
        assert_eq!(Port::Left.abbreviation(), 'L');
        assert_eq!(Port::Right.abbreviation(), 'R');
        assert_eq!(Port::Top.abbreviation(), 'T');
        assert_eq!(Port::Bottom.abbreviation(), 'B');
    }

    #[test]
    fn find_group_and_service_helpers() {
        let arch = Architecture {
            groups: vec![ArchGroup {
                id: "g1".to_string(),
                icon: None,
                label: Some("G1".to_string()),
            }],
            services: vec![
                ArchService {
                    id: "s1".to_string(),
                    icon: None,
                    label: None,
                    group: Some("g1".to_string()),
                },
                ArchService {
                    id: "s2".to_string(),
                    icon: None,
                    label: None,
                    group: None,
                },
            ],
            edges: vec![],
        };

        assert!(arch.find_group("g1").is_some());
        assert!(arch.find_group("missing").is_none());
        assert!(arch.find_service("s1").is_some());
        assert!(arch.find_service("nope").is_none());

        let in_g1 = arch.services_in_group("g1");
        assert_eq!(in_g1.len(), 1);
        assert_eq!(in_g1[0].id, "s1");

        let top_level = arch.top_level_services();
        assert_eq!(top_level.len(), 1);
        assert_eq!(top_level[0].id, "s2");
    }

    #[test]
    fn arch_edge_port_is_optional() {
        let with_ports = ArchEdge {
            source: "a".to_string(),
            source_port: Some(Port::Left),
            target: "b".to_string(),
            target_port: Some(Port::Right),
            label: None,
        };
        assert!(with_ports.source_port.is_some());
        assert!(with_ports.target_port.is_some());

        let without_ports = ArchEdge {
            source: "a".to_string(),
            source_port: None,
            target: "b".to_string(),
            target_port: None,
            label: None,
        };
        assert!(without_ports.source_port.is_none());
        assert!(without_ports.target_port.is_none());
        assert_ne!(with_ports, without_ports);
    }
}
