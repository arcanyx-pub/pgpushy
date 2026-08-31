//! `pgpushy generate` (spec §4.7): vendor a command's output into the tree.
//!
//! Generation sits entirely upstream of discovery. The commands named by
//! `[[generate]]` run here and nowhere else — `validate`, `plan` and `apply`
//! read only files — so the SQL that reaches the pipeline is always the SQL
//! that was committed and reviewed, never whatever a tool emitted this
//! minute. `--check` is the freshness guarantee: run in CI, it fails the
//! moment a dependency bump changes the emission, forcing the change into a
//! reviewed diff.

use crate::config::{GenerateEntry, Loaded};
use crate::output::Output;
use crate::report;
use crate::run::Outcome;
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

/// The generated-source marker (spec §4.1, §4.7).
///
/// Opposite polarity to [`pgpushy_core::GENERATED_MARKER`]: a *document*
/// marker makes discovery skip pgpushy's output, while this marks *input* —
/// discovered like any hand-written file — and is what lets `generate` know
/// which files it may overwrite. The two must stay distinguishable from a
/// file's opening bytes.
pub const GENERATED_SOURCE_MARKER: &str =
    "-- Generated source. Do not edit; regenerate with `pgpushy generate`.";

/// `pgpushy generate`, or `pgpushy generate --check`.
pub fn run(loaded: &Loaded, output: Output, check: bool) -> Result<Outcome> {
    let entries = &loaded.file.generate;
    if entries.is_empty() {
        report::generate_nothing();
        return Ok(Outcome::Ok);
    }

    let settings = loaded.settings();
    let source_root = settings.source_root.clone();
    let seed_root = settings.seed_root.clone();
    let matchers: Vec<globset::GlobMatcher> = settings
        .exclude
        .iter()
        .map(|pattern| {
            globset::Glob::new(pattern)
                .map(|glob| glob.compile_matcher())
                .with_context(|| format!("invalid exclude pattern {pattern:?}"))
        })
        .collect::<Result<_>>()?;

    let mut outputs = BTreeSet::new();
    let mut written = Vec::new();
    let mut stale = Vec::new();

    for entry in entries {
        let path = resolve_output(entry, loaded.base(), &source_root, seed_root.as_deref())?;
        if let Ok(relative) = path.strip_prefix(&source_root) {
            let relative = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            if let Some(pattern) = settings
                .exclude
                .iter()
                .zip(&matchers)
                .find_map(|(pattern, matcher)| matcher.is_match(&relative).then_some(pattern))
            {
                bail!(
                    "[[generate]] output {} matches the exclude pattern {pattern:?}\n\
                     \n\
                     Discovery would never read the file, so its content could not \
                     become desired state (spec §4.7). Drop the pattern or move the \
                     output.",
                    entry.output.display(),
                );
            }
        }
        if !outputs.insert(path.clone()) {
            bail!(
                "two [[generate]] entries name the same output {}\n\
                 \n\
                 The second would silently overwrite the first; give each generator \
                 its own file.",
                entry.output.display(),
            );
        }

        let content = generate(entry, loaded.base(), output)?;

        if check {
            match std::fs::read(&path) {
                Ok(existing) if existing == content.as_bytes() => {}
                Ok(_) => stale.push((path, "differs from what the command emits")),
                Err(_) => stale.push((path, "does not exist")),
            }
            continue;
        }

        // Never overwrite a file this command cannot prove is its own —
        // neither an operator's SQL, nor a §8.7 document, nor anything it
        // cannot even read (spec §4.7). Bytes, not text: a non-UTF-8 file is
        // exactly the kind generate did not write.
        match std::fs::read(&path) {
            Ok(existing) if !existing.starts_with(GENERATED_SOURCE_MARKER.as_bytes()) => {
                bail!(
                    "{} exists and does not carry the generated-source marker\n\
                     \n\
                     pgpushy generate only overwrites files it wrote. Move or delete the \
                     file if its content should come from [[generate]].",
                    path.display(),
                )
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", path.display()));
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, &content).with_context(|| format!("writing {}", path.display()))?;
        written.push(path);
    }

    if check {
        if stale.is_empty() {
            report::generate_current(entries.len());
            Ok(Outcome::Ok)
        } else {
            report::generate_stale(&stale);
            Ok(Outcome::Failed)
        }
    } else {
        report::generate_wrote(&written);
        Ok(Outcome::Ok)
    }
}

/// Where one entry's output lands, after every containment rule (spec §4.7).
///
/// Resolved lexically — no symlink following — and required to land under
/// the source root or the seed root as a file discovery will retain, because
/// a generated file nothing will discover is a configuration mistake.
fn resolve_output(
    entry: &GenerateEntry,
    base: &Path,
    source_root: &Path,
    seed_root: Option<&Path>,
) -> Result<PathBuf> {
    let output = &entry.output;
    let bad = |why: &str| -> anyhow::Error {
        anyhow::anyhow!(
            "[[generate]] output {} {why}\n\
             \n\
             An output is a relative, `..`-free *.sql path under the source root or \
             the seed root (spec §4.7).",
            output.display(),
        )
    };

    if output.is_absolute() {
        return Err(bad("is absolute"));
    }
    for component in output.components() {
        match component {
            Component::Normal(name) => {
                if name.to_string_lossy().starts_with('.') {
                    return Err(bad("contains a hidden component, which discovery ignores"));
                }
            }
            Component::CurDir => {}
            _ => return Err(bad("leaves the directory it starts in")),
        }
    }
    if !output
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("sql"))
    {
        return Err(bad(
            "does not end in .sql, so discovery would never read it",
        ));
    }

    let resolved = base.join(output);
    if !resolved.starts_with(source_root) && !seed_root.is_some_and(|s| resolved.starts_with(s)) {
        return Err(bad("lands under neither the source root nor the seed root"));
    }
    Ok(resolved)
}

/// Run one entry's command and assemble the file content.
fn generate(entry: &GenerateEntry, base: &Path, output: Output) -> Result<String> {
    let Some(program) = entry.command.first() else {
        bail!(
            "[[generate]] entry for {} has an empty command",
            entry.output.display(),
        );
    };
    if output.verbose {
        report::generate_running(&entry.command);
    }

    // An argv vector, never a shell: there is no quoting or injection
    // surface, and the command's own stderr passes through untouched.
    let result = std::process::Command::new(program)
        .args(&entry.command[1..])
        .current_dir(base)
        .output()
        .with_context(|| format!("running {program}"))?;

    if !result.stderr.is_empty() {
        std::io::stderr().write_all(&result.stderr).ok();
    }
    if !result.status.success() {
        bail!(
            "the command for {} failed ({})",
            entry.output.display(),
            result.status,
        );
    }
    let stdout = String::from_utf8(result.stdout).map_err(|_| {
        anyhow::anyhow!(
            "the command for {} emitted bytes that are not UTF-8; its output becomes \
             a source file and must be text",
            entry.output.display(),
        )
    })?;
    if stdout.trim().is_empty() {
        bail!(
            "the command for {} produced no output\n\
             \n\
             An empty emission would schedule everything the file previously \
             described for deletion, so it is refused rather than written.",
            entry.output.display(),
        );
    }

    let newline = if stdout.ends_with('\n') { "" } else { "\n" };
    Ok(format!(
        "{GENERATED_SOURCE_MARKER}\n-- Command: {}\n\n{stdout}{newline}",
        entry.command.join(" "),
    ))
}
