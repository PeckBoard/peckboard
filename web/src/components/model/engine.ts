// The three.js side of `ModelViewer` — a lazy chunk, fetched only when a 3D
// file is shown. Loaders are imported per format on top of that.
//
// ONE WebGLRenderer (one WebGL context) serves every viewer: its canvas
// moves into whichever viewer is live, and snapshots of the others are
// rendered with it offscreen and read back with toDataURL. A stage holds GPU
// memory only while it is live; snapshotting or detaching frees it (the CPU
// copies stay, so re-attaching just re-uploads).

import {
  AmbientLight,
  AnimationMixer,
  Box3,
  BufferAttribute,
  BufferGeometry,
  Clock,
  Color,
  DirectionalLight,
  GridHelper,
  Group,
  HemisphereLight,
  LineBasicMaterial,
  LineSegments,
  Mesh,
  MeshStandardMaterial,
  PerspectiveCamera,
  PMREMGenerator,
  Points,
  PointsMaterial,
  Scene,
  Texture,
  TextureLoader,
  Vector2,
  Vector3,
  WebGLRenderer,
  type AnimationAction,
  type AnimationClip,
  type Material,
  type Object3D,
} from 'three'
import { OrbitControls } from 'three/examples/jsm/controls/OrbitControls.js'
import { isPrintable, sniffModelFormat, SNIFF_BYTES, type ModelFormat } from './formats'
import { parseGcodeAsync, type ParsedGcode } from './gcode'
import { parse3mf } from './threemf'

export interface ModelInfo {
  /** Bounding-box size, X × Y × Z. */
  size: [number, number, number]
  /** Printables are millimetres; game assets are unitless. */
  units: 'mm' | 'units'
  triangles: number
  /** G-code layer count (0 for meshes). */
  layers: number
  /** Animation clip names (glTF / FBX / DAE). */
  clips: string[]
}

interface Palette {
  bg: string
  grid: string
  gridMinor: string
  model: string
  extrude: string
  extrudeTop: string
  travel: string
}

const PALETTES: Record<'light' | 'dark', Palette> = {
  light: {
    bg: '#eef0f4',
    grid: '#9aa1ad',
    gridMinor: '#d3d7de',
    model: '#4f8cd6',
    extrude: '#d9822b',
    extrudeTop: '#2563eb',
    travel: '#8a8f99',
  },
  dark: {
    bg: '#161922',
    grid: '#525868',
    gridMinor: '#2b2f3a',
    model: '#6aa5ef',
    extrude: '#e8913a',
    extrudeTop: '#60a5fa',
    travel: '#6b7280',
  },
}

// ── Shared renderer ───────────────────────────────────────────────────

let renderer: WebGLRenderer | null = null
let envTexture: Texture | null = null
let contextLost = false
let liveStage: ModelStage | null = null
let stageCount = 0
const lostListeners = new Set<(lost: boolean) => void>()

/** Subscribe to WebGL context loss / restore; returns the unsubscribe. */
export function onContextLost(cb: (lost: boolean) => void): () => void {
  lostListeners.add(cb)
  cb(contextLost)
  return () => lostListeners.delete(cb)
}

function sharedRenderer(): WebGLRenderer {
  if (renderer) return renderer
  // three r163+ is WebGL2-only and throws when it can't get a context.
  const r = new WebGLRenderer({ antialias: true, powerPreference: 'high-performance' })
  const gl = r.getContext()
  const dbg = gl.getExtension('WEBGL_debug_renderer_info')
  const name = String((dbg && gl.getParameter(dbg.UNMASKED_RENDERER_WEBGL)) || '')
  // Software WebGL (SwiftShader / llvmpipe) crawls at 2x: render at 1x.
  const software = /swiftshader|llvmpipe|softpipe|software|basic render/i.test(name)
  r.setPixelRatio(software ? 1 : Math.min(2, window.devicePixelRatio || 1))
  r.domElement.className = 'model-viewer-canvas'
  r.domElement.setAttribute('data-testid', 'model-canvas')
  r.domElement.addEventListener('webglcontextlost', (e) => {
    e.preventDefault()
    if (renderer !== r) return
    contextLost = true
    envTexture = null
    lostListeners.forEach((cb) => cb(true))
  })
  r.domElement.addEventListener('webglcontextrestored', () => {
    if (renderer !== r) return
    contextLost = false
    lostListeners.forEach((cb) => cb(false))
    liveStage?.restored()
  })
  renderer = r
  return r
}

