use crate::connector::traits::Connector;
use std::collections::HashMap;
use std::sync::Arc;

pub struct ConnectorRegistry {
    connectors: HashMap<String, Arc<dyn Connector>>,
}

impl ConnectorRegistry {
    pub fn new() -> Self {
        Self {
            connectors: HashMap::new(),
        }
    }

    pub fn register(&mut self, connector: Arc<dyn Connector>) {
        let name = connector.name().to_string();
        self.connectors.insert(name, connector);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Connector>> {
        self.connectors.get(name).cloned()
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
