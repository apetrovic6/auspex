//! The sentinel spellings the templates use, kept here because the api generators' test
//! fixtures render a service skeleton with them. The full scaffolder lives in the CLI crate;
//! products keep only `Sentinels` for that fixture.
//!
//! `Sentinels` is used only by `#[cfg(test)]` fixtures, so the whole module is dead code in
//! the shipped binary.
#![allow(dead_code)]

use crate::repo;

/// The five spellings a template name takes.
pub struct Sentinels {
    kebab: String,
    snake: String,
    pascal: String,
    screaming: String,
    /// `None` for a template with no development server of its own.
    port: Option<u16>,
}

impl Sentinels {
    pub fn new(name: &str) -> Self {
        let snake = name.replace('-', "_");
        Self {
            kebab: name.to_owned(),
            screaming: snake.to_uppercase(),
            snake,
            pascal: repo::to_pascal(name),
            port: None,
        }
    }

    pub(crate) fn with_port(name: &str, port: u16) -> Self {
        Self {
            port: Some(port),
            ..Self::new(name)
        }
    }

    pub fn apply(&self, text: &str) -> String {
        let text = text
            .replace("__name__", &self.kebab)
            .replace("__screaming__", &self.screaming)
            .replace("__snake__", &self.snake)
            .replace("__pascal__", &self.pascal);
        match self.port {
            Some(port) => text.replace("__port__", &port.to_string()),
            None => text,
        }
    }
}