/** Image-based lighting so PBR (metal) assets don't render black. Made once
 *  per context. */
function environment(r: WebGLRenderer, room: Scene | null): Texture | null {
  if (envTexture || !room) return envTexture
  const pmrem = new PMREMGenerator(r)
  envTexture = pmrem.fromScene(room, 0.04).texture
  pmrem.dispose()
  return envTexture
}

let roomScene: Scene | null = null
async function loadRoom(): Promise<void> {
  if (roomScene) return
  const { RoomEnvironment } = await import('three/examples/jsm/environments/RoomEnvironment.js')
  roomScene = new RoomEnvironment()
}

// ── Loading ───────────────────────────────────────────────────────────

/** Turn a loader failure into something a user can act on. */
function friendly(e: unknown, format: ModelFormat): Error {
  const msg = e instanceof Error ? e.message : String(e)
  if (/crate files/i.test(msg))
    return new Error(
      'This USDZ uses binary USD, which the browser viewer can’t read — download it to view it on an Apple device.',
    )
  if (/draco|meshopt|KHR_texture_basisu/i.test(msg))
    return new Error(
      'This model uses mesh/texture compression the inline viewer doesn’t support — download it to open it in a 3D app.',
    )
  return new Error(
    `Couldn’t read this ${format.toUpperCase()} file: ${msg.replace(/^THREE\.\w+:\s*/, '')}`,
  )
}

function b64ToBytes(b64: string): Uint8Array {
  const s = atob(b64)
  const out = new Uint8Array(s.length)
  for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i)
  return out
}

/**
 * A `.gltf` whose buffers are embedded `data:` URIs, repacked as one GLB.
 * GLTFLoader fetches buffer URIs with `fetch()`, which the page CSP
 * (`connect-src 'self'`) refuses for `data:`; a GLB's binary chunk needs no
 * fetch. Returns the JSON text unchanged when there are no buffers.
 */
function gltfToGlb(text: string): ArrayBuffer | string {
  const json = JSON.parse(text) as {
    buffers?: { uri?: string; byteLength: number }[]
    bufferViews?: { buffer: number; byteOffset?: number }[]
  }
  const buffers = json.buffers ?? []
  if (!buffers.length) return text
  const parts: Uint8Array[] = []
  const offsets: number[] = []
  let total = 0
  for (const b of buffers) {
    const m = b.uri ? /^data:[^,]*?(;base64)?,(.*)$/s.exec(b.uri) : null
    if (!m)
      throw new Error(
        'it references external files — share it as a single .glb to preview it inline',
      )
    const bytes = m[1] ? b64ToBytes(m[2]) : new TextEncoder().encode(decodeURIComponent(m[2]))
    total = (total + 3) & ~3
    offsets.push(total)
    parts.push(bytes)
    total += bytes.length
  }
  for (const bv of json.bufferViews ?? []) {
    bv.byteOffset = (bv.byteOffset ?? 0) + offsets[bv.buffer]
    bv.buffer = 0
  }
  json.buffers = [{ byteLength: total }]
  const enc = new TextEncoder().encode(JSON.stringify(json))
  const jsonLen = (enc.length + 3) & ~3
  const binLen = (total + 3) & ~3
  const out = new Uint8Array(12 + 8 + jsonLen + 8 + binLen)
  const dv = new DataView(out.buffer)
  dv.setUint32(0, 0x46546c67, true) // "glTF"
  dv.setUint32(4, 2, true)
  dv.setUint32(8, out.length, true)
  dv.setUint32(12, jsonLen, true)
  dv.setUint32(16, 0x4e4f534a, true) // "JSON"
  out.set(enc, 20)
  out.fill(0x20, 20 + enc.length, 20 + jsonLen)
  const bin = 20 + jsonLen
  dv.setUint32(bin, binLen, true)
  dv.setUint32(bin + 4, 0x004e4942, true) // "BIN\0"
  parts.forEach((p, i) => out.set(p, bin + 8 + offsets[i]))
  return out.buffer
}

interface Loaded {
  root: Object3D
  clips: AnimationClip[]
  gcode: ParsedGcode | null
  /** Meshes painted with the theme's model colour (recoloured on theme change). */
  themed: MeshStandardMaterial | null
}

