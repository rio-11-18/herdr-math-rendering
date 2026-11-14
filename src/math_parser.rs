//! Pure detection of display equations in terminal cells.

use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MathBlock {
    pub row: usize,
    pub rows: usize,
    /// Terminal column where the equation's rows begin. Cells left of it (an
    /// agent's indentation or message bullet) keep their original text.
    pub col: usize,
    pub latex: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InlineMath {
    pub row: usize,
    pub col: usize,
    pub cols: usize,
    pub latex: String,
}

fn escaped(bytes: &[u8], index: usize) -> bool {
    let mut backslashes = 0;
    let mut cursor = index;
    while cursor > 0 && bytes[cursor - 1] == b'\\' {
        backslashes += 1;
        cursor -= 1;
    }
    backslashes % 2 == 1
}

/// Locate one-row TeX spans without consuming nearby prose or code literals.
pub(crate) fn inline_blocks(lines: &[String], display: &[MathBlock]) -> Vec<InlineMath> {
    let mut found = Vec::new();
    let mut fence: Option<&str> = None;
    let mut display_end: Option<&str> = None;
    for (row, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let marker = if trimmed.starts_with("```") {
            Some("```")
        } else if trimmed.starts_with("~~~") {
            Some("~~~")
        } else {
            None
        };
        if let Some(marker) = marker {
            fence = match fence {
                Some(current) if current == marker => None,
                Some(current) => Some(current),
                None => Some(marker),
            };
            continue;
        }
        if fence.is_some() {
            continue;
        }
        if let Some(end) = display_end {
            if trimmed == end {
                display_end = None;
            }
            continue;
        }
        if trimmed == "$$" {
            display_end = Some("$$");
            continue;
        }
        if trimmed == "\\[" {
            display_end = Some("\\]");
            continue;
        }
        if display
            .iter()
            .any(|block| row >= block.row && row < block.row + block.rows)
            || line.contains('\t')
        {
            continue;
        }

        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'`' && !escaped(bytes, i) {
                let count = bytes[i..].iter().take_while(|&&byte| byte == b'`').count();
                let mut end = i + count;
                while end < bytes.len() {
                    if bytes[end..].starts_with(&bytes[i..i + count]) {
                        break;
                    }
                    end += 1;
                }
                i = if end < bytes.len() {
                    end + count
                } else {
                    bytes.len()
                };
                continue;
            }
            let paren = bytes[i..].starts_with(b"\\(") && !escaped(bytes, i);
            let dollar = bytes[i] == b'$' && !escaped(bytes, i);
            if !paren && !dollar {
                i += 1;
                continue;
            }
            if dollar && ((i > 0 && bytes[i - 1] == b'$') || bytes.get(i + 1) == Some(&b'$')) {
                i += 1;
                continue;
            }
            let open_len = if paren { 2 } else { 1 };
            let close: &[u8] = if paren { b"\\)" } else { b"$" };
            let mut end = i + open_len;
            while end < bytes.len() && (!bytes[end..].starts_with(close) || escaped(bytes, end)) {
                end += 1;
            }
            if end == bytes.len() {
                i += open_len;
                continue;
            }
            let latex = &line[i + open_len..end];
            let last = end + close.len();
            if latex.trim().is_empty()
                || latex.len() > 256
                || !latex.is_ascii()
                || latex.trim() != latex
                || (dollar
                    && (bytes.get(last) == Some(&b'$')
                        || bytes.get(last).is_some_and(u8::is_ascii_digit)))
            {
                i += open_len;
                continue;
            }
            found.push(InlineMath {
                row,
                col: UnicodeWidthStr::width(&line[..i]),
                cols: UnicodeWidthStr::width(&line[i..last]),
                latex: latex.to_owned(),
            });
            i = last;
        }
    }
    found
}

