// G-code → line segments for the layer viewer. Pure (no DOM, no three.js).
// Copied from peck-plugins/print-farm/ui/gcode.ts (tested there in
// test/ui-gcode.test.ts); keep the two in step.
//
// A new layer starts when an EXTRUDING move happens above the current layer's
// Z, so z-hops and travel moves between layers never create phantom layers.
// Supports G0/G1, G2/G3 arcs (I/J, flattened), G90/G91, M82/M83, G92, G20/G21.
//
// Every layer's segments go into ONE flat Float32Array per kind (in layer
// order), so the viewer uploads a single GPU buffer each and shows "layers
// 0..n" with a draw range instead of one draw call per layer.

export interface GcodeLayer {
  z: number
  /// Float offsets [start, end) into ParsedGcode.extrude / .travel.
  e0: number
  e1: number
  t0: number
  t1: number
}

export interface ParsedGcode {
  layers: GcodeLayer[]
  /// Flat [x1,y1,z1, x2,y2,z2, ...] segment pairs, all layers in order.
  extrude: Float32Array
  travel: Float32Array
  bounds: { min: [number, number, number]; max: [number, number, number] } | null
  /// Extruding moves seen (for "nothing to show" detection).
  moves: number
}

export const EMPTY_GCODE: ParsedGcode = {
  layers: [],
  extrude: new Float32Array(0),
  travel: new Float32Array(0),
  bounds: null,
  moves: 0,
}

const EPS = 1e-4

class FloatBuf {
  a = new Float32Array(1 << 15)
  n = 0
  seg(x1: number, y1: number, z1: number, x2: number, y2: number, z2: number) {
    if (this.n + 6 > this.a.length) {
      const b = new Float32Array(this.a.length * 2)
      b.set(this.a)
      this.a = b
    }
    const a = this.a
    const n = this.n
    a[n] = x1
    a[n + 1] = y1
    a[n + 2] = z1
    a[n + 3] = x2
    a[n + 4] = y2
    a[n + 5] = z2
    this.n = n + 6
  }
  done(): Float32Array {
    return this.a.slice(0, this.n)
  }
}

// Parameter slots read per move.
const X = 0,
  Y = 1,
  Z = 2,
  E = 3,
  I = 4,
  J = 5
const SLOT: Record<string, number> = { X, Y, Z, E, I, J }

/// Incremental parser: `feed` consumes whole lines up to a char offset, so the
/// async wrapper can yield to the page between chunks.
class Parser {
  private x = 0
  private y = 0
  private z = 0
  private e = 0
  private absPos = true
  private absE = true
  private scale = 1
  private started = false
  private curZ = 0
  private cur: GcodeLayer | null = null
  private layers: GcodeLayer[] = []
  private ex = new FloatBuf()
  private tr = new FloatBuf()
  private moves = 0
  private min: [number, number, number] = [Infinity, Infinity, Infinity]
  private max: [number, number, number] = [-Infinity, -Infinity, -Infinity]
  private p = new Float64Array(6)
  private has = new Uint8Array(6)
  pos = 0

  private text: string

  constructor(text: string) {
    this.text = text
  }

  private grow(px: number, py: number, pz: number) {
    const min = this.min
    const max = this.max
    if (px < min[0]) min[0] = px
    if (py < min[1]) min[1] = py
    if (pz < min[2]) min[2] = pz
    if (px > max[0]) max[0] = px
    if (py > max[1]) max[1] = py
    if (pz > max[2]) max[2] = pz
  }

  /// Parse lines until at least `until` chars are consumed (or the end).
  feed(until: number) {
    const text = this.text
    const n = text.length
    while (this.pos < n && this.pos < until) {
      let end = text.indexOf('\n', this.pos)
      if (end < 0) end = n
      this.line(this.pos, end)
      this.pos = end + 1
    }
  }

  private line(s: number, end: number) {
    const t = this.text
    // Command word: letter + number, e.g. G1 / M83 / G92.
    let i = s
    while (i < end && (t.charCodeAt(i) === 32 || t.charCodeAt(i) === 9)) i++
    if (i >= end) return
    const letter = t.charCodeAt(i) | 0x20 // lowercase
    if (letter !== 103 /* g */ && letter !== 109 /* m */) return
    let j = i + 1
    while (j < end) {
      const c = t.charCodeAt(j)
      if (c === 32 || c === 9 || c === 59 || c === 13) break
      j++
    }
    const code = Number(t.slice(i + 1, j))
    if (letter === 109) {
      if (code === 82) this.absE = true
      else if (code === 83) this.absE = false
      return
    }
    if (code === 90) this.absPos = true
    else if (code === 91) this.absPos = false
    else if (code === 20) this.scale = 25.4
    else if (code === 21) this.scale = 1
    else if (code === 92 || code === 0 || code === 1 || code === 2 || code === 3) {
      this.params(j, end)
      if (code === 92) this.g92()
      else this.move(code)
    }
  }

