import { test, expect, type APIRequestContext, type Page } from '../harness'
import { mkdtempSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'

/**
 * 3D files an agent shares render an interactive viewer in their card
 * (`SharedFileCard` → `ModelViewer`, three.js in a lazy chunk):
 *
 *  - a binary STL saved as `.bin` (found by magic bytes), an ASCII STL, a
 *    GLB and a two-object 3MF each get a viewer with the right X × Y × Z
 *    dimensions;
 *  - only ONE viewer is live (owns the WebGL canvas); the others show a
 *    snapshot image, and clicking one makes it live instead;
 *  - Fullscreen opens the preview modal with a live viewer while the inline
 *    ones fall back to snapshots, and closing it hands the canvas back;
 *  - a STEP file is download-only ("needs a CAD app").
 */

const E2E_USER = 'e2e-user'
const E2E_PASS = 'e2e-password-1234'

// Headless Chromium only offers WebGL through SwiftShader, which recent
// Chromium no longer falls back to without this flag.
test.use({
  launchOptions: { args: ['--enable-unsafe-swiftshader', '--use-angle=swiftshader'] },
  // Tall enough that every card is on screen: off-screen viewers don't load.
  viewport: { width: 1280, height: 2400 },
})

type V3 = [number, number, number]

/** The 12 triangles of an axis-aligned box from `o` with size `s`. */
function boxTris(s: V3, o: V3 = [0, 0, 0]): [V3, V3, V3][] {
  const c = (i: number): V3 => [
    o[0] + (i & 1 ? s[0] : 0),
    o[1] + (i & 2 ? s[1] : 0),
    o[2] + (i & 4 ? s[2] : 0),
  ]
  const quads = [
    [0, 2, 3, 1],
    [4, 5, 7, 6],
    [0, 1, 5, 4],
    [2, 6, 7, 3],
    [0, 4, 6, 2],
    [1, 3, 7, 5],
  ]
  return quads.flatMap(([a, b, cc, d]) => [
    [c(a), c(b), c(cc)],
    [c(a), c(cc), c(d)],
  ])
}

function binaryStl(tris: [V3, V3, V3][]): Buffer {
  const buf = Buffer.alloc(84 + tris.length * 50)
  buf.writeUInt32LE(tris.length, 80)
  tris.forEach((t, i) => {
    const at = 84 + i * 50 + 12 // skip the (zero) normal
    t.flat().forEach((v, j) => buf.writeFloatLE(v, at + j * 4))
  })
  return buf
}

const TETRA: V3[] = [
  [0, 0, 0],
  [5, 0, 0],
  [0, 4, 0],
  [0, 0, 3],
]
const TETRA_FACES = [
  [0, 2, 1],
  [0, 1, 3],
  [0, 3, 2],
  [1, 2, 3],
]

function asciiStl(): string {
  const facets = TETRA_FACES.map(
    (f) =>
      `  facet normal 0 0 0\n    outer loop\n${f.map((i) => `      vertex ${TETRA[i].join(' ')}\n`).join('')}    endloop\n  endfacet\n`,
  )
  return `solid wedge\n${facets.join('')}endsolid wedge\n`
}

/** A one-mesh GLB: a tetrahedron 2 × 3 × 4 units, 4 triangles. */
function glb(): Buffer {
  const pos = new Float32Array([0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 4])
  const idx = new Uint16Array([0, 2, 1, 0, 1, 3, 0, 3, 2, 1, 2, 3])
  const bin = Buffer.concat([Buffer.from(pos.buffer), Buffer.from(idx.buffer)])
  const json = {
    asset: { version: '2.0' },
    scene: 0,
    scenes: [{ nodes: [0] }],
    nodes: [{ mesh: 0 }],
    meshes: [{ primitives: [{ attributes: { POSITION: 0 }, indices: 1 }] }],
    accessors: [
      {
        bufferView: 0,
        componentType: 5126,
        count: 4,
        type: 'VEC3',
        min: [0, 0, 0],
        max: [2, 3, 4],
      },
      { bufferView: 1, componentType: 5123, count: 12, type: 'SCALAR' },
    ],
    bufferViews: [
      { buffer: 0, byteOffset: 0, byteLength: 48 },
      { buffer: 0, byteOffset: 48, byteLength: 24 },
    ],
    buffers: [{ byteLength: bin.length }],
  }
  let jsonBuf = Buffer.from(JSON.stringify(json))
  jsonBuf = Buffer.concat([jsonBuf, Buffer.alloc((4 - (jsonBuf.length % 4)) % 4, 0x20)])
  const header = Buffer.alloc(12)
  header.writeUInt32LE(0x46546c67, 0)
  header.writeUInt32LE(2, 4)
  header.writeUInt32LE(12 + 8 + jsonBuf.length + 8 + bin.length, 8)
  const chunk = (len: number, type: number) => {
    const b = Buffer.alloc(8)
    b.writeUInt32LE(len, 0)
    b.writeUInt32LE(type, 4)
    return b
  }
  return Buffer.concat([
    header,
    chunk(jsonBuf.length, 0x4e4f534a),
    jsonBuf,
    chunk(bin.length, 0x004e4942),
    bin,
  ])
}

function crc32(data: Buffer): number {
  let crc = 0xffffffff
  for (const byte of data) {
    crc ^= byte
    for (let k = 0; k < 8; k++) crc = crc & 1 ? (crc >>> 1) ^ 0xedb88320 : crc >>> 1
  }
  return (crc ^ 0xffffffff) >>> 0
}

/** A minimal stored (uncompressed) zip. */
function zip(files: [string, string][]): Buffer {
  const out: Buffer[] = []
  const central: Buffer[] = []
  let offset = 0
  for (const [name, content] of files) {
    const data = Buffer.from(content)
    const nameBuf = Buffer.from(name)
    const crc = crc32(data)
    const local = Buffer.alloc(30)
    local.writeUInt32LE(0x04034b50, 0)
    local.writeUInt16LE(20, 4)
    local.writeUInt32LE(crc, 14)
    local.writeUInt32LE(data.length, 18)
    local.writeUInt32LE(data.length, 22)
    local.writeUInt16LE(nameBuf.length, 26)
    out.push(local, nameBuf, data)
    const cd = Buffer.alloc(46)
    cd.writeUInt32LE(0x02014b50, 0)
    cd.writeUInt16LE(20, 4)
    cd.writeUInt16LE(20, 6)
    cd.writeUInt32LE(crc, 16)
    cd.writeUInt32LE(data.length, 20)
    cd.writeUInt32LE(data.length, 24)
    cd.writeUInt16LE(nameBuf.length, 28)
    cd.writeUInt32LE(offset, 42)
    central.push(cd, nameBuf)
    offset += 30 + nameBuf.length + data.length
  }
  const cdBuf = Buffer.concat(central)
  const end = Buffer.alloc(22)
  end.writeUInt32LE(0x06054b50, 0)
  end.writeUInt16LE(files.length, 8)
  end.writeUInt16LE(files.length, 10)
  end.writeUInt32LE(cdBuf.length, 12)
  end.writeUInt32LE(offset, 16)
  return Buffer.concat([...out, cdBuf, end])
}

/** Two coloured 10 mm cubes, the second moved 20 mm along X: 30 × 10 × 10. */
function threeMf(): Buffer {
  const mesh = () => {
    const tris = boxTris([10, 10, 10])
    const verts: V3[] = []
    const key = (v: V3) => v.join(',')
    const index = new Map<string, number>()
    const faces = tris.map((t) =>
      t.map((v) => {
        if (!index.has(key(v))) {
          index.set(key(v), verts.length)
          verts.push(v)
        }
        return index.get(key(v))!
      }),
    )
    return `<mesh><vertices>${verts.map((v) => `<vertex x="${v[0]}" y="${v[1]}" z="${v[2]}"/>`).join('')}</vertices><triangles>${faces.map((f) => `<triangle v1="${f[0]}" v2="${f[1]}" v3="${f[2]}"/>`).join('')}</triangles></mesh>`
  }
  const model = `<?xml version="1.0" encoding="UTF-8"?>
<model unit="millimeter" xml:lang="en-US" xmlns="http://schemas.microsoft.com/3dmanufacturing/core/2015/02">
<resources>
<basematerials id="1"><base name="Red" displaycolor="#D03030FF"/><base name="Blue" displaycolor="#3050D0FF"/></basematerials>
<object id="2" type="model" pid="1" pindex="0">${mesh()}</object>
<object id="3" type="model" pid="1" pindex="1">${mesh()}</object>
</resources>
<build><item objectid="2"/><item objectid="3" transform="1 0 0 0 1 0 0 0 1 20 0 0"/></build>
</model>`
  return zip([
    [
      '[Content_Types].xml',
      '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="model" ContentType="application/vnd.ms-package.3dmanufacturing-3dmodel+xml"/></Types>',
    ],
    [
      '_rels/.rels',
      '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Target="/3D/3dmodel.model" Id="rel0" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/></Relationships>',
    ],
    ['3D/3dmodel.model', model],
  ])
}

const STEP = `ISO-10303-21;
HEADER;
FILE_DESCRIPTION(('e2e'),'2;1');
FILE_NAME('part.step','2026-01-01T00:00:00',(''),(''),'','','');
FILE_SCHEMA(('AUTOMOTIVE_DESIGN'));
ENDSEC;
DATA;
ENDSEC;
END-ISO-10303-21;
`

async function authenticate(request: APIRequestContext) {
  const res = await request.post('/api/auth/login', {
    data: { username: E2E_USER, password: E2E_PASS },
  })
  expect(res.ok(), `login failed: ${await res.text()}`).toBeTruthy()
  const { token } = (await res.json()) as { token: string }
  return { token, authHeader: { Authorization: `Bearer ${token}` } }
}

/** A session in a fresh folder holding the fixtures, with a live
 *  `mock:block` agent so the per-session MCP token stays valid. */
async function seedSession(request: APIRequestContext, authHeader: Record<string, string>) {
  const folderPath = mkdtempSync(path.join(tmpdir(), 'peckboard-e2e-models-'))
  writeFileSync(path.join(folderPath, 'cube.bin'), binaryStl(boxTris([10, 20, 30])))
  writeFileSync(path.join(folderPath, 'wedge.stl'), asciiStl())
  writeFileSync(path.join(folderPath, 'tetra.glb'), glb())
  writeFileSync(path.join(folderPath, 'plate.3mf'), threeMf())
  writeFileSync(path.join(folderPath, 'part.step'), STEP)
  const folderRes = await request.post('/api/folders', {
    headers: authHeader,
    data: { name: 'e2e-shared-models', path: folderPath },
  })
  expect(folderRes.ok(), `create folder failed: ${await folderRes.text()}`).toBeTruthy()
  const folder = (await folderRes.json()) as { id: string }
  const sessionRes = await request.post('/api/sessions', {
    headers: authHeader,
    data: { name: 'shared models', folder_id: folder.id },
  })
  expect(sessionRes.ok(), `create session failed: ${await sessionRes.text()}`).toBeTruthy()
  const session = (await sessionRes.json()) as { id: string }
  const sendRes = await request.post(`/api/sessions/${session.id}/message`, {
    headers: authHeader,
    data: { text: 'go', model: 'mock:block' },
  })
  expect(sendRes.ok(), `send message failed: ${await sendRes.text()}`).toBeTruthy()
  const cfgPath = path.join(process.env.PECKBOARD_E2E_DATA_DIR!, 'worker-mcp', `${session.id}.json`)
  await expect
    .poll(() => {
      try {
        readFileSync(cfgPath, 'utf8')
        return true
      } catch {
        return false
      }
    })
    .toBe(true)
  const cfg = JSON.parse(readFileSync(cfgPath, 'utf8')) as {
    mcpServers: { peckboard: { headers: { Authorization: string } } }
  }
  const mcpToken = cfg.mcpServers.peckboard.headers.Authorization.replace(/^Bearer /, '')
  return { sessionId: session.id, mcpToken }
}

async function share(request: APIRequestContext, mcpToken: string, file: string) {
  const res = await request.post('/mcp', {
    headers: { Authorization: `Bearer ${mcpToken}` },
    data: {
      jsonrpc: '2.0',
      id: 1,
      method: 'tools/call',
      params: { name: 'share_file', arguments: { path: file } },
    },
  })
  expect(res.ok()).toBeTruthy()
  const body = (await res.json()) as { error?: unknown; result?: { isError?: boolean } }
  expect(body.error, `share_file ${file} failed: ${JSON.stringify(body)}`).toBeFalsy()
  expect(body.result?.isError).toBeFalsy()
}

async function loadAppAt(page: Page, token: string, route: string) {
  await page.addInitScript((injectedToken) => {
    localStorage.setItem('peckboard_token', injectedToken)
  }, token)
  await page.goto(route)
}

test('shared 3D files render inline viewers with one live canvas', async ({ request, page }) => {
  const pageErrors: string[] = []
  page.on('pageerror', (e) => pageErrors.push(e.message))
  const { token, authHeader } = await authenticate(request)
  const { sessionId, mcpToken } = await seedSession(request, authHeader)
  for (const f of ['cube.bin', 'wedge.stl', 'tetra.glb', 'plate.3mf', 'part.step']) {
    await share(request, mcpToken, f)
  }

  await loadAppAt(page, token, `/sessions/${sessionId}`)
  expect(
    await page.evaluate(() => !!document.createElement('canvas').getContext('webgl2')),
    'headless Chromium has no WebGL2',
  ).toBe(true)

  const cards = page.getByTestId('shared-file-card')
  await expect(cards).toHaveCount(5, { timeout: 10_000 })
  const card = (name: string) => cards.filter({ hasText: name })
  const viewer = (name: string) => card(name).getByTestId('model-viewer')

  // Dimensions per fixture; every viewer settles as live or snapshot.
  const expected: [string, string, string][] = [
    ['cube.bin', '10 × 20 × 30 mm', '12 triangles'],
    ['wedge.stl', '5 × 4 × 3 mm', '4 triangles'],
    ['tetra.glb', '2 × 3 × 4 units', '4 triangles'],
    ['plate.3mf', '30 × 10 × 10 mm', '24 triangles'],
  ]
  for (const [name, dims, tris] of expected) {
    await expect(card(name)).toHaveAttribute('data-kind', 'model', { timeout: 10_000 })
    await expect(viewer(name)).toHaveAttribute('data-state', /^(live|snapshot)$/, {
      timeout: 20_000,
    })
    await expect(viewer(name).getByTestId('model-dims')).toHaveText(dims)
    await expect(viewer(name).getByTestId('model-tris')).toHaveText(tris)
  }

  // Exactly one live canvas; the others are snapshot images.
  const inlineViewers = cards.getByTestId('model-viewer')
  const live = cards.locator('[data-testid="model-viewer"][data-state="live"]')
  const snaps = cards.locator('[data-testid="model-viewer"][data-state="snapshot"]')
  await expect(inlineViewers).toHaveCount(4)
  await expect(live).toHaveCount(1)
  await expect(snaps).toHaveCount(3)
  await expect(page.getByTestId('model-canvas')).toHaveCount(1)
  const canvas = live.getByTestId('model-canvas')
  await expect(canvas).toBeVisible()
  expect((await canvas.boundingBox())!.height).toBeGreaterThan(100)
  for (const img of await snaps.getByTestId('model-snapshot').all()) {
    await expect(img).toHaveAttribute('src', /^data:image\//)
    await expect
      .poll(() => img.evaluate((el) => (el as HTMLImageElement).naturalWidth))
      .toBeGreaterThan(0)
  }

  // Clicking a snapshot moves the live canvas to that viewer.
  const firstLiveName = await live.evaluate(
    (el) =>
      el.closest('[data-testid="shared-file-card"]')!.querySelector('.shared-file-name')!
        .textContent,
  )
  const other = expected.map((e) => e[0]).find((n) => n !== firstLiveName)!
  await viewer(other).getByTestId('model-snapshot-button').click()
  await expect(viewer(other)).toHaveAttribute('data-state', 'live')
  await expect(viewer(firstLiveName!)).toHaveAttribute('data-state', 'snapshot')
  await expect(live).toHaveCount(1)
  await expect(viewer(other).getByTestId('model-canvas')).toBeVisible()

  // Toolbar: wireframe toggles on the live viewer.
  const wire = viewer(other).getByRole('button', { name: 'Wireframe' })
  await wire.click()
  await expect(wire).toHaveAttribute('aria-pressed', 'true')

  // Fullscreen: the modal's viewer goes live, every inline one is a snapshot.
  await viewer('cube.bin').getByTestId('model-expand').click()
  const modal = page.getByTestId('shared-file-modal')
  await expect(modal.getByTestId('model-viewer')).toHaveAttribute('data-state', 'live', {
    timeout: 20_000,
  })
  await expect(modal.getByTestId('model-dims')).toHaveText('10 × 20 × 30 mm')
  await expect(live).toHaveCount(0)
  await expect(page.getByTestId('model-canvas')).toHaveCount(1)
  await modal.getByRole('button', { name: 'Close preview' }).click()
  await expect(modal).toBeHidden()
  await expect(live).toHaveCount(1)

  // STEP: download-only.
  const step = card('part.step')
  await expect(step).toHaveAttribute('data-kind', 'cad')
  await expect(step).toContainText('needs a CAD app')
  await expect(step.getByTestId('model-viewer')).toHaveCount(0)
  await expect(step.getByTestId('shared-file-download')).toBeVisible()
  await step.getByTestId('shared-file-open').click()
  await expect(modal.getByTestId('shared-file-nopreview')).toContainText('CAD app')
  await page.keyboard.press('Escape')

  expect(pageErrors).toEqual([])
})