/// A display equation cut by the edge of the pane. Its own rows cannot be
/// typeset alone; they are matched against complete blocks seen earlier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClippedMath {
    /// First visible row of the fragment, including any rows that are not
    /// math (for example a sticky header above a top fragment).
    pub row: usize,
    /// Trimmed visible rows. A top fragment ends with its closing delimiter;
    /// a bottom fragment starts with its opening delimiter.
    pub lines: Vec<String>,
    pub top: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DisplayScan {
    pub blocks: Vec<MathBlock>,
    pub clipped: Vec<ClippedMath>,
}

/// Find complete display equations in one viewport. Rows are relative to the
/// pane, so a subsequent scroll produces fresh positions from fresh cells.
pub(crate) fn display_blocks(lines: &[String]) -> Vec<MathBlock> {
    display_scan(lines).blocks
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Row<'a> {
    Fence(&'a str),
    Delimiter,
    /// TeX followed by `$$` (closing) or `$$$$` (closing then opening).
    Joined(&'a str, bool),
    BracketOpen,
    BracketClose,
    Text,
}

/// Codex begins each answer with a `• ` bullet, so its first display equation
/// opens on a `• $$` row. Treat list markers before a delimiter as layout.
fn strip_marker(line: &str) -> &str {
    for marker in ["• ", "◦ ", "- ", "* "] {
        if let Some(rest) = line.strip_prefix(marker) {
            let rest = rest.trim_start();
            if matches!(rest, "$$" | "\\[") {
                return rest;
            }
        }
    }
    line
}

/// Width of the indentation (and any delimiter list marker) before a row's
/// content, or None for a blank row.
pub(crate) fn content_column(line: &str) -> Option<usize> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let content = strip_marker(trimmed);
    let start = (line.len() - line.trim_start().len()) + (trimmed.len() - content.len());
    Some(UnicodeWidthStr::width(&line[..start]))
}

fn classify(line: &str) -> Row<'_> {
    let line = strip_marker(line);
    if line.starts_with("```") {
        return Row::Fence("```");
    }
    if line.starts_with("~~~") {
        return Row::Fence("~~~");
    }
    match line {
        "$$" => return Row::Delimiter,
        "$$$$" => return Row::Joined("", true),
        "\\[" => return Row::BracketOpen,
        "\\]" => return Row::BracketClose,
        _ => {}
    }
    for (suffix, reopen) in [("$$$$", true), ("$$", false)] {
        if let Some(last) = line.strip_suffix(suffix) {
            if !last.is_empty() && !last.contains('$') && last.is_ascii() {
                return Row::Joined(last, reopen);
            }
        }
    }
    Row::Text
}

fn tex_rows_valid(rows: &[&str]) -> bool {
    let latex = rows.join("\n");
    // A blank line inside display math is a paragraph break, which TeX
    // rejects. Codex lays out a row before typing it, so such a block is a
    // streaming snapshot rather than an equation.
    !rows.iter().any(|row| row.trim().is_empty())
        && !latex.trim().is_empty()
        && latex.is_ascii()
        && latex.len() <= 1024
        && !latex.contains('$')
        && rows.iter().all(|value| {
            value.len() <= 3
                || value
                    .bytes()
                    .any(|byte| b"\\=^_+-*/()[]{}<>".contains(&byte))
        })
}

