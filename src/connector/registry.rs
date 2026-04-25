use crate::connector::spec::ConnectorSpec;
use crate::connector::traits::Connector;
use dashmap::DashMap;
use std::sync::Arc;

struct RegisteredConnector {
    connector: Arc<dyn Connector>,
    spec: Option<ConnectorSpec>,
}

pub struct ConnectorRegistry {
    connectors: DashMap<String, RegisteredConnector>,
}

impl ConnectorRegistry {
    pub fn new() -> Self {
        Self {
            connectors: DashMap::new(),
        }
    }

    /// Register a connector without a spec (native connectors).
    pub fn register(&self, connector: Arc<dyn Connector>) {
        let name = connector.name().to_string();
        self.connectors.insert(
            name,
            RegisteredConnector {
                connector,
                spec: None,
            },
        );
    }

    /// Register a connector with its spec (YAML-driven connectors).
    pub fn register_with_spec(&self, connector: Arc<dyn Connector>, spec: ConnectorSpec) {
        let name = connector.name().to_string();
        self.connectors.insert(
            name,
            RegisteredConnector {
                connector,
                spec: Some(spec),
            },
        );
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Connector>> {
        self.connectors.get(name).map(|r| r.connector.clone())
    }

    /// Get the spec for a connector, if one was registered.
    pub fn get_spec(&self, name: &str) -> Option<ConnectorSpec> {
        self.connectors
            .get(name)
            .and_then(|r| r.spec.clone())
    }

    /// Remove a connector by name.
    pub fn deregister(&self, name: &str) -> bool {
        self.connectors.remove(name).is_some()
    }

    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = self.connectors.iter().map(|r| r.key().clone()).collect();
        names.sort();
        names
    }
}

impl Default for ConnectorRegistry {
    fn default() -> Self {
        Self::new()
    }
}
