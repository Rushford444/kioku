//! Command fingerprint mode selection.

use crate::fingerprint::Mode;

/// Selects an explicit mode, defaulting to exact bytes for every command.
#[must_use]
pub fn effective_mode(_argv: &[String], explicit: Option<Mode>) -> Mode {
    explicit.unwrap_or(Mode::Bytes)
}

#[cfg(test)]
mod tests {
    use super::{Mode, effective_mode};

    /// Builds an argument vector.
    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    /// An explicit mode always wins.
    #[test]
    fn explicit_mode_wins() {
        assert_eq!(
            effective_mode(&argv(&["prettier", "--check"]), Some(Mode::Ast)),
            Mode::Ast
        );
        assert_eq!(
            effective_mode(&argv(&["cargo", "test"]), Some(Mode::Bytes)),
            Mode::Bytes
        );
    }

    /// Formatters and linters read whitespace, so they need byte fingerprints.
    #[test]
    fn lint_and_format_commands_force_bytes() {
        let commands: &[&[&str]] = &[
            &["prettier", "--check"],
            &["eslint", "src"],
            &["biome", "check"],
            &["rustfmt", "main.rs"],
            &["ruff", "check"],
            &["black", "--check"],
            &["stylelint", "*.css"],
            &["markdownlint", "README.md"],
            &["cargo", "fmt"],
            &["cargo", "fmt", "--check"],
            &["cargo", "clippy"],
            &["/usr/local/bin/prettier"],
            &["prettier.cmd"],
            &["npx", "prettier"],
            &["npx", "-", "prettier"],
            &["npx", "--", "prettier"],
            &["npx", "--yes", "eslint"],
            &["npx", "-p", "prettier", "eslint"],
            &["npm", "exec", "prettier"],
            &["npm", "exec", "--", "eslint"],
            &["pnpm", "exec", "eslint"],
            &["pnpm", "dlx", "prettier"],
            &["yarn", "dlx", "eslint"],
            &["bunx", "prettier"],
            &["bun", "x", "prettier"],
            &["uvx", "ruff"],
            &["uv", "tool", "run", "black"],
            &["pipx", "run", "black"],
            &["npm", "run", "lint"],
            &["pnpm", "run", "lint"],
            &["yarn", "run", "lint"],
            &["uv", "run", "lint"],
            &["npm", "run", "format:check"],
            &["npm", "run-script", "lint"],
            &["yarn", "lint"],
            &["pnpm", "prettier"],
            &["bun", "run", "fmt"],
        ];
        for command in commands {
            assert_eq!(
                effective_mode(&argv(command), None),
                Mode::Bytes,
                "command {command:?}"
            );
        }
    }

    /// Every command defaults to byte fingerprints.
    #[test]
    fn other_commands_use_bytes() {
        let commands: &[&[&str]] = &[
            &["cargo", "test"],
            &["cargo", "build"],
            &["tsc", "--noEmit"],
            &["npm", "test"],
            &["npm", "run", "build"],
            &["yarn", "build"],
            &["pnpm", "test"],
            &["npx", "tsc"],
            &["node", "script.js"],
            &["make", "check"],
        ];
        for command in commands {
            assert_eq!(
                effective_mode(&argv(command), None),
                Mode::Bytes,
                "command {command:?}"
            );
        }
    }
}