/// Scan one viewport under a fixed pairing of `$$` rows. `inside_at_top` says whether
/// row 0 is already inside an equation whose opening row scrolled away.
fn scan_parity(trimmed: &[&str], inside_at_top: bool) -> (i32, DisplayScan) {
    let mut scan = DisplayScan::default();
    let mut score = 0;
    let mut fence: Option<&str> = None;
    // (opening row or None when clipped at the top, closing kind, content rows)
    let mut open: Option<(Option<usize>, &str, Vec<&str>)> =
        inside_at_top.then(|| (None, "$$", Vec::new()));
    let close_block = |scan: &mut DisplayScan,
                       score: &mut i32,
                       start: Option<usize>,
                       end: usize,
                       body: &[&str]| {
        let valid = tex_rows_valid(body);
        match start {
            Some(start) if valid && end - start < 24 => {
                *score += 2;
                scan.blocks.push(MathBlock {
                    row: start,
                    rows: end - start + 1,
                    col: 0,
                    latex: body.join("\n"),
                });
            }
            Some(_) => *score -= 3,
            None => {
                // Only the rows after the pane top are known. Prose above
                // a closing row is evidence for the other pairing.
                if body.iter().any(|row| !row.is_ascii() && !row.is_empty()) {
                    *score -= 3;
                }
                if !body.is_empty() && body.iter().all(|row| row.trim().is_empty()) {
                    *score -= 3;
                }
                scan.clipped.push(ClippedMath {
                    row: 0,
                    lines: trimmed[..=end]
                        .iter()
                        .map(|row| (*row).to_owned())
                        .collect(),
                    top: true,
                });
            }
        }
    };
    for (row, line) in trimmed.iter().enumerate() {
        let kind = classify(line);
        if let Row::Fence(marker) = kind {
            if open.take().is_some() {
                score -= 1;
            }
            fence = match fence {
                Some(current) if current == marker => None,
                Some(current) => Some(current),
                None => Some(marker),
            };
            continue;
        }
        if fence.is_some() {
            continue;
        }
        match (kind, open.take()) {
            (Row::Delimiter, None) => open = Some((Some(row), "$$", Vec::new())),
            (Row::BracketOpen, None) => open = Some((Some(row), "\\]", Vec::new())),
            (Row::Delimiter, Some((start, "$$", body)))
            | (Row::BracketClose, Some((start, "\\]", body))) => {
                close_block(&mut scan, &mut score, start, row, &body);
            }
            (Row::Joined(last, reopen), Some((start, "$$", mut body))) => {
                if !last.is_empty() {
                    body.push(last);
                }
                close_block(&mut scan, &mut score, start, row, &body);
                if reopen {
                    // The joined opening delimiter has no row of its own; the
                    // next block begins with its first TeX row.
                    open = Some((Some(row + 1), "$$", Vec::new()));
                }
            }
            (Row::Joined(..), None) => {
                // TeX joined to a closing delimiter cannot follow prose.
                score -= 3;
            }
            (_, Some((start, closing, mut body))) => {
                body.push(line);
                open = Some((start, closing, body));
            }
            (_, None) => {}
        }
    }
    if let Some((Some(start), "$$", body)) = open {
        // An equation running past the bottom edge.
        if body.iter().any(|row| !row.is_ascii()) {
            score -= 1;
        }
        scan.clipped.push(ClippedMath {
            row: start,
            lines: trimmed[start..]
                .iter()
                .map(|row| (*row).to_owned())
                .collect(),
            top: false,
        });
    }
    (score, scan)
}

/// Pair delimiters for a viewport whose first `$$` may close an equation that
/// began above it. Interactive agents such as Codex scroll their transcript on
/// the alternate screen, so the opening row is often absent from the terminal.
pub(crate) fn display_scan(lines: &[String]) -> DisplayScan {
    let trimmed = lines.iter().map(|line| line.trim()).collect::<Vec<_>>();
    let (opened_score, opened) = scan_parity(&trimmed, false);
    let (closed_score, closed) = scan_parity(&trimmed, true);
    let mut scan = if closed_score > opened_score {
        closed
    } else {
        opened
    };
    // Reconstructing TeX from terminal text art is ambiguous: adjacent prose
    // and fractions can produce an image in a different place than the text
    // being hidden. Keep it opt-in until the reconstruction is exact.
    if std::env::var_os("HERDR_MATH_TEXT_ART").as_deref() == Some(std::ffi::OsStr::new("1")) {
        let art = art_blocks(lines, &scan.blocks);
        scan.blocks.extend(art);
    }
    for block in &mut scan.blocks {
        block.col = lines[block.row..block.row + block.rows]
            .iter()
            .filter_map(|line| content_column(line))
            .min()
            .unwrap_or(0);
    }
    scan.blocks.sort_by_key(|block| block.row);
    scan
}

