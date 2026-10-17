//! The paper workspace and its LaTeX export, as in `packages/evidence/src/paper.ts`.

use crate::{Error, Result, regex, rejected};
use nosh_sys::paths::{extname, relative_inside, resolve};
use regex::Captures;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

/// `manifest.json` of an export, in `JSON.stringify` field order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaperManifest {
    pub markdown_path: String,
    pub bibliography_path: String,
    pub latex_path: String,
    pub pdf_path: Option<String>,
    pub source_hash: String,
    pub command: Vec<String>,
    pub warnings: Vec<String>,
    pub exported_at: String,
}

const LATEX_COMMAND: [&str; 5] = [
    "latexmk",
    "-pdf",
    "-interaction=nonstopmode",
    "-halt-on-error",
    "paper.tex",
];

static ACTIVE_EXPORTS: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Mutex::default);

/// Creates `docs/paper.md` (with the standard sections), `docs/paper.bib`, and `docs/figures`.
pub fn initialize_paper_workspace(repository_root: &Path, title: &str) -> Result<()> {
    let docs = repository_root.join("docs");
    assert_unlinked_contained(repository_root, &docs, "Paper docs")?;
    std::fs::create_dir_all(docs.join("figures"))?;
    let markdown = docs.join("paper.md");
    let bibliography = docs.join("paper.bib");
    if markdown.exists() {
        assert_regular_file(&markdown, "Paper Markdown")?;
    } else {
        std::fs::write(
            &markdown,
            format!(
                "# {title}\n\n## Abstract\n\n## Introduction\n\n## Related work\n\n## Method\n\n## Experiments\n\n## Limitations\n\n## Conclusion\n"
            ),
        )?;
    }
    if bibliography.exists() {
        assert_regular_file(&bibliography, "Paper bibliography")?;
    } else {
        std::fs::write(&bibliography, "")?;
    }
    Ok(())
}

/// Converts the paper to LaTeX, builds a PDF with `latexmk` when available, and swaps the result
/// into `output_directory` atomically.
pub fn export_paper(
    markdown_path: &Path,
    bibliography_path: &Path,
    output_directory: &Path,
) -> Result<PaperManifest> {
    let output = resolve(output_directory)?;
    {
        let mut active = ACTIVE_EXPORTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !active.insert(output.clone()) {
            return rejected(format!(
                "Paper export is already running for {}",
                output.display()
            ));
        }
    }
    let result = export(markdown_path, bibliography_path, &output);
    ACTIVE_EXPORTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&output);
    result
}

