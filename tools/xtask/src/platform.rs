//! The native platforms a product can carry. The full generator (deriving a project from a
//! donor or a skeleton) lives in the CLI crate; products keep only the enum, for
//! `i18n-resources`, which names a platform per invocation.

use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Platform {
    /// A Gradle project, built by `just apps/<app>/android`.
    Android,
    /// An Xcode project generated from `project.yml`, macOS hosts only.
    Apple,
}

impl Platform {
    pub const fn dir(self) -> &'static str {
        match self {
            Self::Android => "android",
            Self::Apple => "apple",
        }
    }
}