/// Number of rows at the cut edge of a clipped fragment that coincide with a
/// complete block seen earlier: a top fragment's last rows against the block's
/// last rows, a bottom fragment's first rows against its first rows. A whole
/// block never matches; it would not have been clipped.
pub(crate) fn match_clipped(clip: &ClippedMath, known: &[String]) -> Option<usize> {
    let max = clip.lines.len().min(known.len().saturating_sub(1));
    let same = |k: usize| {
        if clip.top {
            clip.lines[clip.lines.len() - k] == known[known.len() - k]
        } else {
            clip.lines[k - 1] == known[k - 1]
        }
    };
    (1..=max).take_while(|&k| same(k)).last()
}

fn unicode_to_tex(text: &str) -> String {
    text.replace("H\u{0302}", "\\hat{H}")
        .replace('Ĥ', "\\hat{H}")
        .replace('ℏ', "\\hbar ")
        .replace('ħ', "\\hbar ")
        .replace('Ψ', "\\Psi ")
        .replace('ψ', "\\psi ")
        .replace('∇', "\\nabla ")
        .replace('π', "\\pi ")
        .replace('∞', "\\infty ")
        .replace('²', "^2")
        .replace('³', "^3")
        .replace('−', "-")
        .replace('×', "\\times ")
        .replace("exp(", "\\exp(")
}

fn near_token(line: &str, from: usize, to: usize) -> Option<String> {
    let chars = line.chars().collect::<Vec<_>>();
    let center = (from + to) / 2;
    let mut best: Option<(usize, String)> = None;
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() || chars[i] == '│' {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '│' {
            i += 1;
        }
        let end = i;
        let distance = if center < start {
            start - center
        } else {
            center.saturating_sub(end)
        };
        if distance <= (to - from).max(3) && best.as_ref().is_none_or(|(d, _)| distance < *d) {
            best = Some((distance, chars[start..end].iter().collect()));
        }
    }
    best.map(|(_, token)| token)
}

fn is_box_top(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('┌') && trimmed.ends_with('┐') && trimmed.contains("──")
}

fn is_box_bottom(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('└') && trimmed.ends_with('┘') && trimmed.contains("──")
}

