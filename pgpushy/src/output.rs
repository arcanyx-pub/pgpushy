//! How much pgpushy says, and whether anything is colored.
//!
//! Both decisions are global to a run and consulted from several places, so
//! they are resolved once and carried rather than re-derived.
//!
//! The colour question is really about **pgschema**, not pgpushy: pgpushy's own
//! output is plain text, but pgschema colours unconditionally, so a
//! `pgpushy plan > review.txt` used to capture escape sequences. pgschema has
//! `--no-color`; pgpushy decides when to pass it.

use std::io::IsTerminal;

/// Run-wide output settings.
#[derive(Debug, Clone, Copy)]
pub struct Output {
    /// Print the pgschema command line and the synthesized document's path.
    pub verbose: bool,
    /// Whether anything downstream may use colour.
    pub color: bool,
}

impl Output {
    /// Decide from the flags and the environment.
    ///
    /// Colour is on only when stdout is a terminal, nobody asked for it off,
    /// and `NO_COLOR` is unset. The `NO_COLOR` convention is presence-based:
    /// any value at all, including the empty string, means no colour.
    pub fn resolve(verbose: bool, no_color: bool) -> Self {
        let disabled = no_color || std::env::var_os("NO_COLOR").is_some();
        Self {
            verbose,
            color: !disabled && std::io::stdout().is_terminal(),
        }
    }

    /// The colour flag to pass pgschema, if any.
    pub fn pgschema_color_flags(&self) -> Vec<String> {
        if self.color {
            Vec::new()
        } else {
            vec!["--no-color".into()]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Output;

    /// Under `cargo test` stdout is captured rather than a terminal, which is
    /// exactly the case colour must stay off for.
    #[test]
    fn no_color_when_stdout_is_not_a_terminal() {
        let output = Output::resolve(false, false);
        assert!(!output.color);
        assert_eq!(output.pgschema_color_flags(), vec!["--no-color".to_owned()]);
    }

    #[test]
    fn asking_for_no_color_is_honored() {
        assert!(!Output::resolve(false, true).color);
    }

    #[test]
    fn color_output_passes_no_flag() {
        let colored = Output {
            verbose: false,
            color: true,
        };
        assert!(colored.pgschema_color_flags().is_empty());
    }
}
