//! Typeset equations within the original pane.
//!
//! The anchored path writes to the server-owned terminal model, never to the
//! agent's PTY. Historical image placements then follow terminal scrollback.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use ratatui::buffer::Buffer;
use tokio::sync::Notify;

use crate::kitty_graphics::HostCellSize;
use crate::layout::{PaneId, PaneInfo};
use crate::math_parser::{display_blocks, display_scan, inline_blocks, match_clipped};
use crate::render_signal::RenderSignal;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ImageKey {
    latex: String,
    cols: u16,
    rows: u16,
    inline: bool,
    cell_width_px: u32,
    cell_height_px: u32,
}

#[derive(Debug)]
pub(crate) struct MathImage {
    pub width: u32,
    pub height: u32,
    pub png: Arc<[u8]>,
    pub fingerprint: u64,
}

#[derive(Debug)]
enum ImageState {
    Pending,
    Ready(Arc<MathImage>),
    Failed,
}

static IMAGES: OnceLock<Mutex<HashMap<ImageKey, ImageState>>> = OnceLock::new();
static CLIENT_PLACEMENTS: OnceLock<Mutex<HashMap<u64, Vec<MathPlacement>>>> = OnceLock::new();
static MATH_PANES: OnceLock<Mutex<HashSet<PaneId>>> = OnceLock::new();
static NEXT_TERMINAL_IMAGE_ID: AtomicU32 = AtomicU32::new(0x7000_0000);
static READY_GENERATION: AtomicU64 = AtomicU64::new(0);
static LAST_TERMINAL_SCAN: OnceLock<Mutex<HashMap<PaneId, (u64, u64)>>> = OnceLock::new();
static DIRTY_MATH_CANDIDATES: OnceLock<Mutex<HashSet<PaneId>>> = OnceLock::new();
static KNOWN_BLOCKS: OnceLock<Mutex<HashMap<PaneId, Vec<KnownBlock>>>> = OnceLock::new();
const KNOWN_BLOCKS_PER_PANE: usize = 256;

/// A complete display equation seen in a pane, kept so that the part left on
/// screen after the pane's application scrolls it across an edge still maps
/// to the same typeset image.
#[derive(Debug, Clone, PartialEq, Eq)]
struct KnownBlock {
    lines: Vec<String>,
    latex: String,
    col: usize,
    /// Rows seen just above and below the block. When only an edge of an
    /// equation is on screen, these neighbours tell equal edges apart.
    before: Vec<String>,
    after: Vec<String>,
}

const KNOWN_CONTEXT_ROWS: usize = 16;

fn trimmed(rows: &[String]) -> Vec<String> {
    rows.iter().map(|row| row.trim().to_owned()).collect()
}

fn common_prefix(a: &[String], b: &[String]) -> usize {
    a.iter().zip(b).take_while(|(a, b)| a == b).count()
}

fn common_suffix(a: &[String], b: &[String]) -> usize {
    a.iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
}

fn native_math_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("HERDR_NATIVE_MATH").is_some_and(|mode| mode == "1" || mode == "2")
    })
}

pub(crate) fn mark_candidate(pane_id: PaneId, bytes: &[u8]) {
    // Composited mode needs the full renderer for the first equation of a
    // pane too; a text-only retained patch would never request its image.
    if !native_math_enabled() || (!bytes.contains(&b'$') && !bytes.contains(&b'\\')) {
        return;
    }
    let candidates = DIRTY_MATH_CANDIDATES.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut candidates) = candidates.lock() {
        candidates.insert(pane_id);
    }
}

pub(crate) fn take_candidates(panes: &HashSet<PaneId>) -> bool {
    let Some(candidates) = DIRTY_MATH_CANDIDATES.get() else {
        return false;
    };
    let Ok(mut candidates) = candidates.lock() else {
        return false;
    };
    let found = panes.iter().any(|pane| candidates.contains(pane));
    if found {
        candidates.retain(|pane| !panes.contains(pane));
    }
    found
}

