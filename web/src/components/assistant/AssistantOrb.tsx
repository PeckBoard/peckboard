import { useEffect, useRef } from 'react'
import type { AssistantTopic, AudioLevels, OrbState } from '../../voice/assistantContract'

interface AssistantOrbProps {
  state: OrbState
  audioLevels: AudioLevels
  currentTopic: AssistantTopic | null
  expanded: boolean
  absorbing: boolean
}

type GL = WebGLRenderingContext | WebGL2RenderingContext

interface GlResources {
  program: WebGLProgram
  position: WebGLBuffer
  detailTexture: WebGLTexture
  positionLocation: number
  uniforms: {
    resolution: WebGLUniformLocation | null
    time: WebGLUniformLocation | null
    previousState: WebGLUniformLocation | null
    stateMix: WebGLUniformLocation | null
    state: WebGLUniformLocation | null
    input: WebGLUniformLocation | null
    output: WebGLUniformLocation | null
    absorb: WebGLUniformLocation | null
    voicePhase: WebGLUniformLocation | null
    detail: WebGLUniformLocation | null
  }
}

const ABSORB_DURATION_MS = 1100

const STATE_LABEL: Record<OrbState, string> = {
  offline: 'Assistant offline',
  idle: 'Assistant idle',
  listening: 'Assistant listening',
  thinking: 'Assistant thinking',
  speaking: 'Assistant speaking',
}

const STATE_VALUE: Record<OrbState, number> = {
  offline: 0,
  idle: 1,
  listening: 2,
  thinking: 3,
  speaking: 4,
}

const VERTEX_BODY = `
  void main() {
    v_uv = a_position * 0.5 + 0.5;
    gl_Position = vec4(a_position, 0.0, 1.0);
  }
`

/** Sphere radius as a fraction of the canvas edge. The canvas overhangs the
 *  orb element (see `.assistant-orb-canvas`) so the wobble, halo and dust
 *  have room to fade out before the canvas edge. */
const SPHERE_RADIUS = 0.3125

/*
 * A luminous, translucent sphere on a dark glass core:
 *  - a wavy wireframe shell (lat/long grid displaced by noise) with bright
 *    dots on the mesh crossings;
 *  - flowing light ribbons inside (ridged, domain-warped noise);
 *  - a fresnel rim with a two-tone gradient per assistant state;
 *  - particle dust drifting off the rim, and a soft halo.
 * The voice drives it: the silhouette swells and ripples (ripples travel
 * faster the louder it is — `u_voice_phase`), the mesh deforms harder, the
 * ribbons/rim/core brighten and the dust thickens and is pushed outward.
 */