fn export(markdown_path: &Path, bibliography_path: &Path, output: &Path) -> Result<PaperManifest> {
    let unique = || {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let random = nosh_wire::create_id(nosh_wire::IdPrefix::Cmd);
        format!("{}-{millis}-{}", std::process::id(), &random[4..17])
    };
    let staging = PathBuf::from(format!("{}.staging-{}", output.display(), unique()));
    let rollback = PathBuf::from(format!("{}.rollback-{}", output.display(), unique()));
    let markdown_absolute = resolve(markdown_path)?;
    let repository_root = markdown_absolute
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    assert_unlinked_contained(&repository_root, &staging, "Paper export staging")?;
    let built = (|| -> Result<PaperManifest> {
        assert_unlinked_contained(&repository_root, markdown_path, "Paper Markdown")?;
        assert_unlinked_contained(&repository_root, bibliography_path, "Paper bibliography")?;
        assert_regular_file(markdown_path, "Paper Markdown")?;
        if bibliography_path.exists() {
            assert_regular_file(bibliography_path, "Paper bibliography")?;
        }
        let markdown = std::fs::read_to_string(markdown_path)?;
        let bibliography = if bibliography_path.exists() {
            std::fs::read_to_string(bibliography_path)?
        } else {
            String::new()
        };
        std::fs::create_dir_all(&staging)?;
        let mut warnings = Vec::new();
        if bibliography_path.exists() {
            std::fs::copy(bibliography_path, staging.join("paper.bib"))?;
        }
        let latex = markdown_to_latex(
            &markdown,
            &mut warnings,
            bibliography_path,
            markdown_path,
            &staging,
        )?;
        std::fs::write(staging.join("paper.tex"), latex)?;
        let ran = nosh_sys::process::hidden(LATEX_COMMAND[0])
            .args(&LATEX_COMMAND[1..])
            .current_dir(&staging)
            .stdin(std::process::Stdio::null())
            .output();
        let (status_ok, log) = match &ran {
            Ok(out) => (
                out.status.success(),
                format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                ),
            ),
            Err(_) => (false, "\n".to_owned()),
        };
        std::fs::write(staging.join("export.log"), log)?;
        let has_pdf = status_ok && staging.join("paper.pdf").exists();
        if !has_pdf {
            warnings.push("PDF was not produced; install latexmk and a TeX distribution, then rerun the same command.".into());
        }
        let mut hasher = Sha256::new();
        hasher.update(markdown.as_bytes());
        hasher.update(b"\0");
        hasher.update(bibliography.as_bytes());
        let mut source_hash = String::from("sha256:");
        for byte in hasher.finalize() {
            write!(source_hash, "{byte:02x}").expect("writing to a String cannot fail");
        }
        let manifest = PaperManifest {
            markdown_path: markdown_absolute.to_string_lossy().into_owned(),
            bibliography_path: resolve(bibliography_path)?.to_string_lossy().into_owned(),
            latex_path: output.join("paper.tex").to_string_lossy().into_owned(),
            pdf_path: has_pdf.then(|| output.join("paper.pdf").to_string_lossy().into_owned()),
            source_hash,
            command: LATEX_COMMAND.iter().map(|s| (*s).to_owned()).collect(),
            warnings,
            exported_at: nosh_wire::now_utc(),
        };
        std::fs::write(
            staging.join("manifest.json"),
            format!("{}\n", serde_json::to_string(&manifest)?),
        )?;
        let prior = output.exists();
        if prior {
            std::fs::rename(output, &rollback)?;
        }
        if let Err(error) = std::fs::rename(&staging, output) {
            if prior && rollback.exists() {
                std::fs::rename(&rollback, output)?;
            }
            return Err(error.into());
        }
        if prior && let Err(error) = std::fs::remove_dir_all(&rollback) {
            eprintln!("nosh paper export retained {}: {error}", rollback.display());
        }
        Ok(manifest)
    })();
    if built.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    built
}

/// The LaTeX for a paper; its figures are copied into `output_directory/figures`.
pub fn markdown_to_latex(
    markdown: &str,
    warnings: &mut Vec<String>,
    bibliography_path: &Path,
    markdown_path: &Path,
    output_directory: &Path,
) -> Result<String> {
    let mut body: Vec<String> = Vec::new();
    let mut in_code = false;
    let mut table: Vec<String> = Vec::new();
    for source in markdown.replace("\r\n", "\n").split('\n') {
        let trimmed = js_trim(source);
        // `/^\|.*\|$/`: two pipes, and `.` matches no line terminator.
        let table_row = trimmed.len() >= 2
            && trimmed.starts_with('|')
            && trimmed.ends_with('|')
            && !trimmed.contains(['\r', '\u{2028}', '\u{2029}']);
        if !in_code && table_row {
            table.push(trimmed.to_owned());
            continue;
        }
        if !table.is_empty() {
            body.push(latex_table(&table, warnings));
            table.clear();
        }
        if source.starts_with("```") {
            body.push(
                if in_code {
                    "\\end{verbatim}"
                } else {
                    "\\begin{verbatim}"
                }
                .into(),
            );
            in_code = !in_code;
            continue;
        }
        if in_code {
            body.push(source.to_owned());
            continue;
        }
        if let Some(heading) = regex!(r"^(#{1,3})[\t\n\x0B\f\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]+([^\n\r\u{2028}\u{2029}]+)$").captures(source) {
            let command = match heading[1].len() {
                1 => "title",
                2 => "section",
                _ => "subsection",
            };
            body.push(format!("\\{command}{{{}}}", escape_latex(&heading[2])));
            continue;
        }
        if let Some(image) = regex!(r"^!\[([^\]]*)\]\(([^)]+)\)$").captures(source) {
            let figure = copy_paper_figure(markdown_path, output_directory, &image[2])?;
            body.push(format!("\\begin{{figure}}[ht]\\centering\\includegraphics[width=\\linewidth]{{{}}}\\caption{{{}}}\\end{{figure}}", escape_latex(&figure), escape_latex(&image[1])));
            continue;
        }
        if let Some(item) = source.strip_prefix("- ") {
            body.push(format!(
                "\\begin{{itemize}}\\item {}\\end{{itemize}}",
                inline(item)
            ));
            continue;
        }
        body.push(if source.is_empty() {
            String::new()
        } else {
            format!("{}\n", inline(source))
        });
    }
    if !table.is_empty() {
        body.push(latex_table(&table, warnings));
    }
    if in_code {
        warnings.push("Unclosed fenced code block.".into());
    }
    let has_bibliography = bibliography_path.exists()
        && !js_trim(&std::fs::read_to_string(bibliography_path)?).is_empty();
    let bibliography = if has_bibliography {
        "\\bibliographystyle{plain}\\bibliography{paper}"
    } else {
        ""
    };
    // `.replace(/(\\title\{[^}]+\})/, "$1\\maketitle")`: the first title only.
    let joined = body.join("\n");
    let with_title = regex!(r"(\\title\{[^}]+\})").replace(&joined, |captures: &Captures<'_>| {
        format!("{}\\maketitle", &captures[1])
    });
    Ok(format!(
        "\\documentclass[11pt]{{article}}\n\\usepackage[margin=1in]{{geometry}}\n\\usepackage[T1]{{fontenc}}\n\\usepackage{{graphicx}}\n\\usepackage{{tabularx}}\n\\usepackage{{hyperref}}\n\\begin{{document}}\n{with_title}\n{bibliography}\n\\end{{document}}\n"
    ))
}

