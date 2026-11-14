import stringWidth from 'string-width';

const fenceMark = line => /^\s*(?:[•●⏺]\s+)?(`{3,}|~{3,})/.exec(line)?.[1];
const isBoxTop = line => /^\s*┌[─┬]+┐\s*$/.test(line);
const isBoxBottom = line => /^\s*└[─┴]+┘\s*$/.test(line);

export function unicodeToTex(value) {
  return value
    .replace(/H\u0302/g, '\\hat{H}')
    .replace(/([A-Za-z])\u0302/g, (_, letter) => `\\hat{${letter}}`)
    .replace(/ℏ|ħ/g, '\\hbar ')
    .replace(/Ψ/g, '\\Psi ')
    .replace(/ψ/g, '\\psi ')
    .replace(/∇/g, '\\nabla ')
    .replace(/π/g, '\\pi ')
    .replace(/∞/g, '\\infty ')
    .replace(/²/g, '^2')
    .replace(/³/g, '^3')
    .replace(/⁻/g, '^-')
    .replace(/−/g, '-')
    .replace(/×/g, '\\times ')
    .replace(/Cexp/g, 'C\\exp')
    .replace(/(?<!\\)\bexp(?=\s*\()/g, '\\exp')
    .replace(/\s+/g, ' ')
    .trim();
}

function nearToken(line, from, to) {
  const center = (from + to) / 2;
  let choice;
  for (const match of line.matchAll(/[^\s│]+/g)) {
    const start = match.index;
    const end = start + match[0].length;
    const distance = Math.max(0, start - center, center - end);
    if (distance > Math.max(3, to - from)) continue;
    if (!choice || distance < choice.distance) choice = { text: match[0], distance };
  }
  return choice?.text || '';
}

// Recognize a narrow, stacked Unicode fraction in terminal output. This is a
// conservative fallback for agents that print line-art math instead of TeX.
// It intentionally refuses prose separators and tables.
export function findTextArt(text, excludedBlocks = []) {
  const lines = text.split('\n');
  const excluded = new Set(excludedBlocks.flatMap(block =>
    Array.from({ length: block.rows }, (_, i) => block.row + i)));
  const result = [];
  let fence;
  for (let row = 0; row < lines.length; row++) {
    const mark = fenceMark(lines[row]);
    if (mark) {
      if (!fence) fence = mark;
      else if (mark[0] === fence[0] && mark.length >= fence.length) fence = undefined;
      continue;
    }
    if (fence || row === 0 || row + 1 >= lines.length) continue;
    if ([row - 1, row, row + 1].some(index => excluded.has(index))) continue;
    const baseline = lines[row];
    if (baseline.includes('$') || baseline.includes('\\')) continue;
    const bars = [...baseline.matchAll(/─{2,}/g)];
    if (bars.length !== 1 || !/[=+\-()\[\]ψΨ∇ℏH]/u.test(baseline)) continue;
    const bar = bars[0];
    const from = bar.index;
    const to = from + bar[0].length;
    const numerator = nearToken(lines[row - 1], from, to);
    const denominator = nearToken(lines[row + 1], from, to);
    if (!numerator || !denominator) continue;
    if (/[│┌┐└┘]/u.test(numerator + denominator)) continue;
    const fraction = `\\frac{${unicodeToTex(numerator)}}{${unicodeToTex(denominator)}}`;
    const equation = baseline.replace(bar[0], fraction).replace(/^\s*│/, '').replace(/│\s*$/, '').trim();
    const latex = unicodeToTex(equation);
    if (!latex || latex.length > 4096) continue;

    let start = row - 1;
    let end = row + 1;
    if (row >= 2 && isBoxTop(lines[row - 2]) && lines[row - 1].includes('│')) start = row - 2;
    if (row + 2 < lines.length && isBoxBottom(lines[row + 2]) && lines[row + 1].includes('│')) end = row + 2;
    if (row >= 2 && !isBoxTop(lines[row - 2])) {
      const extra = lines[row - 2].trim();
      if (/^(?:ℏ|ħ|ψ|Ψ|[A-Za-z])$/u.test(extra) && nearToken(lines[row - 2], from, to)) start = row - 2;
    }
    result.push({ latex, row: start, col: 0, rows: end - start + 1,
      cols: Math.max(...lines.slice(start, end + 1).map(stringWidth)), art: true });
    row = end;
  }
  return result;
}