const FRAGMENT_BODY = `
  uniform vec2 u_resolution;
  uniform float u_time;
  uniform float u_state;
  uniform float u_previous_state;
  uniform float u_state_mix;
  uniform float u_input;
  uniform float u_output;
  uniform float u_absorb;
  uniform float u_voice_phase;
  uniform sampler2D u_detail;

  const float R = ${(SPHERE_RADIUS * 2).toFixed(4)};
  const float TAU = 6.2831853;
  const float PI = 3.1415927;

  float stateMatch(float value, float target) {
    return 1.0 - step(0.5, abs(value - target));
  }

  float stateWeight(float target) {
    return mix(stateMatch(u_previous_state, target), stateMatch(u_state, target), u_state_mix);
  }

  float hash12(vec2 p) {
    vec3 p3 = fract(vec3(p.xyx) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
  }

  vec4 renderOrb(vec2 uv) {
    // q: sphere space, |q| = 1 on the resting rim, aspect-corrected.
    vec2 q = (uv - 0.5) * 2.0 / R;
    q.x *= u_resolution.x / max(1.0, u_resolution.y);

    float offline = stateWeight(0.0);
    float idle = stateWeight(1.0);
    float listening = stateWeight(2.0);
    float thinking = stateWeight(3.0);
    float speaking = stateWeight(4.0);

    // Two-tone palette per state (c1 lights the upper left, c2 the lower right).
    vec3 c1 =
      vec3(0.45, 0.55, 0.62) * offline +
      vec3(0.20, 0.70, 1.00) * idle +
      vec3(0.10, 0.95, 0.80) * listening +
      vec3(0.62, 0.38, 1.00) * thinking +
      vec3(0.25, 0.85, 1.00) * speaking;
    vec3 c2 =
      vec3(0.22, 0.26, 0.34) * offline +
      vec3(0.55, 0.30, 1.00) * idle +
      vec3(0.15, 0.55, 1.00) * listening +
      vec3(1.00, 0.30, 0.75) * thinking +
      vec3(0.95, 0.35, 0.95) * speaking;

    float t = u_time;
    float vp = u_voice_phase;
    float voice = clamp(listening * u_input + speaking * u_output, 0.0, 1.0);
    float energy = (0.45 + voice * 1.2 + thinking * 0.3) * mix(1.0, 0.3, offline);

    // Absorb: a gravity-lens pinch toward the core and a shockwave.
    float absorbOn = step(0.0001, u_absorb) * step(u_absorb, 0.9999);
    float r0 = length(q);
    float shockR = u_absorb * 1.6;
    float shock = absorbOn * exp(-abs(r0 - shockR) * 14.0) * (1.0 - u_absorb);
    float lens = absorbOn * sin(u_absorb * PI);
    q *= 1.0 + lens * 0.2 * exp(-r0 * r0 * 1.5);
    q /= 1.0 + lens * 0.05;

    float r = length(q);
    float ang = atan(q.y, q.x);
    float au = ang / TAU;

    // ── Wavy silhouette: slow organic wobble + voice ripples round the rim ──
    // Sums of whole-number harmonics of the angle wrap seamlessly; a texture
    // lookup keyed on atan() would draw a mip seam where it jumps at ±PI.
    float wob =
      0.5 * sin(ang * 3.0 + t * 0.35) + 0.3 * sin(ang * 5.0 - t * 0.27 + 1.3) +
      0.2 * sin(ang * 8.0 + t * 0.6 + 2.1);
    float wob2 = 0.6 * sin(ang * 4.0 + vp * 1.7) + 0.4 * sin(ang * 6.0 - vp * 1.3 + 0.8);
    float ripple = 0.5 * sin(ang * 7.0 + vp * 6.0) + 0.5 * sin(ang * 11.0 - vp * 4.3);
    float edge = 1.0 + wob * 0.035 + wob2 * voice * 0.07 + ripple * voice * 0.03;
    edge *= 1.0 + voice * 0.04;
    float rr = r / edge;
    float aa = 3.0 / (max(1.0, u_resolution.y) * R);
    float inside = 1.0 - smoothstep(1.0 - aa, 1.0 + aa, rr);

    // ── Wireframe shell on a slowly turning, noise-displaced globe ──
    float z = sqrt(max(0.0, 1.0 - min(rr * rr, 1.0)));
    vec3 n = vec3(q / edge, z);
    float lon = atan(n.x, n.z) / TAU + t * 0.025;
    float lat = asin(clamp(n.y, -1.0, 1.0)) / PI + 0.5;
    vec4 dsp = SAMPLE_DETAIL(vec2(lon, lat) * vec2(2.0, 1.0) + vec2(t * 0.012, t * 0.005));
    vec2 g = vec2(lon * 44.0, lat * 26.0) + (dsp.rg - 0.5) * vec2(2.6, 1.8) * (1.0 + voice * 1.6);
    vec2 gd = abs(fract(g) - 0.5);
    float mesh = max(smoothstep(0.43, 0.5, gd.x), smoothstep(0.43, 0.5, gd.y));
    float dots = smoothstep(0.17, 0.0, length(0.5 - gd));
    float band = 0.3 + 0.7 * smoothstep(0.35, 0.8, dsp.b);
    float meshFade = smoothstep(0.0, 0.35, z);

    // ── Flowing light ribbons inside ──
    vec4 w = SAMPLE_DETAIL(q * 0.32 + vec2(t * 0.01, -t * 0.007));
    vec2 warp = (w.rg - 0.5) * (0.8 + thinking * 0.3);
    // Broad, smooth sheets of light (the low-octave channels), not fine
    // squiggles — they read as flowing energy rather than coastlines.
    vec4 a = SAMPLE_DETAIL(q * 0.2 + warp * 0.6 + vec2(-t * 0.02, t * 0.013) + vp * 0.012);
    vec4 b = SAMPLE_DETAIL(q * 0.28 - warp * 0.8 + vec2(t * 0.017, t * 0.02));
    float ribbons =
      pow(clamp(1.0 - abs(a.g - 0.5) * 2.0, 0.0, 1.0), 9.0) * 0.9 +
      pow(clamp(1.0 - abs(b.r - 0.5) * 2.0, 0.0, 1.0), 12.0) * 0.6;

    float side = 0.5 + 0.5 * dot(q / max(r, 0.0001), vec2(-0.7071, 0.7071));
    vec3 col = mix(c2, c1, smoothstep(0.1, 0.9, side + (dsp.b - 0.5) * 0.4));
    vec3 hot = mix(col, vec3(1.0), 0.45);
    float fres = pow(1.0 - z, 2.4);
    float rimLine = exp(-abs(rr - 1.0) * 60.0);
    float core = exp(-rr * rr * 2.6);
    float sweep = thinking * pow(0.5 + 0.5 * sin(ang - t * 2.2), 10.0);

    vec3 e = vec3(0.0);
    e += col * mesh * band * meshFade * (0.24 + energy * 0.3) * (0.6 + fres * 1.2);
    e += hot * dots * band * meshFade * (0.5 + energy * 0.6);
    e += hot * ribbons * (0.28 + energy * 0.7);
    e += col * fres * (0.9 + energy * 0.7 + sweep * 0.8);
    e += hot * rimLine * (0.7 + voice * 0.8);
    e += mix(c1, vec3(1.0), 0.5) * core * (0.08 + energy * 0.25 + voice * 0.35);
    e += hot * shock * 1.3;
    // Dark glass core, so the orb reads on light and dark pages alike.
    vec3 body = vec3(0.012, 0.014, 0.035) + c2 * 0.035 + e;
    body = vec3(1.0) - exp(-body * 1.5);

    // ── Outside: halo, a voice ring, the absorb shockwave and drifting dust ──
    float d = rr - 1.0;
    float od = max(0.0, d) * edge;
    float fade = 1.0 - smoothstep(0.2, 0.38, od);
    float halo = exp(-od * 7.0) * (0.3 + energy * 0.3);
    float ring = exp(-abs(d - 0.06 - voice * 0.08) * 80.0) * (0.1 + voice * 0.6);
    float wave = exp(-abs(r0 - shockR) * 14.0) * (1.0 - u_absorb) * absorbOn;
    float glowA = clamp((halo + ring + wave * 0.8) * fade, 0.0, 1.0) * (1.0 - inside);

    float drift = t * 0.4 + vp * 0.35;
    vec2 pc = vec2(fract(au + 0.5) * 180.0, (d + 0.04) * 36.0 - drift);
    vec2 cell = floor(pc);
    cell.x = mod(cell.x, 180.0);
    vec2 f = fract(pc) - 0.5;
    float h = hash12(cell);
    vec2 off = vec2(hash12(cell + 17.3), hash12(cell + 41.9)) - 0.5;
    float twinkle = 0.55 + 0.45 * sin(t * 2.5 + h * 40.0);
    float dens = step(0.62 - voice * 0.2, h);
    float reach = exp(-max(d + 0.04, 0.0) * (6.0 - voice * 2.5)) * step(-0.04, d);
    float dust = smoothstep(0.2, 0.02, length(f - off * 0.55)) * dens * twinkle * reach;
    float dustA = clamp(dust * (0.65 + energy * 0.4) * fade, 0.0, 1.0);

    vec3 gcol = mix(c2, c1, side);
    float alpha = clamp(inside + glowA + dustA, 0.0, 1.0);
    vec3 rgb = body * inside + gcol * glowA + mix(gcol, vec3(1.0), 0.5) * dustA;
    return vec4(min(rgb, vec3(alpha)), alpha);
  }
`