/// A GitHub pipe table as a text-width `tabularx`; alignment from the separator row.
fn latex_table(rows: &[String], warnings: &mut Vec<String>) -> String {
    let cells = |row: &str| -> Vec<String> {
        let inner = row.get(1..row.len().saturating_sub(1)).unwrap_or_default();
        inner
            .split('|')
            .map(|cell| js_trim(cell).to_owned())
            .collect()
    };
    let separator = rows.get(1).map(|row| cells(row)).filter(|cells| {
        cells
            .iter()
            .all(|cell| regex!(r"^:?-{3,}:?$").is_match(cell))
    });
    let Some(separator) = separator else {
        warnings.push(format!(
            "Markdown table without a separator row was kept as text: {}",
            nosh_wire::js::slice_to(&rows[0], 80)
        ));
        return rows
            .iter()
            .map(|row| format!("{}\n", inline(row)))
            .collect::<Vec<_>>()
            .join("\n");
    };
    let columns: String = separator
        .iter()
        .map(|cell| {
            if cell.starts_with(':') && cell.ends_with(':') {
                "c"
            } else if cell.ends_with(':') {
                "r"
            } else {
                ">{\\raggedright\\arraybackslash}X"
            }
        })
        .collect();
    let line = |row: &str| {
        format!(
            "{} \\\\",
            cells(row)
                .iter()
                .map(|cell| inline(cell)
                    .replace("\\_", "\\_\\allowbreak{}")
                    .replace(" / ", " /\\allowbreak{} "))
                .collect::<Vec<_>>()
                .join(" & ")
        )
    };
    let data: Vec<String> = rows.iter().skip(2).map(|row| line(row)).collect();
    format!(
        "\\begin{{center}}\\footnotesize\\begin{{tabularx}}{{\\linewidth}}{{{columns}}}\\hline\n{}\n\\hline\n{}\n\\hline\\end{{tabularx}}\\end{{center}}",
        line(&rows[0]),
        data.join("\n")
    )
}

fn inline(value: &str) -> String {
    let escaped = escape_latex(value);
    let bold = regex!(r"\*\*([^*]+)\*\*").replace_all(&escaped, "\\textbf{$1}");
    let code = regex!(r"`([^`]+)`").replace_all(&bold, |captures: &Captures<'_>| {
        format!("\\texttt{{{}}}", break_hex_runs(&captures[1]))
    });
    let links = regex!(r"\[([^\]]+)\]\(([^)]+)\)").replace_all(&code, |captures: &Captures<'_>| {
        let url = &captures[2];
        if regex!(r"(?i)^https?://").is_match(&url.replace("\\%", "%")) {
            format!("\\href{{{url}}}{{{}}}", &captures[1])
        } else {
            captures[1].to_owned()
        }
    });
    regex!(r"\[@([^\]]+)\]")
        .replace_all(&links, "\\cite{$1}")
        .into_owned()
}