fn images() -> &'static Mutex<HashMap<ImageKey, ImageState>> {
    IMAGES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn context_matches_visible(visible_start: usize, lines: &[String], visible: &[String]) -> bool {
    lines
        .get(visible_start..visible_start.saturating_add(visible.len()))
        .is_some_and(|viewport| viewport == visible)
}

pub(crate) fn remember_client(client_id: u64, placements: &[MathPlacement]) {
    if !placements.is_empty() {
        let panes = MATH_PANES.get_or_init(|| Mutex::new(HashSet::new()));
        let mut panes = panes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        panes.extend(placements.iter().map(|placement| placement.pane_id));
    }
    let store = CLIENT_PLACEMENTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut store = store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if store.len() >= 64 && !store.contains_key(&client_id) {
        store.clear();
    }
    store.insert(client_id, placements.to_vec());
}

pub(crate) fn client_placements(client_id: u64) -> Vec<MathPlacement> {
    CLIENT_PLACEMENTS
        .get()
        .and_then(|store| store.lock().ok()?.get(&client_id).cloned())
        .unwrap_or_default()
}

/// Whether equations have been placed in this pane for any client.
pub(crate) fn is_math_pane(pane: PaneId) -> bool {
    MATH_PANES
        .get()
        .is_some_and(|seen| seen.lock().is_ok_and(|seen| seen.contains(&pane)))
}

/// Whether a retained row patch for `pane` leaves every equation untouched for
/// this client: no changed row holds TeX source or lies under a placed image.
pub(crate) fn patch_avoids_math(
    client_id: u64,
    pane: PaneId,
    rows: &[(u16, Vec<crate::protocol::CellData>)],
) -> bool {
    if rows.iter().any(|(_, cells)| {
        cells
            .iter()
            .any(|cell| cell.symbol.contains('$') || cell.symbol.contains('\\'))
    }) {
        return false;
    }
    let Some(store) = CLIENT_PLACEMENTS.get() else {
        return true;
    };
    let Ok(store) = store.lock() else {
        return false;
    };
    let Some(placements) = store.get(&client_id) else {
        return true;
    };
    !placements
        .iter()
        .filter(|placement| placement.pane_id == pane)
        .any(|placement| {
            rows.iter()
                .any(|(row, _)| *row >= placement.row && *row < placement.row + placement.rows)
        })
}

fn rasterize(key: &ImageKey) -> Option<MathImage> {
    let helper = std::env::var_os("HERDR_MATH_HELPER")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/math-helper/render.mjs"
            ))
        });
    let mut child = Command::new("node")
        .arg(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let request = serde_json::json!({
        "latex": key.latex.as_str(),
        "cols": key.cols,
        "rows": key.rows,
        "inline": key.inline,
        "cellWidth": key.cell_width_px,
        "cellHeight": key.cell_height_px,
    });
    child
        .stdin
        .take()?
        .write_all(request.to_string().as_bytes())
        .ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() || output.stdout.len() < 8 {
        return None;
    }
    let width = u32::from_le_bytes(output.stdout[0..4].try_into().ok()?);
    let height = u32::from_le_bytes(output.stdout[4..8].try_into().ok()?);
    if width == 0
        || height == 0
        || width > 8192
        || height > 8192
        || output.stdout.len() > 4 * 1024 * 1024 + 8
        || !output.stdout[8..].starts_with(b"\x89PNG\r\n\x1a\n")
    {
        return None;
    }
    let png: Arc<[u8]> = output.stdout[8..].into();
    let mut hasher = DefaultHasher::new();
    width.hash(&mut hasher);
    height.hash(&mut hasher);
    png.hash(&mut hasher);
    Some(MathImage {
        width,
        height,
        png,
        fingerprint: hasher.finish(),
    })
}

