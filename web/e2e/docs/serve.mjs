// Static server for the built docs site (docs/_site), served from the
// root — matching production, where the site lives at the root of
// https://peckboard.com/ with an empty baseurl. Build the site first:
//   docker run --rm -v "$PWD:/github/workspace" -w /github/workspace \
//     -e INPUT_SOURCE=./docs -e INPUT_DESTINATION=./docs/_site \
//     -e GITHUB_WORKSPACE=/github/workspace \
//     ghcr.io/actions/jekyll-build-pages:v1.0.13
import { createServer } from 'node:http'
import { existsSync, statSync, createReadStream } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const SITE = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../../docs/_site')
const PORT = Number(process.env.DOCS_E2E_PORT ?? '4448')

if (!existsSync(path.join(SITE, 'index.html'))) {
  console.error(`docs site not built: ${SITE}/index.html missing — run the Jekyll build first`)
  process.exit(1)
}

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.css': 'text/css',
  '.js': 'text/javascript',
  '.mjs': 'text/javascript',
  '.json': 'application/json',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.gif': 'image/gif',
  '.webp': 'image/webp',
  '.ico': 'image/x-icon',
  '.woff': 'font/woff',
  '.woff2': 'font/woff2',
  '.txt': 'text/plain; charset=utf-8',
  '.xml': 'application/xml',
}

createServer((req, res) => {
  const url = new URL(req.url, `http://127.0.0.1:${PORT}`)
  const p = decodeURIComponent(url.pathname)
  let file = path.normalize(path.join(SITE, p))
  if (!file.startsWith(SITE)) {
    res.writeHead(403)
    return res.end()
  }
  if (existsSync(file) && statSync(file).isDirectory()) file = path.join(file, 'index.html')
  if (!existsSync(file)) {
    res.writeHead(404, { 'content-type': 'text/plain' })
    return res.end(`not found: ${p}`)
  }
  res.writeHead(200, { 'content-type': MIME[path.extname(file)] ?? 'application/octet-stream' })
  createReadStream(file).pipe(res)
}).listen(PORT, '127.0.0.1', () => {
  console.log(`docs site at http://127.0.0.1:${PORT}/`)
})