function themedMaterial(color: string, vertexColors = false): MeshStandardMaterial {
  return new MeshStandardMaterial({
    color: new Color(color),
    roughness: 0.55,
    metalness: 0.05,
    vertexColors,
  })
}

function hasRenderable(root: Object3D): boolean {
  let found = false
  root.traverse((o) => {
    if ((o as Mesh).isMesh || (o as LineSegments).isLineSegments || (o as Points).isPoints)
      found = true
  })
  return found
}

async function parseFormat(
  buf: ArrayBuffer,
  format: ModelFormat,
  palette: Palette,
  cancelled: () => boolean,
): Promise<Loaded> {
  const text = () => new TextDecoder().decode(buf)
  switch (format) {
    case 'stl': {
      const { STLLoader } = await import('three/examples/jsm/loaders/STLLoader.js')
      const geo = new STLLoader().parse(buf)
      geo.computeVertexNormals()
      const colored = !!(geo as BufferGeometry & { hasColors?: boolean }).hasColors
      const mat = themedMaterial(colored ? '#ffffff' : palette.model, colored)
      return { root: new Mesh(geo, mat), clips: [], gcode: null, themed: colored ? null : mat }
    }
    case '3mf': {
      // three's loader keeps colours / materials but can't follow the 3MF
      // Production extension (Bambu / Orca projects); print-farm's parser
      // does, without colours.
      try {
        const { ThreeMFLoader } = await import('three/examples/jsm/loaders/3MFLoader.js')
        const group = new ThreeMFLoader().parse(buf)
        if (hasRenderable(group)) return { root: group, clips: [], gcode: null, themed: null }
      } catch {
        /* fall through */
      }
      const { unzipSync } = await import('three/examples/jsm/libs/fflate.module.js')
      const parsed = parse3mf(unzipSync(new Uint8Array(buf)))
      const mat = themedMaterial(palette.model)
      mat.flatShading = true
      const group = new Group()
      for (const o of parsed.objects) {
        for (const p of o.parts) {
          const geo = new BufferGeometry()
          geo.setAttribute('position', new BufferAttribute(p.positions, 3))
          geo.setIndex(new BufferAttribute(p.indices, 1))
          geo.computeVertexNormals()
          group.add(new Mesh(geo, mat))
        }
      }
      return { root: group, clips: [], gcode: null, themed: mat }
    }
    case 'obj': {
      const { OBJLoader } = await import('three/examples/jsm/loaders/OBJLoader.js')
      const group = new OBJLoader().parse(text())
      const mat = themedMaterial(palette.model)
      group.traverse((o) => {
        const m = o as Mesh
        if (m.isMesh) {
          disposeMaterial(m.material)
          m.material = mat
        }
      })
      return { root: group, clips: [], gcode: null, themed: mat }
    }
    case 'ply': {
      const { PLYLoader } = await import('three/examples/jsm/loaders/PLYLoader.js')
      const geo = new PLYLoader().parse(buf)
      const colored = !!geo.getAttribute('color')
      if (!geo.index) {
        // No faces: a point cloud.
        const pm = new PointsMaterial({
          size: 2,
          sizeAttenuation: false,
          vertexColors: colored,
          color: colored ? 0xffffff : palette.model,
        })
        return { root: new Points(geo, pm), clips: [], gcode: null, themed: null }
      }
      if (!geo.getAttribute('normal')) geo.computeVertexNormals()
      const mat = themedMaterial(colored ? '#ffffff' : palette.model, colored)
      return { root: new Mesh(geo, mat), clips: [], gcode: null, themed: colored ? null : mat }
    }
    case 'amf': {
      const { AMFLoader } = await import('three/examples/jsm/loaders/AMFLoader.js')
      return { root: new AMFLoader().parse(buf), clips: [], gcode: null, themed: null }
    }
    case 'gcode': {
      const parsed = await parseGcodeAsync(text(), undefined, cancelled)
      if (!parsed) throw new Error('cancelled')
      if (!parsed.moves) throw new Error('no extrusion moves found')
      return { root: gcodeGroup(parsed, palette), clips: [], gcode: parsed, themed: null }
    }
    case 'glb':
    case 'gltf': {
      const { GLTFLoader } = await import('three/examples/jsm/loaders/GLTFLoader.js')
      const loader = new GLTFLoader()
      // GLTFLoader decodes images with ImageBitmapLoader, i.e. `fetch()` of a
      // blob: URL — refused by the page CSP (`connect-src 'self'`). An <img>
      // may load blob: / data: (`img-src`), so swap in TextureLoader.
      loader.register((parser) => {
        parser.textureLoader = new TextureLoader(parser.options.manager)
        parser.textureLoader.setCrossOrigin(parser.options.crossOrigin)
        return { name: 'peckboard_img_textures' }
      })
      const data = format === 'gltf' ? gltfToGlb(text()) : buf
      const gltf = await loader.parseAsync(data, '')
      return { root: gltf.scene, clips: gltf.animations, gcode: null, themed: null }
    }
    case 'fbx': {
      const { FBXLoader } = await import('three/examples/jsm/loaders/FBXLoader.js')
      const group = new FBXLoader().parse(buf, '')
      return { root: group, clips: group.animations, gcode: null, themed: null }
    }
    case 'dae': {
      const { ColladaLoader } = await import('three/examples/jsm/loaders/ColladaLoader.js')
      const collada = new ColladaLoader().parse(text(), '')
      if (!collada) throw new Error('not a COLLADA document')
      return {
        root: collada.scene,
        clips: collada.scene.animations ?? [],
        gcode: null,
        themed: null,
      }
    }
    case 'usdz': {
      const { USDZLoader } = await import('three/examples/jsm/loaders/USDZLoader.js')
      return { root: new USDZLoader().parse(buf), clips: [], gcode: null, themed: null }
    }
  }
}