fn get_or_request(
    key: ImageKey,
    render_dirty: Arc<RenderSignal>,
    render_notify: Arc<Notify>,
) -> Option<Arc<MathImage>> {
    let mut cache = images()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match cache.get(&key) {
        Some(ImageState::Ready(image)) => return Some(Arc::clone(image)),
        Some(ImageState::Pending | ImageState::Failed) => return None,
        None => {}
    }
    if cache
        .values()
        .filter(|state| matches!(state, ImageState::Pending))
        .count()
        >= 4
    {
        return None;
    }
    if cache.len() >= 128 {
        cache.retain(|_, state| matches!(*state, ImageState::Pending));
    }
    cache.insert(key.clone(), ImageState::Pending);
    drop(cache);
    std::thread::spawn(move || {
        let image = rasterize(&key);
        let mut cache = images()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.insert(
            key,
            match image {
                Some(image) => ImageState::Ready(Arc::new(image)),
                None => ImageState::Failed,
            },
        );
        drop(cache);
        READY_GENERATION.fetch_add(1, Ordering::Release);
        render_dirty.request_generic();
        render_notify.notify_one();
    });
    None
}

/// Typeset into Ghostty's terminal model, where images are owned by the same
/// rows as the surrounding text. Only touch the live viewport: cursor moves
/// cannot safely address a historical viewport after the user scrolls.
pub(crate) fn prepare_terminal_rows(
    app: &crate::app::App,
    target: Option<crate::ui::TabSurfaceTarget>,
    panes: &[PaneInfo],
    cell_size: HostCellSize,
    render_dirty: Arc<RenderSignal>,
    render_notify: Arc<Notify>,
) {
    if !cell_size.is_known()
        || !crate::kitty_graphics::is_enabled()
        || std::env::var_os("HERDR_NATIVE_MATH").as_deref() != Some(std::ffi::OsStr::new("2"))
    {
        return;
    }
    let Some(target) = target else { return };
    for pane in panes {
        if let Some(candidates) = DIRTY_MATH_CANDIDATES.get() {
            if let Ok(mut candidates) = candidates.lock() {
                candidates.remove(&pane.id);
            }
        }
        let Some(runtime) = app.state.runtime_for_pane_in_workspace(
            &app.terminal_runtimes,
            target.workspace_index,
            pane.id,
        ) else {
            continue;
        };
        if runtime.synchronized_output_active() {
            continue;
        }
        if runtime
            .scroll_metrics()
            .is_none_or(|metrics| metrics.offset_from_bottom != 0)
        {
            continue;
        }
        let revision = runtime.content_seq();
        if !revision.is_multiple_of(2) {
            continue;
        }
        let generation = READY_GENERATION.load(Ordering::Acquire);
        let scans = LAST_TERMINAL_SCAN.get_or_init(|| Mutex::new(HashMap::new()));
        if scans
            .lock()
            .ok()
            .and_then(|scans| scans.get(&pane.id).copied())
            == Some((revision, generation))
        {
            continue;
        }
        let Some((visible_start, lines)) = runtime.viewport_context_rows(0, 0) else {
            continue;
        };
        if visible_start != 0 || lines.len() != usize::from(pane.inner_rect.height) {
            continue;
        }
        if runtime.content_seq() != revision {
            continue;
        }
        if let Ok(mut scans) = scans.lock() {
            if scans.len() > 512 {
                scans.clear();
            }
            scans.insert(pane.id, (revision, generation));
        }
        if !lines.iter().any(|line| line.contains('$')) {
            continue;
        }
        let display = display_blocks(&lines);
        let inline = inline_blocks(&lines, &display);
        let mut output = Vec::new();
        let mut rendered = 0;
        // Agent prompts occupy the last few rows. Never rewrite user input.
        let output_end = lines.len().saturating_sub(4);
        for block in display {
            if block.row + block.rows > output_end || block.rows == 0 || block.rows > 8 {
                continue;
            }
            let key = ImageKey {
                latex: block.latex,
                cols: pane.inner_rect.width,
                rows: block.rows as u16,
                inline: false,
                cell_width_px: cell_size.width_px,
                cell_height_px: cell_size.height_px,
            };
            let Some(image) =
                get_or_request(key, Arc::clone(&render_dirty), Arc::clone(&render_notify))
            else {
                continue;
            };
            let id = NEXT_TERMINAL_IMAGE_ID.fetch_add(1, Ordering::Relaxed);
            let Some(bytes) = terminal_math_bytes(
                &image,
                id,
                block.row as u16,
                0,
                pane.inner_rect.width,
                block.rows as u16,
                true,
            ) else {
                continue;
            };
            output.extend(bytes);
            rendered += 1;
        }
        for span in inline {
            if span.row >= output_end
                || span.cols == 0
                || span.col + span.cols > usize::from(pane.inner_rect.width)
            {
                continue;
            }
            let key = ImageKey {
                latex: span.latex,
                cols: span.cols as u16,
                rows: 1,
                inline: true,
                cell_width_px: cell_size.width_px,
                cell_height_px: cell_size.height_px,
            };
            let Some(image) =
                get_or_request(key, Arc::clone(&render_dirty), Arc::clone(&render_notify))
            else {
                continue;
            };
            let id = NEXT_TERMINAL_IMAGE_ID.fetch_add(1, Ordering::Relaxed);
            let Some(bytes) = terminal_math_bytes(
                &image,
                id,
                span.row as u16,
                span.col as u16,
                span.cols as u16,
                1,
                false,
            ) else {
                continue;
            };
            output.extend(bytes);
            rendered += 1;
        }
        if rendered > 0 && runtime.write_renderer_bytes_if_viewport(&lines, &output) {
            render_dirty.request_generic();
            render_notify.notify_one();
        }
    }
}

