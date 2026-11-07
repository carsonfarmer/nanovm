// Optional parser-backed local-link checker. Never fetches an external URL.
import { readFile, realpath, stat } from 'node:fs/promises';
import path from 'node:path';
import {createRequire} from 'node:module';
import {pathToFileURL} from 'node:url';

try {
  const prefix = process.env.NICE_MARKDOWN_TOOLS;
  if (!prefix) throw Error('NICE_MARKDOWN_TOOLS must name the isolated npm prefix');
  const require = createRequire(path.join(path.resolve(prefix), 'package.json'));
  async function pinned(name, version) {
    const metadata = JSON.parse(await readFile(path.join(prefix, 'node_modules', name, 'package.json')));
    if (metadata.version !== version) throw Error(`expected ${name}@${version}`);
    return import(pathToFileURL(require.resolve(name)).href);
  }
  const {unified} = await pinned('unified', '11.0.5');
  const {default: remarkParse} = await pinned('remark-parse', '11.0.0');
  const {default: Slugger} = await pinned('github-slugger', '2.0.0');
  const {toString} = await pinned('mdast-util-to-string', '4.0.0');
  const [rootArg, ...documents] = process.argv.slice(2);
  if (!rootArg || !documents.length) throw Error('usage: markdown_check.mjs ROOT FILE...');
  const root = await realpath(rootArg);
  const cache = new Map();
  function within(filename) {
    const relative = path.relative(root, filename);
    return relative !== '..' && !relative.startsWith(`..${path.sep}`) && !path.isAbsolute(relative);
  }
  async function parse(filename) {
    if (cache.has(filename)) return cache.get(filename);
    const tree = unified().use(remarkParse).parse(await readFile(filename, 'utf8'));
    const headings = new Set(), definitions = new Map(), links = [];
    const slugger = new Slugger();
    let html = 0;
    function visit(node) {
      if (node.type === 'heading') headings.add(slugger.slug(toString(node, {includeHtml: false})));
      if (node.type === 'definition' && !definitions.has(node.identifier)) definitions.set(node.identifier, node.url);
      if (['link', 'image', 'linkReference', 'imageReference'].includes(node.type)) links.push(node);
      if (node.type === 'html') html++;
      for (const child of node.children ?? []) visit(child);
    }
    visit(tree);
    const result = {headings, definitions, links, html};
    cache.set(filename, result);
    return result;
  }
  let checked = 0, external = 0, unsupported = 0;
  const problems = [];
  for (const document of documents) {
    const filename = await realpath(document);
    if (!within(filename)) throw Error('document outside root');
    const parsed = await parse(filename);
    unsupported += parsed.html;
    for (const link of parsed.links) {
      const url = link.url ?? parsed.definitions.get(link.identifier);
      if (!url) continue;
      if (/^(https?:|mailto:|\/\/)/i.test(url)) { external++; continue; }
      checked++;
      try {
        if (/^[a-z][a-z0-9+.-]*:/i.test(url) || url.startsWith('/')) throw Error('absolute or unsupported scheme');
        const hash = url.indexOf('#');
        const filePart = (hash < 0 ? url : url.slice(0, hash)).split('?')[0];
        const fragment = hash < 0 ? '' : decodeURIComponent(url.slice(hash + 1));
        const target = await realpath(filePart ? path.resolve(path.dirname(filename), decodeURIComponent(filePart)) : filename);
        if (!within(target)) throw Error('target escapes root');
        if (fragment) {
          if (/\.md$/i.test(target) && (await stat(target)).isFile()) {
            if (!(await parse(target)).headings.has(fragment)) throw Error('missing heading anchor');
          } else { unsupported++; }
        }
      } catch (error) {
        problems.push({document: path.relative(root, filename), line: link.position.start.line,
                       reason: error.code ?? error.message});
      }
    }
  }
  console.log(JSON.stringify({checked, external_skipped: external, unsupported_html_or_nonmarkdown_anchors: unsupported,
                             problems, scope: 'CommonMark links and github-slugger headings; no HTML/custom anchors or external fetch'}, null, 2));
  process.exitCode = problems.length ? 1 : 0;
} catch (error) {
  console.error(`checker unavailable: ${error.code ?? error.message}`);
  process.exitCode = 2;
}