function gcodeGroup(parsed: ParsedGcode, palette: Palette): Group {
  // One GPU buffer per kind; "layers 0..n" is a draw range (print-farm's
  // viewer does the same). `below` and `top` share the extrusion buffer.
  const exAttr = new BufferAttribute(parsed.extrude, 3)
  const belowGeo = new BufferGeometry()
  belowGeo.setAttribute('position', exAttr)
  const topGeo = new BufferGeometry()
  topGeo.setAttribute('position', exAttr)
  const travelGeo = new BufferGeometry()
  travelGeo.setAttribute('position', new BufferAttribute(parsed.travel, 3))
  const below = new LineSegments(belowGeo, new LineBasicMaterial({ color: palette.extrude }))
  const top = new LineSegments(topGeo, new LineBasicMaterial({ color: palette.extrudeTop }))
  const travel = new LineSegments(
    travelGeo,
    new LineBasicMaterial({ color: palette.travel, transparent: true, opacity: 0.55 }),
  )
  below.name = 'below'
  top.name = 'top'
  travel.name = 'travel'
  travel.visible = false
  for (const l of [below, top, travel]) l.frustumCulled = false
  const g = new Group()
  g.add(below, top, travel)
  return g
}

/**
 * Parse a 3D file into a stage. Magic bytes win over the extension (a GLB or
 * binary STL saved under the wrong name still loads).
 */
export async function loadModel(
  buf: ArrayBuffer,
  hint: ModelFormat,
  dark: boolean,
  cancelled: () => boolean,
): Promise<ModelStage> {
  const head = new Uint8Array(buf, 0, Math.min(SNIFF_BYTES, buf.byteLength))
  const sniffed = sniffModelFormat(head, buf.byteLength)
  const format = sniffed ?? hint
  const palette = PALETTES[dark ? 'dark' : 'light']
  let loaded: Loaded
  try {
    ;[loaded] = await Promise.all([parseFormat(buf, format, palette, cancelled), loadRoom()])
  } catch (e) {
    throw friendly(e, format)
  }
  if (!loaded.gcode && !hasRenderable(loaded.root)) {
    disposeTree(loaded.root)
    throw new Error('Nothing to display — this file has no meshes.')
  }
  return new ModelStage(loaded, format, dark)
}

// ── Stage ─────────────────────────────────────────────────────────────

function disposeMaterial(mat: Material | Material[]) {
  for (const m of Array.isArray(mat) ? mat : [mat]) {
    for (const v of Object.values(m)) if (v instanceof Texture) v.dispose()
    m.dispose()
  }
}

/** Free GPU resources under `o`. CPU-side data stays, so a later render
 *  re-uploads it. */
function disposeTree(o: Object3D) {
  o.traverse((c) => {
    const m = c as Mesh
    if (m.geometry) m.geometry.dispose()
    if (m.material) disposeMaterial(m.material)
  })
}

