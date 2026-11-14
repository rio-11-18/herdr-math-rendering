// Render one complete display equation for Herdr's native client frame.
// stdout: little-endian u32 width, u32 height, then PNG bytes.
import { renderFrame } from './src/math.mjs';

const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);
const request = JSON.parse(Buffer.concat(chunks).toString('utf8'));
const { latex, cols, rows, inline = false, cellWidth = 10, cellHeight = 22 } = request;
if (typeof latex !== 'string' || latex.length > 16384 ||
    !Number.isSafeInteger(cols) || cols < 1 || cols > 512 ||
    !Number.isSafeInteger(rows) || rows < (inline ? 1 : 3) || rows > 100 ||
    typeof inline !== 'boolean' || (inline && rows !== 1)) {
  throw new Error('Invalid equation geometry.');
}
let source;
if (inline) {
  source = `$${latex}$`;
} else {
  const latexRows = latex.split('\n').length;
  if (latexRows > rows - 2) throw new Error('Equation has more lines than source rows.');
  source = `$$\n${latex}${'\n'.repeat(rows - latexRows - 1)}$$`;
}
const frame = renderFrame(source, cellWidth, cellHeight, cols);
const tile = frame.placements[0]?.tile;
if (!tile || tile.width > 8192 || tile.height > 8192 || tile.png.length > 4 * 1024 * 1024) {
  throw new Error('Equation could not fit in the available rows.');
}
const header = Buffer.alloc(8);
header.writeUInt32LE(tile.width, 0);
header.writeUInt32LE(tile.height, 4);
process.stdout.write(header);
process.stdout.write(tile.png);