fn terminal_math_bytes(
    image: &MathImage,
    id: u32,
    row: u16,
    col: u16,
    cols: u16,
    rows: u16,
    full_rows: bool,
) -> Option<Vec<u8>> {
    if cols == 0 || rows == 0 || image.png.len() > 4 * 1024 * 1024 {
        return None;
    }
    let mut bytes = b"\x1b7".to_vec();
    for offset in 0..rows {
        bytes.extend_from_slice(format!("\x1b[{};{}H", row + offset + 1, col + 1).as_bytes());
        if full_rows {
            bytes.extend_from_slice(b"\x1b[2K");
        } else {
            bytes.extend(std::iter::repeat_n(b' ', usize::from(cols)));
        }
    }
    bytes.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
    let upload = format!(
        "a=t,t=d,f=100,s={},v={},i={id},q=2",
        image.width, image.height
    );
    crate::kitty_graphics::write_kitty_data(&mut bytes, &upload, &image.png).ok()?;
    bytes.extend_from_slice(
        format!("\x1b_Ga=p,i={id},p=1,c={cols},r={rows},z=1,C=1,q=2;\x1b\\").as_bytes(),
    );
    bytes.extend_from_slice(b"\x1b8");
    Some(bytes)
}

#[derive(Debug, Clone)]
pub(crate) struct MathPlacement {
    pub pane_id: PaneId,
    pub row: u16,
    pub rows: u16,
    pub col: u16,
    pub cols: u16,
    pub source_y: u32,
    pub source_height: u32,
    pub image_id: u32,
    pub image: Arc<MathImage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DisplayPlacement {
    /// First row of the whole equation; negative when it began above `lines`.
    row: isize,
    rows: usize,
    /// Rows of `lines` that actually show this equation. A clipped fragment
    /// owns only its matched rows, never the header or composer around it.
    shown: std::ops::Range<usize>,
    col: usize,
    latex: String,
}

/// Complete equations in `lines`, plus fragments clipped by the viewport edge
/// whose full source was seen earlier in the same pane. Applications on the
/// alternate screen (Codex scrolls its transcript there) repaint the pane, so
/// the clipped rows are the only part of the equation the terminal holds.
fn resolve_display(pane_id: PaneId, lines: &[String]) -> Vec<DisplayPlacement> {
    let scan = display_scan(lines);
    let mut placements = scan
        .blocks
        .iter()
        .map(|block| DisplayPlacement {
            row: block.row as isize,
            rows: block.rows,
            shown: block.row..block.row + block.rows,
            col: block.col,
            latex: block.latex.clone(),
        })
        .collect::<Vec<_>>();
    let store = KNOWN_BLOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut store = store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let known = store.entry(pane_id).or_default();
    for block in &scan.blocks {
        let end = block.row + block.rows;
        let mut entry = KnownBlock {
            lines: trimmed(&lines[block.row..end]),
            latex: block.latex.clone(),
            col: block.col,
            before: trimmed(&lines[block.row.saturating_sub(KNOWN_CONTEXT_ROWS)..block.row]),
            after: trimmed(&lines[end..(end + KNOWN_CONTEXT_ROWS).min(lines.len())]),
        };
        if let Some(index) = known.iter().position(|seen| {
            seen.lines == entry.lines && seen.latex == entry.latex && seen.col == entry.col
        }) {
            // Keep the widest neighbourhood seen; an edge of the pane may have
            // cut it in this frame.
            let seen = known.remove(index);
            if seen.before.len() > entry.before.len() {
                entry.before = seen.before;
            }
            if seen.after.len() > entry.after.len() {
                entry.after = seen.after;
            }
        }
        known.push(entry);
    }
    if known.len() > KNOWN_BLOCKS_PER_PANE {
        let excess = known.len() - KNOWN_BLOCKS_PER_PANE;
        known.drain(..excess);
    }
    for clip in &scan.clipped {
        // Rows of the equation on screen plus agreeing neighbour rows. Two
        // different equations scoring the same would make the hidden part a
        // guess, so such a fragment is left as text.
        let mut best: Option<(usize, usize, &KnownBlock)> = None;
        let mut ambiguous = false;
        for seen in known.iter().rev() {
            let Some(shown) = match_clipped(clip, &seen.lines) else {
                continue;
            };
            let context = if clip.top {
                let end = clip.row + clip.lines.len();
                common_prefix(&trimmed(&lines[end..]), &seen.after)
            } else {
                common_suffix(&trimmed(&lines[..clip.row]), &seen.before)
            };
            // Delimiter and blank rows alone identify nothing.
            let evidence = if clip.top {
                &clip.lines[clip.lines.len() - shown..]
            } else {
                &clip.lines[..shown]
            };
            let informative = evidence.iter().any(|row| row != "$$" && !row.is_empty())
                || (context >= 2
                    && (if clip.top {
                        &seen.after[..context]
                    } else {
                        &seen.before[seen.before.len() - context..]
                    })
                    .iter()
                    .any(|row| row != "$$" && !row.is_empty()));
            if !informative {
                continue;
            }
            let score = shown + context;
            match best {
                Some((best_score, _, best_seen)) if score == best_score => {
                    ambiguous |= best_seen.latex != seen.latex;
                }
                Some((best_score, _, _)) if score < best_score => {}
                _ => {
                    best = Some((score, shown, seen));
                    ambiguous = false;
                }
            }
        }
        let Some((_, shown, seen)) = best.filter(|_| !ambiguous) else {
            continue;
        };
        let (first, offset) = if clip.top {
            let first = clip.row + clip.lines.len() - shown;
            (first, seen.lines.len() - shown)
        } else {
            (clip.row, 0)
        };
        placements.push(DisplayPlacement {
            row: first as isize - offset as isize,
            rows: seen.lines.len(),
            shown: first..first + shown,
            col: seen.col,
            latex: seen.latex.clone(),
        });
    }
    placements
}

/// Called after the pure terminal render, before the semantic frame and its
/// graphics scene are committed. Only mask cells when their image is ready.
pub(crate) fn augment(
    app: &crate::app::App,
    target: Option<crate::ui::TabSurfaceTarget>,
    buffer: &mut Buffer,
    panes: &[PaneInfo],
    cell_size: HostCellSize,
    render_dirty: Arc<RenderSignal>,
    render_notify: Arc<Notify>,
) -> Vec<MathPlacement> {
    if !cell_size.is_known()
        || !crate::kitty_graphics::is_enabled()
        || std::env::var_os("HERDR_NATIVE_MATH").as_deref() != Some(std::ffi::OsStr::new("1"))
    {
        return Vec::new();
    }
    let mut placements = Vec::new();
    let mut image_bytes = 0usize;
    for pane in panes {
        let area = pane.inner_rect;
        if area.width == 0 || area.height == 0 {
            continue;
        }
        let has_math_marker = (area.y..area.y + area.height).any(|y| {
            (area.x..area.x + area.width).any(|x| {
                let symbol = buffer[(x, y)].symbol();
                symbol.contains('$')
                    || symbol.contains('─')
                    || symbol.contains('\\')
                    || ((y < area.y.saturating_add(4) || y >= area.bottom().saturating_sub(4))
                        && (symbol.contains('^')
                            || symbol.contains('²')
                            || symbol.contains('ψ')
                            || symbol.contains('ℏ')))
            })
        });
        if !has_math_marker {
            continue;
        }
        let mut visible_lines = Vec::with_capacity(area.height as usize);
        for y in area.y..area.y + area.height {
            let mut line = String::new();
            for x in area.x..area.x + area.width {
                line.push_str(buffer[(x, y)].symbol());
            }
            visible_lines.push(line.trim_end().to_owned());
        }
        let context = target
            .and_then(|target| {
                app.state.runtime_for_pane_in_workspace(
                    &app.terminal_runtimes,
                    target.workspace_index,
                    pane.id,
                )
            })
            .filter(|runtime| !runtime.alternate_screen_active())
            .and_then(|runtime| runtime.viewport_context_rows(16, 16));
        let (visible_start, lines) = if let Some((visible_start, lines)) = context {
            // A pane can display an alternate-screen application while its
            // scrollback API still describes the primary screen. Never place
            // pixels from that history over unrelated visible cells.
            let matches_frame = context_matches_visible(visible_start, &lines, &visible_lines);
            if matches_frame {
                (visible_start, lines)
            } else {
                (0, visible_lines)
            }
        } else {
            (0, visible_lines)
        };
        let visible_end = visible_start.saturating_add(area.height as usize);
        let display = resolve_display(pane.id, &lines);
        // A terminal cell cannot reflow the following prose when a MathJax
        // glyph is wider than its source. Keep inline source readable until
        // row-level layout can reserve the exact pixel width.
        let inline: Vec<crate::math_parser::InlineMath> = Vec::new();
        for block in display {
            let col = u16::try_from(block.col).unwrap_or(u16::MAX);
            if col >= area.width {
                continue;
            }
            let cols = area.width - col;
            // Rows are relative to `lines`; a block clipped at the top starts
            // above it, so its first row may be negative.
            let overlap_start = (block.shown.start as isize).max(visible_start as isize);
            let overlap_end = (block.shown.end as isize).min(visible_end as isize);
            if overlap_start >= overlap_end {
                continue;
            }
            let key = ImageKey {
                latex: block.latex,
                cols,
                rows: block.rows as u16,
                inline: false,
                cell_width_px: cell_size.width_px,
                cell_height_px: cell_size.height_px,
            };
            let Some(image) = get_or_request(
                key.clone(),
                Arc::clone(&render_dirty),
                Arc::clone(&render_notify),
            ) else {
                continue;
            };
            if image_bytes.saturating_add(image.png.len()) > 4 * 1024 * 1024 {
                continue;
            }
            image_bytes += image.png.len();
            let mut hasher = DefaultHasher::new();
            key.hash(&mut hasher);
            let image_id = 0x7000_0000 | ((hasher.finish() as u32) & 0x0fff_ffff);
            let source_y = ((overlap_start - block.row) as u64 * u64::from(image.height)
                / block.rows as u64) as u32;
            let source_end = ((overlap_end - block.row) as u64 * u64::from(image.height)
                / block.rows as u64) as u32;
            let first = (overlap_start - visible_start as isize) as u16;
            let last = (overlap_end - visible_start as isize) as u16;
            for y in area.y + first..area.y + last {
                for x in area.x + col..area.x + area.width {
                    buffer[(x, y)].set_symbol(" ");
                }
            }
            placements.push(MathPlacement {
                pane_id: pane.id,
                row: first,
                rows: last - first,
                col,
                cols,
                source_y,
                source_height: source_end.saturating_sub(source_y),
                image_id,
                image,
            });
        }
        for span in inline {
            if span.row < visible_start
                || span.row >= visible_end
                || span.cols == 0
                || span.col + span.cols > area.width as usize
            {
                continue;
            }
            let key = ImageKey {
                latex: span.latex,
                cols: span.cols as u16,
                rows: 1,
                inline: true,
                cell_width_px: cell_size.width_px,
                cell_height_px: cell_size.height_px,
            };
            let Some(image) = get_or_request(
                key.clone(),
                Arc::clone(&render_dirty),
                Arc::clone(&render_notify),
            ) else {
                continue;
            };
            if image_bytes.saturating_add(image.png.len()) > 4 * 1024 * 1024 {
                continue;
            }
            image_bytes += image.png.len();
            let row = (span.row - visible_start) as u16;
            let col = span.col as u16;
            let cols = span.cols as u16;
            for x in area.x + col..area.x + col + cols {
                buffer[(x, area.y + row)].set_symbol(" ");
            }
            let mut hasher = DefaultHasher::new();
            key.hash(&mut hasher);
            placements.push(MathPlacement {
                pane_id: pane.id,
                row,
                rows: 1,
                col,
                cols,
                source_y: 0,
                source_height: image.height,
                image_id: 0x7000_0000 | ((hasher.finish() as u32) & 0x0fff_ffff),
                image,
            });
        }
    }
    placements
}

#[cfg(test)]
mod tests {
    use super::{
        context_matches_visible, is_math_pane, patch_avoids_math, remember_client,
        terminal_math_bytes, MathImage, MathPlacement,
    };
    use std::sync::Arc;