function countTriangles(root: Object3D): number {
  let n = 0
  root.traverse((o) => {
    const m = o as Mesh & { count?: number; isInstancedMesh?: boolean }
    if (!m.isMesh || !m.geometry) return
    const g = m.geometry
    const tris = g.index ? g.index.count / 3 : (g.getAttribute('position')?.count ?? 0) / 3
    n += Math.floor(tris) * (m.isInstancedMesh ? (m.count ?? 1) : 1)
  })
  return n
}

export class ModelStage {
  readonly info: ModelInfo
  private scene = new Scene()
  private camera = new PerspectiveCamera(40, 1, 0.01, 1e6)
  private target = new Vector3()
  private controls: OrbitControls | null = null
  private host: HTMLElement | null = null
  private ro: ResizeObserver | null = null
  private raf = 0
  private frames = 0
  private visible = true
  private disposed = false
  /** The still `detach` took last (kept when another stage forced it). */
  lastSnapshot: string | null = null
  private content: Object3D
  private grid: GridHelper | null = null
  private gridSize = 220
  private palette: Palette
  private readonly zUp: boolean
  private readonly loaded: Loaded
  private readonly usesEnv: boolean
  private wire = false
  private mixer: AnimationMixer | null = null
  private action: AnimationAction | null = null
  private playing = false
  private clock = new Clock()
  private layer = 0

  constructor(loaded: Loaded, format: ModelFormat, dark: boolean) {
    stageCount++
    this.loaded = loaded
    this.palette = PALETTES[dark ? 'dark' : 'light']
    this.zUp = isPrintable(format) && format !== 'obj'
    this.usesEnv = !isPrintable(format)
    this.content = loaded.root
    this.camera.up.set(0, this.zUp ? 0 : 1, this.zUp ? 1 : 0)

    // Assets are lit mostly by the room environment (PBR); printables by
    // plain lights, which read better on a flat-coloured mesh.
    this.scene.add(new HemisphereLight(0xffffff, 0x444444, this.usesEnv ? 0.25 : 1.4))
    this.scene.add(new AmbientLight(0xffffff, this.usesEnv ? 0 : 0.35))
    const d = new DirectionalLight(0xffffff, this.usesEnv ? 0.9 : 1.6)
    d.position.copy(this.zUp ? new Vector3(1, -1.4, 2) : new Vector3(1, 2, 1.4))
    this.scene.add(d)
    this.scene.background = new Color(this.palette.bg)
    this.scene.add(this.content)

    let size: Vector3
    const g = loaded.gcode
    if (g?.bounds) {
      // G-code coordinates ARE bed coordinates: leave them where they are.
      const b = g.bounds
      size = new Vector3(b.max[0] - b.min[0], b.max[1] - b.min[1], b.max[2] - b.min[2])
      this.gridSize = Math.max(220, Math.ceil(Math.max(b.max[0], b.max[1]) / 10) * 10)
      this.layer = g.layers.length - 1
      this.applyLayer()
    } else {
      const box = new Box3().setFromObject(this.content)
      size = box.getSize(new Vector3())
      const c = box.getCenter(new Vector3())
      if (this.zUp) {
        this.gridSize = Math.max(220, Math.ceil((Math.max(size.x, size.y) * 1.3) / 10) * 10)
        this.content.position.set(this.gridSize / 2 - c.x, this.gridSize / 2 - c.y, -box.min.z)
      } else {
        this.gridSize = niceGrid(Math.max(size.x, size.z) * 2.2)
        this.content.position.set(-c.x, -box.min.y, -c.z)
      }
      this.content.updateMatrixWorld(true)
    }
    this.buildGrid()

    if (loaded.clips.length) {
      this.mixer = new AnimationMixer(this.content)
      this.action = this.mixer.clipAction(loaded.clips[0])
      this.action.play()
      this.mixer.update(0)
    }
    this.info = {
      size: [size.x, size.y, size.z],
      units: isPrintable(format) ? 'mm' : 'units',
      triangles: countTriangles(this.content),
      layers: g ? g.layers.length : 0,
      clips: loaded.clips.map((c, i) => c.name || `Clip ${i + 1}`),
    }
    this.fit()
  }