fn art_blocks(lines: &[String], existing: &[MathBlock]) -> Vec<MathBlock> {
    let mut found = Vec::new();
    let mut fence: Option<&str> = None;
    let mut row = 0;
    while row < lines.len() {
        let line = lines[row].trim();
        let marker = if line.starts_with("```") {
            Some("```")
        } else if line.starts_with("~~~") {
            Some("~~~")
        } else {
            None
        };
        if let Some(marker) = marker {
            fence = match fence {
                Some(current) if current == marker => None,
                Some(current) => Some(current),
                None => Some(marker),
            };
            row += 1;
            continue;
        }
        if fence.is_some()
            || row == 0
            || row + 1 >= lines.len()
            || existing
                .iter()
                .any(|block| row >= block.row && row < block.row + block.rows)
        {
            row += 1;
            continue;
        }
        let baseline = lines[row].chars().collect::<Vec<_>>();
        if !baseline
            .iter()
            .any(|ch| matches!(ch, '=' | '+' | '-' | '−' | '(' | 'ψ' | 'Ψ'))
        {
            row += 1;
            continue;
        }
        let mut bars = Vec::new();
        let mut i = 0;
        while i < baseline.len() {
            if baseline[i] != '─' {
                i += 1;
                continue;
            }
            let from = i;
            while i < baseline.len() && baseline[i] == '─' {
                i += 1;
            }
            if i - from >= 2 {
                bars.push((from, i));
            }
        }
        if bars.is_empty() || bars.len() > 6 {
            row += 1;
            continue;
        }
        let mut latex = String::new();
        let mut previous = 0;
        let mut valid = true;
        for (from, to) in bars {
            let (Some(numerator), Some(denominator)) = (
                near_token(&lines[row - 1], from, to),
                near_token(&lines[row + 1], from, to),
            ) else {
                valid = false;
                break;
            };
            latex.extend(baseline[previous..from].iter());
            latex.push_str(&format!(
                "\\frac{{{}}}{{{}}}",
                unicode_to_tex(&numerator),
                unicode_to_tex(&denominator)
            ));
            previous = to;
        }
        if !valid {
            row += 1;
            continue;
        }
        latex.extend(baseline[previous..].iter());
        let latex = unicode_to_tex(latex.trim().trim_matches('│').trim());
        if latex.is_empty() || latex.len() > 4096 {
            row += 1;
            continue;
        }
        let start = if row >= 2 && is_box_top(&lines[row - 2]) {
            row - 2
        } else {
            row - 1
        };
        let end = if row + 2 < lines.len() && is_box_bottom(&lines[row + 2]) {
            row + 2
        } else {
            row + 1
        };
        found.push(MathBlock {
            row: start,
            rows: end - start + 1,
            col: 0,
            latex,
        });
        row = end + 1;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn complete_display_has_viewport_rows() {
        let found = display_blocks(&rows("prose\n$$\nE = \\frac{mc^2}{2}\n$$\nmore"));
        assert_eq!(
            found,
            vec![MathBlock {
                row: 1,
                rows: 3,
                col: 0,
                latex: "E = \\frac{mc^2}{2}".into(),
            }]
        );
    }

    #[test]
    fn incomplete_and_fenced_math_remains_text() {
        assert!(display_blocks(&rows("$$\nx^2")).is_empty());
        assert!(display_blocks(&rows("```latex\n$$\nx^2\n$$\n```")).is_empty());
    }

    #[test]
    fn delimiters_around_prose_do_not_replace_prose_with_an_image() {
        assert!(display_blocks(&rows("$$\n分数を説明します。\n\\frac{1}{2}\n$$")).is_empty());
        assert!(inline_blocks(&rows("$長い日本語の説明$"), &[]).is_empty());
    }

    #[test]
    fn long_multiline_equation_is_still_one_display_block() {
        let lines = rows(
            "$$\n\\frac{i\\hbar}{T(t)}\n\\frac{dT(t)}{dt}\n=\n\\frac{1}{\\psi(\\mathbf r)}\n\\left[\n-\\frac{\\hbar^2}{2m}\\nabla^2+V(\\mathbf r)\n\\right]\n\\psi(\\mathbf r)\n$$",
        );
        let found = display_blocks(&lines);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].rows, 10);
    }

    #[test]
    fn codex_scrollback_joins_closing_delimiters_to_equation_lines() {
        let lines = rows(
            "$$\n\\ln T(t)\n=\n-\\frac{iEt}{\\hbar}+\\ln C$$\n$$\nT(t)=C e^{-iEt/\\hbar}$$\nprose",
        );
        let found = display_blocks(&lines);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].row, 0);
        assert_eq!(found[0].rows, 4);
        assert_eq!(found[0].latex, "\\ln T(t)\n=\n-\\frac{iEt}{\\hbar}+\\ln C");
        assert_eq!(found[1].row, 4);
        assert_eq!(found[1].latex, "T(t)=C e^{-iEt/\\hbar}");
    }

    #[test]
    fn codex_scrollback_can_join_two_display_delimiters() {
        let lines = rows(
            "$$\n\\boxed{\\hat H\\psi(\\mathbf r)=E\\psi(\\mathbf r)}$$$$\n\\boxed{\\Psi(\\mathbf r,t)=C\\psi(\\mathbf r)e^{-iEt/\\hbar}}\n$$",
        );
        let found = display_blocks(&lines);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].row, 0);
        assert_eq!(found[0].rows, 2);
        assert_eq!(found[1].row, 2);
        assert_eq!(found[1].rows, 2);
        assert!(found[1].latex.starts_with("\\boxed{"));
    }

    #[test]
    fn stacked_fraction_becomes_typeset_math() {
        let found = art_blocks(&rows("           iEt\nT(t)=Cexp(-───)\n            ℏ"), &[]);
        assert_eq!(found.len(), 1);
        assert!(found[0].latex.contains("\\frac{iEt}{\\hbar"));
        assert_eq!(found[0].rows, 3);
    }

    #[test]
    fn multiple_fractions_on_one_baseline_are_one_equation() {
        let found = art_blocks(&rows(" iℏ   dT(t)\n ─── ───── = ET(t)\n T(t)   dt"), &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].latex.matches("\\frac").count(), 2);
    }

    #[test]
    fn boxed_equation_masks_the_box_too() {
        let found = art_blocks(&rows("┌──────────────┐\n│     ℏ²       │\n│  [-──∇²+V]ψ=E│\n│     2m       │\n└──────────────┘"), &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].row, 0);
        assert_eq!(found[0].rows, 5);
        assert!(found[0].latex.contains("\\frac{\\hbar ^2}{2m}"));
    }

    #[test]
    fn line_art_inside_code_fence_remains_literal() {
        assert!(display_blocks(&rows("```text\n 1\n──H=E\n 2\n```")).is_empty());
    }

    #[test]
    fn unmarked_text_art_is_left_untouched_by_default() {
        assert!(display_blocks(&rows(" 1\n──H=E\n 2")).is_empty());
    }

    #[test]
    fn inline_math_keeps_prose_and_uses_terminal_columns() {
        let lines = rows("式は $E=mc^2$ です");
        assert_eq!(
            inline_blocks(&lines, &[]),
            vec![InlineMath {
                row: 0,
                col: 5,
                cols: 8,
                latex: "E=mc^2".into(),
            }]
        );
    }

    #[test]
    fn inline_math_skips_code_and_currency() {
        let lines = rows("`$x$` and $y^2$ and $5 dollars and \\(z+1\\)\n```text\n$a$\n```");
        let found = inline_blocks(&lines, &[]);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].latex, "y^2");
        assert_eq!(found[1].latex, "z+1");
    }

    #[test]
    fn shell_command_containing_display_delimiters_is_not_an_equation() {
        let lines = rows("printf '$$\\n\\\\frac{1}{2}\\n$$\\n'\nmore text");
        let display = display_blocks(&lines);
        assert!(display.is_empty());
        assert!(inline_blocks(&lines, &display).is_empty());
    }
}