    #[test]
    fn historical_equation_cannot_replace_unrelated_visible_content() {
        let context = vec!["$$".to_owned(), "\\frac{1}{2}".to_owned(), "$$".to_owned()];
        let visible = vec!["printf '$$\\n\\\\frac{1}{2}\\n$$\\n'".to_owned()];
        assert!(!context_matches_visible(1, &context, &visible));
        assert!(context_matches_visible(1, &context, &context[1..2]));
    }

    #[test]
    fn pane_remains_math_capable_after_equation_scrolls_out() {
        let pane = crate::layout::PaneId::alloc();
        let image = Arc::new(MathImage {
            width: 1,
            height: 1,
            png: Arc::from([]),
            fingerprint: 1,
        });
        let client = 987_654_321;
        remember_client(
            client,
            &[MathPlacement {
                pane_id: pane,
                row: 0,
                rows: 1,
                col: 0,
                cols: 1,
                source_y: 0,
                source_height: 1,
                image_id: 0x7000_0001,
                image,
            }],
        );
        remember_client(client, &[]);
        assert!(is_math_pane(pane));
    }

    #[test]
    fn row_patch_is_retained_only_away_from_equations() {
        let pane = crate::layout::PaneId::alloc();
        let image = Arc::new(MathImage {
            width: 1,
            height: 1,
            png: Arc::from([]),
            fingerprint: 1,
        });
        let client = 987_654_322;
        remember_client(
            client,
            &[MathPlacement {
                pane_id: pane,
                row: 4,
                rows: 3,
                col: 2,
                cols: 10,
                source_y: 0,
                source_height: 1,
                image_id: 0x7000_0002,
                image,
            }],
        );
        let row = |y: u16, text: &str| {
            (
                y,
                text.chars()
                    .map(|c| crate::protocol::CellData {
                        symbol: c.to_string(),
                        fg: 0,
                        bg: 0,
                        modifier: 0,
                        skip: false,
                        hyperlink: None,
                    })
                    .collect::<Vec<_>>(),
            )
        };
        // A spinner row below the equation keeps the cheap path.
        assert!(patch_avoids_math(client, pane, &[row(9, "⠋ Working")]));
        // Rows under the image, or rows carrying TeX, need the full renderer.
        assert!(!patch_avoids_math(client, pane, &[row(5, "  =")]));
        assert!(!patch_avoids_math(client, pane, &[row(1, "  $$")]));
        assert!(!patch_avoids_math(client, pane, &[row(1, "\\frac{1}{2}")]));
        // Another pane's equations are irrelevant.
        let other = crate::layout::PaneId::alloc();
        assert!(patch_avoids_math(client, other, &[row(5, "  =")]));
    }

