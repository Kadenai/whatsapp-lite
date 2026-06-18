import { createHash } from 'node:crypto';
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { basename, join } from 'node:path';

const dir = process.argv[2] || 'release';
const files = readdirSync(dir)
  .filter((name) => name !== 'SHA256SUMS.txt')
  .sort();

const lines = files.map((name) => {
  const file = join(dir, name);
  const hash = createHash('sha256').update(readFileSync(file)).digest('hex');
  return `${hash}  ${basename(file)}`;
});

writeFileSync(join(dir, 'SHA256SUMS.txt'), `${lines.join('\n')}\n`);
console.log(`OK: ${join(dir, 'SHA256SUMS.txt')}`);
