import test from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';

test('renders a display fraction to bounded PNG pixels', () => {
  const result = spawnSync(process.execPath, ['render.mjs'], {
    input: JSON.stringify({ latex: String.raw`E=\frac{mc^2}{2}`, rows: 3, cols: 80 }),
    cwd: new URL('..', import.meta.url),
    maxBuffer: 16 * 1024 * 1024,
  });
  assert.equal(result.status, 0, result.stderr.toString());
  const width = result.stdout.readUInt32LE(0);
  const height = result.stdout.readUInt32LE(4);
  assert.equal(width, 800);
  assert.equal(height, 66);
  assert(result.stdout.length < 4 * 1024 * 1024);
  assert.equal(result.stdout.subarray(8, 16).toString('hex'), '89504e470d0a1a0a');
});

test('renders an inline equation to one transparent row', () => {
  const result = spawnSync(process.execPath, ['render.mjs'], {
    input: JSON.stringify({ latex: 'E=mc^2', rows: 1, cols: 8, inline: true }),
    cwd: new URL('..', import.meta.url),
    maxBuffer: 4 * 1024 * 1024,
  });
  assert.equal(result.status, 0, result.stderr.toString());
  assert.equal(result.stdout.readUInt32LE(0), 80);
  assert.equal(result.stdout.readUInt32LE(4), 22);
  assert.equal(result.stdout.subarray(8, 16).toString('hex'), '89504e470d0a1a0a');
});

test('renders a long multiline equation at the host cell size', () => {
  const latex = String.raw`\frac{i\hbar}{T(t)}
\frac{dT(t)}{dt}
=
\frac{1}{\psi(\mathbf r)}
\left[
-\frac{\hbar^2}{2m}\nabla^2+V(\mathbf r)
\right]
\psi(\mathbf r)`;
  const result = spawnSync(process.execPath, ['render.mjs'], {
    input: JSON.stringify({ latex, rows: 10, cols: 80, cellWidth: 16, cellHeight: 35 }),
    cwd: new URL('..', import.meta.url),
    maxBuffer: 8 * 1024 * 1024,
  });
  assert.equal(result.status, 0, result.stderr.toString());
  assert.equal(result.stdout.readUInt32LE(0), 1280);
  assert.equal(result.stdout.readUInt32LE(4), 350);
  assert.equal(result.stdout.subarray(8, 16).toString('hex'), '89504e470d0a1a0a');
});
