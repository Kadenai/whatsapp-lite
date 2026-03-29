import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';

const ROOT = process.cwd();
const TARGET_DIRS = ['src', join('src-tauri', 'src')];
const ALLOWED_EXTENSIONS = new Set(['.js', '.ts', '.tsx', '.jsx', '.rs', '.html', '.css']);
const EXCLUDE_DIRS = new Set(['node_modules', 'target', '.git', 'dist', 'build']);

const checks = [
  { key: 'contextmenu', regex: /contextmenu/i },
  { key: 'auxclick', regex: /auxclick/i },
  { key: 'oncontextmenu', regex: /oncontextmenu/i },
  { key: 'mouse_button_2', regex: /button\s*={2,3}\s*2/i },
  { key: 'mouse_button_right', regex: /MouseButton::Right/i },
  { key: 'right_click_text', regex: /right[ -]?click/i },
];

function hasAllowedExtension(filePath) {
  for (const ext of ALLOWED_EXTENSIONS) {
    if (filePath.endsWith(ext)) {
      return true;
    }
  }
  return false;
}

function collectFiles(dirPath, files = []) {
  const entries = readdirSync(dirPath);
  for (const entry of entries) {
    const fullPath = join(dirPath, entry);
    const stats = statSync(fullPath);
    if (stats.isDirectory()) {
      if (!EXCLUDE_DIRS.has(entry)) {
        collectFiles(fullPath, files);
      }
      continue;
    }
    if (hasAllowedExtension(fullPath)) {
      files.push(fullPath);
    }
  }
  return files;
}

function getLineNumber(content, index) {
  return content.slice(0, index).split('\n').length;
}

const violations = [];

for (const targetDir of TARGET_DIRS) {
  const absoluteDir = join(ROOT, targetDir);
  const files = collectFiles(absoluteDir);

  for (const file of files) {
    const content = readFileSync(file, 'utf8');

    for (const check of checks) {
      const match = content.match(check.regex);
      if (!match || match.index === undefined) {
        continue;
      }

      violations.push({
        file: relative(ROOT, file).replaceAll('\\', '/'),
        line: getLineNumber(content, match.index),
        rule: check.key,
        sample: match[0],
      });
    }
  }
}

if (violations.length > 0) {
  console.error('Falha: detectadas ocorrencias de interacoes de clique direito no runtime.');
  for (const v of violations) {
    console.error(`- ${v.file}:${v.line} [${v.rule}] -> ${v.sample}`);
  }
  process.exit(1);
}

console.log('OK: nenhuma interacao de clique direito detectada no runtime (src + src-tauri/src).');