#[cfg(test)]
mod scrolled_viewport_tests {
    use super::*;

    fn rows(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    // Transcribed from a Codex pane after scrolling its transcript: the first
    // visible `$$` closes an equation whose opening row is above the pane.
    const CODEX_SCROLLED: &str = "› シュレディンガー方程式を時間に依存しない方程式に式変形して\n  -\\frac{iE}{\\hbar}\\int dt\n  $$\n\n  $$\n  \\ln T(t)\n  =\n  -\\frac{iEt}{\\hbar}+\\ln C\n  $$\n\n  $$\n  T(t)\n  =\n  C e^{-iEt/\\hbar}\n  $$\n\n  $$\n  \\boxed{\n  \\hat H\\psi(\\mathbf r)\n  =\n  E\\psi(\\mathbf r)\n  }\n  $$\n\n  $$\n  \\boxed{\n  \\Psi(\\mathbf r,t)\n  =";

    #[test]
    fn closing_delimiter_at_pane_top_does_not_invert_every_pair() {
        let scan = display_scan(&rows(CODEX_SCROLLED));
        let latex = scan
            .blocks
            .iter()
            .map(|b| b.latex.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            latex,
            [
                "\\ln T(t)\n=\n-\\frac{iEt}{\\hbar}+\\ln C",
                "T(t)\n=\nC e^{-iEt/\\hbar}",
                "\\boxed{\n\\hat H\\psi(\\mathbf r)\n=\nE\\psi(\\mathbf r)\n}",
            ]
        );
        assert_eq!((scan.blocks[0].row, scan.blocks[0].rows), (4, 5));
        assert_eq!(scan.clipped.len(), 2);
        assert!(scan.clipped[0].top);
        assert_eq!(scan.clipped[0].lines.last().unwrap(), "$$");
        assert!(!scan.clipped[1].top);
        assert_eq!(scan.clipped[1].row, 24);
    }

    #[test]
    fn clipped_fragments_resolve_against_complete_blocks() {
        let scan = display_scan(&rows(CODEX_SCROLLED));
        let top = rows("$$\n\\int \\frac{dT}{T}\n=\n-\\frac{iE}{\\hbar}\\int dt\n$$");
        // Header row 0 is not part of the equation; rows 1..=2 are its tail.
        assert_eq!(match_clipped(&scan.clipped[0], &top), Some(2));
        let bottom =
            rows("$$\n\\boxed{\n\\Psi(\\mathbf r,t)\n=\nC\\psi(\\mathbf r)e^{-iEt/\\hbar}\n}\n$$");
        assert_eq!(match_clipped(&scan.clipped[1], &bottom), Some(4));
        // A shared delimiter row is weak evidence; callers weigh it with context.
        let unrelated = rows("$$\nx\n$$");
        assert_eq!(match_clipped(&scan.clipped[0], &unrelated), Some(1));
    }

    #[test]
    fn every_scroll_offset_of_a_transcript_keeps_complete_blocks() {
        let transcript = rows("答えです。\n$$\na=b\n$$\n\n$$\n\\frac{1}{2}\n=\nc\n$$\n\n説明の文章です。\n$$\nE=mc^2\n$$\n");
        for top in 0..transcript.len() {
            for height in 3..=transcript.len() - top {
                let view = &transcript[top..top + height];
                for block in display_scan(view).blocks {
                    let absolute = top + block.row;
                    assert_eq!(transcript[absolute].trim(), "$$", "top={top} h={height}");
                    assert_eq!(transcript[absolute + block.rows - 1].trim(), "$$");
                }
                let complete = [(1, 3), (5, 5), (12, 3)]
                    .iter()
                    .filter(|(row, rows)| *row >= top && row + rows <= top + height)
                    .count();
                assert_eq!(
                    display_scan(view).blocks.len(),
                    complete,
                    "top={top} h={height}"
                );
            }
        }
    }

    #[test]
    fn codex_streaming_snapshot_with_unfilled_row_is_not_an_equation() {
        // Captured from Codex while it typed the last row of the block.
        let lines = rows(
            "  $$\n  i\\hbar\\frac{dT(t)}{dt}=ET(t)\n  \\qquad\\Longrightarrow\\qquad\n\n  $$",
        );
        assert!(display_scan(&lines).blocks.is_empty());
    }

    #[test]
    fn codex_answer_bullet_before_opening_delimiter() {
        let lines = rows(
            "› 質問\n\n• $$\n  i\\hbar\\frac{d}{dt}\\Psi\n  =\n  H\\Psi\n  $$\n\n  $$\n  x=1\n  $$",
        );
        let scan = display_scan(&lines);
        assert_eq!(scan.blocks.len(), 2);
        assert_eq!(
            (scan.blocks[0].row, scan.blocks[0].rows, scan.blocks[0].col),
            (2, 5, 2)
        );
        assert_eq!(scan.blocks[0].latex, "i\\hbar\\frac{d}{dt}\\Psi\n=\nH\\Psi");
        assert!(scan.clipped.is_empty());
        // Prose list items are not delimiters.
        assert!(display_scan(&rows("- $5 and $6")).blocks.is_empty());
    }

    #[test]
    fn prose_between_delimiters_never_becomes_an_equation() {
        let scan = display_scan(&rows("$$\n日本語の説明\n$$\n$$\nx\n$$"));
        assert_eq!(scan.blocks.len(), 1);
        assert_eq!(scan.blocks[0].latex, "x");
    }
}