const VERTEX_WEBGL2 =
  `#version 300 es
  in vec2 a_position;
  out vec2 v_uv;
  ` + VERTEX_BODY

const FRAGMENT_WEBGL2 =
  `#version 300 es
  precision highp float;
  in vec2 v_uv;
  out vec4 out_color;
  #define SAMPLE_DETAIL(uv) texture(u_detail, uv)
  ` +
  FRAGMENT_BODY +
  `
  void main() {
    out_color = renderOrb(v_uv);
  }
`

const VERTEX_WEBGL1 =
  `
  attribute vec2 a_position;
  varying vec2 v_uv;
  ` + VERTEX_BODY

const FRAGMENT_WEBGL1 =
  `
  precision highp float;
  varying vec2 v_uv;
  #define SAMPLE_DETAIL(uv) texture2D(u_detail, uv)
  ` +
  FRAGMENT_BODY +
  `
  void main() {
    gl_FragColor = renderOrb(v_uv);
  }
`

function createShader(gl: GL, type: number, source: string) {
  const shader = gl.createShader(type)
  if (!shader) throw new Error('Unable to create WebGL shader')
  gl.shaderSource(shader, source)
  gl.compileShader(shader)
  if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
    const message = gl.getShaderInfoLog(shader) ?? 'Unknown shader compilation error'
    gl.deleteShader(shader)
    throw new Error(message)
  }
  return shader
}

