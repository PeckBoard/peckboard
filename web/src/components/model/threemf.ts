// 3MF → plain triangle meshes for the preview, including the 3MF Production
// extension Bambu Studio / OrcaSlicer projects use: the root
// `3D/3dmodel.model` only holds <components> whose `p:path` points at
// `3D/Objects/object_N.model` files carrying the meshes. three.js'
// 3MFLoader doesn't follow those (→ "reading 'mesh'" of undefined).
//
// Handles: meshes in any model part, nested components (with transforms),
// several build items, the unit attribute, and Bambu/Orca plates from
// `Metadata/model_settings.config` (object → plate) plus the
// `Metadata/plate_N.png` thumbnails. Pure (no DOM). Copied from
// peck-plugins/print-farm/ui/threemf.ts (tested there); keep the two in step.

export interface MeshPart {
  /// Vertex positions (x, y, z) in millimetres, already transformed.
  positions: Float32Array
  /// Triangle vertex indices.
  indices: Uint32Array
}

export interface BuildObject {
  /// The build item's object id in the root model.
  id: string
  name: string
  /// Plate number (1-based) from model_settings.config, or null.
  plate: number | null
  parts: MeshPart[]
}

export interface Parsed3mf {
  objects: BuildObject[]
  /// Plates that hold at least one object, ascending (empty = no plate info).
  plates: number[]
}

/// A 3MF affine transform as 12 numbers (row-vector form: p' = [x y z 1]·M).
type Mat = number[]
const IDENTITY: Mat = [1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0]

/// Parse a transform attribute; malformed → identity. Pure.
export function parseTransform(s: string | undefined): Mat {
  if (!s) return IDENTITY
  const n = s.trim().split(/\s+/).map(Number)
  return n.length === 12 && n.every((x) => isFinite(x)) ? n : IDENTITY
}

/// a then b (apply `a` first). Pure.
export function compose(a: Mat, b: Mat): Mat {
  const r: Mat = new Array(12)
  for (let row = 0; row < 4; row++) {
    for (let col = 0; col < 3; col++) {
      let v = row === 3 ? b[9 + col] : 0
      for (let k = 0; k < 3; k++) v += a[row * 3 + k] * b[k * 3 + col]
      r[row * 3 + col] = v
    }
  }
  return r
}

const UNIT_MM: Record<string, number> = {
  micron: 0.001,
  millimeter: 1,
  centimeter: 10,
  inch: 25.4,
  foot: 304.8,
  meter: 1000,
}

function attrs(s: string): Record<string, string> {
  const out: Record<string, string> = {}
  const re = /([\w:.-]+)\s*=\s*"([^"]*)"/g
  let m: RegExpExecArray | null
  while ((m = re.exec(s))) out[m[1]] = m[2]
  return out
}

interface RawObject {
  name: string
  mesh: { vertices: Float32Array; triangles: Uint32Array } | null
  components: { objectid: string; path: string | null; transform: Mat }[]
}

interface ModelFile {
  objects: Map<string, RawObject>
  build: { objectid: string; path: string | null; transform: Mat }[]
  scale: number
}

const VERTEX = /<(?:\w+:)?vertex\b[^>]*?\bx="([^"]*)"[^>]*?\by="([^"]*)"[^>]*?\bz="([^"]*)"/g
const TRIANGLE = /<(?:\w+:)?triangle\b[^>]*?\bv1="(\d+)"[^>]*?\bv2="(\d+)"[^>]*?\bv3="(\d+)"/g

function parseMesh(body: string): { vertices: Float32Array; triangles: Uint32Array } {
  const v: number[] = []
  let m: RegExpExecArray | null
  VERTEX.lastIndex = 0
  while ((m = VERTEX.exec(body))) v.push(+m[1], +m[2], +m[3])
  const t: number[] = []
  TRIANGLE.lastIndex = 0
  while ((m = TRIANGLE.exec(body))) t.push(+m[1], +m[2], +m[3])
  return { vertices: new Float32Array(v), triangles: new Uint32Array(t) }
}

/// Parse one model part's XML. Pure.
export function parseModelXml(xml: string): ModelFile {
  const unit = /<model\b[^>]*\bunit="([^"]*)"/.exec(xml)?.[1] ?? 'millimeter'
  const objects = new Map<string, RawObject>()
  const objRe = /<object\b([^>]*?)(\/>|>([\s\S]*?)<\/object>)/g
  let m: RegExpExecArray | null
  while ((m = objRe.exec(xml))) {
    const a = attrs(m[1])
    const body = m[3] ?? ''
    const meshAt = body.search(/<(?:\w+:)?mesh\b/)
    const components: RawObject['components'] = []
    const compRe = /<(?:\w+:)?component\b([^>]*?)\/?>/g
    let c: RegExpExecArray | null
    while ((c = compRe.exec(body))) {
      const ca = attrs(c[1])
      if (!ca.objectid) continue
      components.push({
        objectid: ca.objectid,
        path: ca['p:path'] ?? pathAttr(ca),
        transform: parseTransform(ca.transform),
      })
    }
    objects.set(a.id, {
      name: a.name ?? '',
      mesh: meshAt >= 0 ? parseMesh(body.slice(meshAt)) : null,
      components,
    })
  }
  const build: ModelFile['build'] = []
  const buildBody = /<build\b[^>]*>([\s\S]*?)<\/build>/.exec(xml)?.[1] ?? ''
  const itemRe = /<item\b([^>]*?)\/?>/g
  while ((m = itemRe.exec(buildBody))) {
    const a = attrs(m[1])
    if (a.objectid)
      build.push({
        objectid: a.objectid,
        path: a['p:path'] ?? pathAttr(a),
        transform: parseTransform(a.transform),
      })
  }
  return { objects, build, scale: UNIT_MM[unit] ?? 1 }
}

