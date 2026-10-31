// 3D file detection — by extension, then magic bytes for binary STL / GLB
// when the extension is missing or wrong. No three.js here: this module is
// part of the main bundle; the viewer engine is a lazy chunk.

export type ModelFormat =
  | 'stl'
  | '3mf'
  | 'obj'
  | 'ply'
  | 'amf'
  | 'gcode'
  | 'glb'
  | 'gltf'
  | 'fbx'
  | 'dae'
  | 'usdz'

const MODEL_EXTS: Record<string, ModelFormat> = {
  stl: 'stl',
  '3mf': '3mf',
  obj: 'obj',
  ply: 'ply',
  amf: 'amf',
  gcode: 'gcode',
  gco: 'gcode',
  glb: 'glb',
  gltf: 'gltf',
  fbx: 'fbx',
  dae: 'dae',
  usdz: 'usdz',
}

/** CAD / authoring formats a browser can't tessellate: download-only. */
const CAD_EXTS = new Set(['step', 'stp', 'iges', 'igs', 'blend'])

/** Printable meshes / toolpaths: Z-up, millimetres. */
const PRINTABLE = new Set<ModelFormat>(['stl', '3mf', 'obj', 'ply', 'amf', 'gcode'])

/** What the print-farm plugin can take (its slicer's upload filter). */
const PRINT_FARM_EXTS = new Set(['stl', '3mf', 'obj', 'amf', 'gcode', 'gco', 'step', 'stp'])

/** Above this a 3D file is download-only: fetching and parsing it in a tab
 *  costs several times its size in memory. */
export const MAX_MODEL_BYTES = 100 * 1024 * 1024

function extOf(name: string): string {
  return name.match(/\.([A-Za-z0-9]+)$/)?.[1]?.toLowerCase() ?? ''
}

export function modelFormatFromName(name: string): ModelFormat | null {
  return MODEL_EXTS[extOf(name)] ?? null
}

const MODEL_MIMES: Record<string, ModelFormat> = {
  'model/stl': 'stl',
  'model/x.stl-binary': 'stl',
  'model/x.stl-ascii': 'stl',
  'application/sla': 'stl',
  'model/3mf': '3mf',
  'application/vnd.ms-package.3dmanufacturing-3dmodel+xml': '3mf',
  'model/obj': 'obj',
  'model/gltf-binary': 'glb',
  'model/gltf+json': 'gltf',
  'model/vnd.collada+xml': 'dae',
  'model/vnd.usdz+zip': 'usdz',
  'text/x-gcode': 'gcode',
}

export function modelFormatFromMime(mime: string): ModelFormat | null {
  return MODEL_MIMES[mime.split(';')[0].trim().toLowerCase()] ?? null
}
export function isCadFile(name: string): boolean {
  return CAD_EXTS.has(extOf(name))
}

export function isPrintable(format: ModelFormat): boolean {
  return PRINTABLE.has(format)
}

export function printFarmAccepts(name: string): boolean {
  return PRINT_FARM_EXTS.has(extOf(name))
}

/** Bytes `sniffModelFormat` needs from the start of a file. */
export const SNIFF_BYTES = 84

/** Binary STL / GLB by content. Binary STL has no magic: an 80-byte header,
 *  a triangle count, and exactly 50 bytes per triangle — the size check is
 *  what makes it reliable. Zip-based formats (3MF, USDZ) can't be told
 *  apart from any other zip, so they need their extension. */
export function sniffModelFormat(head: Uint8Array, size: number): ModelFormat | null {
  if (
    head.length >= 4 &&
    head[0] === 0x67 &&
    head[1] === 0x6c &&
    head[2] === 0x54 &&
    head[3] === 0x46
  )
    return 'glb' // "glTF"
  if (head.length >= 84 && size >= 84) {
    const tris = new DataView(head.buffer, head.byteOffset, head.byteLength).getUint32(80, true)
    if (tris > 0 && 84 + tris * 50 === size) return 'stl'
  }
  return null
}