function createProgram(gl: GL, webgl2: boolean) {
  const vertex = createShader(gl, gl.VERTEX_SHADER, webgl2 ? VERTEX_WEBGL2 : VERTEX_WEBGL1)
  const fragment = createShader(gl, gl.FRAGMENT_SHADER, webgl2 ? FRAGMENT_WEBGL2 : FRAGMENT_WEBGL1)
  const program = gl.createProgram()
  if (!program) throw new Error('Unable to create WebGL program')
  gl.attachShader(program, vertex)
  gl.attachShader(program, fragment)
  gl.linkProgram(program)
  gl.deleteShader(vertex)
  gl.deleteShader(fragment)
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
    const message = gl.getProgramInfoLog(program) ?? 'Unknown program link error'
    gl.deleteProgram(program)
    throw new Error(message)
  }
  return program
}

/** Integer lattice hash → [0, 1). */
function latticeHash(ix: number, iy: number, seed: number) {
  let h = Math.imul(ix, 374761393) + Math.imul(iy, 668265263) + Math.imul(seed, 1442695041)
  h = Math.imul(h ^ (h >>> 13), 1274126177)
  h ^= h >>> 16
  return (h >>> 0) / 4294967296
}

/** Value noise on a lattice that wraps every `period` cells, so the
 *  texture tiles seamlessly under REPEAT. Quintic fade, no grid artefacts. */
function periodicValueNoise(u: number, v: number, period: number, seed: number) {
  const x = u * period
  const y = v * period
  const x0 = Math.floor(x)
  const y0 = Math.floor(y)
  const fx = x - x0
  const fy = y - y0
  const ix0 = ((x0 % period) + period) % period
  const iy0 = ((y0 % period) + period) % period
  const ix1 = (ix0 + 1) % period
  const iy1 = (iy0 + 1) % period
  const sx = fx * fx * fx * (fx * (fx * 6 - 15) + 10)
  const sy = fy * fy * fy * (fy * (fy * 6 - 15) + 10)
  const a = latticeHash(ix0, iy0, seed)
  const b = latticeHash(ix1, iy0, seed)
  const c = latticeHash(ix0, iy1, seed)
  const d = latticeHash(ix1, iy1, seed)
  return a + (b - a) * sx + (c - a) * sy + (a - b - c + d) * sx * sy
}

/** Four independent tileable fBm fields, one per channel: r/g are broad
 *  warp fields, b is the plasma body, a is finer detail for the veins. */
const DETAIL_CHANNELS = [
  { period: 3, octaves: 3, seed: 11 },
  { period: 4, octaves: 3, seed: 29 },
  { period: 4, octaves: 5, seed: 47 },
  { period: 8, octaves: 4, seed: 83 },
]

