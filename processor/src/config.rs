use serde::{Deserialize, Serialize};

/// Top-level config file structure.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Config {
    #[serde(default)]
    pub processor: Vec<ProcessorRule>,
}

/// One rule: a filename pattern and what to do when it matches.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProcessorRule {
    /// Glob pattern matched against the filename (not the full path).
    /// e.g. "*.pdf", "*.{jpg,png}"
    pub r#match: String,

    /// Explicit chain of steps. Each step's output feeds the next.
    #[serde(default)]
    pub chain: Vec<ChainStep>,

    /// Shell expression passed to `sh -c`. Mutually exclusive with `chain`.
    /// `{input}` is substituted with the temp file path.
    /// Output is read from stdout.
    pub shell: Option<String>,

    /// I/O mode for `shell` expressions.
    #[serde(default)]
    pub io: IoMode,
}

/// A single step in a processor chain.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChainStep {
    pub command: String,
    /// Arguments. Use `{input}` and `{output}` as placeholders.
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub io: IoMode,
}

/// How a processor step receives input and delivers output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[cfg_attr(feature = "clap", derive(clap::ValueEnum))]
#[cfg_attr(feature = "clap", clap(rename_all = "kebab-case"))]
#[serde(rename_all = "kebab-case")]
pub enum IoMode {
    /// Tool modifies the file at `{input}` in place (`{input}` == `{output}`).
    #[default]
    InPlace,
    /// Tool reads `{input}`, writes to a separate `{output}` path.
    FileToFile,
    /// Tool reads `{input}` as a file argument, result is captured from stdout.
    FileToStdout,
    /// Tool reads from stdin, result is captured from stdout.
    StdinStdout,
}

impl Config {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&text)?;
        Ok(config)
    }

    /// Find the first rule whose pattern matches `filename`.
    pub fn find_rule(&self, filename: &str) -> Option<&ProcessorRule> {
        self.processor.iter().find(|rule| glob_match(&rule.r#match, filename))
    }
}

fn glob_match(pattern: &str, filename: &str) -> bool {
    match glob::Pattern::new(pattern) {
        Ok(pat) => pat.matches(filename),
        Err(_) => false,
    }
}
