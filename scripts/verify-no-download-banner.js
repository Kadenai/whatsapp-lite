import { readFileSync } from 'node:fs';

const source = readFileSync('src-tauri/src/lib.rs', 'utf8');

const required = [
  ['replace hook', /window\.__waLiteReplaceBanner\s*=\s*__waReplaceBanner/],
  ['logo placeholder', /data-wa-lite-logo-placeholder/],
  ['real DOM replacement', /card\.replaceWith\(__waCreateLogo\(card\)\)/],
  ['embedded logo png', /include_bytes!\("\.\.\/\.\.\/WhatsApp Lite Logo\.png"\)/],
  ['logo base64 injection', /__WA_LITE_LOGO_B64__/],
  ['sync mutation observer', /new MutationObserver\(\(\) => \{\s*try \{ __waReplaceBanner\(\); \} catch \(e\) \{\}\s*\}\)\.observe\(/s],
  ['text mutations observed', /characterData:\s*true/],
];

const forbidden = [
  ['old banner CSS marker', /wa-lite-banner/],
  ['old replace hook', /__waLiteForceReplace/],
  ['paint-delayed banner removal', /requestAnimationFrame|setInterval/],
];

const failures = [];

for (const [name, pattern] of required) {
  if (!pattern.test(source)) failures.push(`missing: ${name}`);
}

for (const [name, pattern] of forbidden) {
  if (pattern.test(source)) failures.push(`forbidden: ${name}`);
}

if (failures.length) {
  console.error('Falha: banner de download pode voltar, piscar ou perder a logo.');
  for (const failure of failures) console.error(`- ${failure}`);
  process.exit(1);
}

console.log('OK: banner de download trocado pela logo sem timer/pisca.');