function createDetailTexture(gl: GL, webgl2: boolean) {
  const size = 256
  const pixels = new Uint8Array(size * size * 4)
  const field = new Float32Array(size * size)

  DETAIL_CHANNELS.forEach(({ period, octaves, seed }, channel) => {
    let min = Infinity
    let max = -Infinity
    for (let y = 0; y < size; y += 1) {
      for (let x = 0; x < size; x += 1) {
        let sum = 0
        let amplitude = 0.5
        let cells = period
        for (let octave = 0; octave < octaves; octave += 1) {
          sum += amplitude * periodicValueNoise(x / size, y / size, cells, seed + octave * 101)
          amplitude *= 0.5
          cells *= 2
        }
        field[y * size + x] = sum
        if (sum < min) min = sum
        if (sum > max) max = sum
      }
    }
    const span = Math.max(1e-6, max - min)
    for (let i = 0; i < field.length; i += 1) {
      pixels[i * 4 + channel] = Math.round(((field[i] - min) / span) * 255)
    }
  })
  const texture = gl.createTexture()
  if (!texture) throw new Error('Unable to create WebGL texture')
  gl.bindTexture(gl.TEXTURE_2D, texture)
  gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, 0)
  const internalFormat = webgl2 ? (gl as WebGL2RenderingContext).RGBA8 : gl.RGBA
  gl.texImage2D(gl.TEXTURE_2D, 0, internalFormat, size, size, 0, gl.RGBA, gl.UNSIGNED_BYTE, pixels)
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.REPEAT)
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.REPEAT)
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR)
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR_MIPMAP_LINEAR)
  gl.hint(gl.GENERATE_MIPMAP_HINT, gl.NICEST)
  gl.generateMipmap(gl.TEXTURE_2D)
  gl.bindTexture(gl.TEXTURE_2D, null)
  return texture
}

function createResources(gl: GL, webgl2: boolean): GlResources {
  const program = createProgram(gl, webgl2)
  const position = gl.createBuffer()
  if (!position) {
    gl.deleteProgram(program)
    throw new Error('Unable to create WebGL buffer')
  }
  gl.bindBuffer(gl.ARRAY_BUFFER, position)
  gl.bufferData(
    gl.ARRAY_BUFFER,
    new Float32Array([-1, -1, 1, -1, -1, 1, -1, 1, 1, -1, 1, 1]),
    gl.STATIC_DRAW,
  )
  const detailTexture = createDetailTexture(gl, webgl2)
  return {
    program,
    position,
    detailTexture,
    positionLocation: gl.getAttribLocation(program, 'a_position'),
    uniforms: {
      resolution: gl.getUniformLocation(program, 'u_resolution'),
      time: gl.getUniformLocation(program, 'u_time'),
      state: gl.getUniformLocation(program, 'u_state'),
      previousState: gl.getUniformLocation(program, 'u_previous_state'),
      stateMix: gl.getUniformLocation(program, 'u_state_mix'),
      input: gl.getUniformLocation(program, 'u_input'),
      output: gl.getUniformLocation(program, 'u_output'),
      absorb: gl.getUniformLocation(program, 'u_absorb'),
      voicePhase: gl.getUniformLocation(program, 'u_voice_phase'),
      detail: gl.getUniformLocation(program, 'u_detail'),
    },
  }
}

function disposeResources(gl: GL, resources: GlResources | null) {
  if (!resources) return
  gl.deleteTexture(resources.detailTexture)
  gl.deleteBuffer(resources.position)
  gl.deleteProgram(resources.program)
}

function drawStaticFallback(canvas: HTMLCanvasElement, state: OrbState) {
  const context = canvas.getContext('2d')
  if (!context) return
  const ratio = Math.min(window.devicePixelRatio || 1, 2)
  const bounds = canvas.getBoundingClientRect()
  canvas.width = Math.max(1, Math.round(bounds.width * ratio))
  canvas.height = Math.max(1, Math.round(bounds.height * ratio))
  context.setTransform(ratio, 0, 0, ratio, 0, 0)
  context.clearRect(0, 0, bounds.width, bounds.height)
  const cx = bounds.width / 2
  const cy = bounds.height / 2
  const radius = Math.min(bounds.width, bounds.height) * SPHERE_RADIUS
  const accent = state === 'offline' ? '125, 154, 170' : '47, 203, 255'
  const halo = context.createRadialGradient(cx, cy, radius, cx, cy, radius * 1.4)
  halo.addColorStop(0, `rgba(${accent}, .35)`)
  halo.addColorStop(1, `rgba(${accent}, 0)`)
  context.fillStyle = halo
  context.fillRect(0, 0, bounds.width, bounds.height)
  const body = context.createRadialGradient(
    cx - radius * 0.3,
    cy - radius * 0.35,
    0,
    cx,
    cy,
    radius,
  )
  body.addColorStop(0, state === 'offline' ? '#2a3a42' : '#1d6f9a')
  body.addColorStop(0.6, '#071d31')
  body.addColorStop(1, '#020812')
  context.fillStyle = body
  context.beginPath()
  context.arc(cx, cy, radius, 0, Math.PI * 2)
  context.fill()
  context.strokeStyle = `rgba(${accent}, .75)`
  context.lineWidth = 2
  context.stroke()
}