    #[tokio::test]
    async fn equation_image_follows_ghostty_scrollback_rows() {
        let pane = crate::pane::PaneRuntime::test_with_scrollback_bytes(
            40,
            6,
            100_000,
            b"before\r\n$$\r\n\\frac{1}{2}\r\n$$\r\nafter",
        );
        let png: Arc<[u8]> =
            include_bytes!("../vendor/libghostty-vt/src/terminal/kitty/testdata/dog.png")
                .as_slice()
                .into();
        let image = MathImage {
            width: 500,
            height: 306,
            png,
            fingerprint: 1,
        };
        let bytes = terminal_math_bytes(&image, 0x7000_0001, 1, 0, 40, 3, true).unwrap();
        assert!(pane.write_renderer_bytes(&bytes));
        let placements = pane.kitty_image_placements_with_data_filter(|_| false);
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].render.viewport_row, 1);
        let (_, lines) = pane.viewport_context_rows(0, 0).unwrap();
        assert!(!lines.iter().any(|line| line.contains("\\frac")));

        pane.test_process_pty_bytes(b"\r\nnew\r\nnewer");
        let placements = pane.kitty_image_placements_with_data_filter(|_| false);
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].render.viewport_row, 0);
        pane.scroll_up(1);
        let placements = pane.kitty_image_placements_with_data_filter(|_| false);
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].render.viewport_row, 1);
        let (_, scrolled_lines) = pane.viewport_context_rows(0, 0).unwrap();
        assert!(!pane.write_renderer_bytes_if_viewport(&scrolled_lines, &bytes));
    }

    #[tokio::test]
    async fn outdated_equation_cannot_replace_new_pane_text() {
        let pane = crate::pane::PaneRuntime::test_with_scrollback_bytes(
            40,
            4,
            100_000,
            b"$$\r\n\\frac{1}{2}\r\n$$\r\nafter",
        );
        let (_, expected) = pane.viewport_context_rows(0, 0).unwrap();
        let png: Arc<[u8]> =
            include_bytes!("../vendor/libghostty-vt/src/terminal/kitty/testdata/dog.png")
                .as_slice()
                .into();
        let image = MathImage {
            width: 500,
            height: 306,
            png,
            fingerprint: 1,
        };
        let bytes = terminal_math_bytes(&image, 0x7000_0002, 0, 0, 40, 3, true).unwrap();
        pane.test_process_pty_bytes(b"\x1b[1;1Hnew answer");
        assert!(!pane.write_renderer_bytes_if_viewport(&expected, &bytes));
        assert!(pane
            .kitty_image_placements_with_data_filter(|_| false)
            .is_empty());
    }
}
