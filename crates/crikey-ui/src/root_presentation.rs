//! Host-admitted root presentation, independent of executable row identities.
use crikey_core::result_presentation::ScopeId;
use crikey_core::{Generation, PluginId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicationIdentity {
    Catalog {
        instance: u64,
        generation: Generation,
        producer: u64,
        publication: u64,
    },
    Suggestion {
        generation: Generation,
        producer: u64,
    },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeIdentity {
    pub owner: PluginId,
    pub id: ScopeId,
    pub source: PublicationIdentity,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeChoice {
    pub identity: ScopeIdentity,
    pub label: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionHeader {
    /// Logical executable-row index. Headers never participate in selection.
    pub row: usize,
    pub owner: PluginId,
    pub id: crikey_core::result_presentation::SectionId,
    pub label: String,
}