export default function AssistantOrb({
  state,
  audioLevels,
  currentTopic,
  expanded,
  absorbing,
}: AssistantOrbProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const shellRef = useRef<HTMLDivElement>(null)
  const stateRef = useRef(state)
  const previousStateRef = useRef(state)
  const stateChangedAt = useRef(0)
  const absorbingRef = useRef(absorbing)
  const absorbStartedAt = useRef(0)
  const redrawRef = useRef<(() => void) | null>(null)

  useEffect(() => {
    previousStateRef.current = stateRef.current
    stateRef.current = state
    stateChangedAt.current = performance.now()
    redrawRef.current?.()
  }, [state])

  useEffect(() => {
    absorbingRef.current = absorbing
    if (absorbing) absorbStartedAt.current = performance.now()
    redrawRef.current?.()
  }, [absorbing])

  useEffect(() => {
    const canvas = canvasRef.current
    const shell = shellRef.current
    if (!canvas || !shell) return

    const contextOptions: WebGLContextAttributes = {
      // Transparent: the halo the shader draws past the sphere blends
      // straight onto the page.
      alpha: true,
      antialias: true,
      depth: false,
      premultipliedAlpha: true,
      preserveDrawingBuffer: false,
      powerPreference: 'high-performance',
      stencil: false,
    }
    const webgl2 = canvas.getContext('webgl2', contextOptions)
    const gl = (webgl2 ??
      canvas.getContext('webgl', contextOptions) ??
      canvas.getContext('experimental-webgl', contextOptions)) as GL | null
    if (!gl) {
      drawStaticFallback(canvas, stateRef.current)
      const observer = new ResizeObserver(() => drawStaticFallback(canvas, stateRef.current))
      observer.observe(canvas)
      redrawRef.current = () => drawStaticFallback(canvas, stateRef.current)
      return () => {
        observer.disconnect()
        redrawRef.current = null
      }
    }

    const usingWebgl2 = Boolean(webgl2)
    shell.dataset.renderer = usingWebgl2 ? 'webgl2' : 'webgl1'
    const motionQuery = window.matchMedia('(prefers-reduced-motion: reduce)')
    let resources: GlResources | null = null
    let frame = 0
    let lost = false
    let visible = !document.hidden
    let smoothIn = 0
    let smoothOut = 0
    let voicePhase = 0
    let lastFrame = performance.now()

    const resize = () => {
      const bounds = canvas.getBoundingClientRect()
      const ratio = Math.min(window.devicePixelRatio || 1, 2)
      const drawingWidth = Math.max(1, Math.round(bounds.width * ratio))
      const drawingHeight = Math.max(1, Math.round(bounds.height * ratio))
      if (canvas.width !== drawingWidth || canvas.height !== drawingHeight) {
        canvas.width = drawingWidth
        canvas.height = drawingHeight
      }
      gl.viewport(0, 0, drawingWidth, drawingHeight)
    }

    const render = (now: number) => {
      if (lost || !resources) return
      const reducedMotion = motionQuery.matches
      const input = reducedMotion ? 0 : Math.max(0, Math.min(1, audioLevels.input()))
      const output = reducedMotion ? 0 : Math.max(0, Math.min(1, audioLevels.output()))
      // Fast attack, slow release: the orb jumps with each syllable and
      // settles smoothly. `voicePhase` advances with loudness, so the rim
      // ripples travel faster the louder the voice.
      const dt = Math.min(0.1, Math.max(0, (now - lastFrame) / 1000))
      lastFrame = now
      const follow = (current: number, target: number) =>
        current + (target - current) * (1 - Math.exp(-dt * (target > current ? 28 : 6)))
      smoothIn = follow(smoothIn, input)
      smoothOut = follow(smoothOut, output)
      voicePhase = (voicePhase + dt * (0.4 + Math.max(smoothIn, smoothOut) * 5)) % 1000
      const absorb = absorbingRef.current
        ? Math.max(0.001, Math.min(0.999, (now - absorbStartedAt.current) / ABSORB_DURATION_MS))
        : 0

      const stateProgress = reducedMotion
        ? 1
        : Math.max(0, Math.min(1, (now - stateChangedAt.current) / 400))
      const stateMix = stateProgress * stateProgress * (3 - 2 * stateProgress)
      gl.uniform2f(resources.uniforms.resolution, canvas.width, canvas.height)
      gl.uniform1f(resources.uniforms.time, reducedMotion ? 0 : now * 0.001)
      gl.uniform1f(resources.uniforms.state, STATE_VALUE[stateRef.current])
      gl.uniform1f(resources.uniforms.previousState, STATE_VALUE[previousStateRef.current])
      gl.uniform1f(resources.uniforms.stateMix, stateMix)
      gl.uniform1f(resources.uniforms.input, smoothIn)
      gl.uniform1f(resources.uniforms.output, smoothOut)
      gl.uniform1f(resources.uniforms.voicePhase, voicePhase)
      gl.uniform1f(resources.uniforms.absorb, reducedMotion ? 0 : absorb)
      gl.drawArrays(gl.TRIANGLES, 0, 6)

      if (visible && !reducedMotion) frame = requestAnimationFrame(render)
    }

    const start = () => {
      cancelAnimationFrame(frame)
      if (lost) return
      resize()
      render(performance.now())
    }

    const initialize = () => {
      disposeResources(gl, resources)
      resources = createResources(gl, usingWebgl2)
      gl.disable(gl.BLEND)
      gl.disable(gl.DEPTH_TEST)
      gl.disable(gl.CULL_FACE)
      gl.useProgram(resources.program)
      gl.bindBuffer(gl.ARRAY_BUFFER, resources.position)
      gl.enableVertexAttribArray(resources.positionLocation)
      gl.vertexAttribPointer(resources.positionLocation, 2, gl.FLOAT, false, 0, 0)
      gl.activeTexture(gl.TEXTURE0)
      gl.bindTexture(gl.TEXTURE_2D, resources.detailTexture)
      gl.uniform1i(resources.uniforms.detail, 0)
      start()
    }

    const onVisibility = () => {
      visible = !document.hidden
      if (visible) start()
      else cancelAnimationFrame(frame)
    }
    const onMotionChange = () => start()
    const onContextLost = (event: Event) => {
      event.preventDefault()
      lost = true
      cancelAnimationFrame(frame)
      resources = null
    }
    const onContextRestored = () => {
      lost = false
      initialize()
    }

    const observer = new ResizeObserver(start)
    observer.observe(canvas)
    document.addEventListener('visibilitychange', onVisibility)
    motionQuery.addEventListener('change', onMotionChange)
    canvas.addEventListener('webglcontextlost', onContextLost)
    canvas.addEventListener('webglcontextrestored', onContextRestored)
    redrawRef.current = start

    try {
      initialize()
    } catch (error) {
      console.error('Unable to initialize the Assistant orb renderer', error)
      shell.dataset.renderer = 'canvas-2d-fallback'
      disposeResources(gl, resources)
      drawStaticFallback(canvas, stateRef.current)
    }

    return () => {
      cancelAnimationFrame(frame)
      observer.disconnect()
      document.removeEventListener('visibilitychange', onVisibility)
      motionQuery.removeEventListener('change', onMotionChange)
      canvas.removeEventListener('webglcontextlost', onContextLost)
      canvas.removeEventListener('webglcontextrestored', onContextRestored)
      if (!lost) disposeResources(gl, resources)
      redrawRef.current = null
    }
  }, [audioLevels])

  return (
    <div
      ref={shellRef}
      className="assistant-orb"
      data-testid="assistant-orb"
      data-state={state}
      data-expanded={expanded ? 'true' : undefined}
      data-absorbing={absorbing ? 'true' : undefined}
      role="img"
      aria-label={STATE_LABEL[state]}
    >
      <canvas ref={canvasRef} className="assistant-orb-canvas" aria-hidden />
      {currentTopic && (
        <div className="assistant-current-topic" data-testid="assistant-current-topic">
          <span className="assistant-current-kicker">Current focus</span>
          <strong>{currentTopic.title}</strong>
          {currentTopic.summary && <span>{currentTopic.summary}</span>}
        </div>
      )}
    </div>
  )
}
