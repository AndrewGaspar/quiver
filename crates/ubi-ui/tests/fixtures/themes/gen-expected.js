// Regenerates expected-tokens.json: runs the mock-up's deriveTokens (design/mockup/src/theme.js)
// over every *.toml in this directory. Usage: node gen-expected.js > expected-tokens.json
const fs = require('fs');
const path = require('path');
const vm = require('vm');

const here = __dirname;
const src = fs.readFileSync(path.join(here, '../../../../../design/mockup/src/theme.js'), 'utf8');
// Only the pure part of theme.js (everything before the THEME_LIST/DOM code).
const pure = src.slice(0, src.indexOf('const THEME_LIST'));
const ctx = {};
vm.createContext(ctx);
vm.runInContext(pure + '\nthis.parseToml = parseToml; this.deriveTokens = deriveTokens;', ctx);

const out = {};
for (const f of fs.readdirSync(here).filter((f) => f.endsWith('.toml')).sort()) {
  out[f.slice(0, -5)] = ctx.deriveTokens(ctx.parseToml(fs.readFileSync(path.join(here, f), 'utf8')));
}
process.stdout.write(JSON.stringify(out, null, 1) + '\n');