  private buildGrid() {
    if (this.grid) {
      this.scene.remove(this.grid)
      this.grid.geometry.dispose()
      disposeMaterial(this.grid.material)
    }
    const s = this.gridSize
    const div = this.zUp ? Math.max(4, Math.round(s / 10)) : 20
    const grid = new GridHelper(
      s,
      div,
      new Color(this.palette.grid),
      new Color(this.palette.gridMinor),
    )
    if (this.zUp) {
      grid.rotation.x = Math.PI / 2 // XZ → XY: the bed
      grid.position.set(s / 2, s / 2, 0)
    }
    this.grid = grid
    this.scene.add(grid)
  }

  /** Reset the camera to frame the model. */
  fit() {
    const b = this.loaded.gcode?.bounds
    const box = b
      ? new Box3(new Vector3(...b.min), new Vector3(...b.max))
      : new Box3().setFromObject(this.content)
    if (box.isEmpty()) box.set(new Vector3(), new Vector3(1, 1, 1))
    const c = box.getCenter(new Vector3())
    const size = box.getSize(new Vector3())
    const r = Math.max(size.x, size.y, size.z, 1e-3) * 0.6
    const dist = r / Math.sin((this.camera.fov * Math.PI) / 360)
    const dir = this.zUp ? new Vector3(0.55, -0.75, 0.6) : new Vector3(0.6, 0.45, 0.85)
    this.camera.position.copy(c).addScaledVector(dir, dist)
    this.camera.near = dist / 100
    this.camera.far = dist * 100
    this.camera.updateProjectionMatrix()
    this.target.copy(c)
    this.camera.lookAt(c)
    if (this.controls) {
      this.controls.target.copy(c)
      this.controls.update()
    }
    this.kick()
  }

  setWireframe(on: boolean) {
    this.wire = on
    this.content.traverse((o) => {
      const m = o as Mesh
      if (!m.isMesh) return
      for (const mat of Array.isArray(m.material) ? m.material : [m.material])
        (mat as MeshStandardMaterial).wireframe = on
    })
    this.kick()
  }

  get wireframe() {
    return this.wire
  }

  /** G-code: show layers 0..n, the top one highlighted. */
  setLayer(n: number) {
    this.layer = n
    this.applyLayer()
    this.kick()
  }

  private applyLayer() {
    const g = this.loaded.gcode
    if (!g) return
    const below = this.content.getObjectByName('below') as LineSegments
    const top = this.content.getObjectByName('top') as LineSegments
    const travel = this.content.getObjectByName('travel') as LineSegments
    const L = g.layers[this.layer]
    if (!L) {
      for (const l of [below, top, travel]) l.geometry.setDrawRange(0, 0)
      return
    }
    below.geometry.setDrawRange(0, L.e0 / 3)
    top.geometry.setDrawRange(L.e0 / 3, (L.e1 - L.e0) / 3)
    travel.geometry.setDrawRange(L.t0 / 3, (L.t1 - L.t0) / 3)
  }

  setClip(i: number) {
    const clip = this.loaded.clips[i]
    if (!this.mixer || !clip) return
    this.action?.stop()
    this.action = this.mixer.clipAction(clip)
    this.action.play()
    this.mixer.update(0)
    this.kick()
  }

  setPlaying(on: boolean) {
    this.playing = on && !!this.mixer
    this.clock.getDelta()
    this.kick()
  }

  setTheme(dark: boolean) {
    this.palette = PALETTES[dark ? 'dark' : 'light']
    this.scene.background = new Color(this.palette.bg)
    this.loaded.themed?.color.set(this.palette.model)
    if (this.loaded.gcode) {
      const set = (name: string, c: string) =>
        (
          (this.content.getObjectByName(name) as LineSegments).material as LineBasicMaterial
        ).color.set(c)
      set('below', this.palette.extrude)
      set('top', this.palette.extrudeTop)
      set('travel', this.palette.travel)
    }
    this.buildGrid()
    this.kick()
  }

  /** Off-screen viewers stop rendering (and animating). */
  setVisible(on: boolean) {
    this.visible = on
    if (on) this.kick()
    else if (this.raf) {
      cancelAnimationFrame(this.raf)
      this.raf = 0
    }
  }