/// `p:path` under whatever prefix the file bound the Production namespace to.
function pathAttr(a: Record<string, string>): string | null {
  for (const [k, v] of Object.entries(a)) if (/:path$/.test(k)) return v
  return null
}

function normPath(p: string): string {
  return p.replace(/^\/+/, '')
}

/// Object id → plate from Bambu/Orca `Metadata/model_settings.config`. Pure.
export function parsePlates(config: string): Map<string, number> {
  const out = new Map<string, number>()
  const plateRe = /<plate\b[^>]*>([\s\S]*?)<\/plate>/g
  let m: RegExpExecArray | null
  while ((m = plateRe.exec(config))) {
    const body = m[1]
    const id = /<metadata\b[^>]*key="plater_id"[^>]*value="(\d+)"/.exec(body)?.[1]
    if (!id) continue
    const instRe = /<model_instance\b[^>]*>([\s\S]*?)<\/model_instance>/g
    let i: RegExpExecArray | null
    while ((i = instRe.exec(body))) {
      const obj = /<metadata\b[^>]*key="object_id"[^>]*value="([^"]*)"/.exec(i[1])?.[1]
      if (obj && !out.has(obj)) out.set(obj, Number(id))
    }
  }
  return out
}

const MAX_DEPTH = 8

/// Turn unzipped 3MF entries into build objects. Throws when there's no
/// root model or nothing to show.
export function parse3mf(files: Record<string, Uint8Array>): Parsed3mf {
  const dec = new TextDecoder()
  const names = Object.keys(files)
  const rootName =
    relsTarget(files['_rels/.rels'] ? dec.decode(files['_rels/.rels']) : '') ??
    names.find((n) => /^3D\/[^/]+\.model$/i.test(n)) ??
    names.find((n) => /\.model$/i.test(n))
  if (!rootName || !files[rootName]) throw new Error('no 3D model inside this 3MF')
  const models = new Map<string, ModelFile>()
  const model = (path: string): ModelFile | null => {
    const key = normPath(path)
    if (!models.has(key)) {
      const bytes =
        files[key] ?? files[names.find((n) => n.toLowerCase() === key.toLowerCase()) ?? '']
      if (!bytes) return null
      models.set(key, parseModelXml(dec.decode(bytes)))
    }
    return models.get(key)!
  }
  const root = model(rootName)!
  const plates = files['Metadata/model_settings.config']
    ? parsePlates(dec.decode(files['Metadata/model_settings.config']))
    : new Map<string, number>()

  const collect = (file: string, objectid: string, xf: Mat, out: MeshPart[], depth: number) => {
    if (depth > MAX_DEPTH) return
    const mf = model(file)
    const obj = mf?.objects.get(objectid)
    if (!mf || !obj) return
    if (obj.mesh && obj.mesh.triangles.length) out.push(bake(obj.mesh, xf, mf.scale))
    for (const c of obj.components)
      collect(
        c.path ? normPath(c.path) : file,
        c.objectid,
        compose(c.transform, xf),
        out,
        depth + 1,
      )
  }

  const objects: BuildObject[] = []
  const items = root.build.length
    ? root.build
    : [...root.objects.keys()].map((id) => ({ objectid: id, path: null, transform: IDENTITY }))
  for (const it of items) {
    const parts: MeshPart[] = []
    collect(it.path ? normPath(it.path) : normPath(rootName), it.objectid, it.transform, parts, 0)
    if (!parts.length) continue
    const name = root.objects.get(it.objectid)?.name || `Object ${it.objectid}`
    objects.push({ id: it.objectid, name, plate: plates.get(it.objectid) ?? null, parts })
  }
  if (!objects.length) throw new Error('this 3MF has no printable meshes')
  const plateSet = [
    ...new Set(objects.map((o) => o.plate).filter((p): p is number => p !== null)),
  ].sort((a, b) => a - b)
  return { objects, plates: plateSet }
}

function relsTarget(rels: string): string | null {
  const m = /<Relationship\b[^>]*Target="([^"]+\.model)"/i.exec(rels)
  return m ? normPath(m[1]) : null
}

function bake(
  mesh: { vertices: Float32Array; triangles: Uint32Array },
  m: Mat,
  scale: number,
): MeshPart {
  const v = mesh.vertices
  const out = new Float32Array(v.length)
  for (let i = 0; i < v.length; i += 3) {
    const x = v[i]
    const y = v[i + 1]
    const z = v[i + 2]
    out[i] = (x * m[0] + y * m[3] + z * m[6] + m[9]) * scale
    out[i + 1] = (x * m[1] + y * m[4] + z * m[7] + m[10]) * scale
    out[i + 2] = (x * m[2] + y * m[5] + z * m[8] + m[11]) * scale
  }
  return { positions: out, indices: mesh.triangles }
}

/// The plate thumbnail PNG (`Metadata/plate_<n>.png`), if the project has one.
export function plateThumbnail(files: Record<string, Uint8Array>, plate = 1): Uint8Array | null {
  return (
    files[`Metadata/plate_${plate}.png`] ??
    files['Metadata/thumbnail.png'] ??
    files['Thumbnails/thumbnail.png'] ??
    null
  )
}