/// `/([0-9a-f]{8})(?=[0-9a-f]{8})/g` → `$1\allowbreak{}`: a break after every 8 hex characters
/// that are followed by 8 more.
fn break_hex_runs(code: &str) -> String {
    let bytes = code.as_bytes();
    let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    let mut out = String::with_capacity(code.len() + 16);
    let mut index = 0;
    while index < bytes.len() {
        let window = |start: usize| {
            start + 8 <= bytes.len() && bytes[start..start + 8].iter().all(|b| hex(*b))
        };
        if window(index) && window(index + 8) {
            out.push_str(&code[index..index + 8]);
            out.push_str("\\allowbreak{}");
            index += 8;
        } else {
            let next = code[index..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&code[index..index + next]);
            index += next;
        }
    }
    out
}

fn escape_latex(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\textbackslash{}"),
            '&' | '%' | '$' | '#' | '_' | '{' | '}' => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out
}

fn copy_paper_figure(
    markdown_path: &Path,
    output_directory: &Path,
    reference: &str,
) -> Result<String> {
    let reference_path = Path::new(reference);
    if reference_path.is_absolute()
        || reference.starts_with('/')
        || reference.starts_with('\\')
        || regex!(r"(?i)^(?:[a-z][a-z0-9+.-]*:|//)").is_match(reference)
    {
        return rejected(format!(
            "Paper image path is not a local figure: {reference}"
        ));
    }
    let docs_root = markdown_path.parent().unwrap_or(Path::new("."));
    let figures_root = resolve(&docs_root.join("figures"))?;
    let source = resolve(&docs_root.join(reference))?;
    let inside = relative_inside(&figures_root, &source)?;
    if inside.is_none_or(|rest| rest.as_os_str().is_empty() || rest.starts_with(".."))
        || !source.exists()
    {
        return rejected(format!(
            "Paper image is not a readable docs/figures asset: {reference}"
        ));
    }
    assert_unlinked_contained(docs_root, &source, &format!("Paper image {reference}"))?;
    assert_unlinked_contained(
        output_directory.parent().unwrap_or(output_directory),
        output_directory,
        "Paper export output",
    )?;
    assert_regular_file(&source, &format!("Paper image {reference}"))?;
    let extension = extname(&source.to_string_lossy()).to_lowercase();
    if ![".eps", ".jpeg", ".jpg", ".pdf", ".png", ".svg"].contains(&extension.as_str()) {
        return rejected(format!(
            "Paper image is not a supported regular figure file: {reference}"
        ));
    }
    let mut digest = String::new();
    for byte in Sha256::digest(source.to_string_lossy().as_bytes()) {
        write!(digest, "{byte:02x}").expect("writing to a String cannot fail");
    }
    let name = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let target_name = format!("{}-{name}", &digest[..16]);
    let target = output_directory.join("figures").join(&target_name);
    assert_unlinked_contained(output_directory, &target, "Paper export figure output")?;
    std::fs::create_dir_all(target.parent().expect("a figures directory"))?;
    assert_unlinked_contained(output_directory, &target, "Paper export figure output")?;
    std::fs::copy(&source, &target)?;
    Ok(format!("figures/{target_name}"))
}

pub(crate) fn assert_regular_file(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(stat) if stat.is_file() && !stat.is_symlink() => Ok(()),
        _ => Err(Error::Rejected(format!("{label} is not a regular file"))),
    }
}

/// `candidate` must be inside `root`, with no symbolic link or junction from the filesystem root down.
pub(crate) fn assert_unlinked_contained(root: &Path, candidate: &Path, label: &str) -> Result<()> {
    let base = resolve(root)?;
    let target = resolve(candidate)?;
    let Some(inside) = relative_inside(&base, &target)? else {
        return rejected(format!("{label} escapes its allowed root"));
    };
    let mut chain: Vec<PathBuf> = base.ancestors().map(Path::to_path_buf).collect();
    let mut current = base.clone();
    for segment in inside.components() {
        current.push(segment);
        chain.push(current.clone());
    }
    for path in chain {
        if std::fs::symlink_metadata(&path).is_ok_and(|stat| stat.is_symlink()) {
            return rejected(format!("{label} traverses a symlink or reparse point"));
        }
    }
    Ok(())
}

/// `String.prototype.trim`.
pub(crate) fn js_trim(value: &str) -> &str {
    crate::episodes::js_trim(value)
}