  /** Become the live viewer: take the shared canvas into `host`. */
  attach(host: HTMLElement) {
    if (this.disposed) return
    const r = sharedRenderer()
    if (liveStage && liveStage !== this) liveStage.detach()
    // The module tracks which stage owns the shared canvas.
    // eslint-disable-next-line @typescript-eslint/no-this-alias
    liveStage = this
    this.host = host
    host.appendChild(r.domElement)
    this.prepare(r)
    this.controls = new OrbitControls(this.camera, r.domElement)
    this.controls.enableDamping = true
    // Default 0.05 keeps the camera drifting (and rendering) for seconds
    // after a drag; 0.15 settles in well under one.
    this.controls.dampingFactor = 0.15
    this.controls.target.copy(this.target)
    this.controls.addEventListener('change', () => this.kick())
    this.controls.update()
    this.ro = new ResizeObserver(() => this.resize())
    this.ro.observe(host)
    this.resize()
  }

  /** Give the canvas up; returns a snapshot of the last view. */
  detach(): string | null {
    if (liveStage !== this) return null
    const snap = this.host ? this.capture(this.host.clientWidth, this.host.clientHeight) : null
    if (this.raf) cancelAnimationFrame(this.raf)
    this.raf = 0
    this.ro?.disconnect()
    this.ro = null
    if (this.controls) {
      this.target.copy(this.controls.target)
      this.controls.dispose()
      this.controls = null
    }
    if (renderer && this.host?.contains(renderer.domElement)) renderer.domElement.remove()
    this.host = null
    liveStage = null
    disposeTree(this.scene)
    this.lastSnapshot = snap ?? this.lastSnapshot
    return snap
  }

  /** A still of this stage at w × h CSS px, rendered with the shared
   *  canvas without disturbing the live viewer. */
  snapshot(w: number, h: number): string | null {
    if (this.disposed) return null
    if (liveStage === this) return this.capture(w, h)
    const r = sharedRenderer()
    const prev = r.getSize(new Vector2())
    const url = this.capture(w, h)
    r.setSize(prev.x, prev.y, false)
    liveStage?.renderNow()
    disposeTree(this.scene)
    return url
  }

  private capture(w: number, h: number): string | null {
    if (contextLost || w < 1 || h < 1) return null
    const r = sharedRenderer()
    this.prepare(r)
    r.setSize(w, h, false)
    this.camera.aspect = w / h
    this.camera.updateProjectionMatrix()
    r.render(this.scene, this.camera)
    // Same task as the render: the drawing buffer is still intact.
    return r.domElement.toDataURL('image/webp', 0.85)
  }

  private prepare(r: WebGLRenderer) {
    this.scene.environment = this.usesEnv ? environment(r, roomScene) : null
  }

  /** The context came back: three re-inits itself; re-upload and redraw. */
  restored() {
    if (renderer) this.prepare(renderer)
    this.kick()
  }

  renderNow() {
    if (!renderer || !this.host || contextLost) return
    this.resize()
    renderer.render(this.scene, this.camera)
  }

  private resize() {
    if (!renderer || !this.host) return
    const w = Math.max(1, this.host.clientWidth)
    const h = Math.max(1, this.host.clientHeight)
    renderer.setSize(w, h, false)
    this.camera.aspect = w / h
    this.camera.updateProjectionMatrix()
    this.kick()
  }

  /** Render on demand: a frame or two after a change, continuously only
   *  while damping settles or an animation plays. */
  private kick() {
    this.frames = 2
    if (this.raf || !this.host || !this.visible || this.disposed) return
    const loop = () => {
      this.raf = 0
      if (!renderer || !this.host || contextLost) return
      const dt = this.clock.getDelta()
      if (this.playing) this.mixer?.update(dt)
      const moved = this.controls?.update() ?? false
      renderer.render(this.scene, this.camera)
      if (this.visible && (moved || this.playing || --this.frames > 0))
        this.raf = requestAnimationFrame(loop)
    }
    this.clock.getDelta()
    this.raf = requestAnimationFrame(loop)
  }

  dispose() {
    if (this.disposed) return
    this.detach()
    this.disposed = true
    this.mixer?.stopAllAction()
    this.mixer?.uncacheRoot(this.content)
    disposeTree(this.scene)
    stageCount--
    // Last viewer gone: give the WebGL context back to the browser.
    if (stageCount === 0 && renderer) {
      // Cleared first: the loss this forces must not reach the listeners.
      const r = renderer
      renderer = null
      envTexture = null
      contextLost = false
      r.dispose()
      r.forceContextLoss()
    }
  }
}

function niceGrid(n: number): number {
  if (!(n > 0)) return 10
  const p = Math.pow(10, Math.floor(Math.log10(n)))
  for (const m of [1, 2, 5, 10]) if (m * p >= n) return m * p
  return 10 * p
}
