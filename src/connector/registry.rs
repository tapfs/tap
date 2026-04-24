use crate::connector::spec::ConnectorSpec;
use crate::connector::traits::Connector;
use std::collections::HashMap;
use std::sync::Arc;

struct RegisteredConnector {
    connector: Arc<dyn Connector>,
    spec: Option<ConnectorSpec>,
}

pub struct ConnectorRegistry {
    connectors: HashMap<String, RegisteredConnector>,
}

impl ConnectorRegistry {
    pub fn new() -> Self {
        Self {
            connectors: HashMap::new(),
        }
    }

    /// Register a connector without a spec (native connectors).
    pub fn register(&mut self, connector: Arc<dyn Connector>) {
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
    pub fn register_with_spec(&mut self, connector: Arc<dyn Connector>, spec: ConnectorSpec) {
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
    pub fn get_spec(&self, name: &str) -> Option<&ConnectorSpec> {
        self.connectors.get(name).and_then(|r| r.spec.as_ref())
    }

    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = self.connectors.keys().cloned().collect();
        names.sort();
        names
    }
}

impl Default for ConnectorRegistry {
    fn default() -> Self {
        Self::new()
    }
}