  private params(i: number, end: number) {
    const t = this.text
    const p = this.p
    const has = this.has
    has.fill(0)
    while (i < end) {
      const c = t.charCodeAt(i)
      if (c === 59) return // ; comment
      if (c === 32 || c === 9 || c === 13) {
        i++
        continue
      }
      let j = i + 1
      while (j < end) {
        const d = t.charCodeAt(j)
        if (d === 32 || d === 9 || d === 59 || d === 13) break
        j++
      }
      const slot = SLOT[t[i].toUpperCase()]
      if (slot !== undefined && j > i + 1) {
        const v = Number(t.slice(i + 1, j))
        if (isFinite(v)) {
          p[slot] = v
          has[slot] = 1
        }
      }
      i = j
    }
  }

  private g92() {
    const { p, has } = this
    if (has[E]) this.e = p[E]
    if (has[X]) this.x = p[X] * this.scale
    if (has[Y]) this.y = p[Y] * this.scale
    if (has[Z]) this.z = p[Z] * this.scale
  }

  private move(code: number) {
    const { p, has, scale, absPos } = this
    const { x, y, z } = this
    const nx = has[X] ? (absPos ? p[X] * scale : x + p[X] * scale) : x
    const ny = has[Y] ? (absPos ? p[Y] * scale : y + p[Y] * scale) : y
    const nz = has[Z] ? (absPos ? p[Z] * scale : z + p[Z] * scale) : z
    let extruding = false
    if (has[E]) {
      const de = this.absE ? p[E] - this.e : p[E]
      this.e = this.absE ? p[E] : this.e + p[E]
      extruding = de > 0
    }
    if (nx !== x || ny !== y) {
      if (extruding) {
        if (!this.started || nz > this.curZ + EPS) {
          if (!this.started) {
            // Travel before the first extrusion (purge setup) is dropped.
            this.tr.n = 0
            this.started = true
          } else this.close()
          this.curZ = nz
          this.cur = { z: nz, e0: this.ex.n, e1: 0, t0: this.tr.n, t1: 0 }
        }
        this.moves++
      }
      const buf = extruding ? this.ex : this.tr
      if ((code === 2 || code === 3) && (has[I] || has[J])) {
        const cx = x + (has[I] ? p[I] : 0) * scale
        const cy = y + (has[J] ? p[J] : 0) * scale
        const r = Math.hypot(x - cx, y - cy)
        const a0 = Math.atan2(y - cy, x - cx)
        let a1 = Math.atan2(ny - cy, nx - cx)
        if (code === 2 && a1 >= a0) a1 -= Math.PI * 2
        if (code === 3 && a1 <= a0) a1 += Math.PI * 2
        const steps = Math.max(2, Math.min(64, Math.ceil(Math.abs(a1 - a0) / 0.2)))
        let px = x,
          py = y,
          pz = z
        for (let k = 1; k <= steps; k++) {
          const f = k / steps
          const a = a0 + (a1 - a0) * f
          const qx = k === steps ? nx : cx + r * Math.cos(a)
          const qy = k === steps ? ny : cy + r * Math.sin(a)
          const qz = z + (nz - z) * f
          buf.seg(px, py, pz, qx, qy, qz)
          if (extruding) this.grow(qx, qy, qz)
          px = qx
          py = qy
          pz = qz
        }
      } else buf.seg(x, y, z, nx, ny, nz)
      if (extruding) {
        this.grow(x, y, z)
        this.grow(nx, ny, nz)
      }
    }
    this.x = nx
    this.y = ny
    this.z = nz
  }

  private close() {
    if (!this.cur) return
    this.cur.e1 = this.ex.n
    this.cur.t1 = this.tr.n
    this.layers.push(this.cur)
    this.cur = null
  }

  result(): ParsedGcode {
    this.close()
    if (!this.started) return { ...EMPTY_GCODE, layers: [] }
    return {
      layers: this.layers,
      extrude: this.ex.done(),
      travel: this.tr.done(),
      bounds: this.moves ? { min: this.min, max: this.max } : null,
      moves: this.moves,
    }
  }
}

export function parseGcode(text: string): ParsedGcode {
  const p = new Parser(text)
  p.feed(text.length)
  return p.result()
}

/// parseGcode in ~4 MB slices, yielding to the page between them so a large
/// file never freezes it. Returns null once `cancelled()` turns true.
export async function parseGcodeAsync(
  text: string,
  onProgress?: (f: number) => void,
  cancelled?: () => boolean,
): Promise<ParsedGcode | null> {
  const p = new Parser(text)
  const STEP = 4 * 1024 * 1024
  while (p.pos < text.length) {
    p.feed(p.pos + STEP)
    onProgress?.(Math.min(1, p.pos / text.length))
    await new Promise<void>((r) => setTimeout(r, 0))
    if (cancelled?.()) return null
  }
  return p.result()
}

/// Layer `i`'s extrusion / travel segments (tests, debugging).
export function layerExtrude(g: ParsedGcode, i: number): Float32Array {
  return g.extrude.subarray(g.layers[i].e0, g.layers[i].e1)
}
export function layerTravel(g: ParsedGcode, i: number): Float32Array {
  return g.travel.subarray(g.layers[i].t0, g.layers[i].t1)
}
